//! Icon resolution + decode — a general bar capability (first used by the tray).
//!
//! Resolves a freedesktop icon **name** against the installed icon themes
//! (per the Icon Theme spec: search roots, `index.theme` `Inherits`, size-ranked
//! directories, `hicolor` fallback), then decodes the file — **PNG** (hicolor /
//! legacy themes) or **SVG** (Adwaita et al.) — to premultiplied **BGRA8888** at
//! a target pixel size, ready to blit into the bar's `Argb8888` shm buffer.
//!
//! PNG+SVG are both required because appindicator items (nm-applet, blueman)
//! advertise icon *names*, not pixmaps. See the icon-rendering decision memo.

use std::path::{Path, PathBuf};

// tiny-skia comes in transitively via resvg; use its re-export so versions
// can't skew.
use resvg::tiny_skia;

/// A decoded icon: premultiplied BGRA8888, `width * height * 4` bytes, matching
/// the bar's little-endian `wl_shm` `Argb8888` (and Wayland's premultiplied
/// alpha). Blit with `dst = src + dst*(1 - a)`.
pub struct Icon {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

/// Resolved icon search configuration: the theme inherit chain to try (always
/// ending in `hicolor`) and the icon roots, both in priority order.
pub struct IconTheme {
    roots: Vec<PathBuf>,
    themes: Vec<String>,
}

impl IconTheme {
    /// Build from the environment: XDG icon roots + the active GTK icon theme
    /// (with its `Inherits` chain) + `hicolor`.
    pub fn detect() -> IconTheme {
        let mut roots = Vec::new();
        let mut push = |p: PathBuf| {
            if !roots.contains(&p) {
                roots.push(p);
            }
        };
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            // $XDG_DATA_HOME/icons, ~/.local/share/icons, legacy ~/.icons.
            let data_home = std::env::var_os("XDG_DATA_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".local/share"));
            push(data_home.join("icons"));
            push(home.join(".icons"));
        }
        let data_dirs =
            std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
        for d in data_dirs.split(':').filter(|s| !s.is_empty()) {
            push(PathBuf::from(d).join("icons"));
        }
        push(PathBuf::from("/usr/share/pixmaps"));

        let active = detect_gtk_theme().unwrap_or_else(|| "Adwaita".to_string());
        let mut themes = Vec::new();
        resolve_inherits(&active, &roots, &mut themes);
        if !themes.iter().any(|t| t == "hicolor") {
            themes.push("hicolor".to_string());
        }

        IconTheme { roots, themes }
    }

    /// Find the best file for `name` at `size` px. `extra_dirs` (e.g. an SNI
    /// item's `IconThemePath`) are searched first, flat. Returns `None` if
    /// nothing matches anywhere.
    pub fn lookup(&self, name: &str, size: u16, extra_dirs: &[PathBuf]) -> Option<PathBuf> {
        // An absolute path is its own answer.
        let p = Path::new(name);
        if p.is_absolute() && p.is_file() {
            return Some(p.to_path_buf());
        }

        // Item-private directories first, flat.
        for dir in extra_dirs {
            if let Some(hit) = file_in_dir(dir, name) {
                return Some(hit);
            }
        }

        for theme in &self.themes {
            if let Some(hit) = self.lookup_in_theme(theme, name, size) {
                return Some(hit);
            }
        }

        // Last resort: a flat file directly under a root (e.g. /usr/share/pixmaps).
        for root in &self.roots {
            if let Some(hit) = file_in_dir(root, name) {
                return Some(hit);
            }
        }
        None
    }

    fn lookup_in_theme(&self, theme: &str, name: &str, size: u16) -> Option<PathBuf> {
        // Gather this theme's directories (from any root's index.theme), ranked
        // by how well they fit `size`.
        let mut dirs: Vec<ThemeDir> = Vec::new();
        for root in &self.roots {
            if let Some(parsed) = parse_index_theme(&root.join(theme).join("index.theme")) {
                dirs = parsed;
                break;
            }
        }
        dirs.sort_by_key(|d| d.size_distance(size));

        for d in &dirs {
            for root in &self.roots {
                let base = root.join(theme).join(&d.subdir).join(name);
                for ext in ["svg", "png"] {
                    let cand = base.with_extension(ext);
                    if cand.is_file() {
                        return Some(cand);
                    }
                }
            }
        }
        None
    }
}

/// PNG or SVG (by extension) → premultiplied BGRA at `size`×`size`.
pub fn decode(path: &Path, size: u16) -> Option<Icon> {
    let bytes = std::fs::read(path).ok()?;
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    let pixmap = match ext.as_deref() {
        Some("svg") | Some("svgz") => render_svg(&bytes, size)?,
        Some("png") => render_png(&bytes, size)?,
        _ => return None,
    };
    Some(pixmap_to_icon(pixmap))
}

fn render_svg(bytes: &[u8], size: u16) -> Option<tiny_skia::Pixmap> {
    let opt = resvg::usvg::Options::default();
    let tree = resvg::usvg::Tree::from_data(bytes, &opt).ok()?;
    let ts = tree.size();
    let (sw, sh) = (ts.width(), ts.height());
    if sw <= 0.0 || sh <= 0.0 {
        return None;
    }
    let target = size.max(1) as f32;
    // Preserve aspect; center within a square target.
    let scale = (target / sw).min(target / sh);
    let dx = (target - sw * scale) / 2.0;
    let dy = (target - sh * scale) / 2.0;
    let mut pixmap = tiny_skia::Pixmap::new(size.max(1) as u32, size.max(1) as u32)?;
    let transform = tiny_skia::Transform::from_scale(scale, scale).post_translate(dx, dy);
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    Some(pixmap)
}

fn render_png(bytes: &[u8], size: u16) -> Option<tiny_skia::Pixmap> {
    let mut decoder = png::Decoder::new(bytes);
    // Expand palette/grayscale and add an alpha channel so we always land on
    // RGBA8 (8-bit) regardless of the source.
    decoder.set_transformations(png::Transformations::ALPHA | png::Transformations::EXPAND);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width, info.height);
    if w == 0 || h == 0 || info.bit_depth != png::BitDepth::Eight {
        return None;
    }

