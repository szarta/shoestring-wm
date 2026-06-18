//! PipeWire side of the screencast backend.
//!
//! We hold one connection to the PipeWire daemon ([`Pw`]) and create one
//! output [`Cast`] (a `pw_stream`) per active session. The stream advertises a
//! video format; once a consumer (the sharing app, via the portal frontend)
//! connects and negotiates, the `process` callback fills buffers and queues
//! them.
//!
//! We offer the consumer **two** formats — one carrying a LINEAR dmabuf
//! modifier and one plain — and let it pick: dmabuf-capable consumers (browsers,
//! OBS) negotiate the modifier format and get zero-copy GPU buffers the WM
//! renders straight into; pickier ones (Zoom, whose bundled Mesa can't import
//! our dmabuf) fall back to shm. `param_changed` discovers which was chosen; the
//! `add_buffer`/`process` callbacks then either wire a gbm dmabuf into each
//! PipeWire buffer or copy shm pixels, respectively.
//!
//! The PipeWire loop is *not* run on its own thread: its fd is added to the
//! backend's `poll(2)` set and [`Pw::iterate`] is pumped from there, keeping
//! everything single-threaded alongside D-Bus.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Cursor;
use std::os::fd::{AsRawFd, IntoRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::rc::Rc;
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
use spa::pod::deserialize::PodDeserializer;
use spa::pod::{serialize::PodSerializer, Object, Pod, Property, PropertyFlags, Value};
use spa::utils::{Direction, Fraction, Rectangle, SpaTypes};

use crate::capture::{Capture, DmabufSlot, FrameInfo};

/// DRM fourcc / wl_shm / PipeWire `BGRx` are the same little-endian `[B,G,R,X]`
/// byte order. We capture and stream exactly this one format.
const FOURCC_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");
/// `DRM_FORMAT_MOD_LINEAR` — the single modifier we allocate + advertise.
const MOD_LINEAR: u64 = 0;

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

/// fd → the gbm/wl_buffer slot backing one dmabuf PipeWire buffer. Shared
/// (`Rc`) between the `add_buffer`/`remove_buffer`/`process` callbacks, keyed by
/// the dmabuf fd we write into each buffer's `spa_data` (read back in `process`
/// to find the matching screencopy target).
type SlotMap = Rc<RefCell<HashMap<RawFd, DmabufSlot>>>;

/// Per-session stream state held by the listener: the negotiated geometry +
/// whether the consumer chose dmabuf (a format carrying a modifier) or shm. Set
/// in `param_changed`, read in `add_buffer`/`process`.
#[derive(Default)]
struct StreamData {
    width: u32,
    height: u32,
    have_format: bool,
    /// True once a format carrying `SPA_FORMAT_VIDEO_modifier` was negotiated —
    /// the consumer (e.g. a browser) imports dmabuf. False ⇒ shm (e.g. Zoom).
    use_dmabuf: bool,
}

/// One active screencast: the output stream plus its listener. Dropping it
/// tears the stream down (and drops every [`DmabufSlot`], freeing the gbm
/// buffers + wl_buffers).
pub struct Cast {
    _stream: StreamRc,
    _listener: StreamListener<StreamData>,
    _slots: SlotMap,
}

/// Create + connect an output stream of the given size, returning the cast and
/// its PipeWire node id (the value handed back to the app in Start's
/// `results["streams"]`). Pumps the loop until the node id is assigned.
///
/// `capture` is the WM frame source; the `process` callback captures a fresh
/// frame and copies it into the PipeWire buffer (BGRx, top-to-bottom). It is
/// shared (`Rc`) so the callback — which outlives this call — can keep using it.
pub fn start(
    pw: &Pw,
    capture: Rc<RefCell<Capture>>,
    width: u32,
    height: u32,
) -> Result<(Cast, u32)> {
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

    // Shared dmabuf slot table, cloned into the buffer callbacks.
    let slots: SlotMap = Rc::new(RefCell::new(HashMap::new()));
    let (cap_add, cap_proc) = (capture.clone(), capture.clone());
    let (slots_add, slots_rm, slots_proc) = (slots.clone(), slots.clone(), slots.clone());

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
            // A format carrying a modifier ⇒ the consumer negotiated dmabuf.
            data.use_dmabuf = format_has_modifier(param);
            tracing::info!(
                w = data.width,
                h = data.height,
                dmabuf = data.use_dmabuf,
                "screencast format negotiated"
            );

            // Size the buffers and pick the memory type (dmabuf vs shm).
            let stride = (data.width * 4) as i32;
            let size = stride * data.height as i32;
            let bytes = buffers_param(stride, size, data.use_dmabuf);
            if let Some(pod) = Pod::from_bytes(&bytes) {
                if let Err(e) = stream.update_params(&mut [pod]) {
                    tracing::warn!(error = %e, "update_params(Buffers) failed");
                }
            }
        })
        // dmabuf only: allocate a gbm buffer per PipeWire buffer and wire its fd
        // into the spa_data. shm buffers are allocated + mapped by PipeWire.
        .add_buffer(move |_stream, data, pw_buffer| {
            if !data.use_dmabuf {
                return;
            }
            let slot =
                match cap_add
                    .borrow_mut()
                    .alloc_dmabuf(data.width, data.height, FOURCC_XRGB8888)
                {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(error = %e, "dmabuf alloc failed; buffer left empty");
                        return;
                    }
                };
            let fd = slot.fd.as_raw_fd();
            // SAFETY: pw_buffer is valid for this callback; we fill its single
            // data block (single-plane XRGB) with our dmabuf.
            unsafe { write_dmabuf_data(pw_buffer, &slot) };
            slots_add.borrow_mut().insert(fd, slot);
        })
        .remove_buffer(move |_stream, _data, pw_buffer| {
            // SAFETY: read back the fd we stored to drop the matching slot
            // (destroys the wl_buffer + frees the gbm bo).
            if let Some(fd) = unsafe { buffer_fd(pw_buffer) } {
                slots_rm.borrow_mut().remove(&fd);
            }
        })
        .process(move |stream, data| {
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

            if data.use_dmabuf {
                // Zero-copy: the WM renders the output straight into the dmabuf
                // PipeWire will read. Match this buffer to its slot by fd.
                let fd = datas[0].as_raw().fd as RawFd;
                let h = data.height as i32;
                let stride = {
                    let slots = slots_proc.borrow();
                    match slots.get(&fd) {
                        Some(slot) => match cap_proc.borrow_mut().capture_into_dmabuf(slot) {
                            Ok(_) => slot.stride as i32,
                            Err(e) => {
                                tracing::warn!(error = %e, "dmabuf screencast capture failed");
                                0
                            }
                        },
                        None => {
                            tracing::warn!(fd, "process: no dmabuf slot for buffer");
                            0
                        }
                    }
                };
                let chunk = datas[0].chunk_mut();
                *chunk.offset_mut() = 0;
                *chunk.stride_mut() = stride;
                *chunk.size_mut() = if stride > 0 { (stride * h) as u32 } else { 0 };
                return;
            }

            // shm path: capture to our own shm buffer, then copy into the
            // PipeWire-mapped buffer (BGRx, top-to-bottom).
            let (dst_w, dst_h) = (data.width as usize, data.height as usize);
            let dst_stride = dst_w * 4;
            let mut cap = cap_proc.borrow_mut();
            let captured = match cap.capture() {
                Ok(()) => true,
                Err(e) => {
                    tracing::warn!(error = %e, "screencast capture failed");
                    false
                }
            };
            let d = &mut datas[0];
            let filled = if captured {
                match (d.data(), cap.frame()) {
                    (Some(dst), Some((info, src))) => {
                        copy_frame(dst, dst_stride, dst_w, dst_h, src, &info)
                    }
                    _ => 0,
                }
            } else {
                0
            };
            let chunk = d.chunk_mut();
            *chunk.size_mut() = filled as u32;
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = dst_stride as i32;
        })
        .register()
        .map_err(|e| anyhow!("pw stream register: {e}"))?;

    // Offer two formats, both BGRx / requested size / 30fps: one carrying a
    // LINEAR dmabuf modifier (zero-copy consumers — browsers, OBS) and one plain
    // (shm consumers — Zoom, whose Mesa can't import our dmabuf). The consumer
    // picks; we discover which in `param_changed`.
    let dmabuf_bytes = format_pod(width, height, true);
    let shm_bytes = format_pod(width, height, false);
    let mut params = [
        Pod::from_bytes(&dmabuf_bytes).context("dmabuf format pod")?,
        Pod::from_bytes(&shm_bytes).context("shm format pod")?,
    ];

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
            _slots: slots,
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

