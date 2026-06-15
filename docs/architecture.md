# shoestring-wm — Architecture (v1)

This document is the architecture for shoestring-wm based on research of
Smithay (the framework we'll build on) and niri (the most polished
real-world Smithay consumer).

See `research/notes/smithay.md` and `research/notes/niri.md` for the
underlying research.

**Resolved decisions** (from initial Q&A — see § 11 for details):
- Workspaces are **shared/global** across all monitors (16 total)
- Config format: **TOML**
- IPC: **unix socket + JSON line-delimited**
- XWayland: deferred at start, **now shipped** (forced by GIMP); see `src/xwayland.rs`
- Tile-half: **operates on current monitor only**
- Focus: **configurable**, default **click-to-focus**
- New windows: **centered on active monitor**

---

## 1. Project Goals (Recap)

**What we're building:** A Rust Wayland window manager that replaces the
user's current Openbox/X11 setup. Lightweight, low dependencies,
Openbox-inspired ergonomics.

**Hard requirements (must work on day one of daily-driver use):**
- Floating windows (no tiling tree; tmux handles tiling-within-terminal)
- Snap-to-half (Super+E/W), maximize (Super+M), minimize (Super+D)
- Super+drag for move/resize
- 16 virtual workspaces with keybind switching
- Multi-monitor
- No window decorations by default
- Rich keybinding system (config-driven)

**Explicit non-goals (v1):**
- Animations / fancy transitions
- Window decoration polish
- Built-in bar / status panel (separate companion project)
- Wayland gimmicks (gestures, etc.)

---

## 2. Stack & Dependencies

**Core:**
- `smithay` (git pinned, with default-features off and selective features)
- `calloop` (event loop — re-exported from smithay)
- `xkbcommon` (re-exported from smithay)
- `wayland-server` / `wayland-protocols` (re-exported)

**Smithay features (initial):**
- `wayland_frontend` — required
- `desktop` — for `Space`, `Window`, `LayerMap`
- `backend_winit` — dev/test
- `renderer_gl` — paired with winit
- `backend_libinput` — for real hardware later
- `backend_drm`, `backend_gbm`, `backend_session_libseat`, `backend_udev` —
  added in milestone 7 for native TTY operation
- `xwayland` — added when GIMP forced X11 support
- *Not enabled:* `backend_vulkan`, `renderer_multi`, `renderer_pixman`

**Beyond smithay (kept deliberately small):**
- `tracing` + `tracing-subscriber` — logging (matches smithay convention)
- `thiserror` / `anyhow` — error handling
- `serde` + a config format crate — see § 11 open Q on format choice
- `notify` — config hot-reload (milestone 9+)

**No tokio / async runtime.** Smithay is sync; we use calloop's futures
adapter (`event_loop.adapt_io()`) for IPC like niri does.

---

## 3. Crate Layout

A small workspace, designed so a future companion bar can depend on the
`shoestring-ipc` types crate without pulling in Smithay.

```
shoestring-wm/                  (workspace root)
├── Cargo.toml                  (workspace + main binary)
├── src/                        (the WM binary)
│   ├── main.rs
│   ├── state.rs                (State, ShoestringWm)
│   ├── backend/                (mod.rs, winit.rs; udev.rs later)
│   ├── handlers/               (Smithay protocol handler impls)
│   │   ├── mod.rs              (delegate_*! macros, small handlers)
│   │   ├── compositor.rs
│   │   ├── xdg_shell.rs
│   │   ├── layer_shell.rs
│   │   └── foreign_toplevel.rs (milestone 8)
│   ├── input.rs                (process_input_event, key filter)
│   ├── grabs/                  (move_grab.rs, resize_grab.rs)
│   ├── layout.rs               (tile/maximize/minimize positioning)
│   ├── workspace.rs            (workspace data + switching)
│   ├── output.rs               (per-output state, hotplug)
│   ├── config/                 (parse, watch, types)
│   ├── ipc.rs                  (M9 — server; newline-JSON over unix socket)
│   └── util.rs
├── crates/
│   ├── shoestring-ipc/         (Request/Response/Event wire types)
│   │   ├── Cargo.toml
│   │   └── src/lib.rs
│   ├── shoestring-ctl/         (reference CLI client; M9)
│   │   ├── Cargo.toml
│   │   └── src/main.rs
│   └── shoestring-config/      (config types, parser; depends on serde only)
│       ├── Cargo.toml
│       └── src/lib.rs
├── docs/
│   └── architecture.md         (this file)
└── research/                   (gitignored — Smithay + niri clones, notes)
```