    // Straight RGBA -> premultiplied RGBA (what tiny-skia stores).
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
    let src_pixmap = tiny_skia::Pixmap::from_vec(premul, tiny_skia::IntSize::from_wh(w, h)?)?;

    let target = size.max(1) as u32;
    if w == target && h == target {
        return Some(src_pixmap);
    }
    // Scale into a square target, aspect-preserved + centered, bilinear.
    let mut dst = tiny_skia::Pixmap::new(target, target)?;
    let scale = (target as f32 / w as f32).min(target as f32 / h as f32);
    let dx = (target as f32 - w as f32 * scale) / 2.0;
    let dy = (target as f32 - h as f32 * scale) / 2.0;
    let paint = tiny_skia::PixmapPaint {
        quality: tiny_skia::FilterQuality::Bilinear,
        ..Default::default()
    };
    dst.draw_pixmap(
        0,
        0,
        src_pixmap.as_ref(),
        &paint,
        tiny_skia::Transform::from_scale(scale, scale).post_translate(dx, dy),
        None,
    );
    Some(dst)
}

/// tiny-skia stores premultiplied RGBA; the bar's `Argb8888` is premultiplied
/// BGRA in memory — swap R/B.
fn pixmap_to_icon(pixmap: tiny_skia::Pixmap) -> Icon {
    let (width, height) = (pixmap.width(), pixmap.height());
    let mut bgra = pixmap.take();
    for px in bgra.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    Icon {
        width,
        height,
        bgra,
    }
}

// ── icon-theme spec plumbing ────────────────────────────────────────────────

/// One entry from an `index.theme` `[dir]` section, enough to rank by size.
struct ThemeDir {
    subdir: String,
    size: u16,
    kind: DirKind,
    min: u16,
    max: u16,
    threshold: u16,
}

enum DirKind {
    Fixed,
    Scalable,
    Threshold,
}

impl ThemeDir {
    /// Distance from `target` per the Icon Theme spec; 0 = perfect fit.
    fn size_distance(&self, target: u16) -> u32 {
        match self.kind {
            DirKind::Fixed => (self.size as i32 - target as i32).unsigned_abs(),
            DirKind::Scalable => {
                if target < self.min {
                    (self.min - target) as u32
                } else if target > self.max {
                    (target - self.max) as u32
                } else {
                    0
                }
            }
            DirKind::Threshold => {
                let lo = self.size.saturating_sub(self.threshold);
                let hi = self.size.saturating_add(self.threshold);
                if target < lo {
                    (lo - target) as u32
                } else if target > hi {
                    (target - hi) as u32
                } else {
                    0
                }
            }
        }
    }
}

/// Parse the subset of `index.theme` we rank on. Returns `None` if the file is
/// absent/unreadable.
fn parse_index_theme(path: &Path) -> Option<Vec<ThemeDir>> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut section = String::new();
    let mut directories: Vec<String> = Vec::new();
    // Per-section scratch.
    let mut sec_size = 0u16;
    let mut sec_kind = String::new();
    let mut sec_min = 0u16;
    let mut sec_max = 0u16;
    let mut sec_threshold = 2u16;
    let mut dirs: std::collections::HashMap<String, ThemeDir> = std::collections::HashMap::new();

    let flush = |dirs: &mut std::collections::HashMap<String, ThemeDir>,
                 section: &str,
                 size: u16,
                 kind: &str,
                 min: u16,
                 max: u16,
                 threshold: u16| {
        if section.is_empty() || section == "Icon Theme" {
            return;
        }
        let kind = match kind {
            "Fixed" => DirKind::Fixed,
            "Scalable" => DirKind::Scalable,
            _ => DirKind::Threshold,
        };
        dirs.insert(
            section.to_string(),
            ThemeDir {
                subdir: section.to_string(),
                size,
                kind,
                min: if min == 0 { size } else { min },
                max: if max == 0 { size } else { max },
                threshold,
            },
        );
    };

    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            // New section: flush the previous one.
            flush(
                &mut dirs,
                &section,
                sec_size,
                &sec_kind,
                sec_min,
                sec_max,
                sec_threshold,
            );
            section = rest.to_string();
            sec_size = 0;
            sec_kind.clear();
            sec_min = 0;
            sec_max = 0;
            sec_threshold = 2;
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        if section == "Icon Theme" {
            if k == "Directories" {
                directories = v.split(',').map(|s| s.trim().to_string()).collect();
            }
        } else {
            match k {
                "Size" => sec_size = v.parse().unwrap_or(0),
                "Type" => sec_kind = v.to_string(),
                "MinSize" => sec_min = v.parse().unwrap_or(0),
                "MaxSize" => sec_max = v.parse().unwrap_or(0),
                "Threshold" => sec_threshold = v.parse().unwrap_or(2),
                _ => {}
            }
        }
    }
    flush(
        &mut dirs,
        &section,
        sec_size,
        &sec_kind,
        sec_min,
        sec_max,
        sec_threshold,
    );

    // Honour the declared `Directories` order/membership when present, else take
    // whatever sections we found.
    let result: Vec<ThemeDir> = if directories.is_empty() {
        dirs.into_values().collect()
    } else {
        directories
            .into_iter()
            .filter_map(|d| dirs.remove(&d))
            .collect()
    };
    Some(result)
}

