//! Tray context menu — a popup rendered from a `com.canonical.dbusmenu` layout.
//!
//! When the user clicks a tray icon the bar fetches that item's menu tree
//! (see [`crate::tray::Tray::fetch_menu`]) and opens one of these: a second
//! layer-shell surface on the **Overlay** layer, anchored just above (or below)
//! the clicked icon. It requests `KeyboardInteractivity::Exclusive`, which the
//! WM honours by transferring keyboard focus to it — so when the user clicks
//! away (focus moves elsewhere) we get a `wl_keyboard::Leave` and dismiss,
//! exactly like the shoestring-menu launcher. No xdg-popup grab needed (the
//! WM's popup grab is a no-op), and no compositor changes.
//!
//! Submenus are navigated *in place* (a "‹ Back" row appears) rather than as
//! cascading surfaces, which avoids the cross-gap pointer-leave problems of
//! multi-surface menus and keeps everything in one buffer.

use std::os::fd::AsFd;

use fontdue::Font;
use memmap2::MmapMut;
use wayland_client::{
    protocol::{
        wl_compositor::WlCompositor,
        wl_shm::{Format, WlShm},
        wl_surface::WlSurface,
    },
    QueueHandle,
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};

use crate::config::Position;
use crate::tray::MenuNode;
use crate::{fill_bg, fill_rect, measure_text, BarBuffer, State, ACCENT, DIM};

// Layout constants, in *logical* pixels (scaled to physical at render time).
const PAD_X: i32 = 12; // left text inset / right edge inset
const GUTTER: i32 = 20; // right column reserved for submenu arrow / check
const VPAD: i32 = 6; // menu top+bottom padding
const SEP_H: i32 = 9; // separator row height
const MIN_W: i32 = 140;
const MAX_W: i32 = 460;

// Raw evdev keycodes as delivered by wl_keyboard (no +8 xkb offset).
const KEY_ESC: u32 = 1;
const KEY_ENTER: u32 = 28;
const KEY_KPENTER: u32 = 96;
const KEY_UP: u32 = 103;
const KEY_DOWN: u32 = 108;
const KEY_LEFT: u32 = 105;
const KEY_RIGHT: u32 = 106;

/// What a click / keypress resolved to. The bar's input handlers apply the
/// UI-only variants directly to `State.menu`, and queue the tray ones (which
/// need the D-Bus connection that lives outside `State`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    /// Nothing actionable (e.g. clicked a separator or disabled row).
    None,
    /// Dismiss the menu.
    Close,
    /// Pop one submenu level.
    NavBack,
    /// Descend into the submenu with this dbusmenu id.
    NavInto(i32),
    /// Send a `clicked` event for this dbusmenu id, then dismiss.
    Activate(i32),
}

enum RowKind {
    Back,
    Separator,
    Entry {
        id: i32,
        label: String,
        enabled: bool,
        submenu: bool,
        toggle: Option<bool>,
    },
}

struct Row {
    y0: i32, // logical, surface-relative
    y1: i32,
    kind: RowKind,
}

pub struct Menu {
    pub surface: WlSurface,
    pub layer_surface: ZwlrLayerSurfaceV1,
    viewport: Option<WpViewport>,
    buffer: Option<BarBuffer>,
    /// Acked size from the compositor configure (logical). Render no-ops until set.
    size: Option<(u32, u32)>,
    /// Size we asked for via `set_size` (logical); also the basis for hit-tests.
    want_size: (u32, u32),
    /// Which tray item (index into `Tray.items`) this menu belongs to.
    pub tray_idx: usize,
    /// Full fetched tree; `root.children` are the top-level entries.
    root: MenuNode,
    /// Submenu path (dbusmenu ids) we've descended into.
    nav: Vec<i32>,
    /// Display rows for the current level, rebuilt on every (re)layout.
    rows: Vec<Row>,
    /// Currently highlighted row index (into `rows`), if any.
    hover: Option<usize>,
    // Anchoring inputs, kept so a resize on nav can re-clamp the margin.
    anchor_x: i32,
    output_w: u32,
    bar_h: u32,
    position: Position,
}

