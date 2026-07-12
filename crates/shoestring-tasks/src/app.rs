//! Application state: the window/workspace snapshot, selection, input modes,
//! and the key bindings that turn keystrokes into IPC commands.

use std::path::PathBuf;

use anyhow::{anyhow, Result};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::ListState;
use shoestring_ipc::{OutputNode, Request, Response, ScreenshotRegion, WindowGeometry, WindowNode};

use crate::ipc;

/// One displayed line. Headers structure the list; only [`Row::Window`] rows
/// are selectable.
pub enum Row {
    Workspace {
        index: u8,
        name: String,
        focused: bool,
        count: usize,
    },
    Minimized {
        count: usize,
    },
    Window(Win),
}

/// A window as the UI cares about it — flattened from a [`WindowNode`] plus the
/// 1-based workspace it sits on (`0` for the minimized group).
pub struct Win {
    pub id: String,
    pub title: String,
    pub app_id: String,
    pub focused: bool,
    pub minimized: bool,
    pub sticky: bool,
    pub always_on_top: bool,
    pub maximized: bool,
    pub geometry: Option<WindowGeometry>,
    pub output: Option<String>,
}

/// Modal input state. Most keys act immediately in [`Mode::Normal`]; rename and
/// move-to-workspace collect text first, and force-kill asks to confirm.
pub enum Mode {
    Normal,
    Rename { id: String, buffer: String },
    MoveTo { id: String, buffer: String },
    ConfirmKill { id: String, label: String },
    Help,
}

pub struct App {
    socket: PathBuf,
    pub rows: Vec<Row>,
    pub list_state: ListState,
    pub mode: Mode,
    pub status: String,
    /// Whether empty, non-active workspaces are shown. Off by default to keep
    /// the board tight; toggled with `z`.
    pub show_empty: bool,
    pub should_quit: bool,
    /// Sticky selection across refreshes: remember the focused window's id and
    /// re-find it after re-fetching so a live update doesn't move the cursor.
    selected_id: Option<String>,
    /// Output placements, kept for per-window screenshot region math.
    outputs: Vec<OutputNode>,
    pub connected: bool,
}

impl App {
    pub fn new(socket: PathBuf) -> Self {
        Self {
            socket,
            rows: Vec::new(),
            list_state: ListState::default(),
            mode: Mode::Normal,
            status: "j/k move · Enter focus · ? help · q quit".to_string(),
            show_empty: false,
            should_quit: false,
            selected_id: None,
            outputs: Vec::new(),
            connected: false,
        }
    }

    /// Re-query the WM (`workspaces` + `get_tree`) and rebuild the row list.
    pub fn refresh(&mut self) {
        match self.fetch() {
            Ok(()) => self.connected = true,
            Err(e) => {
                self.connected = false;
                self.rows.clear();
                self.status = format!("not connected: {e}");
            }
        }
    }

    fn fetch(&mut self) -> Result<()> {
        let (active, count, names) = match ipc::request(&self.socket, &Request::Workspaces)? {
            Response::Workspaces {
                active,
                count,
                names,
            } => (active, count, names),
            Response::Error { message } => return Err(anyhow!(message)),
            _ => return Err(anyhow!("unexpected response to workspaces")),
        };
        let (outputs, ws_nodes, minimized) = match ipc::request(&self.socket, &Request::GetTree)? {
            Response::Tree {
                outputs,
                workspaces,
                minimized,
            } => (outputs, workspaces, minimized),
            Response::Error { message } => return Err(anyhow!(message)),
            _ => return Err(anyhow!("unexpected response to get_tree")),
        };
        self.outputs = outputs;

        let mut rows = Vec::new();
        for idx in 1..=count {
            let node = ws_nodes.iter().find(|w| w.index == idx);
            let name = node
                .map(|n| n.name.clone())
                .filter(|n| !n.is_empty())
                .or_else(|| names.get((idx - 1) as usize).cloned())
                .unwrap_or_default();
            let wins = node.map(|n| n.windows.as_slice()).unwrap_or(&[]);
            if wins.is_empty() && !self.show_empty && idx != active {
                continue;
            }
            rows.push(Row::Workspace {
                index: idx,
                name,
                focused: idx == active,
                count: wins.len(),
            });
            // Top-of-stack first reads more naturally than bottom-up.
            for wn in wins.iter().rev() {
                rows.push(Row::Window(win_from_node(wn)));
            }
        }
        if !minimized.is_empty() {
            rows.push(Row::Minimized {
                count: minimized.len(),
            });
            for wn in &minimized {
                rows.push(Row::Window(win_from_node(wn)));
            }
        }
        self.rows = rows;
        self.reconcile_selection();
        Ok(())
    }

