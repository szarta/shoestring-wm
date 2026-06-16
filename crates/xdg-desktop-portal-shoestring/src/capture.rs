//! WM-facing frame source: a Wayland client that captures an output via
//! `zwlr_screencopy_v1` into a reusable shm buffer.
//!
//! This is the client half of the protocol the compositor implements in
//! shoestring-wm's `src/screencopy.rs`. We hold one connection to the WM and,
//! on demand, capture the target output synchronously (request + blocking
//! dispatch until `ready`), leaving the pixels in a persistent mmap that the
//! PipeWire `process` callback copies out. Reusing one pool/buffer across
//! captures matters: per-capture allocation would leak WM-side fds (the same
//! bug that once crashed the bar — see the shoestring-bar history).
//!
//! Capture is gated by the WM's screen-capture privacy switch: when it's off
//! the `zwlr_screencopy_manager_v1` global is absent, so [`Capture::new`]
//! fails with a clear message (the user runs `shoestring-ctl screen-capture
//! on`). Adapted from the one-shot `shoestring-screenshot` binary.

use anyhow::{anyhow, Context, Result};
use memmap2::MmapMut;
use wayland_client::{
    globals::{registry_queue_init, GlobalListContents},
    protocol::{
        wl_buffer::WlBuffer,
        wl_output::{self, WlOutput},
        wl_registry::WlRegistry,
        wl_shm::{self, WlShm},
        wl_shm_pool::WlShmPool,
    },
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum,
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, Flags, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

use std::os::fd::AsFd;

/// Geometry + pixel layout of a captured frame.
#[derive(Debug, Clone, Copy)]
pub struct FrameInfo {
    pub width: u32,
    pub height: u32,
    /// Bytes per row in the captured buffer (may exceed `width * 4`).
    pub stride: u32,
    pub format: wl_shm::Format,
    /// True when rows are stored bottom-to-top (WM renders through a GL FBO).
    pub yinvert: bool,
}

/// A persistent screencast frame source bound to one output.
pub struct Capture {
    // `conn` must outlive the proxies; keep it even though it's not read after
    // setup.
    _conn: Connection,
    queue: EventQueue<CaptureState>,
    qh: QueueHandle<CaptureState>,
    manager: ZwlrScreencopyManagerV1,
    shm: WlShm,
    output: WlOutput,
    state: CaptureState,
    buf: Option<Buf>,
}

/// Reused shm buffer the WM copies into. Dropped/recreated only if the output
/// geometry changes.
struct Buf {
    _tmp: std::fs::File,
    mmap: MmapMut,
    pool: WlShmPool,
    buffer: WlBuffer,
    info: FrameInfo,
}

impl Capture {
    /// Connect to the WM and bind the screencopy manager + a target output.
    /// `output_name` picks by `wl_output.name`; `None` uses the first output.
    pub fn new(output_name: Option<&str>) -> Result<Self> {
        let conn =
            Connection::connect_to_env().context("connect to wayland (is WAYLAND_DISPLAY set?)")?;
        let (globals, mut queue) =
            registry_queue_init::<CaptureState>(&conn).context("registry init")?;
        let qh = queue.handle();

        let shm: WlShm = globals
            .bind(&qh, 1..=2, ())
            .context("wl_shm global missing")?;
        let manager: ZwlrScreencopyManagerV1 = globals.bind(&qh, 1..=3, ()).context(
            "zwlr_screencopy_manager_v1 missing — screen capture is disabled \
             (run `shoestring-ctl screen-capture on`)",
        )?;

        let outputs: Vec<WlOutput> = globals.contents().with_list(|list| {
            list.iter()
                .filter(|g| g.interface == "wl_output")
                .map(|g| {
                    globals
                        .registry()
                        .bind::<WlOutput, _, _>(g.name, g.version.min(4), &qh, ())
                })
                .collect()
        });

        let mut state = CaptureState {
            outputs: outputs
                .into_iter()
                .map(|o| OutputEntry {
                    name: None,
                    output: o,
                })
                .collect(),
            frame: FrameState::Initial,
        };
        // Resolve output names (wl_output.name, v4+) before matching.
        queue
            .roundtrip(&mut state)
            .context("output info roundtrip")?;

        let output = pick_output(&state.outputs, output_name)?;

        Ok(Self {
            _conn: conn,
            queue,
            qh,
            manager,
            shm,
            output,
            state,
            buf: None,
        })
    }

    /// Capture one frame into the reusable buffer, blocking until the WM
    /// reports `ready`. Afterwards [`Self::frame`] returns the pixels.
    pub fn capture(&mut self) -> Result<()> {
        self.state.frame = FrameState::Initial;
        let frame = self.manager.capture_output(1, &self.output, &self.qh, ());

        // Wait for the buffer-format advertisement (or failure).
        let (format, width, height, stride) = loop {
            self.queue.blocking_dispatch(&mut self.state)?;
            match &self.state.frame {
                FrameState::Initial => continue,
                FrameState::BufferParams {
                    format,
                    width,
                    height,
                    stride,
                } => break (*format, *width, *height, *stride),
                FrameState::Failed(e) => {
                    frame.destroy();
                    return Err(anyhow!("capture failed: {e}"));
                }
                FrameState::Ready { .. } => continue,
            }
        };

        self.ensure_buf(format, width, height, stride);
        let Some(buf) = self.buf.as_ref() else {
            frame.destroy();
            return Err(anyhow!("no capture buffer"));
        };

        frame.copy(&buf.buffer);
        let yinvert = loop {
            self.queue.blocking_dispatch(&mut self.state)?;
            match &self.state.frame {
                FrameState::Ready { flags, .. } => break flags.contains(Flags::YInvert),
                FrameState::Failed(e) => {
                    frame.destroy();
                    return Err(anyhow!("capture failed: {e}"));
                }
                _ => continue,
            }
        };
        frame.destroy();

        if let Some(buf) = self.buf.as_mut() {
            buf.info.yinvert = yinvert;
        }
        Ok(())
    }

    /// The most recently captured frame: its layout and the raw shm pixels.
    pub fn frame(&self) -> Option<(FrameInfo, &[u8])> {
        self.buf.as_ref().map(|b| (b.info, &b.mmap[..]))
    }

    /// Capture once just to learn the output geometry (used before creating the
    /// PipeWire stream so its format matches).
    pub fn dimensions(&mut self) -> Result<FrameInfo> {
        self.capture()?;
        self.buf
            .as_ref()
            .map(|b| b.info)
            .ok_or_else(|| anyhow!("no frame captured"))
    }

    /// (Re)allocate the shm buffer if missing or if the geometry changed.
    fn ensure_buf(&mut self, format: wl_shm::Format, width: u32, height: u32, stride: u32) {
        let matches = self.buf.as_ref().is_some_and(|b| {
            b.info.width == width
                && b.info.height == height
                && b.info.stride == stride
                && b.info.format == format
        });
        if matches {
            return;
        }
        // Drop the old buffer first so its WM-side resources are released.
        if let Some(old) = self.buf.take() {
            old.buffer.destroy();
            old.pool.destroy();
        }
        let size = stride as u64 * height as u64;
        let tmp = match tempfile::tempfile().and_then(|f| {
            f.set_len(size)?;
            Ok(f)
        }) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(error = %e, "capture: shm tempfile failed");
                return;
            }
        };
        let pool = self.shm.create_pool(tmp.as_fd(), size as i32, &self.qh, ());
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            stride as i32,
            format,
            &self.qh,
            (),
        );
        let mmap = match unsafe { MmapMut::map_mut(&tmp) } {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(error = %e, "capture: mmap failed");
                buffer.destroy();
                pool.destroy();
                return;
            }
        };
        self.buf = Some(Buf {
            _tmp: tmp,
            mmap,
            pool,
            buffer,
            info: FrameInfo {
                width,
                height,
                stride,
                format,
                yinvert: false,
            },
        });
    }
}

