//! PipeWire side of the screencast backend.
//!
//! We hold one connection to the PipeWire daemon ([`Pw`]) and create one
//! output [`Cast`] (a `pw_stream`) per active session. The stream advertises a
//! video format; once a consumer (the sharing app, via the portal frontend)
//! connects and negotiates, the `process` callback fills buffers and queues
//! them.
//!
//! Phase 1b fills a **test pattern over shm** (MemFd/MemPtr) — enough to prove
//! the whole pipe end to end (D-Bus handshake → node id → consumer → frames).
//! Phase 2 swaps the test pattern for real frames captured from the WM via
//! `zwlr_screencopy_v1`; Phase 3 adds dmabuf so fast consumers stay zero-copy.
//!
//! The PipeWire loop is *not* run on its own thread: its fd is added to the
//! backend's `poll(2)` set and [`Pw::iterate`] is pumped from there, keeping
//! everything single-threaded alongside D-Bus.

use std::io::Cursor;
use std::os::fd::{AsRawFd, IntoRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use pipewire as pw;
use pw::loop_::Timeout;
use pw::properties::properties;
use pw::spa;
use pw::stream::{StreamFlags, StreamListener, StreamRc};
use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use spa::param::format_utils::parse_format;
use spa::param::video::{VideoFormat, VideoInfoRaw};
use spa::param::ParamType;
use spa::pod::{serialize::PodSerializer, Object, Pod, Property, Value};
use spa::utils::{Direction, Fraction, Rectangle, SpaTypes};

/// Long-lived PipeWire connection shared by every cast.
pub struct Pw {
    main_loop: pw::main_loop::MainLoopRc,
    _context: pw::context::ContextRc,
    core: pw::core::CoreRc,
}

impl Pw {
    /// Connect to the PipeWire daemon. `pipewire::init()` must already have run.
    pub fn new() -> Result<Self> {
        let main_loop =
            pw::main_loop::MainLoopRc::new(None).map_err(|e| anyhow!("pw main loop: {e}"))?;
        let context = pw::context::ContextRc::new(&main_loop, None)
            .map_err(|e| anyhow!("pw context: {e}"))?;
        let core = context
            .connect_rc(None)
            .map_err(|e| anyhow!("pw connect: {e}"))?;
        Ok(Self {
            main_loop,
            _context: context,
            core,
        })
    }

    /// The PipeWire loop's fd, for the backend's `poll(2)` set.
    pub fn loop_fd(&self) -> RawFd {
        self.main_loop.loop_().fd().as_raw_fd()
    }

    /// Pump the PipeWire loop once, non-blocking. Call when [`Self::loop_fd`]
    /// is readable.
    pub fn iterate(&self) {
        self.main_loop.loop_().iterate(Timeout::None);
    }
}

/// Per-session capture state held by the daemon. Filled in the stream
/// callbacks; the test pattern's frame counter and the negotiated geometry
/// live here. Phase 2 adds the latest captured frame.
#[derive(Default)]
struct StreamData {
    width: u32,
    height: u32,
    have_format: bool,
    frame: u64,
}

/// One active screencast: the output stream plus its listener. Dropping it
/// tears the stream down.
pub struct Cast {
    _stream: StreamRc,
    _listener: StreamListener<StreamData>,
}

/// Create + connect an output stream of the given size, returning the cast and
/// its PipeWire node id (the value handed back to the app in Start's
/// `results["streams"]`). Pumps the loop until the node id is assigned.
pub fn start(pw: &Pw, width: u32, height: u32) -> Result<(Cast, u32)> {
    let stream = StreamRc::new(
        pw.core.clone(),
        "shoestring-screencast",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(|e| anyhow!("pw stream new: {e}"))?;

    let listener = stream
        .add_local_listener_with_user_data(StreamData::default())
        .state_changed(|_, _, old, new| {
            tracing::debug!(?old, ?new, "screencast stream state");
        })
        .param_changed(|stream, data, id, param| {
            let Some(param) = param else { return };
            if id != ParamType::Format.as_raw() {
                return;
            }
            let Ok((mt, ms)) = parse_format(param) else {
                return;
            };
            if mt != MediaType::Video || ms != MediaSubtype::Raw {
                return;
            }
            let mut info = VideoInfoRaw::default();
            if info.parse(param).is_err() {
                return;
            }
            data.width = info.size().width;
            data.height = info.size().height;
            data.have_format = data.width > 0 && data.height > 0;
            tracing::info!(
                w = data.width,
                h = data.height,
                "screencast format negotiated"
            );

            // Tell PipeWire how big our shm buffers must be for this format.
            let stride = (data.width * 4) as i32;
            let size = stride * data.height as i32;
            let bytes = buffers_param(stride, size);
            if let Some(pod) = Pod::from_bytes(&bytes) {
                if let Err(e) = stream.update_params(&mut [pod]) {
                    tracing::warn!(error = %e, "update_params(Buffers) failed");
                }
            }
        })
        .process(|stream, data| {
            if !data.have_format {
                return;
            }
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }
            data.frame = data.frame.wrapping_add(1);
            let (w, h, frame) = (data.width, data.height, data.frame);
            let stride = (w * 4) as i32;
            let d = &mut datas[0];
            let filled = match d.data() {
                Some(buf) => fill_test_pattern(buf, w, h, frame),
                None => 0,
            };
            let chunk = d.chunk_mut();
            *chunk.size_mut() = filled as u32;
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = stride;
        })
        .register()
        .map_err(|e| anyhow!("pw stream register: {e}"))?;

    // Offer a single fixed format: BGRx (4 bytes/px), the requested size, 30fps.
    let format_obj = spa::pod::object!(
        SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        spa::pod::property!(FormatProperties::VideoFormat, Id, VideoFormat::BGRx),
        spa::pod::property!(
            FormatProperties::VideoSize,
            Rectangle,
            Rectangle { width, height }
        ),
        spa::pod::property!(
            FormatProperties::VideoFramerate,
            Fraction,
            Fraction { num: 30, denom: 1 }
        ),
    );
    let format_bytes = pod_bytes(&Value::Object(format_obj));
    let mut params = [Pod::from_bytes(&format_bytes).context("format pod")?];

    stream
        .connect(
            Direction::Output,
            None,
            StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(|e| anyhow!("pw stream connect: {e}"))?;

    // node_id is assigned asynchronously; pump the loop until it appears.
    let mut tries = 0;
    while stream.node_id() == pw::constants::ID_ANY && tries < 100 {
        pw.main_loop
            .loop_()
            .iterate(Timeout::Finite(Duration::from_millis(10)));
        tries += 1;
    }
    let node_id = stream.node_id();
    if node_id == pw::constants::ID_ANY {
        return Err(anyhow!("PipeWire node id was not assigned"));
    }
    tracing::info!(node_id, width, height, "screencast stream started");

    Ok((
        Cast {
            _stream: stream,
            _listener: listener,
        },
        node_id,
    ))
}

/// Open a fresh connection to the PipeWire daemon and hand its fd to the
/// caller (for the portal's `OpenPipeWireRemote`). The app drives the PipeWire
/// protocol on it from scratch via `pw_context_connect_fd`.
pub fn open_remote_fd() -> Result<RawFd> {
    let runtime = std::env::var("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR unset")?;
    let remote = std::env::var("PIPEWIRE_REMOTE").unwrap_or_else(|_| "pipewire-0".to_owned());
    let path = format!("{runtime}/{remote}");
    let sock = UnixStream::connect(&path).with_context(|| format!("connect {path}"))?;
    Ok(sock.into_raw_fd())
}

// ---- pod helpers ---------------------------------------------------------

fn pod_bytes(value: &Value) -> Vec<u8> {
    PodSerializer::serialize(Cursor::new(Vec::new()), value)
        .expect("serialize pod")
        .0
        .into_inner()
}

/// A `SPA_TYPE_OBJECT_ParamBuffers` pod sizing our shm buffers. Built by hand
/// because libspa 0.10 ships no high-level keys for ParamBuffers (the
/// `property!` macro needs `.as_raw()` keys). `dataType` is intentionally
/// omitted so PipeWire defaults to shm (MemFd/MemPtr) for now; Phase 3 adds a
/// `dataType` *flags* choice to also allow dmabuf. (A plain `Int` dataType is
/// wrong — spa reads it as a required exact type and rejects the buffers.)
fn buffers_param(stride: i32, size: i32) -> Vec<u8> {
    let obj = Object {
        type_: spa::sys::SPA_TYPE_OBJECT_ParamBuffers,
        id: ParamType::Buffers.as_raw(),
        properties: vec![
            Property::new(spa::sys::SPA_PARAM_BUFFERS_buffers, Value::Int(8)),
            Property::new(spa::sys::SPA_PARAM_BUFFERS_blocks, Value::Int(1)),
            Property::new(spa::sys::SPA_PARAM_BUFFERS_size, Value::Int(size)),
            Property::new(spa::sys::SPA_PARAM_BUFFERS_stride, Value::Int(stride)),
            Property::new(spa::sys::SPA_PARAM_BUFFERS_align, Value::Int(16)),
        ],
    };
    pod_bytes(&Value::Object(obj))
}

/// Fill a BGRx buffer with a moving diagonal pattern so a consumer sees live,
/// changing video. Returns the number of bytes written. Placeholder until the
/// screencopy capture lands in Phase 2.
fn fill_test_pattern(buf: &mut [u8], width: u32, height: u32, frame: u64) -> usize {
    let stride = (width * 4) as usize;
    let h = height as usize;
    let w = width as usize;
    let phase = (frame & 0xff) as usize;
    let total = (stride * h).min(buf.len());
    for y in 0..h {
        let row = y * stride;
        if row + stride > buf.len() {
            break;
        }
        for x in 0..w {
            let i = row + x * 4;
            let v = ((x + y + phase) & 0xff) as u8;
            buf[i] = v; // B
            buf[i + 1] = v.wrapping_mul(2); // G
            buf[i + 2] = 0x80u8.wrapping_add(v); // R
            buf[i + 3] = 0xff; // x
        }
    }
    total
}