/// A `SPA_TYPE_OBJECT_ParamBuffers` pod sizing our buffers and selecting the
/// memory type. Built by hand because libspa 0.10 ships no high-level keys for
/// ParamBuffers (the `property!` macro needs `.as_raw()` keys).
///
/// For shm, `dataType` is omitted so PipeWire defaults to MemFd/MemPtr (a plain
/// `Int` dataType is wrong — spa reads it as a required exact type and rejects
/// the buffers). For dmabuf we set `dataType = 1 << SPA_DATA_DmaBuf` so PipeWire
/// hands us buffers whose `spa_data` we fill with a gbm dmabuf in `add_buffer`.
fn buffers_param(stride: i32, size: i32, dmabuf: bool) -> Vec<u8> {
    let mut properties = vec![
        Property::new(spa::sys::SPA_PARAM_BUFFERS_buffers, Value::Int(8)),
        Property::new(spa::sys::SPA_PARAM_BUFFERS_blocks, Value::Int(1)),
        Property::new(spa::sys::SPA_PARAM_BUFFERS_size, Value::Int(size)),
        Property::new(spa::sys::SPA_PARAM_BUFFERS_stride, Value::Int(stride)),
        Property::new(spa::sys::SPA_PARAM_BUFFERS_align, Value::Int(16)),
    ];
    if dmabuf {
        properties.push(Property::new(
            spa::sys::SPA_PARAM_BUFFERS_dataType,
            Value::Int(1 << spa::sys::SPA_DATA_DmaBuf),
        ));
    }
    let obj = Object {
        type_: spa::sys::SPA_TYPE_OBJECT_ParamBuffers,
        id: ParamType::Buffers.as_raw(),
        properties,
    };
    pod_bytes(&Value::Object(obj))
}

