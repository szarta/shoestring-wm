//! Desktop background / wallpaper.
//!
//! Replaces the historic hardcoded `[0.1, 0.1, 0.1, 1.0]` clear color with the
//! configurable [`[background]`](shoestring_config::Background) section: a solid
//! clear color plus an optional PNG/SVG wallpaper fitted per
//! [`BackgroundMode`].
//!
//! The wallpaper is pre-composited on the CPU into an output-sized,
//! premultiplied `Argb8888` [`MemoryRenderBuffer`] (color fill + image placed
//! per mode), then handed to the renderer as the bottom-most element of the
//! scene — below every window and layer-shell surface. Unlike the diagnostics
//! overlay it is a real scene element, so it composites identically into
//! screenshots and screencasts.
//!
//! The composited canvas is cached and only rebuilt when its inputs change —
//! the output's physical size, the configured color/mode, or the image path
//! (on config hot-reload). The decoded source image is cached separately so a
//! mere output resize re-fits without re-decoding from disk.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use shoestring_config::{Background, BackgroundMode};
use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            element::{
                memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
                Kind,
            },
            ImportMem, Renderer, Texture,
        },
    },
    utils::{Logical, Physical, Point, Size, Transform},
};
// tiny-skia comes in transitively via resvg; use its re-export so versions
// can't drift (same pattern as the bar's icon renderer).
use resvg::tiny_skia;

/// Everything that determines the pixels of the composited canvas. When this
/// changes the buffer is rebuilt; otherwise the cached buffer is reused.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CanvasKey {
    image: Option<PathBuf>,
    mode: BackgroundMode,
    color: [u8; 4],
    phys: (i32, i32),
}

/// A decoded source image kept in premultiplied-RGBA `tiny_skia::Pixmap` form,
/// cached by path so an output resize re-fits without re-reading the file.
struct DecodedImage {
    path: PathBuf,
    pixmap: tiny_skia::Pixmap,
}

/// A composited output-sized canvas plus the inputs that produced it, so a
/// no-change frame reuses it instead of re-compositing.
struct Canvas {
    key: CanvasKey,
    buffer: MemoryRenderBuffer,
}

/// Per-WM wallpaper state: one composited canvas per distinct output resolution
/// (so a mixed-resolution multi-monitor setup doesn't thrash a single shared
/// buffer), plus the shared decoded source. Outputs of the same size share a
/// canvas.
#[derive(Default)]
pub struct Wallpaper {
    canvases: HashMap<(i32, i32), Canvas>,
    source: Option<DecodedImage>,
}

impl Wallpaper {
    /// Drop the cached canvases (but keep the decoded source) so the next
    /// [`ensure`](Self::ensure) per output rebuilds. Called on config
    /// hot-reload — the color/mode may have changed even if sizes haven't.
    pub fn invalidate(&mut self) {
        self.canvases.clear();
    }

    /// (Re)build the composited canvas for an output of `phys` physical pixels
    /// under `cfg`, if the cache for that size is stale. No-op when nothing
    /// changed. Decodes the image on first use / path change; a missing or
    /// undecodable image logs once and leaves the solid color showing.
    pub fn ensure(&mut self, cfg: &Background, phys: (i32, i32)) {
        if phys.0 <= 0 || phys.1 <= 0 {
            return;
        }
        let image = cfg.image_path();
        let key = CanvasKey {
            image: image.clone(),
            mode: cfg.mode,
            color: color_u8(cfg.color_rgba()),
            phys,
        };
        if self.canvases.get(&phys).is_some_and(|c| c.key == key) {
            return;
        }

        // Refresh the decoded source if the path changed (or was cleared).
        match &image {
            Some(path) => {
                let stale = self.source.as_ref().is_none_or(|s| &s.path != path);
                if stale {
                    self.source = decode_image(path).map(|pixmap| DecodedImage {
                        path: path.clone(),
                        pixmap,
                    });
                }
            }
            None => self.source = None,
        }

        let canvas = compose(
            key.color,
            cfg.mode,
            self.source.as_ref().map(|s| &s.pixmap),
            phys,
        );
        let buffer = MemoryRenderBuffer::from_slice(
            &canvas,
            Fourcc::Argb8888,
            phys,
            1,
            Transform::Normal,
            None,
        );
        self.canvases.insert(phys, Canvas { key, buffer });
    }

    /// Build the bottom-most render element covering the whole output. `logical`
    /// is the output's logical size and `phys` its physical size (which selects
    /// the cached canvas); the canvas (built at physical pixels with buffer
    /// scale 1) is scaled to `logical` so it fills the output at any output
    /// scale. `None` until [`ensure`](Self::ensure) has built this size's canvas.
    pub fn element<R>(
        &self,
        renderer: &mut R,
        phys: (i32, i32),
        logical: Size<i32, Logical>,
    ) -> Option<MemoryRenderBufferRenderElement<R>>
    where
        R: Renderer + ImportMem,
        R::TextureId: Texture + Clone + Send + 'static,
    {
        let buffer = &self.canvases.get(&phys)?.buffer;
        MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            Point::<i32, Physical>::from((0, 0)).to_f64(),
            buffer,
            None,
            None,
            Some(logical),
            Kind::Unspecified,
        )
        .ok()
    }
}