**Why split `shoestring-config` into its own crate?** Lets `shoestring-ipc`
or a future bar depend on config types (e.g., for displaying current
keybindings) without pulling in the WM. Cost is small — one extra Cargo.toml.

**Why `shoestring-ipc`?** A future companion bar (the tint2 replacement) is
a separate binary. It needs to deserialize WM events. Keeping the types in
their own zero-dep crate (just serde) makes that clean.

---

## 4. State Model

Mirror niri's `State { backend, wm }` outer pattern, but rename for our
domain. The outer struct is what calloop carries; the inner `ShoestringWm`
is the bulk of the WM.

```rust
pub struct State {
    pub backend: Backend,       // Winit / Udev (later)
    pub wm: ShoestringWm,
}

pub struct ShoestringWm {
    // ── Wayland / Smithay plumbing ──
    pub display_handle: DisplayHandle,
    pub loop_signal: LoopSignal,
    pub start_time: Instant,
    pub socket_name: OsString,

    // ── Smithay protocol states ──
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<Self>,
    pub data_device_state: DataDeviceState,
    pub layer_shell_state: WlrLayerShellState,           // M5+
    pub foreign_toplevel_list_state: ForeignToplevelListState, // M8+

    // ── Domain state ──
    pub seat: Seat<Self>,
    pub space: Space<Window>,           // single global Space; see § 5
    pub workspaces: WorkspaceManager,   // 16 workspaces (see § 6)
    pub outputs: OutputManager,         // per-output state, active wsp
    pub config: Rc<RefCell<Config>>,    // hot-reloadable
    pub bindings: BindingTable,         // compiled from config
    pub popups: PopupManager,

    // ── IPC (M9+) ──
    pub ipc: Option<IpcServer>,
}
```

`State` is the type parameter for all Smithay generics: `Seat<State>`,
`SeatState<State>`, etc.

---

## 5. Window Storage: Single `Space<Window>`

**Decision:** Use one `Space<Window>` for all windows across all workspaces
and outputs. Workspace and minimize state are stored per-window in metadata
(`UserDataMap` on the Window, or a side `HashMap<Window, WindowState>`).

**Rationale:**
- A `Space` per workspace (16 total) would be 16× the bookkeeping for
  output mapping (each Space needs every output mapped) and complicate
  multi-monitor window movement.
- `Space` is cheap to filter — we can render only windows whose
  `WindowState::workspace == active_workspace_for(output)`.
- Niri uses a similar model (single `global_space: Space<Window>`).

**Per-window state:**
```rust
pub struct WindowState {
    pub workspace: WorkspaceId,
    pub output: Option<OutputId>,         // last output it lived on
    pub layout: LayoutState,              // see below
    pub minimized: bool,
}

pub enum LayoutState {
    Floating { saved_rect: Rectangle<i32, Logical> },
    TiledLeft,
    TiledRight,
    Maximized,
}
```

`saved_rect` lets `Super+M` toggle back to the previous floating position.

---

## 6. Workspaces

**16 workspaces, shared/global across all monitors.** Switching workspace
changes what's visible on every monitor at once. A window belongs to a
workspace (not to a monitor + workspace).

```rust
pub struct WorkspaceManager {
    pub workspaces: [Workspace; 16],
    pub active: WorkspaceId,              // global active wsp
}

pub struct Workspace {
    pub id: WorkspaceId,                  // 0..16
    pub name: Option<String>,             // user-named via config or IPC
}
```

A window's monitor is tracked separately on `WindowState` (the output it
was last placed on). When workspace switches, *every* monitor swaps to
showing windows of the new workspace; per-window monitor assignment is
preserved so windows reappear on the monitor they came from.

Switching workspaces:
1. Update `workspaces.active = new_id`
2. Re-render every output (filter elements by
   `window_state.workspace == new_id && !minimized`)
3. Move focus to last-focused window in new workspace (per workspace MRU)
4. Emit IPC event `WorkspaceChanged`

**Moving a window to another workspace** = set `WindowState::workspace`
and re-render the affected outputs.

---

## 7. Outputs & Multi-Monitor

```rust
pub struct OutputManager {
    pub outputs: Vec<TrackedOutput>,
    pub focused: Option<OutputId>,        // which monitor has focus
}

pub struct TrackedOutput {
    pub output: smithay::output::Output,
    pub id: OutputId,
    pub damage_tracker: OutputDamageTracker,
    pub last_redraw: Instant,
}
```