/// Walk a theme's `Inherits` chain depth-first into `out` (deduped, parents
/// after the theme itself).
fn resolve_inherits(theme: &str, roots: &[PathBuf], out: &mut Vec<String>) {
    if out.iter().any(|t| t == theme) {
        return;
    }
    out.push(theme.to_string());
    for root in roots {
        let path = root.join(theme).join("index.theme");
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let mut in_header = false;
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('[') {
                in_header = line == "[Icon Theme]";
                continue;
            }
            if in_header {
                if let Some(v) = line.strip_prefix("Inherits=") {
                    for parent in v.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
                        resolve_inherits(parent, roots, out);
                    }
                }
            }
        }
        return; // first root with an index.theme wins
    }
}

/// `<dir>/<name>.{svg,png}` if it exists.
fn file_in_dir(dir: &Path, name: &str) -> Option<PathBuf> {
    for ext in ["svg", "png"] {
        let cand = dir.join(format!("{name}.{ext}"));
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

/// Active GTK icon theme from `~/.config/gtk-3.0/settings.ini`
/// (`gtk-icon-theme-name`). Cheap and dependency-free; good enough without
/// pulling in gsettings.
fn detect_gtk_theme() -> Option<String> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let cfg = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    let text = std::fs::read_to_string(cfg.join("gtk-3.0/settings.ini")).ok()?;
    for line in text.lines() {
        if let Some(v) = line.trim().strip_prefix("gtk-icon-theme-name") {
            let v = v.trim_start_matches([' ', '=']).trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// SVG path: parse + rasterize to the requested square at premultiplied BGRA.
    #[test]
    fn decode_svg_to_target_size() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><rect width="10" height="10" fill="red"/></svg>"#;
        let mut f = tempfile::Builder::new().suffix(".svg").tempfile().unwrap();
        f.write_all(svg).unwrap();
        let icon = decode(f.path(), 16).expect("svg decodes");
        assert_eq!((icon.width, icon.height), (16, 16));
        assert_eq!(icon.bgra.len(), 16 * 16 * 4);
        // Opaque red in premultiplied BGRA is [0,0,255,255]; centre is solid.
        let c = ((8 * 16 + 8) * 4) as usize;
        assert_eq!(icon.bgra[c + 3], 255, "centre opaque");
        assert!(icon.bgra[c + 2] > 200, "centre red");
    }

    /// PNG path: decode + scale to target. Encode a tiny PNG so the test needs
    /// no fixture on disk.
    #[test]
    fn decode_png_scaled() {
        let (w, h) = (4u32, 4u32);
        let mut raw = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut raw, w, h);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut writer = enc.write_header().unwrap();
            let data: Vec<u8> = std::iter::repeat_n([0u8, 0, 255, 255], (w * h) as usize)
                .flatten()
                .collect();
            writer.write_image_data(&data).unwrap();
        }
        let mut f = tempfile::Builder::new().suffix(".png").tempfile().unwrap();
        f.write_all(&raw).unwrap();
        let icon = decode(f.path(), 8).expect("png decodes");
        assert_eq!((icon.width, icon.height), (8, 8));
        assert_eq!(icon.bgra.len(), 8 * 8 * 4);
        // Opaque blue in premultiplied BGRA is [255,0,0,255]; centre is solid.
        let c = ((4 * 8 + 4) * 4) as usize;
        assert_eq!(icon.bgra[c + 3], 255, "centre opaque");
        assert!(icon.bgra[c] > 200, "centre blue");
    }

    /// Size ranking: a Fixed dir matching the target beats a far one.
    #[test]
    fn theme_dir_size_distance() {
        let fixed = |size| ThemeDir {
            subdir: String::new(),
            size,
            kind: DirKind::Fixed,
            min: size,
            max: size,
            threshold: 2,
        };
        assert_eq!(fixed(24).size_distance(24), 0);
        assert!(fixed(48).size_distance(24) > fixed(22).size_distance(24));
    }
}