/// Straight RGBA floats → `[r, g, b, a]` bytes.
fn color_u8([r, g, b, a]: [f32; 4]) -> [u8; 4] {
    let c = |x: f32| (x.clamp(0.0, 1.0) * 255.0).round() as u8;
    [c(r), c(g), c(b), c(a)]
}

/// Decode a wallpaper file (PNG or SVG, by extension) into a premultiplied-RGBA
/// `Pixmap` at its native size. SVGs rasterize at their intrinsic dimensions.
/// `None` (with a warning) on a missing file, unknown extension, or decode
/// failure — the caller falls back to the solid color.
fn decode_image(path: &Path) -> Option<tiny_skia::Pixmap> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "wallpaper: cannot read image");
            return None;
        }
    };
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    let pixmap = match ext.as_deref() {
        Some("svg") | Some("svgz") => render_svg(&bytes),
        Some("png") => render_png(&bytes),
        other => {
            tracing::warn!(
                path = %path.display(),
                ext = ?other,
                "wallpaper: unsupported image type (use .png or .svg)"
            );
            None
        }
    };
    if pixmap.is_none() {
        tracing::warn!(path = %path.display(), "wallpaper: failed to decode image");
    }
    pixmap
}

/// SVG → premultiplied-RGBA pixmap at the document's intrinsic size.
fn render_svg(bytes: &[u8]) -> Option<tiny_skia::Pixmap> {
    let opt = resvg::usvg::Options::default();
    let tree = resvg::usvg::Tree::from_data(bytes, &opt).ok()?;
    let size = tree.size();
    let (w, h) = (size.width().ceil() as u32, size.height().ceil() as u32);
    if w == 0 || h == 0 {
        return None;
    }
    let mut pixmap = tiny_skia::Pixmap::new(w, h)?;
    resvg::render(
        &tree,
        tiny_skia::Transform::identity(),
        &mut pixmap.as_mut(),
    );
    Some(pixmap)
}

/// PNG → premultiplied-RGBA pixmap at native size. Mirrors the bar's decoder:
/// expand palette/grayscale + add alpha so we always land on 8-bit RGBA.
fn render_png(bytes: &[u8]) -> Option<tiny_skia::Pixmap> {
    let mut decoder = png::Decoder::new(bytes);
    decoder.set_transformations(png::Transformations::ALPHA | png::Transformations::EXPAND);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width, info.height);
    if w == 0 || h == 0 || info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    // Straight RGBA → premultiplied RGBA (what tiny-skia stores).
    let px = (w * h) as usize;
    let mut premul = vec![0u8; px * 4];
    let src = &buf[..px * 4];
    for i in 0..px {
        let r = src[i * 4] as u16;
        let g = src[i * 4 + 1] as u16;
        let b = src[i * 4 + 2] as u16;
        let a = src[i * 4 + 3] as u16;
        premul[i * 4] = ((r * a + 127) / 255) as u8;
        premul[i * 4 + 1] = ((g * a + 127) / 255) as u8;
        premul[i * 4 + 2] = ((b * a + 127) / 255) as u8;
        premul[i * 4 + 3] = a as u8;
    }
    tiny_skia::Pixmap::from_vec(premul, tiny_skia::IntSize::from_wh(w, h)?)
}

/// Compose the output-sized canvas: fill with `color`, then place `image` (if
/// any) per `mode`. Returns premultiplied `Argb8888` bytes (little-endian byte
/// order `[B, G, R, A]`), `phys.0 * phys.1 * 4` long.
fn compose(
    color: [u8; 4],
    mode: BackgroundMode,
    image: Option<&tiny_skia::Pixmap>,
    phys: (i32, i32),
) -> Vec<u8> {
    let (cw, ch) = (phys.0 as u32, phys.1 as u32);
    let mut canvas = match tiny_skia::Pixmap::new(cw, ch) {
        Some(p) => p,
        None => return vec![0u8; (cw * ch * 4) as usize],
    };
    let [r, g, b, a] = color;
    // tiny-skia fill takes straight (non-premultiplied) RGBA, stores premultiplied.
    canvas.fill(tiny_skia::Color::from_rgba8(r, g, b, a));

    if let Some(img) = image {
        blit(&mut canvas, img, mode);
    }

    // tiny-skia stores premultiplied RGBA; Fourcc::Argb8888 wants premultiplied
    // BGRA in memory — swap R/B.
    let mut bytes = canvas.take();
    for px in bytes.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    bytes
}

