//! ARGB8888 software rendering for notification surfaces.
//!
//! Each surface gets a freshly-laid-out bitmap: background fill, then
//! summary line, then body text wrapped to fit the inner width. No icon
//! column yet (the image-path hint is parsed but a real PNG decoder lands
//! in a later task; we just leave the space blank for now).

#![allow(clippy::too_many_arguments)]

use fontdue::Font;
use memmap2::MmapMut;

/// Result of laying out a notification: pixel dimensions of the surface
/// and the wrapped body lines. Caller uses `(width, height)` for the
/// wl_buffer / layer_surface, then re-passes `body_lines` into `paint`
/// so wrapping happens exactly once.
pub struct Layout {
    pub width: u32,
    pub height: u32,
    pub summary: String,
    pub body_lines: Vec<String>,
}

pub fn layout(
    font: &Font,
    summary_size: f32,
    body_size: f32,
    padding: u32,
    max_width: u32,
    summary: &str,
    body: &str,
) -> Layout {
    let inner_w = max_width.saturating_sub(padding * 2);
    let summary_line = clip_single_line(font, summary_size, summary, inner_w);
    let body_lines = wrap(font, body_size, body, inner_w);

    let summary_h = line_height(font, summary_size);
    let body_h = line_height(font, body_size);

    let body_block_h = if body_lines.is_empty() {
        0
    } else {
        body_h * body_lines.len() as u32
    };
    // Gap between summary and body is half of body_h when there's a body.
    let gap = if body_lines.is_empty() { 0 } else { body_h / 2 };

    let height = padding + summary_h + gap + body_block_h + padding;
    Layout {
        width: max_width,
        height: height.max(padding * 2 + summary_h),
        summary: summary_line,
        body_lines,
    }
}

pub fn paint(
    mmap: &mut MmapMut,
    font: &Font,
    layout: &Layout,
    padding: u32,
    summary_size: f32,
    body_size: f32,
    background: u32,
    foreground: u32,
) {
    fill_bg(mmap, layout.width, layout.height, background);

    let summary_h = line_height(font, summary_size);
    let body_h = line_height(font, body_size);
    let inner_x = padding as i32;
    let mut y = padding;

    draw_text(
        mmap,
        layout.width,
        layout.height,
        font,
        summary_size,
        inner_x,
        y as i32,
        summary_h,
        &layout.summary,
        foreground,
    );
    y += summary_h;
    if !layout.body_lines.is_empty() {
        y += body_h / 2;
        for line in &layout.body_lines {
            draw_text(
                mmap,
                layout.width,
                layout.height,
                font,
                body_size,
                inner_x,
                y as i32,
                body_h,
                line,
                foreground,
            );
            y += body_h;
        }
    }
}

/// One-line advance height including a generous descent — fontdue's
/// `new_line_size` already includes line_gap so we use it directly.
fn line_height(font: &Font, size_px: f32) -> u32 {
    font.horizontal_line_metrics(size_px)
        .map(|m| m.new_line_size.ceil() as u32)
        .unwrap_or((size_px * 1.25).ceil() as u32)
}

/// Measure a single rendered line's pixel width — sum of advance_width.
/// Matches shoestring-bar's measure: no kerning, which our draw also
/// doesn't apply, so values stay consistent with what we paint.
fn measure(font: &Font, size_px: f32, text: &str) -> u32 {
    let mut w = 0.0_f32;
    for ch in text.chars() {
        w += font.metrics(ch, size_px).advance_width;
    }
    w.ceil() as u32
}

/// Single-line clip with trailing ".." when the string overflows. For
/// summaries — multi-line summary headers aren't part of the v1 spec.
fn clip_single_line(font: &Font, size_px: f32, text: &str, max_w: u32) -> String {
    if measure(font, size_px, text) <= max_w {
        return text.to_owned();
    }
    let ellipsis = "..";
    let ellipsis_w = measure(font, size_px, ellipsis);
    let mut out = String::new();
    let mut acc_w = 0_u32;
    for ch in text.chars() {
        let cw = font.metrics(ch, size_px).advance_width.ceil() as u32;
        if acc_w + cw + ellipsis_w > max_w {
            break;
        }
        out.push(ch);
        acc_w += cw;
    }
    out.push_str(ellipsis);
    out
}

