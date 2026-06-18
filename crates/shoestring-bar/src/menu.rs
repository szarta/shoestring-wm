//! Tray context menu — a cascade of `xdg_popup`s rendered from a
//! `com.canonical.dbusmenu` layout.
//!
//! Clicking a tray icon opens the root menu: an `xdg_popup` parented to the
//! bar's layer surface (`zwlr_layer_surface_v1.get_popup`), positioned by the
//! compositor via an `xdg_positioner` and grabbed (`xdg_popup.grab`) so the
//! compositor owns click-outside dismissal + focus. Submenus open as **nested
//! child popups** anchored to the parent row and opening to the side (like
//! GTK/Qt menus) — the grab covers the whole popup chain, so one grab on the
//! root suffices.
//!
//! Each level is a [`Level`] with its own surface/popup/buffer. The menu tracks
//! which level the pointer is over (`active`) to route clicks, and re-fetches
//! each submenu by label on descent (ids churn — see [`crate::tray`]).

use std::os::fd::AsFd;

use fontdue::Font;
use memmap2::MmapMut;
use wayland_client::{
    protocol::{
        wl_compositor::WlCompositor,
        wl_seat::WlSeat,
        wl_shm::{Format, WlShm},
        wl_surface::WlSurface,
    },
    QueueHandle,
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};
use wayland_protocols::xdg::shell::client::{
    xdg_popup::XdgPopup,
    xdg_positioner::{Anchor, ConstraintAdjustment, Gravity},
    xdg_surface::XdgSurface,
    xdg_wm_base::XdgWmBase,
};
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::ZwlrLayerSurfaceV1;

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
/// UI-only variants directly; tray variants are queued (they need the D-Bus
/// connection that lives outside `State`).
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Nothing actionable (separator / disabled / empty).
    None,
    /// Dismiss the whole menu.
    Close,
    /// Close the deepest open submenu level (keyboard ←).
    NavBack,
    /// Open the submenu with this label as a child of the active level.
    NavInto(String),
    /// Send a `clicked` event for this dbusmenu id, then dismiss.
    Activate(i32),
}

enum RowKind {
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

/// One open menu level: its own popup surface + the rows it shows.
struct Level {
    surface: WlSurface,
    xdg_surface: XdgSurface,
    xdg_popup: XdgPopup,
    viewport: Option<WpViewport>,
    buffer: Option<BarBuffer>,
    /// Size from the compositor's `xdg_popup.configure` (logical).
    size: Option<(u32, u32)>,
    /// Size requested via the positioner (logical); the hit-test/layout basis.
    want_size: (u32, u32),
    rows: Vec<Row>,
    hover: Option<usize>,
    /// Submenu label this level represents (`None` for the root level).
    label: Option<String>,
}

pub struct Menu {
    pub tray_idx: usize,
    wm_base: XdgWmBase,
    /// Open levels, root first. `levels[0]` is parented to the bar; each deeper
    /// level is a child popup anchored to its parent row.
    levels: Vec<Level>,
    /// Index of the level the pointer is currently over (for click routing).
    active: Option<usize>,
}

impl Menu {
    /// Open the root menu as an `xdg_popup` parented to the bar's layer surface,
    /// positioned against `anchor_rect` (the clicked icon, in bar-local coords)
    /// and grabbed with `serial` so the compositor handles dismissal + focus.
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        compositor: &WlCompositor,
        wm_base: &XdgWmBase,
        bar_layer: &ZwlrLayerSurfaceV1,
        viewporter: Option<&WpViewporter>,
        qh: &QueueHandle<State>,
        entries: Vec<MenuNode>,
        tray_idx: usize,
        anchor_rect: (i32, i32, i32, i32),
        position: Position,
        seat: &WlSeat,
        serial: u32,
        font: &Font,
        font_size: f32,
    ) -> Menu {
        let surface = compositor.create_surface(qh, ());
        let xdg_surface = wm_base.get_xdg_surface(&surface, qh, ());

        let mut min_w = 0;
        let (rows, want_size) = compute_layout(&entries, font, font_size, &mut min_w);

        // Root: anchor to the icon, open away from the bar edge.
        let (anchor, gravity) = match position {
            Position::Bottom => (Anchor::Top, Gravity::Top),
            Position::Top => (Anchor::Bottom, Gravity::Bottom),
        };
        let positioner = make_positioner(wm_base, qh, want_size, anchor_rect, anchor, gravity);
        let xdg_popup = xdg_surface.get_popup(None, &positioner, qh, ());
        positioner.destroy();
        bar_layer.get_popup(&xdg_popup);
        xdg_popup.grab(seat, serial);
        let viewport = viewporter.map(|vp| vp.get_viewport(&surface, qh, ()));
        surface.commit();
        tracing::debug!(
            ?anchor_rect,
            want_w = want_size.0,
            want_h = want_size.1,
            "tray menu open"
        );

        let hover = first_selectable_in(&rows);
        Menu {
            tray_idx,
            wm_base: wm_base.clone(),
            levels: vec![Level {
                surface,
                xdg_surface,
                xdg_popup,
                viewport,
                buffer: None,
                size: None,
                want_size,
                rows,
                hover,
                label: None,
            }],
            active: Some(0),
        }
    }

