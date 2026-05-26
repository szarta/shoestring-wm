//! Shared/global workspaces. All outputs swap together when the active
//! workspace changes; per-window monitor assignment is preserved separately.
//!
//! Off-workspace windows are unmapped from the [`Space`] but kept in the
//! manager's tracking so we can restore them at their saved position when
//! their workspace becomes active again.
//!
//! Count and optional names come from `[workspaces]` in the WM config
//! (see [`shoestring_config::Workspaces`]). Reading the count or a
//! display name goes through [`WorkspaceManager`].

use std::collections::HashMap;

use smithay::{
    desktop::Window,
    utils::{IsAlive, Logical, Point},
    wayland::shell::xdg::ToplevelSurface,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WorkspaceId(u8);

impl WorkspaceId {
    pub const FIRST: Self = Self(0);

    pub fn zero_based(self) -> u8 {
        self.0
    }

    pub fn one_based(self) -> u8 {
        self.0 + 1
    }
}

#[derive(Debug, Clone, Copy)]
struct WindowInfo {
    workspace: WorkspaceId,
    /// Last-known location in the space — used to re-map at the same spot when
    /// the window's workspace becomes active again.
    saved_location: Point<i32, Logical>,
}

pub struct WorkspaceManager {
    active: WorkspaceId,
    windows: HashMap<Window, WindowInfo>,
    /// Per-workspace MRU focus stack; most-recently-focused window at the end.
    focus_history: Vec<Vec<Window>>,
    /// Total workspace count, 1..=`MAX_WORKSPACE_COUNT` from
    /// `[workspaces].count`. Drives `shifted` clamping, `from_one_based`
    /// validation, and the IPC `Workspaces` response.
    count: u8,
    /// Display name per workspace, length == `count`. Empty string for
    /// slots the user didn't name (callers should render the 1-based
    /// number in that case).
    names: Vec<String>,
}

impl WorkspaceManager {
    /// Construct with `count` workspaces. Each focus history starts
    /// empty. `names` length is grown to `count` with empty strings;
    /// extra entries are dropped.
    pub fn new(count: u8, names: Vec<String>) -> Self {
        let count = count.max(1);
        let mut names = names;
        names.resize(count as usize, String::new());
        Self {
            active: WorkspaceId::FIRST,
            windows: HashMap::new(),
            focus_history: (0..count).map(|_| Vec::new()).collect(),
            count,
            names,
        }
    }

    /// Number of workspaces this manager was constructed with.
    pub fn count(&self) -> u8 {
        self.count
    }

    /// Snapshot of every workspace name in 1-based order. Length
    /// equals [`Self::count`]. Used by the IPC `Workspaces` reply.
    pub fn name_list(&self) -> Vec<String> {
        self.names.clone()
    }

    /// Build a [`WorkspaceId`] from a 1-based config index. Returns
    /// `None` for 0 or anything past `self.count`. Named with the
    /// `from_` prefix to match the convention used by the rest of the
    /// codebase for index-conversion helpers; clippy's
    /// `wrong-self-convention` lint is intentionally suppressed here
    /// (renaming to `parse_one_based` would be misleading — we're not
    /// parsing, we're validating against runtime state).
    #[allow(clippy::wrong_self_convention)]
    pub fn from_one_based(&self, idx: u8) -> Option<WorkspaceId> {
        (1..=self.count)
            .contains(&idx)
            .then(|| WorkspaceId(idx - 1))
    }

    /// Apply a signed offset to `ws`, clamping to `[0, count - 1]`. No wrap.
    pub fn shifted(&self, ws: WorkspaceId, delta: i32) -> WorkspaceId {
        let new = (ws.0 as i32 + delta).clamp(0, self.count as i32 - 1);
        WorkspaceId(new as u8)
    }

    pub fn active(&self) -> WorkspaceId {
        self.active
    }

    pub fn set_active(&mut self, ws: WorkspaceId) {
        self.active = ws;
    }

    /// Register a window with a workspace + initial location. Idempotent —
    /// later calls overwrite the assignment.
    pub fn assign(
        &mut self,
        window: Window,
        workspace: WorkspaceId,
        location: Point<i32, Logical>,
    ) {
        self.windows.insert(
            window,
            WindowInfo {
                workspace,
                saved_location: location,
            },
        );
    }

    /// Move a known window to a different workspace, preserving its location.
    pub fn reassign(&mut self, window: &Window, workspace: WorkspaceId) {
        if let Some(info) = self.windows.get_mut(window) {
            info.workspace = workspace;
        }
    }

    /// Update the saved location (call before unmapping during a workspace switch).
    pub fn record_location(&mut self, window: &Window, location: Point<i32, Logical>) {
        if let Some(info) = self.windows.get_mut(window) {
            info.saved_location = location;
        }
    }

    pub fn saved_location(&self, window: &Window) -> Option<Point<i32, Logical>> {
        self.windows.get(window).map(|i| i.saved_location)
    }

    /// Windows assigned to a given workspace (in insertion order — fine for our use).
    pub fn windows_on(&self, ws: WorkspaceId) -> Vec<Window> {
        self.windows
            .iter()
            .filter(|(_, info)| info.workspace == ws)
            .map(|(w, _)| w.clone())
            .collect()
    }

    /// Every tracked window with its workspace. Used by IPC `Windows` queries.
    pub fn windows_on_any(&self) -> impl Iterator<Item = (Window, WorkspaceId)> + '_ {
        self.windows
            .iter()
            .map(|(w, info)| (w.clone(), info.workspace))
    }

    pub fn forget(&mut self, window: &Window) {
        self.windows.remove(window);
        for stack in &mut self.focus_history {
            stack.retain(|w| w != window);
        }
    }

    /// Drop tracking for any window whose toplevel surface is dead. Cheaper
    /// than walking `XdgShellHandler::toplevel_destroyed` for every quit; we
    /// run this opportunistically before a workspace switch.
    pub fn prune_dead(&mut self) {
        self.windows.retain(|w, _| w.alive());
        for stack in &mut self.focus_history {
            stack.retain(|w| w.alive());
        }
    }

    /// Push `window` onto the MRU stack for its workspace, deduping prior entries.
    pub fn record_focus(&mut self, ws: WorkspaceId, window: &Window) {
        let stack = &mut self.focus_history[ws.zero_based() as usize];
        stack.retain(|w| w != window);
        stack.push(window.clone());
    }

    /// Most-recently-focused still-alive window on `ws`. Drops dead entries
    /// encountered along the way.
    pub fn last_focused(&mut self, ws: WorkspaceId) -> Option<Window> {
        let stack = &mut self.focus_history[ws.zero_based() as usize];
        while let Some(w) = stack.last().cloned() {
            if w.alive() {
                return Some(w);
            }
            stack.pop();
        }
        None
    }

    /// Pop the top-of-stack focus entry for `ws` unconditionally. Used when a
    /// candidate from [`Self::last_focused`] can't actually be focused (e.g.
    /// it's been minimized so it's not in the space).
    pub fn discard_top_focus(&mut self, ws: WorkspaceId) {
        self.focus_history[ws.zero_based() as usize].pop();
    }

    /// Drop `window` from `ws`'s MRU history without touching its workspace
    /// assignment. Used when moving a window off a workspace so it doesn't
    /// resurface as a focus target if the user comes back.
    pub fn remove_from_active_focus(&mut self, ws: WorkspaceId, window: &Window) {
        self.focus_history[ws.zero_based() as usize].retain(|w| w != window);
    }

    /// Look up a tracked window by its xdg toplevel surface. Covers both
    /// on-workspace and currently-off-workspace windows since the manager
    /// holds all of them.
    pub fn find_by_toplevel(&self, surface: &ToplevelSurface) -> Option<Window> {
        self.windows
            .keys()
            .find(|w| w.toplevel().map(|t| t == surface).unwrap_or(false))
            .cloned()
    }
}
