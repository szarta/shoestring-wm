//! shoestring-screenshot — capture an output via `zwlr_screencopy_v1` and
//! write the result to a PNG.
//!
//! Default destination: `$XDG_PICTURES_DIR/Screenshot-YYYYMMDD-HHMMSS.png`
//! (falling back to `$HOME/Pictures`). The chosen path is printed to stdout
//! so callers can pipe it into `wl-copy`, `xdg-open`, etc.
//!
//! Output selection:
//! - `--output NAME` picks by `wl_output.name` (e.g. `eDP-1`, `winit`).
//! - Otherwise the first output the registry advertises is used.
//!
//! The protocol is wlr-screencopy v3; we only use the wl_shm path (no
//! linux-dmabuf), so this works wherever the WM advertises the manager.

use std::{
    fs::File,
    io::Write,
    os::fd::AsFd,
    path::PathBuf,
    process::ExitCode,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// What the caller wants done with the captured PNG bytes.
enum Destination {
    /// Save to this path (and print it).
    File(PathBuf),
    /// Pipe to `wl-copy --type image/png`; print nothing.
    Clipboard,
    /// Save to path AND copy to clipboard.
    Both(PathBuf),
}

use anyhow::{anyhow, Context, Result};
use clap::Parser;
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

#[derive(Debug, Parser)]
#[command(
    name = "shoestring-screenshot",
    version,
    about = "Capture a screen to PNG."
)]
struct Cli {
    /// Pick a specific output by name (e.g. `eDP-1`, `HDMI-A-1`, `winit`).
    /// Defaults to the first output the compositor advertises. Ignored when
    /// `--region` is set (the picker chooses the output).
    #[arg(short, long)]
    output: Option<String>,

    /// Destination PNG path. Defaults to
    /// `$XDG_PICTURES_DIR/Screenshot-YYYYMMDD-HHMMSS.png`.
    #[arg(short = 'f', long)]
    file: Option<PathBuf>,

    /// Run `shoestring-region` first to drag-select a rectangle, then
    /// capture only that region via `capture_output_region`. The picker
    /// path is `$SHOESTRING_REGION_BIN`, else `shoestring-region` on
    /// $PATH.
    #[arg(short = 'r', long, conflicts_with = "region_rect")]
    region: bool,

    /// Capture an explicit rectangle without spawning the picker.
    /// Format: `X,Y,W,H` in the named output's logical pixel coords.
    /// Requires `--output` so the region's coordinate space is
    /// unambiguous. Intended for IPC-driven captures
    /// (`shoestring-ctl screenshot --region …`) where coordinates are
    /// already known.
    #[arg(long, value_name = "X,Y,W,H", requires = "output")]
    region_rect: Option<String>,

    /// Copy the captured PNG to the clipboard via `wl-copy` instead of (or
    /// in addition to) saving a file. When combined with `--file` the image
    /// is both saved and copied; when used alone no file is written and
    /// nothing is printed to stdout.
    #[arg(short = 'c', long)]
    clipboard: bool,
}