    /// Re-anchor the selection on the remembered window id after a rebuild,
    /// falling back to the focused window, then the first window row.
    fn reconcile_selection(&mut self) {
        let want = self.selected_id.clone();
        let by_id = want.and_then(|id| self.row_index_of(&id));
        let focused = || {
            self.rows
                .iter()
                .position(|r| matches!(r, Row::Window(w) if w.focused))
        };
        let pick = by_id
            .or_else(focused)
            .or_else(|| self.rows.iter().position(|r| matches!(r, Row::Window(_))));
        self.list_state.select(pick);
        self.selected_id = pick.and_then(|i| self.win_at(i)).map(|w| w.id.clone());
    }

    fn row_index_of(&self, id: &str) -> Option<usize> {
        self.rows
            .iter()
            .position(|r| matches!(r, Row::Window(w) if w.id == id))
    }

    fn win_at(&self, idx: usize) -> Option<&Win> {
        match self.rows.get(idx) {
            Some(Row::Window(w)) => Some(w),
            _ => None,
        }
    }

    fn selected_win(&self) -> Option<&Win> {
        self.list_state.selected().and_then(|i| self.win_at(i))
    }

    /// Move the selection to the next/prev selectable (window) row.
    fn step_selection(&mut self, forward: bool) {
        if self.rows.is_empty() {
            return;
        }
        let start = self.list_state.selected().unwrap_or(0);
        let n = self.rows.len();
        let mut i = start;
        for _ in 0..n {
            i = if forward {
                (i + 1) % n
            } else {
                (i + n - 1) % n
            };
            if matches!(self.rows.get(i), Some(Row::Window(_))) {
                self.list_state.select(Some(i));
                self.selected_id = self.win_at(i).map(|w| w.id.clone());
                return;
            }
        }
    }

    fn select_edge(&mut self, last: bool) {
        let idx = if last {
            self.rows.iter().rposition(|r| matches!(r, Row::Window(_)))
        } else {
            self.rows.iter().position(|r| matches!(r, Row::Window(_)))
        };
        if let Some(i) = idx {
            self.list_state.select(Some(i));
            self.selected_id = self.win_at(i).map(|w| w.id.clone());
        }
    }

