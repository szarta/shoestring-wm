//! F3-style on-screen diagnostics overlay.
//!
//! A Minecraft-F3 / Roblox-style debug panel the WM draws itself, in the
//! top-left corner of the output under the pointer, from the live metrics
//! registry ([`crate::metrics`]). It's a pure *visualization* of the same data
//! [`shoestring_ipc::Request::Metrics`] serves over IPC — toggling it has no
//! effect on the metrics themselves. Off by default; flipped on by
//! [`shoestring_config::Action::ToggleDiagnostics`] (Super+F3).
//!
//! The text is rasterized with [`fontdue`] (the same pure-Rust path the bar,
//! menu, and notifier use — no fontconfig/freetype) into a persistent
//! [`MemoryRenderBuffer`], uploaded once and reused across frames so the
//! damage tracker sees a stable element id. The buffer is only rebuilt when
//! its content or the output scale actually change (throttled to a few hertz),
//! so a quiescent overlay contributes no per-frame damage — the same
//! discipline [`crate::decorations`] follows for borders.
//!
//! Values are only as fresh as the `[diagnostics]` sampler that fills the
//! registry: with `enabled = false` (the sampler off) the panel still draws
//! but shows the last/seeded readings. The default has the sampler on.

use std::time::{Duration, Instant};

use fontdue::{Font, FontSettings};
use shoestring_ipc::MetricValue;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::{
    element::{
        memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
        Kind,
    },
    ImportMem, Renderer, Texture,
};
use smithay::output::Output;
use smithay::utils::{Physical, Point, Transform};

/// Logical-pixel padding between the panel edge and its text.
const PADDING: i32 = 6;
/// Logical-pixel margin from the output's top-left corner to the panel.
const MARGIN: i32 = 8;
/// Minimum wall-clock gap between content rebuilds. The registry updates at
/// most once per `[diagnostics].sample_interval_ms` (default 1s), so a few
/// hertz keeps the readout live without re-rasterizing every frame.
const REFRESH: Duration = Duration::from_millis(200);

/// Candidate system fonts, tried in order after `$SHOESTRING_WM_FONT`. Mirrors
/// the list the client crates carry so the overlay finds a font wherever they
/// do (Linux distros + FreeBSD pkg paths).
const FONT_CANDIDATES: &[&str] = &[
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
    "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans.ttf",
    "/usr/share/fonts/liberation-sans/LiberationSans-Regular.ttf",
    "/usr/local/share/fonts/dejavu/DejaVuSans.ttf",
    "/usr/local/share/fonts/liberation-fonts-ttf/LiberationSans-Regular.ttf",
    "/usr/local/share/fonts/noto/NotoSans-Regular.ttf",
];

/// Persistent state for the diagnostics overlay. Default is "off, nothing
/// loaded" — costs nothing until first toggled on.
#[derive(Default)]
pub struct DiagOverlay {
    /// Whether the overlay is currently shown. Flipped by [`Self::toggle`].
    pub enabled: bool,
    /// Lazily-loaded font, resolved on first refresh.
    font: Option<Font>,
    /// Set once a font load attempt fails so we don't retry (and re-log)
    /// every refresh. The overlay simply draws nothing in that state.
    font_failed: bool,
    /// The uploaded panel, reused across frames. `None` until first built.
    buffer: Option<MemoryRenderBuffer>,
    /// `(rendered text, buffer scale)` the current `buffer` was built from, so
    /// we skip rebuilding while neither changed.
    key: Option<(String, i32)>,
    /// Wall-clock of the last rebuild, for the [`REFRESH`] throttle.
    last_build: Option<Instant>,
}

impl DiagOverlay {
    /// Flip the overlay on/off, returning the new state. Resets the throttle so
    /// a fresh toggle-on rebuilds immediately rather than waiting out
    /// [`REFRESH`].
    pub fn toggle(&mut self) -> bool {
        self.enabled = !self.enabled;
        self.last_build = None;
        self.enabled
    }

    /// (Re)build the panel buffer from `text` at output scale `scale`. No-op
    /// when neither the text nor the scale changed since the last build, or
    /// when no font is available. `font_size` is in logical px; colors are
    /// straight (non-premultiplied) RGBA in `0.0..=1.0`.
    pub fn update(&mut self, text: String, scale: i32, font_size: f32, fg: [f32; 4], bg: [f32; 4]) {
        self.ensure_font();
        let Some(font) = self.font.as_ref() else {
            return;
        };
        if self.buffer.is_some()
            && self
                .key
                .as_ref()
                .is_some_and(|(t, s)| t == &text && *s == scale)
        {
            return;
        }
        let (w, h, pixels) = rasterize(font, &text, scale, font_size, to_u8(fg), to_u8(bg));
        self.buffer = Some(MemoryRenderBuffer::from_slice(
            &pixels,
            Fourcc::Argb8888,
            (w as i32, h as i32),
            scale,
            Transform::Normal,
            None,
        ));
        self.key = Some((text, scale));
    }