/// One rectangle on a named output, in that output's logical coords.
struct Region {
    output: String,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

fn main() -> ExitCode {
    match run() {
        Ok(Some(path)) => {
            println!("{}", path.display());
            ExitCode::SUCCESS
        }
        Ok(None) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("shoestring-screenshot: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<Option<PathBuf>> {
    let cli = Cli::parse();

    // Region picker runs *before* we touch wayland. The picker has its own
    // connection and exits when the user releases the mouse (or hits
    // Escape — we treat that as a clean cancel, not a hard error).
    // --region-rect skips the picker — the caller already knows the rect.
    let region = if let Some(rect) = cli.region_rect.as_deref() {
        // clap's `requires = "output"` guarantees output is Some.
        let output = cli.output.clone().expect("--region-rect requires --output");
        Some(parse_region_rect(&output, rect)?)
    } else if cli.region {
        Some(run_region_picker()?)
    } else {
        None
    };

    let conn =
        Connection::connect_to_env().context("connect to wayland (is WAYLAND_DISPLAY set?)")?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn).context("registry init")?;
    let qh = queue.handle();

    let shm: WlShm = globals
        .bind(&qh, 1..=2, ())
        .context("wl_shm global missing")?;
    let manager: ZwlrScreencopyManagerV1 = globals.bind(&qh, 1..=3, ()).context(
        "zwlr_screencopy_manager_v1 missing — is the compositor shoestring-wm or another \
         wlr-screencopy-capable compositor?",
    )?;

    // Outputs: we let the registry hand them to us; we accumulate their
    // names on the wl_output.name event (v4+) and pick the matching one.
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

    let mut state = State {
        outputs: outputs
            .into_iter()
            .map(|o| OutputEntry {
                name: None,
                output: o,
            })
            .collect(),
        frame_state: FrameState::Initial,
    };

    // Roundtrip so each WlOutput emits its `name` (v4+) and `done`. Without
    // this we'd race the user's `--output NAME` lookup.
    queue
        .roundtrip(&mut state)
        .context("output info roundtrip")?;

    // When --region was passed, the picker has already chosen the output.
    // Otherwise honor --output / first-available.
    let target = match &region {
        Some(r) => pick_output(&state.outputs, Some(&r.output))?,
        None => pick_output(&state.outputs, cli.output.as_deref())?,
    };

    // Kick off the capture. cursor=1 because our WM always composites the
    // cursor anyway; passing 0 wouldn't actually exclude it (documented
    // limitation).
    let frame = match &region {
        Some(r) => manager.capture_output_region(1, &target, r.x, r.y, r.w, r.h, &qh, ()),
        None => manager.capture_output(1, &target, &qh, ()),
    };

    // Drive the queue until we either have buffer params or a failure.
    let (format, width, height, stride) = loop {
        queue.blocking_dispatch(&mut state)?;
        match &state.frame_state {
            FrameState::Initial => continue,
            FrameState::BufferParams {
                format,
                width,
                height,
                stride,
            } => {
                break (*format, *width, *height, *stride);
            }
            FrameState::Failed(e) => anyhow::bail!("capture failed: {e}"),
            _ => continue,
        }
    };

    // Allocate the wl_shm buffer the compositor will fill.
    let size = (stride as u64) * (height as u64);
    let tmp = tempfile::tempfile().context("tempfile for shm pool")?;
    tmp.set_len(size).context("set tempfile len")?;
    let pool: WlShmPool = shm.create_pool(tmp.as_fd(), size as i32, &qh, ());
    let buffer: WlBuffer = pool.create_buffer(
        0,
        width as i32,
        height as i32,
        stride as i32,
        format,
        &qh,
        (),
    );
    pool.destroy();

    // Request the copy and wait for ready/failed.
    frame.copy(&buffer);
    let (flags, _ts) = loop {
        queue.blocking_dispatch(&mut state)?;
        match &state.frame_state {
            FrameState::Ready {
                flags,
                tv_sec,
                tv_nsec,
            } => {
                break (*flags, (*tv_sec, *tv_nsec));
            }
            FrameState::Failed(e) => anyhow::bail!("capture failed: {e}"),
            _ => continue,
        }
    };
    frame.destroy();
    buffer.destroy();

    // Map the buffer and encode to PNG bytes.
    let mmap = unsafe { MmapMut::map_mut(&tmp).context("mmap shm pool")? };
    let png_bytes = encode_png(
        &mmap,
        width,
        height,
        stride,
        format,
        flags.contains(Flags::YInvert),
    )?;
    drop(mmap);
    drop(tmp);
    // queue keeps wayland resources alive — drop after PNG encode.
    drop(queue);

    let dest = match (cli.file, cli.clipboard) {
        (Some(p), true) => Destination::Both(p),
        (Some(p), false) => Destination::File(p),
        (None, true) => Destination::Clipboard,
        (None, false) => Destination::File(default_path()),
    };

    match dest {
        Destination::File(path) => {
            write_png_file(&path, &png_bytes)?;
            Ok(Some(path))
        }
        Destination::Clipboard => {
            copy_to_clipboard(&png_bytes)?;
            Ok(None)
        }
        Destination::Both(path) => {
            write_png_file(&path, &png_bytes)?;
            copy_to_clipboard(&png_bytes)?;
            Ok(Some(path))
        }
    }
}

// ---------------- Region picker ----------------

/// Spawn the picker binary, parse its single output line. Picker contract:
/// stdout = `NAME X Y W H\n`, exit 0 on success. Any non-zero exit is
/// treated as a user cancel — we propagate it as an error so the calling
/// shell sees a failure (so e.g. a hotkey doesn't silently fall through).
fn run_region_picker() -> Result<Region> {
    let bin = std::env::var_os("SHOESTRING_REGION_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("shoestring-region"));
    let out = std::process::Command::new(&bin)
        // Inherit stderr so the picker's diagnostics show up where the
        // user invoked us. Stdin is irrelevant; we just want stdout.
        .stderr(std::process::Stdio::inherit())
        .output()
        .with_context(|| format!("spawning region picker {:?}", bin))?;
    if !out.status.success() {
        // Code 1 from the picker == Escape or degenerate drag. Surface
        // as a tidy error message rather than a confusing nonzero status
        // with no stdout to parse.
        anyhow::bail!("region selection cancelled");
    }
    let line = String::from_utf8(out.stdout)
        .context("region picker stdout was not utf-8")?
        .trim()
        .to_string();
    parse_region_line(&line)
}

/// Parse the `X,Y,W,H` string accepted by `--region-rect`. The output
/// name comes from `--output` rather than the rect itself (unlike the
/// picker's stdout, which carries the output name with the coords).
fn parse_region_rect(output: &str, s: &str) -> Result<Region> {
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() != 4 {
        anyhow::bail!("--region-rect expected X,Y,W,H (got {s:?})");
    }
    let mut nums = [0i32; 4];
    for (i, p) in parts.iter().enumerate() {
        nums[i] = p
            .trim()
            .parse::<i32>()
            .with_context(|| format!("--region-rect field {} not an int: {p:?}", i + 1))?;
    }
    let [x, y, w, h] = nums;
    if w <= 0 || h <= 0 {
        anyhow::bail!("--region-rect size must be positive (got {w}x{h})");
    }
    Ok(Region {
        output: output.to_string(),
        x,
        y,
        w,
        h,
    })
}

fn parse_region_line(line: &str) -> Result<Region> {
    let mut parts = line.split_ascii_whitespace();
    let output = parts
        .next()
        .ok_or_else(|| anyhow!("picker line missing output name: {line:?}"))?
        .to_string();
    let mut next_int = |field: &str| -> Result<i32> {
        let s = parts
            .next()
            .ok_or_else(|| anyhow!("picker line missing {field}: {line:?}"))?;
        s.parse::<i32>()
            .with_context(|| format!("picker {field} not an int: {s:?}"))
    };
    let x = next_int("x")?;
    let y = next_int("y")?;
    let w = next_int("w")?;
    let h = next_int("h")?;
    if w <= 0 || h <= 0 {
        anyhow::bail!("picker returned non-positive size: {w}x{h}");
    }
    Ok(Region { output, x, y, w, h })
}

// ---------------- Output selection ----------------

struct OutputEntry {
    name: Option<String>,
    output: WlOutput,
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

// ---------------- Default path ----------------

fn default_path() -> PathBuf {
    let dir = std::env::var_os("XDG_PICTURES_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Pictures")))
        .unwrap_or_else(|| PathBuf::from("."));
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    let stamp = format_stamp(now);
    dir.join(format!("Screenshot-{stamp}.png"))
}

/// `YYYYMMDD-HHMMSS` from a UNIX duration. Local-time-agnostic (compares
/// against UTC), which is fine for sortable filenames.
fn format_stamp(d: Duration) -> String {
    let secs = d.as_secs() as i64;
    // Crude UTC breakdown — avoids pulling in chrono.
    let (y, m, day, h, min, s) = unix_to_civil(secs);
    format!("{y:04}{m:02}{day:02}-{h:02}{min:02}{s:02}")
}

/// Howard Hinnant's date algorithm. Converts UNIX seconds to civil time.
fn unix_to_civil(secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let h = (rem / 3600) as u32;
    let min = ((rem % 3600) / 60) as u32;
    let s = (rem % 60) as u32;

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = (yoe as i32) + (era as i32) * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, h, min, s)
}

// ---------------- PNG encoding ----------------

fn encode_png(
    src: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    format: wl_shm::Format,
    y_invert: bool,
) -> Result<Vec<u8>> {
    // The WM advertises Argb8888 — that's little-endian BGRA in memory.
    // For PNG output we want RGBA. Swap channels per pixel.
    if !matches!(format, wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888) {
        anyhow::bail!("unexpected buffer format {format:?}; only ARGB8888/XRGB8888 supported");
    }
    let bpp = 4usize;
    let row_bytes = (width as usize) * bpp;
    if (stride as usize) < row_bytes {
        anyhow::bail!("stride {stride} too small for width {width}");
    }

    let mut rgba = vec![0u8; row_bytes * height as usize];
    for y in 0..height as usize {
        let src_y = if y_invert { height as usize - 1 - y } else { y };
        let src_off = src_y * stride as usize;
        let dst_off = y * row_bytes;
        for x in 0..width as usize {
            let so = src_off + x * 4;
            let doff = dst_off + x * 4;
            // ARGB8888 in wl_shm == native-endian uint32 0xAARRGGBB.
            // On little-endian byte order is: [B, G, R, A].
            rgba[doff] = src[so + 2]; // R
            rgba[doff + 1] = src[so + 1]; // G
            rgba[doff + 2] = src[so]; // B
            rgba[doff + 3] = if matches!(format, wl_shm::Format::Xrgb8888) {
                0xFF
            } else {
                src[so + 3] // A
            };
        }
    }

    let mut buf = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut buf, width, height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header()?;
        writer.write_image_data(&rgba)?;
    }
    Ok(buf)
}

