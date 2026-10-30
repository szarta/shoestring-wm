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
//! `--ocr` turns the capture into text instead of a file: the pixels are
//! converted to grayscale and upscaled (tiny on-screen fonts OCR poorly at
//! native size), written to a temp PNG, and run through `tesseract`, whose
//! recognized text is printed to stdout. Pair it with `--region` to grab a
//! coworker's message or a terminal snippet as text, or drive it over IPC
//! (`run_command -- shoestring-screenshot --output … --region-rect … --ocr`)
//! to read a widget's text without a vision round-trip. `--ocr-threshold`
//! adds an Otsu binarization pass (off by default) that normalizes polarity
//! and strips anti-aliasing and faint divider lines — cleaner on crisp UI
//! fonts, worse on soft or stylized text. No build dependency — `tesseract`
//! is spawned at runtime (override the binary with `$SHOESTRING_TESSERACT_BIN`),
//! so OCR simply errors if it isn't installed.
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

    /// OCR the capture and print the recognized text to stdout instead of
    /// saving a PNG. Runs `tesseract` (override with
    /// `$SHOESTRING_TESSERACT_BIN`) on a grayscale, upscaled copy of the
    /// capture. `--file`/`--clipboard` still save/copy the original image if
    /// given (the path is logged to stderr, keeping stdout the text alone).
    #[arg(long)]
    ocr: bool,

    /// Integer factor to upscale the capture by before OCR. Small on-screen
    /// fonts recognize far better enlarged; the default suits a ~10px UI font.
    /// Only meaningful with `--ocr`.
    #[arg(long, value_name = "N", default_value_t = 4, value_parser = clap::value_parser!(u32).range(1..=8))]
    ocr_scale: u32,

    /// Tesseract page-segmentation mode (`--psm`) for `--ocr`. The default, 7,
    /// treats the image as a single text line — right for a status bar or a
    /// one-line label. Use 6 for a block of text, 3 for a full page.
    #[arg(long, value_name = "MODE", default_value_t = 7, value_parser = clap::value_parser!(u32).range(0..=13))]
    ocr_psm: u32,

    /// Binarize the capture (Otsu auto-threshold) before OCR. Off by default:
    /// tesseract binarizes internally and the plain grayscale path already
    /// reads most text. Turn it on to normalize polarity (light-on-dark text
    /// comes out dark-on-light) and strip anti-aliasing and faint divider/grid
    /// lines to pure black and white — cleaner labels on crisp UI fonts, at
    /// the cost of detail on soft or low-contrast text. Only meaningful with
    /// `--ocr`.
    #[arg(long)]
    ocr_threshold: bool,
}

