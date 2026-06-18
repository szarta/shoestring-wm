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
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
    zwp_linux_dmabuf_v1::{self, ZwpLinuxDmabufV1},
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, Flags, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

use drm_fourcc::{DrmFourcc, DrmModifier};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::os::fd::{AsFd, OwnedFd};
use std::path::PathBuf;

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
    /// Whether to composite the cursor into captured frames (the screencopy
    /// `overlay_cursor` arg). True ⇒ EMBEDDED cursor mode; false ⇒ HIDDEN.
    overlay_cursor: bool,
    /// `zwp_linux_dmabuf_v1` (if the WM advertises it), for wrapping gbm buffers
    /// as wl_buffers. `None` ⇒ no dmabuf path, shm only.
    dmabuf: Option<ZwpLinuxDmabufV1>,
    /// gbm device on the render node, opened lazily on first dmabuf allocation.
    gbm: Option<gbm::Device<File>>,
}

/// A connected output, for the screencast chooser. Just the connector name for
/// now (the chooser selects by name); per-output scale for color-pick mapping
/// is read separately via [`Capture::current_scale`].
#[derive(Debug, Clone)]
pub struct OutputInfo {
    pub name: String,
}

/// A GPU buffer that backs one PipeWire dmabuf buffer: the gbm allocation (whose
/// `OwnedFd` keeps the dmabuf alive), the screencopy target `wl_buffer` wrapping
/// it, and the single-plane layout PipeWire needs. Single-plane only (our
/// XRGB/ARGB formats); multi-plane allocations are rejected back to shm.
pub struct DmabufSlot {
    _bo: gbm::BufferObject<()>,
    wl_buffer: WlBuffer,
    /// Owned dmabuf fd (plane 0). Borrowed by PipeWire's `spa_data`; stays alive
    /// as long as the slot does.
    pub fd: OwnedFd,
    pub width: u32,
    pub height: u32,
    pub modifier: u64,
    pub offset: u32,
    pub stride: u32,
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