fn write_png_file(path: &std::path::Path, png_bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).context("create destination dir")?;
        }
    }
    let mut file = File::create(path).with_context(|| format!("create {}", path.display()))?;
    file.write_all(png_bytes)
        .with_context(|| format!("write {}", path.display()))?;
    file.flush().ok();
    Ok(())
}

fn copy_to_clipboard(png_bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    let mut child = std::process::Command::new("wl-copy")
        .arg("--type")
        .arg("image/png")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .context("spawn wl-copy (is wl-clipboard installed?)")?;
    child
        .stdin
        .take()
        .expect("stdin piped")
        .write_all(png_bytes)
        .context("write PNG to wl-copy stdin")?;
    let status = child.wait().context("wait for wl-copy")?;
    if !status.success() {
        anyhow::bail!("wl-copy exited with status {status}");
    }
    Ok(())
}

// ---------------- wayland dispatch glue ----------------

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
        tv_sec: u64,
        tv_nsec: u32,
    },
    Failed(String),
}

struct State {
    outputs: Vec<OutputEntry>,
    frame_state: FrameState,
}

// We don't need to react to most globals after binding — the registry
// listener is here just to satisfy the trait.
impl Dispatch<WlRegistry, GlobalListContents> for State {
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

impl Dispatch<WlOutput, ()> for State {
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

impl Dispatch<WlShm, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlShm,
        _: <WlShm as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<WlShmPool, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlShmPool,
        _: <WlShmPool as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<WlBuffer, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlBuffer,
        _: <WlBuffer as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<ZwlrScreencopyManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrScreencopyManagerV1,
        _: <ZwlrScreencopyManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for State {
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
                        state.frame_state =
                            FrameState::Failed(format!("unknown wl_shm format {v}"));
                        return;
                    }
                };
                state.frame_state = FrameState::BufferParams {
                    format,
                    width,
                    height,
                    stride,
                };
            }
            Event::Flags { flags } => {
                if let FrameState::BufferParams { .. } = &state.frame_state {
                    // Defer until Ready; flags arrives just before ready.
                }
                // Stash temporarily inside the Ready transition.
                // We can't yet finalise Ready until we get the ready event,
                // so latch the flag value into a small enum extension. The
                // easiest: keep prior state, but remember flags in a side
                // channel. Use a static-ish field on State by hijacking
                // FrameState::Ready with default tv values that we'll
                // overwrite when ready arrives.
                if let FrameState::BufferParams { .. } = &state.frame_state {
                    state.frame_state = FrameState::Ready {
                        flags: parse_flags(flags),
                        tv_sec: 0,
                        tv_nsec: 0,
                    };
                }
            }
            Event::Ready {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
            } => {
                let tv_sec = ((tv_sec_hi as u64) << 32) | tv_sec_lo as u64;
                let flags = if let FrameState::Ready { flags, .. } = &state.frame_state {
                    *flags
                } else {
                    Flags::empty()
                };
                state.frame_state = FrameState::Ready {
                    flags,
                    tv_sec,
                    tv_nsec,
                };
            }
            Event::Failed => {
                state.frame_state = FrameState::Failed("compositor sent failed".into());
            }
            _ => {}
        }
    }
}