    /// Open `entries` as a child popup anchored to the row labelled `label` in
    /// level `parent` (which must be the deepest level; callers truncate first).
    /// The child opens to the side; the root grab covers it (no re-grab).
    #[allow(clippy::too_many_arguments)]
    pub fn push_child(
        &mut self,
        parent: usize,
        label: String,
        entries: Vec<MenuNode>,
        compositor: &WlCompositor,
        viewporter: Option<&WpViewporter>,
        qh: &QueueHandle<State>,
        font: &Font,
        font_size: f32,
    ) {
        let Some(parent_level) = self.levels.get(parent) else {
            return;
        };
        // Anchor rect = the parent row's band spanning the parent popup's width,
        // so the child opens off its right edge, top-aligned with the row.
        let row = parent_level
            .rows
            .iter()
            .find(|r| matches!(&r.kind, RowKind::Entry { label: l, .. } if *l == label));
        let (ry0, ry1) = row.map(|r| (r.y0, r.y1)).unwrap_or((0, 0));
        let parent_w = parent_level.want_size.0 as i32;
        let anchor_rect = (0, ry0, parent_w, (ry1 - ry0).max(1));
        let parent_xdg = parent_level.xdg_surface.clone();

        let surface = compositor.create_surface(qh, ());
        let xdg_surface = wm_base_get_xdg_surface(&self.wm_base, &surface, qh);
        let mut min_w = 0;
        let (rows, want_size) = compute_layout(&entries, font, font_size, &mut min_w);
        let positioner = make_positioner(
            &self.wm_base,
            qh,
            want_size,
            anchor_rect,
            Anchor::TopRight,
            Gravity::BottomRight,
        );
        let xdg_popup = xdg_surface.get_popup(Some(&parent_xdg), &positioner, qh, ());
        positioner.destroy();
        let viewport = viewporter.map(|vp| vp.get_viewport(&surface, qh, ()));
        surface.commit();

        let hover = first_selectable_in(&rows);
        self.levels.push(Level {
            surface,
            xdg_surface,
            xdg_popup,
            viewport,
            buffer: None,
            size: None,
            want_size,
            rows,
            hover,
            label: Some(label),
        });
        self.active = Some(self.levels.len() - 1);
    }

    /// The submenu label path down to (and including) level `parent` — the
    /// fetch context for the child of `parent` (see `Tray::fetch_level`).
    pub fn path_to(&self, parent: usize) -> Vec<String> {
        self.levels[1..=parent.min(self.levels.len().saturating_sub(1))]
            .iter()
            .filter_map(|l| l.label.clone())
            .collect()
    }