    /// Build the render element for the current panel, positioned `MARGIN`
    /// logical px in from the output's top-left. `None` when nothing has been
    /// built yet. The element borrows the persistent buffer, so its id stays
    /// stable across frames.
    pub fn element<R>(
        &self,
        renderer: &mut R,
        scale: i32,
    ) -> Option<MemoryRenderBufferRenderElement<R>>
    where
        R: Renderer + ImportMem,
        R::TextureId: Texture + Clone + Send + 'static,
    {
        let buffer = self.buffer.as_ref()?;
        let pos = Point::<i32, Physical>::from((MARGIN * scale, MARGIN * scale));
        MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            pos.to_f64(),
            buffer,
            None,
            None,
            None,
            Kind::Unspecified,
        )
        .ok()
    }

    /// `true` when [`update`](Self::update) should run again — the throttle has
    /// elapsed, the scale changed, or nothing's been built yet. Cheap; called
    /// before the (more expensive) snapshot formatting.
    pub fn due(&self, scale: i32) -> bool {
        if self.buffer.is_none() {
            return true;
        }
        if self.key.as_ref().is_some_and(|(_, s)| *s != scale) {
            return true;
        }
        self.last_build.is_none_or(|t| t.elapsed() >= REFRESH)
    }

    /// Stamp the throttle clock. Called after a refresh attempt so the next is
    /// at least [`REFRESH`] away.
    pub fn mark_built(&mut self) {
        self.last_build = Some(Instant::now());
    }

    /// Resolve a font once, memoizing success and failure alike.
    fn ensure_font(&mut self) {
        if self.font.is_some() || self.font_failed {
            return;
        }
        match load_font() {
            Some(f) => self.font = Some(f),
            None => {
                tracing::warn!(
                    "diagnostics overlay: no usable font found (set $SHOESTRING_WM_FONT); \
                     the overlay will not render text"
                );
                self.font_failed = true;
            }
        }
    }
}

/// Format a metrics snapshot into the panel's text: a title line followed by
/// one `key = value` line per metric (already sorted — the registry is a
/// `BTreeMap`). Clipped to `max_rows` total lines, with a trailing summary of
/// how many were elided so a long client list doesn't run off-screen.
pub fn format_metrics(
    snapshot: &std::collections::BTreeMap<String, MetricValue>,
    max_rows: usize,
) -> String {
    let mut lines = Vec::with_capacity(snapshot.len() + 1);
    lines.push("shoestring-wm diagnostics".to_string());
    for (name, value) in snapshot {
        lines.push(format!("{name} = {}", fmt_value(value)));
    }
    // Reserve the last row for the "N more" note when we have to clip.
    if max_rows >= 2 && lines.len() > max_rows {
        let shown = max_rows - 1;
        let hidden = lines.len() - shown;
        lines.truncate(shown);
        lines.push(format!("… {hidden} more (shoestring-ctl metrics)"));
    }
    lines.join("\n")
}

fn fmt_value(v: &MetricValue) -> String {
    match v {
        MetricValue::Gauge { value } => value.to_string(),
        MetricValue::Counter { value } => value.to_string(),
    }
}

/// Convert straight RGBA in `0.0..=1.0` to `[r, g, b, a]` bytes.
fn to_u8([r, g, b, a]: [f32; 4]) -> [u8; 4] {
    let c = |x: f32| (x.clamp(0.0, 1.0) * 255.0).round() as u8;
    [c(r), c(g), c(b), c(a)]
}

/// Rasterize the panel into a premultiplied ARGB8888 buffer (little-endian
/// byte order `[B, G, R, A]`, what `Fourcc::Argb8888` expects). Returns
/// `(width, height, pixels)` in physical pixels at `scale`.
fn rasterize(
    font: &Font,
    text: &str,
    scale: i32,
    font_size: f32,
    fg: [u8; 4],
    bg: [u8; 4],
) -> (u32, u32, Vec<u8>) {
    let scale = scale.max(1);
    let size_px = (font_size * scale as f32).max(1.0);
    let pad = PADDING * scale;
    let line_h = line_height(font, size_px) as i32;

    let lines: Vec<&str> = text.split('\n').collect();
    let text_w = lines
        .iter()
        .map(|l| measure(font, size_px, l))
        .max()
        .unwrap_or(0) as i32;

    let w = (text_w + 2 * pad).max(1) as u32;
    let h = (line_h * lines.len() as i32 + 2 * pad).max(1) as u32;

    // Compose in straight RGBA, premultiply at the end.
    let mut buf = vec![0u8; (w * h * 4) as usize];
    fill(&mut buf, w, h, bg);
    let mut y = pad;
    for line in &lines {
        draw_text(&mut buf, w, h, font, size_px, pad, y, line_h, line, fg);
        y += line_h;
    }
    premultiply_to_bgra(&mut buf);
    (w, h, buf)
}