fn pick_output(outputs: &[OutputEntry], wanted: Option<&str>) -> Result<WlOutput> {
    if let Some(name) = wanted {
        outputs
            .iter()
            .find(|o| o.name.as_deref() == Some(name))
            .map(|o| o.output.clone())
            .ok_or_else(|| {
                let avail: Vec<&str> = outputs.iter().filter_map(|o| o.name.as_deref()).collect();
                anyhow!("no output named {name:?}; available: {avail:?}")
            })
    } else {
        outputs
            .first()
            .map(|o| o.output.clone())
            .ok_or_else(|| anyhow!("compositor advertised no outputs"))
    }
}

// ---- wayland dispatch glue (mirrors shoestring-screenshot) ----------------

struct OutputEntry {
    name: Option<String>,
    output: WlOutput,
}

enum FrameState {
    Initial,
    BufferParams {
        format: wl_shm::Format,
        width: u32,
        height: u32,
        stride: u32,
    },
    Ready {
        flags: Flags,
        #[allow(dead_code)]
        tv_sec: u64,
        #[allow(dead_code)]
        tv_nsec: u32,
    },
    Failed(String),
}

struct CaptureState {
    outputs: Vec<OutputEntry>,
    frame: FrameState,
}

impl Dispatch<WlRegistry, GlobalListContents> for CaptureState {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlOutput, ()> for CaptureState {
    fn event(
        state: &mut Self,
        proxy: &WlOutput,
        event: <WlOutput as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            for entry in state.outputs.iter_mut() {
                if entry.output.id() == proxy.id() {
                    entry.name = Some(name);
                    break;
                }
            }
        }
    }
}