/// Build an `EnumFormat` pod: BGRx at `width`×`height`, 30fps. When `dmabuf`,
/// append a mandatory LINEAR `SPA_FORMAT_VIDEO_modifier` so dmabuf-capable
/// consumers select it; the plain (no-modifier) variant is the shm offer.
fn format_pod(width: u32, height: u32, dmabuf: bool) -> Vec<u8> {
    let mut obj = spa::pod::object!(
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
    if dmabuf {
        obj.properties.push(Property {
            key: spa::sys::SPA_FORMAT_VIDEO_modifier,
            flags: PropertyFlags::MANDATORY,
            value: Value::Long(MOD_LINEAR as i64),
        });
    }
    pod_bytes(&Value::Object(obj))
}

/// True if the negotiated format pod carries `SPA_FORMAT_VIDEO_modifier` — i.e.
/// the consumer chose the dmabuf offer.
fn format_has_modifier(param: &Pod) -> bool {
    matches!(
        PodDeserializer::deserialize_any_from(param.as_bytes()),
        Ok((_, Value::Object(obj)))
            if obj
                .properties
                .iter()
                .any(|p| p.key == spa::sys::SPA_FORMAT_VIDEO_modifier)
    )
}

/// Fill a PipeWire buffer's single data block with a dmabuf slot.
///
/// SAFETY: `pw_buffer` must be a valid buffer from an `add_buffer` callback. We
/// only touch its one data block (we offered `blocks = 1`).
unsafe fn write_dmabuf_data(pw_buffer: *mut pw::sys::pw_buffer, slot: &DmabufSlot) {
    let b = (*pw_buffer).buffer;
    if b.is_null() || (*b).n_datas < 1 || (*b).datas.is_null() {
        return;
    }
    let d = (*b).datas; // first spa_data
    (*d).type_ = spa::sys::SPA_DATA_DmaBuf;
    (*d).flags = 0;
    (*d).fd = slot.fd.as_raw_fd() as i64;
    (*d).mapoffset = 0;
    (*d).maxsize = slot.stride * slot.height;
    (*d).data = std::ptr::null_mut();
    let chunk = (*d).chunk;
    if !chunk.is_null() {
        (*chunk).offset = slot.offset;
        (*chunk).stride = slot.stride as i32;
        (*chunk).size = slot.stride * slot.height;
        (*chunk).flags = 0;
    }
}

/// Read the dmabuf fd from a PipeWire buffer's first data block (to match it to
/// its slot on removal). SAFETY: as [`write_dmabuf_data`].
unsafe fn buffer_fd(pw_buffer: *mut pw::sys::pw_buffer) -> Option<RawFd> {
    let b = (*pw_buffer).buffer;
    if b.is_null() || (*b).n_datas < 1 || (*b).datas.is_null() {
        return None;
    }
    Some((*(*b).datas).fd as RawFd)
}

/// Copy a captured frame into the PipeWire buffer, row by row, flipping
/// vertically when the WM marked the capture `YInvert`. Source is wl_shm
/// `Xrgb8888`/`Argb8888` (little-endian `[B,G,R,A]`), which matches our offered
/// PipeWire `BGRx` byte-for-byte. Returns the number of bytes written.
fn copy_frame(
    dst: &mut [u8],
    dst_stride: usize,
    dst_w: usize,
    dst_h: usize,
    src: &[u8],
    info: &FrameInfo,
) -> usize {
    let src_stride = info.stride as usize;
    let src_h = info.height as usize;
    let rows = dst_h.min(src_h);
    let row_bytes = dst_w.min(info.width as usize) * 4;
    for y in 0..rows {
        let src_y = if info.yinvert { src_h - 1 - y } else { y };
        let so = src_y * src_stride;
        let doo = y * dst_stride;
        if so + row_bytes > src.len() || doo + row_bytes > dst.len() {
            break;
        }
        dst[doo..doo + row_bytes].copy_from_slice(&src[so..so + row_bytes]);
    }
    dst_stride * rows
}