/// fontdue's `new_line_size` already folds in the line gap, so use it directly.
fn line_height(font: &Font, size_px: f32) -> u32 {
    font.horizontal_line_metrics(size_px)
        .map(|m| m.new_line_size.ceil() as u32)
        .unwrap_or((size_px * 1.3).ceil() as u32)
}

/// Pixel width of a rendered line (sum of advances; no kerning, matching the
/// draw path so measure and paint agree).
fn measure(font: &Font, size_px: f32, text: &str) -> u32 {
    text.chars()
        .map(|c| font.metrics(c, size_px).advance_width)
        .sum::<f32>()
        .ceil() as u32
}

/// Flood the buffer with a straight-RGBA color.
fn fill(buf: &mut [u8], w: u32, h: u32, [r, g, b, a]: [u8; 4]) {
    for px in buf[..(w * h * 4) as usize].chunks_exact_mut(4) {
        px.copy_from_slice(&[r, g, b, a]);
    }
}

/// Baseline-aligned text inside a `[y_top, y_top + line_h)` band, blending each
/// glyph's coverage over the existing (straight-RGBA) pixels.
#[allow(clippy::too_many_arguments)]
fn draw_text(
    buf: &mut [u8],
    dst_w: u32,
    dst_h: u32,
    font: &Font,
    size_px: f32,
    x_start: i32,
    y_top: i32,
    line_h: i32,
    text: &str,
    color: [u8; 4],
) {
    let metrics = font
        .horizontal_line_metrics(size_px)
        .unwrap_or(fontdue::LineMetrics {
            ascent: size_px * 0.8,
            descent: -size_px * 0.2,
            line_gap: 0.0,
            new_line_size: size_px,
        });
    let band = metrics.ascent - metrics.descent;
    let baseline_y = y_top + ((line_h as f32 - band) / 2.0 + metrics.ascent).round() as i32;

    let mut pen_x = x_start as f32;
    for ch in text.chars() {
        let (m, bitmap) = font.rasterize(ch, size_px);
        let gx = (pen_x + m.xmin as f32).round() as i32;
        let gy = baseline_y - (m.ymin + m.height as i32);
        blit_alpha(
            buf,
            dst_w,
            dst_h,
            gx,
            gy,
            m.width as u32,
            m.height as u32,
            &bitmap,
            color,
        );
        pen_x += m.advance_width;
    }
}

#[allow(clippy::too_many_arguments)]
fn blit_alpha(
    buf: &mut [u8],
    dst_w: u32,
    dst_h: u32,
    dst_x: i32,
    dst_y: i32,
    src_w: u32,
    src_h: u32,
    src: &[u8],
    [fr, fg, fb, _fa]: [u8; 4],
) {
    if src_w == 0 || src_h == 0 {
        return;
    }
    let stride = dst_w as usize * 4;
    for sy in 0..src_h as i32 {
        let dy = dst_y + sy;
        if dy < 0 || dy >= dst_h as i32 {
            continue;
        }
        for sx in 0..src_w as i32 {
            let dx = dst_x + sx;
            if dx < 0 || dx >= dst_w as i32 {
                continue;
            }
            let coverage = src[(sy as u32 * src_w + sx as u32) as usize] as i32;
            if coverage == 0 {
                continue;
            }
            let off = dy as usize * stride + dx as usize * 4;
            for (i, fg_chan) in [fr, fg, fb].iter().enumerate() {
                let bgc = buf[off + i] as i32;
                buf[off + i] = (bgc + coverage * (*fg_chan as i32 - bgc) / 255).clamp(0, 255) as u8;
            }
            // Text is opaque: lift alpha toward full where the glyph covers.
            buf[off + 3] = buf[off + 3].max(coverage as u8);
        }
    }
}