        // zwp_linux_dmabuf_v1 is optional. Bind v3 so the global emits its
        // `format`/`modifier` events during the roundtrip below; we collect them
        // to allocate gbm buffers with a modifier the WM can import. Absent ⇒
        // the dmabuf path is simply never offered.
        let dmabuf: Option<ZwpLinuxDmabufV1> = globals.bind(&qh, 1..=3, ()).ok();
        if dmabuf.is_none() {
            tracing::info!("zwp_linux_dmabuf_v1 absent — screencast will be shm-only");
        }

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
                    scale: 1,
                    output: o,
                })
                .collect(),
            frame: FrameState::Initial,
            dmabuf_formats: HashMap::new(),
        };
        // Resolve output names + geometry (wl_output.name/mode/scale) before
        // matching.
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
            overlay_cursor: true,
            dmabuf,
            gbm: None,
        })
    }

    /// The connected outputs (those whose `wl_output.name` resolved), for the
    /// screencast chooser.
    pub fn outputs(&self) -> Vec<OutputInfo> {
        self.state
            .outputs
            .iter()
            .filter_map(|o| {
                o.name
                    .as_ref()
                    .map(|name| OutputInfo { name: name.clone() })
            })
            .collect()
    }

    /// Switch the capture target to the output named `name`. Drops any cached
    /// buffer (the new output may differ in size). Errors if no such output.
    pub fn set_output(&mut self, name: &str) -> Result<()> {
        let output = pick_output(&self.state.outputs, Some(name))?;
        self.output = output;
        if let Some(old) = self.buf.take() {
            old.buffer.destroy();
            old.pool.destroy();
        }
        Ok(())
    }

    /// Integer scale of the current capture target (1 if unknown). Used to map a
    /// logical pick coordinate to a physical pixel in the captured buffer.
    pub fn current_scale(&self) -> i32 {
        let id = self.output.id();
        self.state
            .outputs
            .iter()
            .find(|o| o.output.id() == id)
            .map(|o| o.scale.max(1))
            .unwrap_or(1)
    }

    /// Set whether captured frames composite the cursor (screencopy
    /// `overlay_cursor`). EMBEDDED cursor mode ⇒ true; HIDDEN ⇒ false.
    pub fn set_overlay_cursor(&mut self, on: bool) {
        self.overlay_cursor = on;
    }

    /// Capture one frame into the reusable buffer, blocking until the WM
    /// reports `ready`. Afterwards [`Self::frame`] returns the pixels.
    pub fn capture(&mut self) -> Result<()> {
        self.state.frame = FrameState::Initial;
        let overlay = self.overlay_cursor as i32;
        let frame = self
            .manager
            .capture_output(overlay, &self.output, &self.qh, ());

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

    /// Whether the dmabuf path can be offered for `fourcc`: the WM bound
    /// `zwp_linux_dmabuf_v1` and advertised at least one modifier for it.
    pub fn dmabuf_supported(&self, fourcc: u32) -> bool {
        self.dmabuf.is_some() && self.state.dmabuf_formats.contains_key(&fourcc)
    }

    /// Allocate a gbm buffer for `(width, height, fourcc)` and wrap it as a
    /// `wl_buffer` the WM can render into via screencopy. The returned slot owns
    /// the gbm bo (keeping the dmabuf fd alive) and the wl_buffer.
    pub fn alloc_dmabuf(&mut self, width: u32, height: u32, fourcc: u32) -> Result<DmabufSlot> {
        let dmabuf = self
            .dmabuf
            .clone()
            .ok_or_else(|| anyhow!("zwp_linux_dmabuf_v1 unavailable"))?;
        let format =
            DrmFourcc::try_from(fourcc).map_err(|_| anyhow!("unknown DRM fourcc {fourcc:#x}"))?;

        // Always allocate LINEAR: it's single-plane, CPU-mappable, and every
        // driver can import + sample it — exactly what we need for a screencast
        // buffer a foreign consumer (Chrome's bundled Mesa, OBS, …) must read.
        // Letting gbm pick freely tends to choose a compressed/tiled vendor
        // modifier with auxiliary planes that arbitrary consumers can't import.
        // Prefer an explicit LINEAR allocation when the WM advertised that
        // modifier (so we know it'll accept the buffer); otherwise force linear
        // via the usage flag.
        let linear = u64::from(DrmModifier::Linear);
        let wm_advertises_linear = self
            .state
            .dmabuf_formats
            .get(&fourcc)
            .is_some_and(|mods| mods.contains(&linear));

        self.ensure_gbm()?;
        let dev = self.gbm.as_ref().expect("gbm device opened");
        let bo: gbm::BufferObject<()> = if wm_advertises_linear {
            dev.create_buffer_object_with_modifiers::<()>(
                width,
                height,
                format,
                std::iter::once(DrmModifier::Linear),
            )
            .context("gbm_bo_create_with_modifiers(LINEAR)")?
        } else {
            dev.create_buffer_object::<()>(
                width,
                height,
                format,
                gbm::BufferObjectFlags::RENDERING | gbm::BufferObjectFlags::LINEAR,
            )
            .context("gbm_bo_create(LINEAR)")?
        };

        if bo.plane_count() != 1 {
            anyhow::bail!(
                "dmabuf has {} planes; only single-plane formats supported",
                bo.plane_count()
            );
        }
        let fd = bo.fd().context("export dmabuf fd")?;
        let stride = bo.stride();
        let offset = bo.offset(0);
        let modifier = u64::from(bo.modifier());

        // Wrap the dmabuf as a wl_buffer. create_immed returns it synchronously;
        // a bad import would arrive later as a `failed` event on the params (rare
        // — we allocate with a modifier the WM advertised).
        let params = dmabuf.create_params(&self.qh, ());
        params.add(
            fd.as_fd(),
            0,
            offset,
            stride,
            (modifier >> 32) as u32,
            (modifier & 0xffff_ffff) as u32,
        );
        let wl_buffer = params.create_immed(
            width as i32,
            height as i32,
            fourcc,
            zwp_linux_buffer_params_v1::Flags::empty(),
            &self.qh,
            (),
        );
        params.destroy();
        self.queue.flush().ok();

        tracing::debug!(width, height, fourcc, modifier, "allocated dmabuf slot");
        Ok(DmabufSlot {
            _bo: bo,
            wl_buffer,
            fd,
            width,
            height,
            modifier,
            offset,
            stride,
        })
    }

    /// Capture the output straight into a dmabuf slot's wl_buffer (zero-copy: the
    /// WM renders into the same dmabuf PipeWire will read). Returns the YInvert
    /// flag — the WM's dmabuf path renders bottom-up, so this is normally true and
    /// the caller must signal a vertical flip to the consumer.
    pub fn capture_into_dmabuf(&mut self, slot: &DmabufSlot) -> Result<bool> {
        self.state.frame = FrameState::Initial;
        let overlay = self.overlay_cursor as i32;
        let frame = self
            .manager
            .capture_output(overlay, &self.output, &self.qh, ());

        // Wait until the WM has advertised buffer params (it sends the shm
        // `buffer` event alongside `linux_dmabuf`); that's the cue it'll accept a
        // copy. We then attach our dmabuf buffer rather than the shm one.
        loop {
            self.queue.blocking_dispatch(&mut self.state)?;
            match &self.state.frame {
                FrameState::BufferParams { .. } => break,
                FrameState::Failed(e) => {
                    frame.destroy();
                    return Err(anyhow!("capture failed: {e}"));
                }
                _ => continue,
            }
        }

        frame.copy(&slot.wl_buffer);
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
        Ok(yinvert)
    }

    /// Open the render node's gbm device, once.
    fn ensure_gbm(&mut self) -> Result<()> {
        if self.gbm.is_some() {
            return Ok(());
        }
        let path = render_node_path()?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("open render node {}", path.display()))?;
        let dev = gbm::Device::new(file).context("gbm_create_device")?;
        tracing::info!(node = %path.display(), "opened gbm device for dmabuf screencast");
        self.gbm = Some(dev);
        Ok(())
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

/// Developer self-test for the dmabuf capture path (not used in normal
/// operation): allocate a dmabuf, have the WM render the output into it, CPU-map
/// it via gbm, and write a PNG so the result can be eyeballed. Proves the
/// modifier negotiation + WM dmabuf import + render work before the PipeWire
/// stream depends on them. Reached via the hidden `--selftest-dmabuf` flag.
pub fn selftest_dmabuf(output: Option<&str>, png_path: &std::path::Path) -> Result<()> {
    let mut cap = Capture::new(output)?;
    let info = cap.dimensions()?;
    let fourcc = DrmFourcc::Xrgb8888 as u32;
    if !cap.dmabuf_supported(fourcc) {
        anyhow::bail!("WM does not advertise dmabuf for XRGB8888 (shm-only)");
    }
    let slot = cap
        .alloc_dmabuf(info.width, info.height, fourcc)
        .context("alloc dmabuf")?;
    let yinvert = cap
        .capture_into_dmabuf(&slot)
        .context("capture into dmabuf")?;
    tracing::info!(
        modifier = slot.modifier,
        wm_yinvert_flag = yinvert,
        "dmabuf capture succeeded; mapping for readback"
    );

    // CPU-map the bo (LINEAR ⇒ mappable). The WM's dmabuf YInvert flag is
    // unreliable — empirically the buffer is top-down/upright — so we read it
    // straight (no flip), which is what the screencast consumer does too.
    let png = slot
        ._bo
        .map(0, 0, slot.width, slot.height, |m| {
            encode_bgrx_png(m.buffer(), slot.width, slot.height, m.stride(), false)
        })
        .context("gbm_bo_map (modifier may not be CPU-mappable)")??;
    if let Some(parent) = png_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(png_path, &png).with_context(|| format!("write {}", png_path.display()))?;
    Ok(())
}

/// Minimal BGRx→RGBA PNG encoder for the self-test (the portal's real PNG path
/// lives in `screenshot.rs`; kept separate to avoid a cross-module dependency).
fn encode_bgrx_png(
    src: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    yinvert: bool,
) -> Result<Vec<u8>> {
    let (w, h, s) = (width as usize, height as usize, stride as usize);
    let row = w * 4;
    if s < row {
        anyhow::bail!("stride {s} < row {row}");
    }
    let mut rgba = vec![0u8; row * h];
    for y in 0..h {
        let sy = if yinvert { h - 1 - y } else { y };
        let so = sy * s;
        let doff = y * row;
        for x in 0..w {
            let p = so + x * 4;
            if p + 3 >= src.len() {
                break;
            }
            rgba[doff + x * 4] = src[p + 2];
            rgba[doff + x * 4 + 1] = src[p + 1];
            rgba[doff + x * 4 + 2] = src[p];
            rgba[doff + x * 4 + 3] = 0xFF;
        }
    }
    let mut buf = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut buf, width, height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header()?.write_image_data(&rgba)?;
    }
    Ok(buf)
}