impl Menu {
    /// Create + map the menu surface for `root`, anchored at `anchor_x` (the
    /// clicked icon's logical left edge) above/below the bar.
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        compositor: &WlCompositor,
        layer_shell: &ZwlrLayerShellV1,
        viewporter: Option<&WpViewporter>,
        qh: &QueueHandle<State>,
        root: MenuNode,
        tray_idx: usize,
        anchor_x: i32,
        output_w: u32,
        bar_h: u32,
        position: Position,
        font: &Font,
        font_size: f32,
    ) -> Menu {
        let surface = compositor.create_surface(qh, ());
        let layer_surface = layer_shell.get_layer_surface(
            &surface,
            None,
            Layer::Overlay,
            "shoestring-bar-menu".to_string(),
            qh,
            (),
        );
        let viewport = viewporter.map(|vp| vp.get_viewport(&surface, qh, ()));

        let mut m = Menu {
            surface,
            layer_surface,
            viewport,
            buffer: None,
            size: None,
            want_size: (MIN_W as u32, 1),
            tray_idx,
            root,
            nav: Vec::new(),
            rows: Vec::new(),
            hover: None,
            anchor_x,
            output_w,
            bar_h,
            position,
        };
        m.relayout(font, font_size);
        m.reanchor();
        m.layer_surface
            .set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        m.surface.commit();
        m
    }

    /// True if `s` is this menu's surface (for routing pointer/keyboard events).
    pub fn owns(&self, s: &WlSurface) -> bool {
        &self.surface == s
    }

    /// The logical size we last requested via `set_size` (configure fallback).
    pub fn want_size(&self) -> (u32, u32) {
        self.want_size
    }

    /// Record the compositor's acked size. Returns whether it changed (→ redraw).
    pub fn set_size(&mut self, w: u32, h: u32) -> bool {
        let new = Some((w.max(1), h.max(1)));
        if self.size != new {
            self.size = new;
            true
        } else {
            false
        }
    }

    /// Rebuild `rows`/`want_size` for the current nav level and request that
    /// size from the compositor. Does not commit (caller does).
    fn relayout(&mut self, font: &Font, font_size: f32) {
        let row_h = (font_size + 10.0).round() as i32;
        let mut rows: Vec<Row> = Vec::new();
        let mut y = VPAD;
        let mut max_label = measure_text(font, font_size, "‹ Back");

        if !self.nav.is_empty() {
            rows.push(Row {
                y0: y,
                y1: y + row_h,
                kind: RowKind::Back,
            });
            y += row_h;
        }
        for n in level_nodes(&self.root, &self.nav) {
            if !n.visible {
                continue;
            }
            if n.separator {
                rows.push(Row {
                    y0: y,
                    y1: y + SEP_H,
                    kind: RowKind::Separator,
                });
                y += SEP_H;
            } else {
                max_label = max_label.max(measure_text(font, font_size, &n.label));
                rows.push(Row {
                    y0: y,
                    y1: y + row_h,
                    kind: RowKind::Entry {
                        id: n.id,
                        label: n.label.clone(),
                        enabled: n.enabled,
                        submenu: n.has_submenu,
                        toggle: n.toggle,
                    },
                });
                y += row_h;
            }
        }
        y += VPAD;

        let w = (PAD_X * 2 + GUTTER + max_label).clamp(MIN_W, MAX_W);
        let h = y.max(row_h + VPAD * 2);
        self.want_size = (w as u32, h as u32);
        self.rows = rows;
        self.hover = self.first_selectable();
        self.layer_surface.set_size(w as u32, h as u32);
    }

    fn reanchor(&self) {
        let edge = match self.position {
            Position::Bottom => Anchor::Bottom,
            Position::Top => Anchor::Top,
        };
        self.layer_surface.set_anchor(edge | Anchor::Left);
        let menu_w = self.want_size.0 as i32;
        let left = self
            .anchor_x
            .clamp(0, (self.output_w as i32 - menu_w).max(0));
        let (top, bottom) = match self.position {
            Position::Bottom => (0, self.bar_h as i32),
            Position::Top => (self.bar_h as i32, 0),
        };
        self.layer_surface.set_margin(top, 0, bottom, left);
    }

    /// Apply a navigation change (into/back), re-laying out and committing.
    pub fn renavigate(&mut self, font: &Font, font_size: f32) {
        self.relayout(font, font_size);
        self.reanchor();
        self.surface.commit();
    }

    pub fn nav_into(&mut self, id: i32) {
        self.nav.push(id);
    }

    /// Pop a level; returns false if already at the root (→ caller closes).
    pub fn nav_back(&mut self) -> bool {
        self.nav.pop().is_some()
    }

    fn selectable(&self, idx: usize) -> bool {
        matches!(
            self.rows.get(idx).map(|r| &r.kind),
            Some(RowKind::Back) | Some(RowKind::Entry { enabled: true, .. })
        )
    }

    fn first_selectable(&self) -> Option<usize> {
        (0..self.rows.len()).find(|&i| self.selectable(i))
    }

    /// Move the highlight by `delta` rows (±1), skipping non-selectable rows
    /// and wrapping at the ends.
    pub fn move_hover(&mut self, delta: i32) {
        let n = self.rows.len();
        if n == 0 {
            return;
        }
        let start = self.hover.unwrap_or(0);
        let mut i = start as i32;
        for _ in 0..n {
            i = (i + delta).rem_euclid(n as i32);
            if self.selectable(i as usize) {
                self.hover = Some(i as usize);
                return;
            }
        }
    }

    /// Set the highlight from a surface-local pointer y (logical). Returns
    /// whether the highlight changed (→ redraw).
    pub fn hover_at(&mut self, y: f64) -> bool {
        let yi = y.round() as i32;
        let hit = self
            .rows
            .iter()
            .position(|r| yi >= r.y0 && yi < r.y1 && row_selectable(&r.kind));
        if hit != self.hover && hit.is_some() {
            self.hover = hit;
            true
        } else {
            false
        }
    }

    /// Resolve the currently highlighted row to an [`Action`] (Enter / click).
    pub fn activate_hover(&self) -> Action {
        match self.hover.and_then(|i| self.rows.get(i)).map(|r| &r.kind) {
            Some(RowKind::Back) => Action::NavBack,
            Some(RowKind::Entry {
                id,
                submenu,
                enabled,
                ..
            }) if *enabled => {
                if *submenu {
                    Action::NavInto(*id)
                } else {
                    Action::Activate(*id)
                }
            }
            _ => Action::None,
        }
    }

    /// Map a raw evdev keycode to an [`Action`]. Hover-moving keys mutate state
    /// and return `None` (the caller always redraws after input).
    pub fn key(&mut self, code: u32) -> Action {
        match code {
            KEY_ESC => {
                if self.nav.is_empty() {
                    Action::Close
                } else {
                    Action::NavBack
                }
            }
            KEY_LEFT => {
                if self.nav.is_empty() {
                    Action::None
                } else {
                    Action::NavBack
                }
            }
            KEY_UP => {
                self.move_hover(-1);
                Action::None
            }
            KEY_DOWN => {
                self.move_hover(1);
                Action::None
            }
            KEY_ENTER | KEY_KPENTER | KEY_RIGHT => self.activate_hover(),
            _ => Action::None,
        }
    }

    /// Compose the menu into its buffer and attach. No-op until configured.
    pub fn render(
        &mut self,
        qh: &QueueHandle<State>,
        shm: &WlShm,
        font: &Font,
        scale: f64,
        font_size: f32,
        bg: u32,
        fg: u32,
    ) -> anyhow::Result<()> {
        let Some((w, h)) = self.size else {
            return Ok(());
        };
        let pw = ((w as f64) * scale).round().max(1.0) as u32;
        let ph = ((h as f64) * scale).round().max(1.0) as u32;
        let s = |v: i32| ((v as f64) * scale).round() as i32;
        let font_px = font_size * scale as f32;
        let stride = pw as i32 * 4;
        let size = (stride as usize) * ph as usize;

        if self.buffer.as_ref().is_none_or(|b| b.dims != (pw, ph)) {
            if let Some(old) = self.buffer.take() {
                old.buffer.destroy();
            }
            let tmp = tempfile::tempfile()?;
            tmp.set_len(size as u64)?;
            let pool = shm.create_pool(tmp.as_fd(), size as i32, qh, ());
            let buffer =
                pool.create_buffer(0, pw as i32, ph as i32, stride, Format::Argb8888, qh, ());
            pool.destroy();
            self.buffer = Some(BarBuffer {
                tmp,
                buffer,
                dims: (pw, ph),
            });
        }

        let mut mmap =
            unsafe { MmapMut::map_mut(&self.buffer.as_ref().expect("buffer ensured").tmp)? };
        fill_bg(&mut mmap, pw, ph, bg);
        // 1px frame so the menu reads as a distinct surface over windows.
        fill_rect(&mut mmap, pw, ph, 0, 0, pw as i32, s(1), DIM);
        fill_rect(&mut mmap, pw, ph, 0, ph as i32 - s(1), pw as i32, s(1), DIM);
        fill_rect(&mut mmap, pw, ph, 0, 0, s(1), ph as i32, DIM);
        fill_rect(&mut mmap, pw, ph, pw as i32 - s(1), 0, s(1), ph as i32, DIM);

        for (i, row) in self.rows.iter().enumerate() {
            match &row.kind {
                RowKind::Separator => {
                    let y = s((row.y0 + row.y1) / 2);
                    fill_rect(
                        &mut mmap,
                        pw,
                        ph,
                        s(PAD_X),
                        y,
                        pw as i32 - s(PAD_X) * 2,
                        s(1),
                        DIM,
                    );
                }
                RowKind::Back => {
                    let hovered = self.hover == Some(i);
                    if hovered {
                        fill_rect(
                            &mut mmap,
                            pw,
                            ph,
                            s(2),
                            s(row.y0),
                            pw as i32 - s(4),
                            s(row.y1 - row.y0),
                            ACCENT,
                        );
                    }
                    draw_row_text(
                        &mut mmap,
                        pw,
                        ph,
                        font,
                        font_px,
                        s(PAD_X),
                        s(row.y0),
                        s(row.y1),
                        "‹ Back",
                        fg,
                    );
                }
                RowKind::Entry {
                    label,
                    enabled,
                    submenu,
                    toggle,
                    ..
                } => {
                    let hovered = self.hover == Some(i);
                    if hovered {
                        fill_rect(
                            &mut mmap,
                            pw,
                            ph,
                            s(2),
                            s(row.y0),
                            pw as i32 - s(4),
                            s(row.y1 - row.y0),
                            ACCENT,
                        );
                    }
                    let color = if *enabled { fg } else { DIM };
                    draw_row_text(
                        &mut mmap,
                        pw,
                        ph,
                        font,
                        font_px,
                        s(PAD_X),
                        s(row.y0),
                        s(row.y1),
                        label,
                        color,
                    );
                    // Right gutter: submenu arrow, else check for a toggled item.
                    let marker = if *submenu {
                        "›"
                    } else if *toggle == Some(true) {
                        "✓"
                    } else {
                        ""
                    };
                    if !marker.is_empty() {
                        let mw = measure_text(font, font_px, marker);
                        let mx = pw as i32 - s(PAD_X) - mw;
                        draw_row_text(
                            &mut mmap,
                            pw,
                            ph,
                            font,
                            font_px,
                            mx,
                            s(row.y0),
                            s(row.y1),
                            marker,
                            color,
                        );
                    }
                }
            }
        }

        drop(mmap);
        let buffer = &self.buffer.as_ref().expect("buffer ensured").buffer;
        self.surface.attach(Some(buffer), 0, 0);
        if let Some(vp) = self.viewport.as_ref() {
            vp.set_destination(w as i32, h as i32);
        }
        self.surface.damage_buffer(0, 0, pw as i32, ph as i32);
        self.surface.commit();
        Ok(())
    }

    /// Tear down the surface (called on dismiss).
    pub fn destroy(self) {
        if let Some(b) = self.buffer {
            b.buffer.destroy();
        }
        if let Some(vp) = self.viewport {
            vp.destroy();
        }
        self.layer_surface.destroy();
        self.surface.destroy();
    }
}