macro_rules! ignore_events {
    ($($t:ty),+ $(,)?) => {$(
        impl Dispatch<$t, ()> for CaptureState {
            fn event(
                _: &mut Self,
                _: &$t,
                _: <$t as Proxy>::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }
    )+};
}
ignore_events!(WlShm, WlShmPool, WlBuffer, ZwlrScreencopyManagerV1);

impl Dispatch<ZwlrScreencopyFrameV1, ()> for CaptureState {
    fn event(
        state: &mut Self,
        _: &ZwlrScreencopyFrameV1,
        event: <ZwlrScreencopyFrameV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_screencopy_frame_v1::Event;
        match event {
            Event::Buffer {
                format,
                width,
                height,
                stride,
            } => {
                let format = match format {
                    WEnum::Value(f) => f,
                    WEnum::Unknown(v) => {
                        state.frame = FrameState::Failed(format!("unknown wl_shm format {v}"));
                        return;
                    }
                };
                state.frame = FrameState::BufferParams {
                    format,
                    width,
                    height,
                    stride,
                };
            }
            // `flags` arrives just before `ready`; latch it, finalise on ready.
            Event::Flags { flags } => {
                let flags = match flags {
                    WEnum::Value(f) => f,
                    WEnum::Unknown(_) => Flags::empty(),
                };
                state.frame = FrameState::Ready {
                    flags,
                    tv_sec: 0,
                    tv_nsec: 0,
                };
            }
            Event::Ready {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
            } => {
                let tv_sec = ((tv_sec_hi as u64) << 32) | tv_sec_lo as u64;
                let flags = match &state.frame {
                    FrameState::Ready { flags, .. } => *flags,
                    _ => Flags::empty(),
                };
                state.frame = FrameState::Ready {
                    flags,
                    tv_sec,
                    tv_nsec,
                };
            }
            Event::Failed => {
                state.frame = FrameState::Failed("compositor sent failed".into());
            }
            _ => {}
        }
    }
}