    /// Handle a keystroke. Returns `true` when the caller should re-fetch the
    /// snapshot (an action that changed WM state was issued).
    pub fn on_key(&mut self, key: KeyEvent) -> bool {
        // Ctrl-C quits from anywhere.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return false;
        }
        match &self.mode {
            Mode::Normal => self.on_key_normal(key),
            Mode::Help => {
                self.mode = Mode::Normal;
                false
            }
            Mode::ConfirmKill { .. } => self.on_key_confirm(key),
            Mode::Rename { .. } | Mode::MoveTo { .. } => self.on_key_prompt(key),
        }
    }

    fn on_key_normal(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                self.should_quit = true;
                false
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.step_selection(true);
                false
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.step_selection(false);
                false
            }
            KeyCode::Char('g') | KeyCode::Home => {
                self.select_edge(false);
                false
            }
            KeyCode::Char('G') | KeyCode::End => {
                self.select_edge(true);
                false
            }
            KeyCode::Char('?') => {
                self.mode = Mode::Help;
                false
            }
            KeyCode::Char('z') => {
                self.show_empty = !self.show_empty;
                self.status = format!(
                    "empty workspaces {}",
                    if self.show_empty { "shown" } else { "hidden" }
                );
                true
            }
            KeyCode::F(5) => true,
            KeyCode::Enter => self.cmd_focus(),
            KeyCode::Char('x') => self.cmd_close(),
            KeyCode::Char('X') => self.begin_confirm_kill(),
            KeyCode::Char('r') => self.begin_rename(),
            KeyCode::Char('m') => self.cmd_toggle_minimize(),
            KeyCode::Char('M') => self.cmd_toggle_maximize(),
            KeyCode::Char('t') => self.cmd_toggle_sticky(),
            KeyCode::Char('a') => self.cmd_toggle_aot(),
            KeyCode::Char('R') => self.cmd_raise(),
            KeyCode::Char('L') => self.cmd_lower(),
            KeyCode::Char('s') => self.cmd_screenshot(),
            KeyCode::Char('w') | KeyCode::Char('>') => self.begin_move(),
            KeyCode::Char(c @ '0'..='9') => {
                // Quick move: 1-9 → that workspace, 0 → workspace 10.
                let ws = if c == '0' { 10 } else { c as u8 - b'0' };
                self.cmd_move_to(ws)
            }
            _ => false,
        }
    }

    fn on_key_confirm(&mut self, key: KeyEvent) -> bool {
        let Mode::ConfirmKill { id, label } = &self.mode else {
            return false;
        };
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                let id = id.clone();
                let label = label.clone();
                self.mode = Mode::Normal;
                let r = ipc::request_ok(&self.socket, &Request::KillWindow { id });
                self.set_result(r, format!("force-killed {label}"));
                true
            }
            _ => {
                self.mode = Mode::Normal;
                self.status = "kill cancelled".into();
                false
            }
        }
    }

    fn on_key_prompt(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.status = "cancelled".into();
                false
            }
            KeyCode::Enter => self.commit_prompt(),
            KeyCode::Backspace => {
                match &mut self.mode {
                    Mode::Rename { buffer, .. } | Mode::MoveTo { buffer, .. } => {
                        buffer.pop();
                    }
                    _ => {}
                }
                false
            }
            KeyCode::Char(c) => {
                match &mut self.mode {
                    Mode::Rename { buffer, .. } => buffer.push(c),
                    // Move prompt only accepts digits.
                    Mode::MoveTo { buffer, .. } if c.is_ascii_digit() => buffer.push(c),
                    _ => {}
                }
                false
            }
            _ => false,
        }
    }

    fn commit_prompt(&mut self) -> bool {
        match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Rename { id, buffer } => {
                let r = ipc::request_ok(
                    &self.socket,
                    &Request::SetWindowName {
                        id,
                        name: buffer.clone(),
                    },
                );
                let msg = if buffer.is_empty() {
                    "name cleared".to_string()
                } else {
                    format!("renamed to {buffer:?}")
                };
                self.set_result(r, msg);
                true
            }
            Mode::MoveTo { id, buffer } => {
                let Ok(index) = buffer.parse::<u8>() else {
                    self.status = "move cancelled (no workspace entered)".into();
                    return false;
                };
                let r =
                    ipc::request_ok(&self.socket, &Request::MoveWindowToWorkspace { id, index });
                self.set_result(r, format!("moved to workspace {index}"));
                true
            }
            _ => false,
        }
    }

    // --- command helpers -------------------------------------------------

    fn cmd_focus(&mut self) -> bool {
        let Some(w) = self.selected_win() else {
            return false;
        };
        let (id, label) = (w.id.clone(), label_of(w));
        let r = ipc::request_ok(&self.socket, &Request::FocusWindow { id });
        self.set_result(r, format!("focused {label}"));
        true
    }

    fn cmd_close(&mut self) -> bool {
        let Some(w) = self.selected_win() else {
            return false;
        };
        let (id, label) = (w.id.clone(), label_of(w));
        let r = ipc::request_ok(&self.socket, &Request::CloseWindow { id });
        self.set_result(r, format!("close sent to {label}"));
        true
    }

    fn cmd_toggle_minimize(&mut self) -> bool {
        let Some(w) = self.selected_win() else {
            return false;
        };
        let (id, label, to) = (w.id.clone(), label_of(w), !w.minimized);
        let r = ipc::request_ok(
            &self.socket,
            &Request::SetWindowMinimized { id, minimized: to },
        );
        let verb = if to { "minimized" } else { "restored" };
        self.set_result(r, format!("{verb} {label}"));
        true
    }

    fn cmd_toggle_maximize(&mut self) -> bool {
        let Some(w) = self.selected_win() else {
            return false;
        };
        let (id, label, to) = (w.id.clone(), label_of(w), !w.maximized);
        let r = ipc::request_ok(
            &self.socket,
            &Request::SetWindowMaximized { id, maximized: to },
        );
        let verb = if to { "maximized" } else { "unmaximized" };
        self.set_result(r, format!("{verb} {label}"));
        true
    }

    fn cmd_toggle_sticky(&mut self) -> bool {
        let Some(w) = self.selected_win() else {
            return false;
        };
        let (id, label, to) = (w.id.clone(), label_of(w), !w.sticky);
        let r = ipc::request_ok(&self.socket, &Request::SetWindowSticky { id, sticky: to });
        let verb = if to { "pinned (sticky)" } else { "unpinned" };
        self.set_result(r, format!("{verb} {label}"));
        true
    }

    fn cmd_toggle_aot(&mut self) -> bool {
        let Some(w) = self.selected_win() else {
            return false;
        };
        let (id, label, to) = (w.id.clone(), label_of(w), !w.always_on_top);
        let r = ipc::request_ok(
            &self.socket,
            &Request::SetWindowAlwaysOnTop {
                id,
                always_on_top: to,
            },
        );
        let verb = if to {
            "always-on-top"
        } else {
            "normal stacking"
        };
        self.set_result(r, format!("{label}: {verb}"));
        true
    }

    fn cmd_raise(&mut self) -> bool {
        let Some(w) = self.selected_win() else {
            return false;
        };
        let (id, label) = (w.id.clone(), label_of(w));
        let r = ipc::request_ok(&self.socket, &Request::RaiseWindow { id });
        self.set_result(r, format!("raised {label}"));
        true
    }

    fn cmd_lower(&mut self) -> bool {
        let Some(w) = self.selected_win() else {
            return false;
        };
        let (id, label) = (w.id.clone(), label_of(w));
        let r = ipc::request_ok(&self.socket, &Request::LowerWindow { id });
        self.set_result(r, format!("lowered {label}"));
        true
    }

    fn cmd_move_to(&mut self, index: u8) -> bool {
        let Some(w) = self.selected_win() else {
            return false;
        };
        let (id, label) = (w.id.clone(), label_of(w));
        let r = ipc::request_ok(&self.socket, &Request::MoveWindowToWorkspace { id, index });
        self.set_result(r, format!("moved {label} to workspace {index}"));
        true
    }

    fn cmd_screenshot(&mut self) -> bool {
        let Some(w) = self.selected_win() else {
            return false;
        };
        let label = label_of(w);
        let (Some(geo), Some(out_name)) = (w.geometry, w.output.clone()) else {
            self.status = format!(
                "{label}: not on a visible workspace — only the active workspace can be shot"
            );
            return false;
        };
        let Some(out) = self.outputs.iter().find(|o| o.name == out_name) else {
            self.status = format!("{label}: output {out_name:?} not found");
            return false;
        };
        // Window geometry is global logical coords; the region is relative to
        // the named output's logical origin.
        let region = ScreenshotRegion {
            x: geo.x - out.x,
            y: geo.y - out.y,
            w: geo.w,
            h: geo.h,
        };
        let req = Request::Screenshot {
            output: Some(out_name),
            region: Some(region),
            path: None,
        };
        match ipc::request(&self.socket, &req) {
            Ok(Response::Screenshot { path }) => {
                self.status = format!("saved {path}");
            }
            Ok(Response::Error { message }) => {
                self.status = format!("screenshot failed: {message}");
            }
            Ok(_) => self.status = "screenshot: unexpected response".into(),
            Err(e) => self.status = format!("screenshot failed: {e}"),
        }
        false
    }

    fn begin_rename(&mut self) -> bool {
        if let Some(w) = self.selected_win() {
            self.mode = Mode::Rename {
                id: w.id.clone(),
                buffer: String::new(),
            };
            self.status = "rename: type a name (empty clears), Enter to confirm, Esc cancel".into();
        }
        false
    }

    fn begin_move(&mut self) -> bool {
        if let Some(w) = self.selected_win() {
            self.mode = Mode::MoveTo {
                id: w.id.clone(),
                buffer: String::new(),
            };
            self.status = "move: type a workspace number, Enter to confirm, Esc cancel".into();
        }
        false
    }

    fn begin_confirm_kill(&mut self) -> bool {
        if let Some(w) = self.selected_win() {
            self.mode = Mode::ConfirmKill {
                id: w.id.clone(),
                label: label_of(w),
            };
        }
        false
    }

    fn set_result(&mut self, r: Result<()>, ok_msg: String) {
        self.status = match r {
            Ok(()) => ok_msg,
            Err(e) => format!("error: {e}"),
        };
    }
}

fn win_from_node(n: &WindowNode) -> Win {
    Win {
        id: n.id.clone(),
        title: n.title.clone(),
        app_id: n.app_id.clone(),
        focused: n.focused,
        minimized: n.minimized,
        sticky: n.sticky,
        always_on_top: n.always_on_top,
        maximized: n.layout == "maximized",
        geometry: n.geometry,
        output: n.output.clone(),
    }
}

/// A short human label for status messages: the title, or the app_id, or the id.
pub fn label_of(w: &Win) -> String {
    if !w.title.is_empty() {
        w.title.clone()
    } else if !w.app_id.is_empty() {
        w.app_id.clone()
    } else {
        w.id.clone()
    }
}