/// In place: convert a straight-RGBA buffer to premultiplied ARGB8888 in
/// little-endian byte order (`[B, G, R, A]`), which is what `wl_shm` /
/// `Fourcc::Argb8888` consumers expect.
fn premultiply_to_bgra(buf: &mut [u8]) {
    for px in buf.chunks_exact_mut(4) {
        let (r, g, b, a) = (px[0] as u16, px[1] as u16, px[2] as u16, px[3] as u16);
        px[0] = (b * a / 255) as u8;
        px[1] = (g * a / 255) as u8;
        px[2] = (r * a / 255) as u8;
        px[3] = a as u8;
    }
}

/// Resolve a usable font: `$SHOESTRING_WM_FONT` first, then [`FONT_CANDIDATES`].
fn load_font() -> Option<Font> {
    let try_path = |p: &str| -> Option<Font> {
        let bytes = std::fs::read(p).ok()?;
        Font::from_bytes(bytes, FontSettings::default()).ok()
    };
    if let Some(p) = std::env::var_os("SHOESTRING_WM_FONT") {
        if let Some(f) = p.to_str().and_then(try_path) {
            return Some(f);
        }
    }
    FONT_CANDIDATES.iter().copied().find_map(try_path)
}

impl crate::state::ShoestringWm {
    /// The single output the overlay draws on: the one under the pointer,
    /// falling back to the first output. `None` only with no outputs at all.
    /// One panel (not one per monitor) keeps it crisp and unduplicated.
    fn diag_overlay_output(&self) -> Option<Output> {
        if let Some(ptr) = self.seat.get_pointer() {
            let loc = ptr.current_location();
            if let Some(o) = self.space.outputs().find(|o| {
                self.space
                    .output_geometry(o)
                    .is_some_and(|g| g.to_f64().contains(loc))
            }) {
                return Some(o.clone());
            }
        }
        self.space.outputs().next().cloned()
    }

    /// Whether `output` is the one the overlay should draw on this frame.
    /// Cheap; the backends call it per output before doing any overlay work.
    pub fn is_diag_overlay_output(&self, output: &Output) -> bool {
        self.diag_overlay.enabled && self.diag_overlay_output().as_ref() == Some(output)
    }

    /// Rebuild the wallpaper canvas for `output` if its physical size or the
    /// `[background]` config changed. A no-op when nothing changed, so it's
    /// safe to call every frame. Must run *before* the renderer borrow (it
    /// takes `&mut self`); the element is built later from the buffer. The
    /// `self.wallpaper` / `self.config` borrows are disjoint fields, so this
    /// needs no clone.
    pub fn refresh_wallpaper(&mut self, output: &Output) {
        let Some(mode) = output.current_mode() else {
            return;
        };
        self.wallpaper
            .ensure(&self.config.background, (mode.size.w, mode.size.h));
    }

    /// The output's physical mode size and logical size, for the wallpaper
    /// element (physical selects the cached canvas; logical sizes the element).
    /// `None` when the output has no current mode. Logical falls back to
    /// deriving from the mode and scale when the output isn't mapped into the
    /// space yet.
    pub fn wallpaper_dims(
        &self,
        output: &Output,
    ) -> Option<(
        (i32, i32),
        smithay::utils::Size<i32, smithay::utils::Logical>,
    )> {
        let mode = output.current_mode()?;
        let phys = (mode.size.w, mode.size.h);
        let logical = self
            .space
            .output_geometry(output)
            .map(|g| g.size)
            .unwrap_or_else(|| {
                let scale = output.current_scale().fractional_scale();
                (
                    (mode.size.w as f64 / scale).round() as i32,
                    (mode.size.h as f64 / scale).round() as i32,
                )
                    .into()
            });
        Some((phys, logical))
    }

    /// Rebuild the overlay buffer for `output` when due (throttled, or on a
    /// scale/content change). Reads the live metrics snapshot and clips it to
    /// the output's height. A no-op when the overlay is off or nothing changed,
    /// so it's safe to call every frame. Must run *before* the renderer borrow
    /// (it takes `&mut self`); the element is built later from the buffer.
    pub fn refresh_diag_overlay(&mut self, output: &Output) {
        if !self.diag_overlay.enabled {
            return;
        }
        let scale = output.current_scale().fractional_scale();
        let scale_int = (scale.ceil() as i32).max(1);
        if !self.diag_overlay.due(scale_int) {
            return;
        }
        let max_rows = self.diag_overlay_max_rows(output, scale);
        let text = format_metrics(&self.metrics.snapshot(), max_rows);
        let cfg = &self.config.diagnostics;
        let (font_size, fg, bg) = (
            cfg.overlay_font_size,
            cfg.overlay_fg_rgba(),
            cfg.overlay_bg_rgba(),
        );
        self.diag_overlay.update(text, scale_int, font_size, fg, bg);
        self.diag_overlay.mark_built();
    }

