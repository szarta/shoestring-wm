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
    xdg_positioner::{Anchor, ConstraintAdjustment, Gravity, XdgPositioner},
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
/// UI-only variants directly to `State.menu`, and queue the tray ones (which
/// need the D-Bus connection that lives outside `State`).
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Nothing actionable (e.g. clicked a separator or disabled row).
    None,
    /// Dismiss the menu.
    Close,
    /// Pop one submenu level.
    NavBack,
    /// Descend into the submenu with this label (ids churn — see [`crate::tray`]).
    NavInto(String),
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
    xdg_surface: XdgSurface,
    xdg_popup: XdgPopup,
    /// `xdg_wm_base`, kept so submenu navigation can build a fresh positioner
    /// for `xdg_popup.reposition`.
    wm_base: XdgWmBase,
    viewport: Option<WpViewport>,
    buffer: Option<BarBuffer>,
    /// Size from the compositor's `xdg_popup.configure` (logical). Render
    /// no-ops until the first configure arrives.
    size: Option<(u32, u32)>,
    /// Size we request via the positioner (logical); also the hit-test basis.
    want_size: (u32, u32),
    /// Which tray item (index into `Tray.items`) this menu belongs to.
    pub tray_idx: usize,
    /// Entries at the level currently displayed.
    current: Vec<MenuNode>,
    /// Saved parent levels, innermost last — restored on "Back".
    stack: Vec<Vec<MenuNode>>,
    /// Submenu labels descended into (depth == `stack.len()`); the fetch context
    /// for re-querying a level with current ids (see [`crate::tray::Tray::fetch_level`]).
    path: Vec<String>,
    /// Display rows for the current level, rebuilt on every (re)layout.
    rows: Vec<Row>,
    /// Currently highlighted row index (into `rows`), if any.
    hover: Option<usize>,
    /// Anchor rectangle (the clicked icon) in the bar surface's local coords —
    /// the compositor positions the popup relative to this. Reused on reposition.
    anchor_rect: (i32, i32, i32, i32),
    position: Position,
    /// Monotonic token for `xdg_popup.reposition`.
    reposition_token: u32,
    /// Widest level seen; the menu never shrinks below this, so descending into
    /// a narrower submenu keeps the column under the pointer (preserves hover).
    min_w: i32,
}

impl Menu {
    /// Create the menu as an `xdg_popup` parented to the bar's layer surface,
    /// positioned by the compositor relative to `anchor_rect` (the clicked
    /// icon's rect in bar-local coords). Grabs the popup with `serial` (the
    /// click's input serial) so the compositor handles click-outside dismissal
    /// and keyboard/pointer focus. Returns `None` if the seat is unavailable.
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

        // Compute the level's rows + size before building the positioner.
        let mut min_w = 0;
        let (rows, want_size) = compute_layout(&entries, false, font, font_size, &mut min_w);

        let positioner = make_positioner(wm_base, qh, want_size, anchor_rect, position);
        let xdg_popup = xdg_surface.get_popup(None, &positioner, qh, ());
        positioner.destroy();
        // Parent the popup to the bar's layer surface, then grab so the
        // compositor handles click-outside dismissal + keyboard/pointer focus.
        bar_layer.get_popup(&xdg_popup);
        xdg_popup.grab(seat, serial);
        let viewport = viewporter.map(|vp| vp.get_viewport(&surface, qh, ()));
        // Initial commit with no buffer: await the first xdg_surface.configure.
        surface.commit();
        tracing::debug!(
            ?anchor_rect,
            want_w = want_size.0,
            want_h = want_size.1,
            "tray menu open"
        );