/// The DRM render node to allocate dmabufs from. `$SHOESTRING_RENDER_NODE`
/// overrides; otherwise the lowest-numbered `/dev/dri/renderD*`.
fn render_node_path() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("SHOESTRING_RENDER_NODE") {
        return Ok(PathBuf::from(p));
    }
    let mut nodes: Vec<PathBuf> = std::fs::read_dir("/dev/dri")
        .context("read /dev/dri")?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("renderD"))
        })
        .collect();
    nodes.sort();
    nodes
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no renderD* node in /dev/dri"))
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
    /// Integer scale factor (from `wl_output.scale`); 1 until reported. Used to
    /// map a logical color-pick coordinate to a physical pixel.
    scale: i32,
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
    /// DRM fourcc → modifiers the WM's `zwp_linux_dmabuf_v1` advertised. Filled
    /// from the global's `format`/`modifier` events during the setup roundtrip.
    dmabuf_formats: HashMap<u32, Vec<u64>>,
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
        let Some(entry) = state
            .outputs
            .iter_mut()
            .find(|e| e.output.id() == proxy.id())
        else {
            return;
        };
        match event {
            wl_output::Event::Name { name } => entry.name = Some(name),
            wl_output::Event::Scale { factor } => entry.scale = factor.max(1),
            _ => {}
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
ignore_events!(
    WlShm,
    WlShmPool,
    WlBuffer,
    ZwlrScreencopyManagerV1,
    ZwpLinuxBufferParamsV1
);

impl Dispatch<ZwpLinuxDmabufV1, ()> for CaptureState {
    fn event(
        state: &mut Self,
        _: &ZwpLinuxDmabufV1,
        event: <ZwpLinuxDmabufV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwp_linux_dmabuf_v1::Event;
        match event {
            // v1 (deprecated): format only ⇒ implicit modifier.
            Event::Format { format } => {
                state
                    .dmabuf_formats
                    .entry(format)
                    .or_default()
                    .push(u64::from(DrmModifier::Invalid));
            }
            // v3: explicit (format, modifier) pairs the WM can import.
            Event::Modifier {
                format,
                modifier_hi,
                modifier_lo,
            } => {
                let modifier = ((modifier_hi as u64) << 32) | modifier_lo as u64;
                state
                    .dmabuf_formats
                    .entry(format)
                    .or_default()
                    .push(modifier);
            }
            _ => {}
        }
    }
}

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