Workspaces are global (see § 6), so there's no per-output active-workspace
field. The focused output determines where new windows spawn and where
tile-half operates.

On hotplug (M7 once on DRM):
1. Backend reports new connector → create `Output`, add to OutputManager
2. Assign a default workspace
3. Migrate orphaned windows (whose last `output` is gone) to primary

For v1 (winit), only one "output" exists — the winit window. Multi-monitor
only becomes meaningful after the DRM backend lands (milestone 7).

---

## 8. Input & Keybindings

### Keyboard pipeline

Reuse Smithay's `keyboard.input()` filter pattern. The filter closure runs
on every keypress before the event is forwarded to the focused client:

```rust
keyboard.input::<(), _>(
    self,
    key_code,
    state,
    serial,
    time,
    |state, modifiers, keysyms| {
        if let Some(action) = state.bindings.match_bind(modifiers, keysyms) {
            state.dispatch_action(action);
            FilterResult::Intercept(())   // do NOT send to client
        } else {
            FilterResult::Forward
        }
    },
);
```

### Binding table

```rust
pub struct BindingTable {
    pub global: Vec<Binding>,
}

pub struct Binding {
    pub modifiers: ModifierMask,           // Super, Ctrl, Shift, Alt
    pub keysym: Keysym,
    pub action: Action,
}

pub enum Action {
    Spawn { command: String, args: Vec<String> },
    TileLeft, TileRight,
    Maximize, Minimize,
    FocusWorkspace(WorkspaceId),
    MoveWindowToWorkspace(WorkspaceId),
    Close,
    Quit,
    ReloadConfig,
    // ...
}
```

### Pointer / Super+drag

Two pointer grabs, mirroring smallvil:
- `MoveSurfaceGrab` — Super+Left-drag triggers; sets cursor, updates
  position on motion, releases on button up
- `ResizeSurfaceGrab` — Super+Right-drag triggers; picks the closest
  window edge and resizes accordingly

We initiate grabs from `process_input_event` (not from xdg-shell move/
resize requests, since Openbox-style is compositor-initiated, not
client-initiated).

---

## 9. Window Operations (the 4 core actions)

Given the active window's output's usable rectangle `r`:

| Action | New position | New size | Saves prev rect? |
|---|---|---|---|
| `TileLeft` | `(r.x, r.y)` | `(r.w/2, r.h)` | yes (if from Floating) |
| `TileRight` | `(r.x + r.w/2, r.y)` | `(r.w/2, r.h)` | yes |
| `Maximize` | `(r.x, r.y)` | `(r.w, r.h)` | yes (toggle on re-press) |
| `Minimize` | unchanged | unchanged | `minimized = true` |

Implementation in `layout.rs`:
```rust
pub fn apply_action(wm: &mut ShoestringWm, window: &Window, action: WindowAction) {
    let output = wm.output_of(window);
    let r = wm.usable_rect(&output);    // accounts for layer-shell exclusive
    // ... compute new geometry, update window via space.map_element() and
    //     send configure via window.toplevel().with_pending_state() + send_configure()
}
```

Re-pressing `Maximize`/`TileLeft`/`TileRight` while already in that state
toggles back to the saved floating rect — matches the Openbox feel.

---

## 10. Backends & Render Loop

**Milestone 1-6:** Winit backend only. Faster iteration, no root, no VT.
Develop & test inside an existing X11 or Wayland session.

**Milestone 7:** Add Udev/DRM backend. This is the big one — it's where
multi-monitor, hotplug, and "real" usage become meaningful. Pattern after
anvil's `udev.rs` but trim aggressively (no multi-GPU, no fractional scale,
no XWayland).

Backend abstraction (custom enum, like niri):
```rust
pub enum Backend {
    Winit(WinitBackend),
    Udev(UdevBackend),         // M7+
}
impl Backend {
    pub fn render(&mut self, wm: &mut ShoestringWm, output: &Output) { ... }
}
```

No common trait — we just match on the enum. Two backends don't justify
trait abstraction overhead.

**Render loop per output:**
1. `bind()` framebuffer
2. Collect render elements: windows whose `workspace == active_workspace`
   and not `minimized`, plus layer-shell surfaces for the output