/// One rectangle on a named output, in that output's logical coords.
struct Region {
    output: String,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

/// What `run` produced for stdout: a saved file's path, OCR text, or nothing
/// (a clipboard-only capture).
enum RunOutput {
    Nothing,
    Path(PathBuf),
    Text(String),
}

fn main() -> ExitCode {
    match run() {
        Ok(RunOutput::Path(path)) => {
            println!("{}", path.display());
            ExitCode::SUCCESS
        }
        Ok(RunOutput::Text(text)) => {
            println!("{text}");
            ExitCode::SUCCESS
        }
        Ok(RunOutput::Nothing) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("shoestring-screenshot: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<RunOutput> {
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

    // Map the buffer and unpack it to tight RGBA, which both PNG encoding
    // and OCR preprocessing build on.
    let mmap = unsafe { MmapMut::map_mut(&tmp).context("mmap shm pool")? };
    let rgba = to_rgba(
        &mmap,
        width,
        height,
        stride,
        format,
        flags.contains(Flags::YInvert),
    )?;
    drop(mmap);
    drop(tmp);
    // queue keeps wayland resources alive — drop after we've copied pixels out.
    drop(queue);

    // OCR takes over stdout: print the recognized text, not a path. A --file
    // or --clipboard alongside it still saves/copies the original capture,
    // but the path is logged to stderr so stdout stays the text alone.
    if cli.ocr {
        if let Some(path) = cli.file.as_deref() {
            write_png_file(path, &encode_png(&rgba, width, height)?)?;
            eprintln!("shoestring-screenshot: saved {}", path.display());
        }
        if cli.clipboard {
            copy_to_clipboard(&encode_png(&rgba, width, height)?)?;
        }
        let text = ocr_region(
            &rgba,
            width,
            height,
            cli.ocr_scale,
            cli.ocr_psm,
            cli.ocr_threshold,
        )?;
        return Ok(RunOutput::Text(text));
    }

    let png_bytes = encode_png(&rgba, width, height)?;
    let dest = match (cli.file, cli.clipboard) {
        (Some(p), true) => Destination::Both(p),
        (Some(p), false) => Destination::File(p),
        (None, true) => Destination::Clipboard,
        (None, false) => Destination::File(default_path()),
    };

    match dest {
        Destination::File(path) => {
            write_png_file(&path, &png_bytes)?;
            Ok(RunOutput::Path(path))
        }
        Destination::Clipboard => {
            copy_to_clipboard(&png_bytes)?;
            Ok(RunOutput::Nothing)
        }
        Destination::Both(path) => {
            write_png_file(&path, &png_bytes)?;
            copy_to_clipboard(&png_bytes)?;
            Ok(RunOutput::Path(path))
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
        // Code 1 from the picker == Escape. Surface as a tidy error
        // message rather than a confusing nonzero status with no stdout
        // to parse. (A plain click exits 0 with a zero-size point, which
        // parse_region_line rejects below.)
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

/// Unpack the compositor's `wl_shm` buffer into tight, top-down RGBA
/// (`width * height * 4`, no stride padding). The WM advertises Argb8888 —
/// little-endian BGRA in memory — so channels are swapped per pixel, and a
/// `YInvert` frame is flipped back upright here.
fn to_rgba(
    src: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    format: wl_shm::Format,
    y_invert: bool,
) -> Result<Vec<u8>> {
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
    Ok(rgba)
}

/// Encode tight top-down RGBA (from [`to_rgba`]) as PNG bytes.
fn encode_png(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut buf, width, height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header()?;
        writer.write_image_data(rgba)?;
    }
    Ok(buf)
}

// ---------------- OCR ----------------

/// Convert tight RGBA to an 8-bit grayscale plane (integer Rec. 601 luma),
/// one output byte per pixel. The alpha channel is ignored.
fn to_gray(rgba: &[u8]) -> Vec<u8> {
    rgba.chunks_exact(4)
        .map(|px| {
            // Integer Rec. 601 luma: (77 R + 150 G + 29 B) / 256.
            ((77 * px[0] as u32 + 150 * px[1] as u32 + 29 * px[2] as u32) >> 8) as u8
        })
        .collect()
}

/// Upscale a grayscale plane `scale`× with nearest-neighbor, returning the
/// enlarged plane and its new dimensions. Nearest-neighbor (not interpolation)
/// keeps the hard edges of a pixel/bitmap UI font crisp rather than blurring
/// them, which reads better for OCR.
fn upscale_gray(gray: &[u8], width: u32, height: u32, scale: u32) -> (Vec<u8>, u32, u32) {
    let sw = width * scale;
    let sh = height * scale;
    let mut out = vec![0u8; (sw as usize) * (sh as usize)];
    for y in 0..sh as usize {
        let sy = (y as u32 / scale) as usize;
        let src_row = sy * width as usize;
        let dst_row = y * sw as usize;
        for x in 0..sw as usize {
            let sx = (x as u32 / scale) as usize;
            out[dst_row + x] = gray[src_row + sx];
        }
    }
    (out, sw, sh)
}

/// Otsu's threshold: the gray level (0-255) that minimizes intra-class
/// variance, picked from the histogram. The classic automatic split between
/// foreground and background with no magic constant.
fn otsu_threshold(gray: &[u8]) -> u8 {
    let mut hist = [0u32; 256];
    for &g in gray {
        hist[g as usize] += 1;
    }
    let total = gray.len() as f64;
    let sum_all: f64 = hist
        .iter()
        .enumerate()
        .map(|(t, &c)| t as f64 * c as f64)
        .sum();
    let (mut w_b, mut sum_b, mut max_var, mut thresh) = (0.0f64, 0.0f64, -1.0f64, 0u8);
    for (t, &count) in hist.iter().enumerate() {
        w_b += count as f64;
        if w_b == 0.0 {
            continue;
        }
        let w_f = total - w_b;
        if w_f == 0.0 {
            break;
        }
        sum_b += t as f64 * count as f64;
        let m_b = sum_b / w_b;
        let m_f = (sum_all - sum_b) / w_f;
        let var = w_b * w_f * (m_b - m_f) * (m_b - m_f);
        if var > max_var {
            max_var = var;
            thresh = t as u8;
        }
    }
    thresh
}

/// Binarize a grayscale plane to pure 0/255 at `thresh`, forcing the majority
/// class to white. Text is usually the minority of pixels, so this lands it
/// dark-on-light — what `tesseract` expects — whether the source was dark text
/// on a light field or light text on a dark one (a terminal, a status bar).
fn binarize(gray: &[u8], thresh: u8) -> Vec<u8> {
    let dark = gray.iter().filter(|&&g| g <= thresh).count();
    let light_is_majority = (gray.len() - dark) >= dark;
    gray.iter()
        .map(|&g| {
            let is_light = g > thresh;
            // Background (the majority class) -> white, foreground -> black.
            if is_light == light_is_majority {
                255
            } else {
                0
            }
        })
        .collect()
}

/// Encode an 8-bit grayscale plane as PNG bytes.
fn encode_gray_png(gray: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut buf, width, height);
        enc.set_color(png::ColorType::Grayscale);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header()?;
        writer.write_image_data(gray)?;
    }
    Ok(buf)
}

/// OCR the capture: grayscale, optional Otsu binarization, nearest-neighbor
/// upscale, hand the PNG to `tesseract`, and return its recognized text
/// (trailing whitespace trimmed). The tesseract binary is
/// `$SHOESTRING_TESSERACT_BIN` or `tesseract` on `$PATH`; a missing binary or
/// a non-zero exit is surfaced as an error.
fn ocr_region(
    rgba: &[u8],
    width: u32,
    height: u32,
    scale: u32,
    psm: u32,
    threshold: bool,
) -> Result<String> {
    let mut gray = to_gray(rgba);
    if threshold {
        let t = otsu_threshold(&gray);
        gray = binarize(&gray, t);
    }
    let (gray, gw, gh) = upscale_gray(&gray, width, height, scale);
    let png_bytes = encode_gray_png(&gray, gw, gh)?;

    // Tesseract reads a real file more reliably across versions than stdin;
    // the temp PNG is removed when `img` drops.
    let mut img = tempfile::Builder::new()
        .prefix("shoestring-ocr-")
        .suffix(".png")
        .tempfile()
        .context("tempfile for OCR image")?;
    img.write_all(&png_bytes).context("write OCR image")?;
    img.flush().ok();

    let bin = std::env::var_os("SHOESTRING_TESSERACT_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("tesseract"));
    let out = std::process::Command::new(&bin)
        .arg(img.path())
        .arg("stdout")
        .arg("--psm")
        .arg(psm.to_string())
        .output()
        .with_context(|| format!("spawning OCR tool {bin:?} (is tesseract installed?)"))?;
    if !out.status.success() {
        anyhow::bail!(
            "{bin:?} exited with {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
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

    #[test]
    fn to_gray_is_rec601_weighted() {
        // 2x1 white then black, and separately green>red>blue by luma weight.
        assert_eq!(to_gray(&[255, 255, 255, 255, 0, 0, 0, 255]), vec![255, 0]);
        let rgb = to_gray(&[255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255]);
        let (r, g, b) = (rgb[0], rgb[1], rgb[2]);
        assert!(g > r && r > b, "expected green>red>blue, got {g} {r} {b}");
    }

    #[test]
    fn upscale_gray_replicates_pixels_nearest_neighbor() {
        // 2x1 white/black upscaled 3x -> 6x3, each source pixel a 3x3 block,
        // every row identical (nearest-neighbor).
        let (up, w, h) = upscale_gray(&[255, 0], 2, 1, 3);
        assert_eq!((w, h), (6, 3));
        let want_row = [255, 255, 255, 0, 0, 0];
        for row in 0..3 {
            assert_eq!(&up[row * 6..row * 6 + 6], &want_row);
        }
    }

    #[test]
    fn otsu_splits_a_bimodal_histogram_between_the_clusters() {
        // Half near-black, half near-white: the threshold lands between them.
        let mut gray = vec![10u8; 50];
        gray.resize(100, 240u8);
        let t = otsu_threshold(&gray);
        assert!((10..240).contains(&t), "threshold {t} not between clusters");
    }

    #[test]
    fn binarize_forces_majority_class_to_white_both_polarities() {
        // Dark text on light field (light is majority) -> text black, field white.
        let light_field = [0u8, 200, 200, 200, 200];
        assert_eq!(binarize(&light_field, 128), vec![0, 255, 255, 255, 255]);
        // Light text on dark field (dark is majority) -> text black, field white.
        let dark_field = [255u8, 20, 20, 20, 20];
        assert_eq!(binarize(&dark_field, 128), vec![0, 255, 255, 255, 255]);
    }

    #[test]
    fn to_rgba_swaps_channels_handles_stride_and_y_invert() {
        // 1x2 ARGB8888 (BGRA in memory) with 4 bytes of row padding:
        //   row 0 = red  (B=0,G=0,R=255,A=255)
        //   row 1 = blue (B=255,G=0,R=0,A=255)
        let stride = 8; // 1px*4 + 4 pad
        let src = [
            0, 0, 255, 255, 0xAA, 0xBB, 0xAA, 0xBB, // red + 4 padding
            255, 0, 0, 255, 0xCC, 0xDD, 0xCC, 0xDD, // blue + 4 padding
        ];
        let rgba = to_rgba(&src, 1, 2, stride, wl_shm::Format::Argb8888, false).unwrap();
        assert_eq!(
            rgba,
            vec![255, 0, 0, 255, 0, 0, 255, 255],
            "upright: red,blue"
        );

        let flipped = to_rgba(&src, 1, 2, stride, wl_shm::Format::Argb8888, true).unwrap();
        assert_eq!(
            flipped,
            vec![0, 0, 255, 255, 255, 0, 0, 255],
            "y-invert: blue,red"
        );

        // Xrgb8888 forces alpha opaque regardless of the source byte.
        let x = to_rgba(&[10, 20, 30, 0], 1, 1, 4, wl_shm::Format::Xrgb8888, false).unwrap();
        assert_eq!(x, vec![30, 20, 10, 255]);
    }

    #[test]
    fn encode_gray_png_roundtrips_dimensions() {
        let gray = vec![0u8; 4 * 2];
        let bytes = encode_gray_png(&gray, 4, 2).unwrap();
        let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        let reader = decoder.read_info().unwrap();
        let info = reader.info();
        assert_eq!((info.width, info.height), (4, 2));
        assert_eq!(info.color_type, png::ColorType::Grayscale);
    }

    /// Exercise the full `ocr_region` glue — grayscale/upscale, temp PNG,
    /// spawn, stdout capture, trim — against a stub `tesseract` so it is
    /// deterministic and needs no real OCR engine. Both env-var cases live in
    /// one test because `set_var` is process-global; no other test reads it.
    #[test]
    fn ocr_region_runs_the_tool_and_returns_trimmed_stdout() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        // A stub that echoes a fixed line regardless of the image, so the test
        // validates plumbing, not recognition. Trailing newline checks trim.
        let stub = dir.path().join("fake-tesseract");
        std::fs::write(&stub, "#!/bin/sh\nprintf 'ID #68  X: 1445\\n'\n").unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();

        let rgba = [255u8, 255, 255, 255, 0, 0, 0, 255]; // 2x1
        std::env::set_var("SHOESTRING_TESSERACT_BIN", &stub);
        let text = ocr_region(&rgba, 2, 1, 4, 7, false).unwrap();
        assert_eq!(text, "ID #68  X: 1445");
        // With thresholding on, the same stub path is exercised end to end.
        assert_eq!(
            ocr_region(&rgba, 2, 1, 4, 7, true).unwrap(),
            "ID #68  X: 1445"
        );

        // A missing binary is a tidy error, not a panic.
        std::env::set_var("SHOESTRING_TESSERACT_BIN", dir.path().join("nope"));
        assert!(ocr_region(&rgba, 2, 1, 4, 7, false).is_err());
        std::env::remove_var("SHOESTRING_TESSERACT_BIN");
    }
}