fn parse_flags(f: WEnum<Flags>) -> Flags {
    match f {
        WEnum::Value(f) => f,
        WEnum::Unknown(_) => Flags::empty(),
    }
}

// Suppress unused warnings on EventQueue helper.
#[allow(dead_code)]
fn _types(_: &EventQueue<State>) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_region_line_basic() {
        let r = parse_region_line("eDP-1 100 200 800 600").unwrap();
        assert_eq!(r.output, "eDP-1");
        assert_eq!((r.x, r.y, r.w, r.h), (100, 200, 800, 600));
    }

    #[test]
    fn parse_region_line_extra_whitespace() {
        let r = parse_region_line("  HDMI-A-1   0  0   1920  1080  ").unwrap();
        assert_eq!(r.output, "HDMI-A-1");
        assert_eq!((r.x, r.y, r.w, r.h), (0, 0, 1920, 1080));
    }

    #[test]
    fn parse_region_line_rejects_zero_size() {
        assert!(parse_region_line("eDP-1 0 0 0 0").is_err());
        assert!(parse_region_line("eDP-1 0 0 10 0").is_err());
    }

    #[test]
    fn parse_region_rect_basic() {
        let r = parse_region_rect("eDP-1", "10,20,800,600").unwrap();
        assert_eq!(r.output, "eDP-1");
        assert_eq!((r.x, r.y, r.w, r.h), (10, 20, 800, 600));
    }

    #[test]
    fn parse_region_rect_rejects_wrong_field_count() {
        assert!(parse_region_rect("o", "1,2,3").is_err());
        assert!(parse_region_rect("o", "1,2,3,4,5").is_err());
    }

    #[test]
    fn parse_region_rect_rejects_non_positive_size() {
        assert!(parse_region_rect("o", "0,0,0,10").is_err());
        assert!(parse_region_rect("o", "0,0,10,-1").is_err());
    }

    #[test]
    fn parse_region_line_rejects_truncated() {
        assert!(parse_region_line("eDP-1 0 0 10").is_err());
        assert!(parse_region_line("").is_err());
    }
}