    /// Destroy every level deeper than `keep` (closing child popups).
    pub fn truncate_to(&mut self, keep: usize) {
        while self.levels.len() > keep + 1 {
            if let Some(l) = self.levels.pop() {
                l.destroy();
            }
        }
        if self.active.is_some_and(|a| a >= self.levels.len()) {
            self.active = Some(self.levels.len() - 1);
        }
    }

    /// Close the deepest submenu level (keyboard ←). Returns false at the root.
    pub fn pop_child(&mut self) -> bool {
        if self.levels.len() > 1 {
            if let Some(l) = self.levels.pop() {
                l.destroy();
            }
            self.active = Some(self.levels.len() - 1);
            true
        } else {
            false
        }
    }

    pub fn active(&self) -> Option<usize> {
        self.active
    }

    /// Which level owns `s` (pointer routing); also records it as active.
    pub fn surface_level(&self, s: &WlSurface) -> Option<usize> {
        self.levels.iter().position(|l| &l.surface == s)
    }

    pub fn set_active(&mut self, idx: Option<usize>) {
        if let Some(i) = idx {
            if i < self.levels.len() {
                self.active = Some(i);
            }
        }
    }

    /// Record the acked size for the popup `p`. Returns true if it changed.
    pub fn set_popup_size(&mut self, p: &XdgPopup, w: u32, h: u32) -> bool {
        if let Some(l) = self.levels.iter_mut().find(|l| &l.xdg_popup == p) {
            let new = Some((w.max(1), h.max(1)));
            if l.size != new {
                l.size = new;
                return true;
            }
        }
        false
    }

    /// Highlight the row under surface-local `y` in level `idx`. Returns whether
    /// the highlight changed (→ redraw).
    pub fn hover_at(&mut self, idx: usize, y: f64) -> bool {
        let Some(l) = self.levels.get_mut(idx) else {
            return false;
        };
        let yi = y.round() as i32;
        let hit = l
            .rows
            .iter()
            .position(|r| yi >= r.y0 && yi < r.y1 && row_selectable(&r.kind));
        if hit.is_some() && hit != l.hover {
            l.hover = hit;
            true
        } else {
            false
        }
    }

    /// Resolve the hovered row in the active level to an [`Action`].
    pub fn activate_active(&self) -> Action {
        let Some(l) = self.active.and_then(|i| self.levels.get(i)) else {
            return Action::None;
        };
        match l.hover.and_then(|i| l.rows.get(i)).map(|r| &r.kind) {
            Some(RowKind::Entry {
                id,
                label,
                submenu,
                enabled,
                ..
            }) if *enabled => {
                if *submenu {
                    Action::NavInto(label.clone())
                } else {
                    Action::Activate(*id)
                }
            }
            _ => Action::None,
        }
    }

    /// Map a raw evdev keycode to an [`Action`], operating on the deepest level.
    pub fn key(&mut self, code: u32) -> Action {
        let deepest = self.levels.len() - 1;
        self.active = Some(deepest);
        match code {
            KEY_ESC => Action::Close,
            KEY_LEFT => {
                if self.levels.len() > 1 {
                    Action::NavBack
                } else {
                    Action::None
                }
            }
            KEY_UP => {
                self.move_hover(deepest, -1);
                Action::None
            }
            KEY_DOWN => {
                self.move_hover(deepest, 1);
                Action::None
            }
            KEY_ENTER | KEY_KPENTER | KEY_RIGHT => self.activate_active(),
            _ => Action::None,
        }
    }

    fn move_hover(&mut self, idx: usize, delta: i32) {
        let Some(l) = self.levels.get_mut(idx) else {
            return;
        };
        let n = l.rows.len();
        if n == 0 {
            return;
        }
        let mut i = l.hover.unwrap_or(0) as i32;
        for _ in 0..n {
            i = (i + delta).rem_euclid(n as i32);
            if row_selectable(&l.rows[i as usize].kind) {
                l.hover = Some(i as usize);
                return;
            }
        }
    }