fn row_selectable(kind: &RowKind) -> bool {
    matches!(kind, RowKind::Back | RowKind::Entry { enabled: true, .. })
}

/// Walk `root.children` down the `nav` path of submenu ids. Returns the slice
/// of nodes at that level (empty if the path no longer resolves).
fn level_nodes<'a>(root: &'a MenuNode, nav: &[i32]) -> &'a [MenuNode] {
    let mut nodes = root.children.as_slice();
    for id in nav {
        match nodes.iter().find(|n| n.id == *id) {
            Some(n) => nodes = n.children.as_slice(),
            None => return &[],
        }
    }
    nodes
}

/// Draw a row's text, vertically centered within the band `[py0, py1)`
/// (physical px). Mirrors the bar's `draw_text` baseline math but centers on
/// the row rather than the whole buffer, blitting through `blit_alpha`.
#[allow(clippy::too_many_arguments)]
fn draw_row_text(
    mmap: &mut MmapMut,
    pw: u32,
    ph: u32,
    font: &Font,
    font_px: f32,
    x: i32,
    py0: i32,
    py1: i32,
    text: &str,
    color: u32,
) {
    let lm = font
        .horizontal_line_metrics(font_px)
        .unwrap_or(fontdue::LineMetrics {
            ascent: font_px * 0.8,
            descent: -font_px * 0.2,
            line_gap: 0.0,
            new_line_size: font_px,
        });
    let band = lm.ascent - lm.descent;
    let cy = (py0 + py1) as f32 / 2.0;
    let baseline = (cy - band / 2.0 + lm.ascent).round() as i32;
    let mut pen = x as f32;
    for ch in text.chars() {
        let (m, bmp) = font.rasterize(ch, font_px);
        let gx = (pen + m.xmin as f32).round() as i32;
        let gy = baseline - (m.ymin + m.height as i32);
        crate::blit_alpha(
            mmap,
            pw,
            ph,
            gx,
            gy,
            m.width as u32,
            m.height as u32,
            &bmp,
            color,
        );
        pen += m.advance_width;
    }
}