        let hover = first_selectable_in(&rows);
        Menu {
            surface,
            xdg_surface,
            xdg_popup,
            wm_base: wm_base.clone(),
            viewport,
            buffer: None,
            size: None,
            want_size,
            tray_idx,
            current: entries,
            stack: Vec::new(),
            path: Vec::new(),
            rows,
            hover,
            anchor_rect,
            position,
            reposition_token: 0,
            min_w,
        }
    }

    /// True if `s` is this menu's surface (for routing pointer/keyboard events).
    pub fn owns(&self, s: &WlSurface) -> bool {
        &self.surface == s
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

    /// Rebuild `rows`/`want_size`/`hover` for the current nav level.
    fn relayout(&mut self, font: &Font, font_size: f32) {
        let (rows, want_size) = compute_layout(
            &self.current,
            !self.stack.is_empty(),
            font,
            font_size,
            &mut self.min_w,
        );
        self.want_size = want_size;
        self.rows = rows;
        self.hover = first_selectable_in(&self.rows);
    }

    /// Apply a navigation change (into/back): re-lay out and ask the compositor
    /// to reposition the popup to the new size via `xdg_popup.reposition`.
    pub fn renavigate(&mut self, font: &Font, font_size: f32, qh: &QueueHandle<State>) {
        self.relayout(font, font_size);
        let positioner = make_positioner(
            &self.wm_base,
            qh,
            self.want_size,
            self.anchor_rect,
            self.position,
        );
        self.reposition_token = self.reposition_token.wrapping_add(1);
        self.xdg_popup
            .reposition(&positioner, self.reposition_token);
        positioner.destroy();
    }

    /// The submenu label path currently descended into — the fetch context for
    /// [`crate::tray::Tray::fetch_level`].
    pub fn path(&self) -> Vec<String> {
        self.path.clone()
    }

    /// Descend into a freshly-fetched submenu `label`: save the current level
    /// and make `children` the new one. (Fetched by the caller via
    /// `Tray::fetch_level`, since it needs the D-Bus connection.)
    pub fn push_level(&mut self, label: String, children: Vec<MenuNode>) {
        let prev = std::mem::take(&mut self.current);
        self.stack.push(prev);
        self.path.push(label);
        self.current = children;
    }

    /// Pop a level; returns false if already at the root (→ caller closes).
    pub fn pop_level(&mut self) -> bool {
        if let Some(prev) = self.stack.pop() {
            self.current = prev;
            self.path.pop();
            true
        } else {
            false
        }
    }

    fn selectable(&self, idx: usize) -> bool {
        matches!(
            self.rows.get(idx).map(|r| &r.kind),
            Some(RowKind::Back) | Some(RowKind::Entry { enabled: true, .. })
        )
    }

    #[allow(dead_code)]
    fn first_selectable(&self) -> Option<usize> {
        first_selectable_in(&self.rows)
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

    /// Map a raw evdev keycode to an [`Action`]. Hover-moving keys mutate state
    /// and return `None` (the caller always redraws after input).
    pub fn key(&mut self, code: u32) -> Action {
        match code {
            KEY_ESC => {
                if self.stack.is_empty() {
                    Action::Close
                } else {
                    Action::NavBack
                }
            }
            KEY_LEFT => {
                if self.stack.is_empty() {
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
        tracing::debug!(
            size_w = w,
            size_h = h,
            rows = self.rows.len(),
            "menu render"
        );
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

    /// Tear down the popup (called on dismiss / popup_done). Destroy in the
    /// xdg-shell order: popup, then xdg_surface, then the wl_surface.
    pub fn destroy(self) {
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

/// Build an `xdg_positioner` for `want_size`, anchored to `anchor_rect` (the
/// icon, in bar-local coords) and opening away from the bar edge.
fn make_positioner(
    wm_base: &XdgWmBase,
    qh: &QueueHandle<State>,
    want_size: (u32, u32),
    anchor_rect: (i32, i32, i32, i32),
    position: Position,
) -> XdgPositioner {
    let p = wm_base.create_positioner(qh, ());
    p.set_size(want_size.0.max(1) as i32, want_size.1.max(1) as i32);
    let (ax, ay, aw, ah) = anchor_rect;
    p.set_anchor_rect(ax, ay, aw.max(1), ah.max(1));
    match position {
        Position::Bottom => {
            p.set_anchor(Anchor::Top);
            p.set_gravity(Gravity::Top);
        }
        Position::Top => {
            p.set_anchor(Anchor::Bottom);
            p.set_gravity(Gravity::Bottom);
        }
    }
    p.set_constraint_adjustment(
        ConstraintAdjustment::FlipY | ConstraintAdjustment::SlideX | ConstraintAdjustment::ResizeY,
    );
    p
}

/// First selectable (Back or enabled Entry) row index, if any.
fn first_selectable_in(rows: &[Row]) -> Option<usize> {
    (0..rows.len()).find(|&i| row_selectable(&rows[i].kind))
}

/// Compute display `rows` + logical `want_size` for `current` (plus a leading
/// "‹ Back" row when `has_back`). `min_w` is the widest level seen so far and is
/// bumped (never shrinks) so descending into a narrower submenu keeps the column
/// under the pointer.
fn compute_layout(
    current: &[MenuNode],
    has_back: bool,
    font: &Font,
    font_size: f32,
    min_w: &mut i32,
) -> (Vec<Row>, (u32, u32)) {
    let row_h = (font_size + 10.0).round() as i32;
    let mut rows: Vec<Row> = Vec::new();
    let mut y = VPAD;
    let mut max_label = measure_text(font, font_size, "‹ Back");

    if has_back {
        rows.push(Row {
            y0: y,
            y1: y + row_h,
            kind: RowKind::Back,
        });
        y += row_h;
    }
    for n in current {
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
    let w = *min_w;
    let h = y.max(row_h + VPAD * 2);
    (rows, (w as u32, h as u32))
}

fn row_selectable(kind: &RowKind) -> bool {
    matches!(kind, RowKind::Back | RowKind::Entry { enabled: true, .. })
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