    /// How many text rows fit the output's logical height, leaving the top/
    /// bottom margins. Bounds the panel so a long client list can't run off
    /// the bottom of the screen.
    fn diag_overlay_max_rows(&self, output: &Output, scale: f64) -> usize {
        let logical_h = self
            .space
            .output_geometry(output)
            .map(|g| g.size.h)
            .or_else(|| {
                output
                    .current_mode()
                    .map(|m| (m.size.h as f64 / scale).round() as i32)
            })
            .unwrap_or(720);
        let line_h = (self.config.diagnostics.overlay_font_size * 1.3)
            .ceil()
            .max(1.0) as i32;
        let usable = (logical_h - 2 * MARGIN).max(line_h);
        (usable / line_h).max(1) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn snap() -> BTreeMap<String, MetricValue> {
        let mut m = BTreeMap::new();
        m.insert("render.fps".into(), MetricValue::Gauge { value: 60 });
        m.insert(
            "render.frames_total".into(),
            MetricValue::Counter { value: 1234 },
        );
        m.insert("wm.windows".into(), MetricValue::Gauge { value: 3 });
        m
    }

    #[test]
    fn format_includes_title_and_each_metric() {
        let text = format_metrics(&snap(), 100);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "shoestring-wm diagnostics");
        // BTreeMap order: render.fps, render.frames_total, wm.windows.
        assert_eq!(lines[1], "render.fps = 60");
        assert_eq!(lines[2], "render.frames_total = 1234");
        assert_eq!(lines[3], "wm.windows = 3");
        assert_eq!(lines.len(), 4);
    }

    #[test]
    fn format_clips_to_max_rows_with_more_note() {
        // 1 title + 3 metrics = 4 lines, clipped to 3 → 2 shown + a "more" row.
        let text = format_metrics(&snap(), 3);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "shoestring-wm diagnostics");
        assert_eq!(lines[1], "render.fps = 60");
        assert!(
            lines[2].starts_with("…"),
            "last row summarizes the rest: {:?}",
            lines[2]
        );
        assert!(lines[2].contains("more"));
    }

    #[test]
    fn toggle_flips_and_resets_throttle() {
        let mut o = DiagOverlay::default();
        assert!(!o.enabled);
        o.last_build = Some(Instant::now());
        assert!(o.toggle());
        assert!(o.enabled);
        assert!(o.last_build.is_none());
        assert!(!o.toggle());
    }

    #[test]
    fn due_when_unbuilt_and_on_scale_change() {
        let mut o = DiagOverlay::default();
        // Nothing built yet → always due.
        assert!(o.due(1));
        // Stand in a (CPU-only) buffer built at scale 1 just now.
        o.key = Some(("x".into(), 1));
        o.buffer = Some(MemoryRenderBuffer::from_slice(
            &[0u8; 4],
            Fourcc::Argb8888,
            (1, 1),
            1,
            Transform::Normal,
            None,
        ));
        o.last_build = Some(Instant::now());
        // Same scale, fresh build → not due (throttled).
        assert!(!o.due(1));
        // Different scale → due regardless of throttle.
        assert!(o.due(2));
    }

    #[test]
    fn rasterize_produces_a_padded_panel() {
        // Without a system font this can't run; skip rather than fail (CI
        // images may be fontless). With one, the panel must be non-empty and
        // wider/taller than its text-free padding.
        let Some(font) = load_font() else {
            return;
        };
        let text = format_metrics(&snap(), 100);
        let (w, h, px) = rasterize(
            &font,
            &text,
            2,
            15.0,
            [224, 224, 224, 255],
            [28, 31, 38, 224],
        );
        assert!(
            w > 2 * (PADDING * 2) as u32,
            "panel wider than padding: {w}"
        );
        assert!(
            h > 2 * (PADDING * 2) as u32,
            "panel taller than padding: {h}"
        );
        assert_eq!(px.len() as u32, w * h * 4);
        // The background is translucent (alpha 224), so corner pixels carry
        // that exact alpha — proof the fill + premultiply ran.
        assert_eq!(px[3], 224, "top-left pixel keeps the background alpha");
    }

    #[test]
    fn premultiply_matches_alpha() {
        // Straight white at 50% alpha → premultiplied ~127 in BGR, A=128.
        let mut buf = [255u8, 255, 255, 128];
        premultiply_to_bgra(&mut buf);
        assert_eq!(buf[3], 128);
        assert_eq!(buf[0], 128); // 255*128/255 = 128
        assert_eq!(buf[1], 128);
        assert_eq!(buf[2], 128);
    }
}