    /// Render every configured level into its buffer.
    pub fn render(
        &mut self,
        qh: &QueueHandle<State>,
        shm: &WlShm,
        font: &Font,
        scale: f64,
        font_size: f32,
        bg: u32,
        fg: u32,
    ) {
        for l in self.levels.iter_mut() {
            if let Err(e) = l.render(qh, shm, font, scale, font_size, bg, fg) {
                tracing::warn!(error = ?e, "tray menu level render failed");
            }
        }
    }

    /// Tear down all levels (deepest first).
    pub fn destroy(mut self) {
        while let Some(l) = self.levels.pop() {
            l.destroy();
        }
    }
}

impl Level {
    fn render(
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
                RowKind::Entry {
                    label,
                    enabled,
                    submenu,
                    toggle,
                    ..
                } => {
                    if self.hover == Some(i) {
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

    /// Destroy in xdg-shell order: popup, xdg_surface, wl_surface.
    fn destroy(self) {
        if let Some(b) = self.buffer {
            b.buffer.destroy();
        }
        if let Some(vp) = self.viewport {
            vp.destroy();
        }
        self.xdg_popup.destroy();
        self.xdg_surface.destroy();
        self.surface.destroy();
    }
}

/// Helper so `push_child` can create the xdg_surface without re-borrowing self
/// for the whole expression (keeps the borrow checker happy alongside `levels`).
fn wm_base_get_xdg_surface(
    wm_base: &XdgWmBase,
    surface: &WlSurface,
    qh: &QueueHandle<State>,
) -> XdgSurface {
    wm_base.get_xdg_surface(surface, qh, ())
}

/// Build an `xdg_positioner` for `want_size` against `anchor_rect`.
fn make_positioner(
    wm_base: &XdgWmBase,
    qh: &QueueHandle<State>,
    want_size: (u32, u32),
    anchor_rect: (i32, i32, i32, i32),
    anchor: Anchor,
    gravity: Gravity,
) -> wayland_protocols::xdg::shell::client::xdg_positioner::XdgPositioner {
    let p = wm_base.create_positioner(qh, ());
    p.set_size(want_size.0.max(1) as i32, want_size.1.max(1) as i32);
    let (ax, ay, aw, ah) = anchor_rect;
    p.set_anchor_rect(ax, ay, aw.max(1), ah.max(1));
    p.set_anchor(anchor);
    p.set_gravity(gravity);
    p.set_constraint_adjustment(
        ConstraintAdjustment::FlipX
            | ConstraintAdjustment::FlipY
            | ConstraintAdjustment::SlideX
            | ConstraintAdjustment::SlideY
            | ConstraintAdjustment::ResizeY,
    );
    p
}

fn row_selectable(kind: &RowKind) -> bool {
    matches!(kind, RowKind::Entry { enabled: true, .. })
}

fn first_selectable_in(rows: &[Row]) -> Option<usize> {
    (0..rows.len()).find(|&i| row_selectable(&rows[i].kind))
}

/// Compute display `rows` + logical `want_size` for `nodes`.
fn compute_layout(
    nodes: &[MenuNode],
    font: &Font,
    font_size: f32,
    min_w: &mut i32,
) -> (Vec<Row>, (u32, u32)) {
    let row_h = (font_size + 10.0).round() as i32;
    let mut rows: Vec<Row> = Vec::new();
    let mut y = VPAD;
    let mut max_label = 0;

    for n in nodes {
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

    let cw = (PAD_X * 2 + GUTTER + max_label).clamp(MIN_W, MAX_W);
    *min_w = (*min_w).max(cw);
    let h = y.max(row_h + VPAD * 2);
    (rows, (*min_w as u32, h as u32))
}

/// Draw a row's text vertically centered within `[py0, py1)` (physical px).
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