/// Word-wrap `text` to lines no wider than `max_w` pixels. Splits on
/// ASCII whitespace; a single word longer than `max_w` is force-broken
/// mid-string rather than overflowing the surface.
fn wrap(font: &Font, size_px: f32, text: &str, max_w: u32) -> Vec<String> {
    let mut lines = Vec::new();
    if text.is_empty() {
        return lines;
    }
    // Preserve explicit newlines in the body — clients pass them
    // intentionally for multi-line messages.
    for paragraph in text.split('\n') {
        if paragraph.is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut current = String::new();
        let mut current_w = 0_u32;
        let space_w = measure(font, size_px, " ");
        for word in paragraph.split_whitespace() {
            let word_w = measure(font, size_px, word);
            // Force-break a word that is wider than the line itself, so
            // it doesn't drag the surface off-screen.
            if word_w > max_w {
                if !current.is_empty() {
                    lines.push(std::mem::take(&mut current));
                    current_w = 0;
                }
                lines.extend(force_break(font, size_px, word, max_w));
                continue;
            }
            let needed = if current.is_empty() {
                word_w
            } else {
                space_w + word_w
            };
            if current_w + needed > max_w {
                lines.push(std::mem::take(&mut current));
                current.push_str(word);
                current_w = word_w;
            } else {
                if !current.is_empty() {
                    current.push(' ');
                }
                current.push_str(word);
                current_w += needed;
            }
        }
        if !current.is_empty() {
            lines.push(current);
        }
    }
    lines
}

fn force_break(font: &Font, size_px: f32, word: &str, max_w: u32) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut buf_w = 0_u32;
    for ch in word.chars() {
        let cw = font.metrics(ch, size_px).advance_width.ceil() as u32;
        if buf_w + cw > max_w && !buf.is_empty() {
            out.push(std::mem::take(&mut buf));
            buf_w = 0;
        }
        buf.push(ch);
        buf_w += cw;
    }
    if !buf.is_empty() {
        out.push(buf);
    }
    out
}

fn fill_bg(mmap: &mut MmapMut, w: u32, h: u32, color: u32) {
    let bytes = color.to_ne_bytes();
    let stride = w as usize * 4;
    let row = bytes.repeat(w as usize);
    for y in 0..h as usize {
        let off = y * stride;
        mmap[off..off + stride].copy_from_slice(&row);
    }
}

/// Baseline-aligned text inside an `[y_top, y_top + line_h)` band.
fn draw_text(
    mmap: &mut MmapMut,
    dst_w: u32,
    dst_h: u32,
    font: &Font,
    size_px: f32,
    x_start: i32,
    y_top: i32,
    line_h: u32,
    text: &str,
    color: u32,
) {
    let line_metrics =
        font.horizontal_line_metrics(size_px)
            .unwrap_or_else(|| fontdue::LineMetrics {
                ascent: size_px * 0.8,
                descent: -size_px * 0.2,
                line_gap: 0.0,
                new_line_size: size_px,
            });
    let band = line_metrics.ascent - line_metrics.descent;
    let baseline_y = y_top + ((line_h as f32 - band) / 2.0 + line_metrics.ascent).round() as i32;

    let mut pen_x = x_start as f32;
    for ch in text.chars() {
        let (metrics, bitmap) = font.rasterize(ch, size_px);
        let gx = (pen_x + metrics.xmin as f32).round() as i32;
        let gy = baseline_y - (metrics.ymin + metrics.height as i32);
        blit_alpha(
            mmap,
            dst_w,
            dst_h,
            gx,
            gy,
            metrics.width as u32,
            metrics.height as u32,
            &bitmap,
            color,
        );
        pen_x += metrics.advance_width;
    }
}

fn blit_alpha(
    mmap: &mut MmapMut,
    dst_w: u32,
    dst_h: u32,
    dst_x: i32,
    dst_y: i32,
    src_w: u32,
    src_h: u32,
    src: &[u8],
    color: u32,
) {
    if src_w == 0 || src_h == 0 {
        return;
    }
    let [fb, fg, fr, _fa] = color.to_le_bytes();
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
            let coverage = src[(sy as u32 * src_w + sx as u32) as usize];
            if coverage == 0 {
                continue;
            }
            let off = dy as usize * stride + dx as usize * 4;
            let a = coverage as i32;
            for (i, fg_chan) in [fb, fg, fr].iter().enumerate() {
                let bg = mmap[off + i] as i32;
                let out = bg + (a * (*fg_chan as i32 - bg)) / 255;
                mmap[off + i] = out.clamp(0, 255) as u8;
            }
            mmap[off + 3] = 0xFF;
        }
    }
}