/// Paint `img` onto `canvas` per `mode`. The source over the solid fill uses
/// normal alpha blending so a translucent image lets the color show through.
fn blit(canvas: &mut tiny_skia::Pixmap, img: &tiny_skia::Pixmap, mode: BackgroundMode) {
    let (cw, ch) = (canvas.width() as f32, canvas.height() as f32);
    let (iw, ih) = (img.width() as f32, img.height() as f32);
    if iw <= 0.0 || ih <= 0.0 {
        return;
    }
    let paint = tiny_skia::PixmapPaint {
        quality: tiny_skia::FilterQuality::Bilinear,
        ..Default::default()
    };

    let draw = |canvas: &mut tiny_skia::Pixmap, sx: f32, sy: f32, dx: f32, dy: f32| {
        canvas.draw_pixmap(
            0,
            0,
            img.as_ref(),
            &paint,
            tiny_skia::Transform::from_scale(sx, sy).post_translate(dx, dy),
            None,
        );
    };

    match mode {
        BackgroundMode::Stretch => {
            draw(canvas, cw / iw, ch / ih, 0.0, 0.0);
        }
        BackgroundMode::Center => {
            let dx = (cw - iw) / 2.0;
            let dy = (ch - ih) / 2.0;
            draw(canvas, 1.0, 1.0, dx, dy);
        }
        BackgroundMode::Fit => {
            let s = (cw / iw).min(ch / ih);
            let dx = (cw - iw * s) / 2.0;
            let dy = (ch - ih * s) / 2.0;
            draw(canvas, s, s, dx, dy);
        }
        BackgroundMode::Fill => {
            let s = (cw / iw).max(ch / ih);
            let dx = (cw - iw * s) / 2.0;
            let dy = (ch - ih * s) / 2.0;
            draw(canvas, s, s, dx, dy);
        }
        BackgroundMode::Tile => {
            // Repeat at native size from the top-left. draw_pixmap clips to the
            // canvas, so over-shooting the last row/column is harmless.
            let (tw, th) = (iw.max(1.0), ih.max(1.0));
            let mut y = 0.0;
            while y < ch {
                let mut x = 0.0;
                while x < cw {
                    draw(canvas, 1.0, 1.0, x, y);
                    x += tw;
                }
                y += th;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A solid `w×h` premultiplied-RGBA pixmap, for compositing tests.
    fn solid(w: u32, h: u32, rgba: [u8; 4]) -> tiny_skia::Pixmap {
        let mut p = tiny_skia::Pixmap::new(w, h).unwrap();
        p.fill(tiny_skia::Color::from_rgba8(
            rgba[0], rgba[1], rgba[2], rgba[3],
        ));
        p
    }

    /// Read the BGRA pixel at (x, y) from a composed canvas of `phys` size.
    fn px(canvas: &[u8], phys: (i32, i32), x: i32, y: i32) -> [u8; 4] {
        let i = ((y * phys.0 + x) * 4) as usize;
        [canvas[i], canvas[i + 1], canvas[i + 2], canvas[i + 3]]
    }

    #[test]
    fn solid_fill_no_image() {
        let phys = (4, 3);
        // Straight color red; output is premultiplied BGRA → [B,G,R,A].
        let canvas = compose([255, 0, 0, 255], BackgroundMode::Fill, None, phys);
        assert_eq!(canvas.len(), 4 * 3 * 4);
        for y in 0..phys.1 {
            for x in 0..phys.0 {
                assert_eq!(px(&canvas, phys, x, y), [0, 0, 255, 255]);
            }
        }
    }

    #[test]
    fn stretch_covers_every_pixel_with_image() {
        let phys = (8, 8);
        // Opaque green source, smaller than the canvas.
        let img = solid(2, 2, [0, 255, 0, 255]);
        let canvas = compose([0, 0, 0, 255], BackgroundMode::Stretch, Some(&img), phys);
        // Every pixel should be the image's green ([B,G,R,A] = [0,255,0,255]),
        // none of the black fill left.
        for y in 0..phys.1 {
            for x in 0..phys.0 {
                assert_eq!(px(&canvas, phys, x, y), [0, 255, 0, 255], "at {x},{y}");
            }
        }
    }

    #[test]
    fn center_leaves_color_border_and_centers_image() {
        let phys = (8, 8);
        let img = solid(2, 2, [0, 255, 0, 255]);
        let canvas = compose([255, 0, 0, 255], BackgroundMode::Center, Some(&img), phys);
        // Corner is the red fill.
        assert_eq!(px(&canvas, phys, 0, 0), [0, 0, 255, 255]);
        // Center (3..5, 3..5) is the green image.
        assert_eq!(px(&canvas, phys, 3, 3), [0, 255, 0, 255]);
        assert_eq!(px(&canvas, phys, 4, 4), [0, 255, 0, 255]);
    }

    #[test]
    fn tile_repeats_to_fill() {
        let phys = (8, 8);
        let img = solid(2, 2, [0, 255, 0, 255]);
        let canvas = compose([255, 0, 0, 255], BackgroundMode::Tile, Some(&img), phys);
        // A 2×2 opaque tile repeated covers everything with green, no red left.
        for y in 0..phys.1 {
            for x in 0..phys.0 {
                assert_eq!(px(&canvas, phys, x, y), [0, 255, 0, 255], "at {x},{y}");
            }
        }
    }

    #[test]
    fn zero_size_is_safe() {
        let canvas = compose([0, 0, 0, 255], BackgroundMode::Fill, None, (0, 0));
        assert!(canvas.is_empty());
    }
}