3. `space::render_output(...)` (Smithay handles damage tracking and z-order)
4. `submit(damage)`
5. `window.send_frame(...)` to all rendered windows
6. Schedule next redraw (winit's `request_redraw`, or vblank on DRM)

---

## 11. Resolved Design Decisions

The following questions were settled during initial planning:

| # | Question | Decision |
|---|---|---|
| Q1 | Workspaces per-monitor or shared? | **Shared/global** across all monitors. Switching workspace changes every monitor. |
| Q2 | Config format? | **TOML** via serde. Lean, plain-text, sufficient for binds/rules. Migrate later if syntax pressure justifies. |
| Q3 | IPC protocol? | **Unix socket + JSON line-delimited**, modeled after niri. EventStream upgrade for live events. |
| Q4 | XWayland? | **Plan the integration point, defer implementation.** Stub the seam in `backend/` but don't wire xwayland feature in v1. |
| Q5 | Tile-half scope? | **Current monitor only.** Half-tiles within the focused window's monitor usable rect. |
| Q6 | Focus model? | **Configurable**, default **click-to-focus**. Support focus-follows-mouse and sloppy focus as opt-in. |
| Q7 | New window placement? | **Centered on active monitor** with small offset per subsequent stacked window. |

These are not contracts — if any decision turns out wrong during
implementation, we revisit. But they're the baseline assumptions for the
task DB.

---

## 12. Milestone Plan

These will be the seeds for the task DB once the architecture is agreed.

1. **Skeleton** — workspace crates set up, winit backend, wayland socket
   listens, can spawn weston-terminal and see it draw. (smallvil-equivalent)
2. **Basic input forwarding** — keyboard + pointer events reach the focused
   client. Click-to-focus.
3. **Pointer grabs** — Super+left-drag moves window. Super+right-drag
   resizes.
4. **Keybinding system** — config file parse, binding table, key filter
   closure, dispatch actions (spawn, quit).
5. **Window actions** — tile-left, tile-right, maximize (toggle), minimize.
   Per-output usable rect.
6. **Workspaces** — 16 workspaces, switch via binds, move-window-to-wsp,
   render filtering.
7. **DRM/udev backend** — TTY operation, real outputs, hotplug.
8. **Layer-shell + foreign-toplevel-list** — bar can attach (without us
   shipping a bar yet).
9. **IPC server** — `shoestring-ipc` crate + socket. Query workspaces /
   windows / outputs; subscribe to events; trigger actions.
10. **Per-app window rules** — match on app_id/title, set workspace,
    floating geometry, etc. Config hot-reload.

Stretch / "maybe v1.1":
- XWayland (if Q4 says we need it)
- Decoration toggle (server-side decorations via xdg-decoration)
- Output configuration (resolution, scale, transform) at runtime

---

## 13. Risks & Mitigations

- **Smithay's API churn:** Smithay is pre-1.0 (currently 0.7). We pin to a
  specific git rev (like niri does) and bump intentionally.
- **DRM backend complexity:** This is where most compositors break. Plan to
  spend a full milestone on it; lean on anvil's code as direct reference.
- **Maintainers warn against LLM code generation** (AI.md). We'll model on
  smallvil, write our own code, and keep architecture deliberate. No
  "translate this Smithay code with AI" shortcuts.
- **Scope creep:** Every Wayland protocol is tempting. Defer aggressively;
  the 4 hard requirements are the only v1 bar.

---

## Appendix A — Why not just use niri?

niri is a scrolling tiler. Its layout model is fundamentally incompatible
with the user's floating + manual-snap workflow. Forking niri would mean
either ripping out most of its layout code (more work than starting fresh)
or contorting our workflow to fit. Starting from smallvil is cleaner.

## Appendix B — Why not Sway?

Sway is i3-like manual tiling, C-based, and pulls in wlroots (which is
fine, but more than we want). Also: not Rust, doesn't match the user's
language preference for this project.

## Appendix C — File budget estimate

Rough projection of source size at MVP (M1-M6, winit only):

| Module | ~LoC |
|---|---|
| main.rs | 80 |
| state.rs | 250 |
| backend/winit.rs | 200 |
| handlers/* | 500 |
| input.rs | 250 |
| grabs/* | 400 |
| layout.rs | 200 |
| workspace.rs | 150 |
| output.rs | 150 |
| config/* | 300 |
| **MVP total** | **~2500** |

DRM backend (M7) likely doubles that. We're aiming for ~5-7k LoC at v1,
versus niri's ~50k. Shoestring achieved.
