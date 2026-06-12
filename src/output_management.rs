//! wlr-output-management state and helpers.
//!
//! Smithay has no built-in delegate for `zwlr_output_management_unstable_v1`,
//! so we implement server-side `Dispatch` manually, following the same pattern
//! as `src/screencopy.rs` / `src/handlers/screencopy.rs`.
//!
//! The protocol lets clients (e.g. `wlr-randr`, `kanshi`) enumerate outputs,
//! inspect their current mode/position/transform/scale, and submit a
//! configuration to reposition or remode them.

use std::sync::{Arc, Mutex};

use smithay::{
    output::Output,
    reexports::wayland_server::backend::GlobalId,
    utils::{Logical, Point, Transform},
};

use smithay::reexports::wayland_protocols_wlr::output_management::v1::server::{
    zwlr_output_head_v1::ZwlrOutputHeadV1, zwlr_output_manager_v1::ZwlrOutputManagerV1,
    zwlr_output_mode_v1::ZwlrOutputModeV1,
};

// ── State ────────────────────────────────────────────────────────────────────

pub struct OutputManagementState {
    /// Keeps the global alive.
    #[allow(dead_code)]
    pub global: GlobalId,
    /// Monotonically-increasing serial, bumped each time the output layout
    /// changes.  Clients attach their serial to `create_configuration`; we
    /// reject `apply`/`test` whose serial is stale.
    pub serial: u32,
    /// All currently-bound manager objects.  We need to broadcast `head` /
    /// `done` events to every manager when outputs change.
    pub managers: Vec<ZwlrOutputManagerV1>,
    /// All live head objects across all managers.  Used to send `finished`
    /// when a connector is unplugged.
    pub heads: Vec<ZwlrOutputHeadV1>,
}

impl OutputManagementState {
    pub fn next_serial(&mut self) -> u32 {
        self.serial = self.serial.wrapping_add(1);
        self.serial
    }
}

// ── User data attached to protocol objects ────────────────────────────────────

/// Attached to each `ZwlrOutputManagerV1` resource.  Empty — no per-manager
/// mutable state is needed beyond what the manager vec in
/// [`OutputManagementState`] provides.
#[derive(Debug, Default, Clone, Copy)]
pub struct OutputManagerData;

/// Attached to each `ZwlrOutputHeadV1` resource.
pub struct HeadData {
    pub output: Output,
}

/// Attached to each `ZwlrOutputModeV1` resource.
#[allow(dead_code)]
pub struct ModeData {
    pub output: Output,
    pub size: smithay::utils::Size<i32, smithay::utils::Physical>,
    pub refresh: i32,
    pub preferred: bool,
}

/// Attached to each `ZwlrOutputConfigurationV1` resource.
pub struct ConfigurationData {
    /// Serial the client quoted when it created this configuration.
    pub serial: u32,
    /// Per-head intent.  Wrapped in a `Mutex` so the `EnableHead` /
    /// `DisableHead` dispatch handlers (which receive `&self`) can push
    /// entries while the `Apply` / `Test` path reads them.
    pub heads: Mutex<Vec<(Output, Arc<Mutex<HeadIntent>>)>>,
}

pub struct HeadIntent {
    pub enabled: bool,
    /// `None` means "keep current mode".
    pub mode: Option<ModeChoice>,
    pub position: Option<Point<i32, Logical>>,
    pub transform: Option<Transform>,
    pub scale: Option<f64>,
}

pub enum ModeChoice {
    /// A specific `zwlr_output_mode_v1` the client selected.
    Named {
        size: smithay::utils::Size<i32, smithay::utils::Physical>,
        refresh: i32,
    },
    /// A custom resolution/refresh the client specified.
    Custom {
        size: smithay::utils::Size<i32, smithay::utils::Physical>,
        refresh: i32,
    },
}

/// Attached to each `ZwlrOutputConfigurationHeadV1` resource.
pub struct ConfigHeadData {
    #[allow(dead_code)]
    pub output: Output,
    pub intent: Arc<Mutex<HeadIntent>>,
}

// ── Broadcast helpers ─────────────────────────────────────────────────────────

use smithay::reexports::wayland_server::Resource;

/// Send the current state of `output` as a `head` event (plus all per-head
/// sub-events) on `manager`, then return the head object so callers can store
/// it if needed.  The caller is responsible for sending `done` after all heads
/// have been announced.
pub fn announce_head(
    manager: &ZwlrOutputManagerV1,
    output: &Output,
    dh: &smithay::reexports::wayland_server::DisplayHandle,
) -> Option<ZwlrOutputHeadV1> {
    use smithay::reexports::wayland_server::Resource;

    let client = match dh.get_client(manager.id()) {
        Ok(c) => c,
        Err(_) => return None,
    };

    let head: ZwlrOutputHeadV1 = match client
        .create_resource::<ZwlrOutputHeadV1, _, crate::state::ShoestringWm>(
            dh,
            manager.version(),
            HeadData {
                output: output.clone(),
            },
        ) {
        Ok(h) => h,
        Err(_) => return None,
    };

    manager.head(&head);

    let info = output.physical_properties();
    head.name(output.name());
    head.description(output.description());
    head.physical_size(info.size.w, info.size.h);

    if manager.version() >= 2 {
        head.make(info.make.clone());
        head.model(info.model.clone());
        if !info.serial_number.is_empty() {
            head.serial_number(info.serial_number.clone());
        }
    }

    // Announce each mode.
    let mut current_mode_obj: Option<ZwlrOutputModeV1> = None;
    for wl_mode in output.modes() {
        let is_preferred = output.preferred_mode().as_ref() == Some(&wl_mode);
        let mode_obj: ZwlrOutputModeV1 = match client
            .create_resource::<ZwlrOutputModeV1, _, crate::state::ShoestringWm>(
                dh,
                manager.version(),
                ModeData {
                    output: output.clone(),
                    size: wl_mode.size,
                    refresh: wl_mode.refresh,
                    preferred: is_preferred,
                },
            ) {
            Ok(m) => m,
            Err(_) => continue,
        };
        head.mode(&mode_obj);
        mode_obj.size(wl_mode.size.w, wl_mode.size.h);
        mode_obj.refresh(wl_mode.refresh);
        if is_preferred {
            mode_obj.preferred();
        }

        if output.current_mode().as_ref() == Some(&wl_mode) {
            current_mode_obj = Some(mode_obj);
        }
    }

    // enabled / current_mode / position / transform / scale
    head.enabled(output.current_mode().is_some() as i32);

    if let Some(m) = current_mode_obj {
        head.current_mode(&m);
    }

    // Position in compositor-space.
    // `current_location` is the top-left of the output in logical coords.
    let pos = output.current_location();
    head.position(pos.x, pos.y);

    head.transform(wl_transform(output.current_transform()));

    let scale = output.current_scale().fractional_scale();
    head.scale(scale);

    Some(head)
}

/// Bump the serial, then send `done(serial)` to every bound manager.
/// Call this after any output layout change (connector plug/unplug, apply).
pub fn broadcast_done(state: &mut crate::state::ShoestringWm) {
    let serial = state.output_management.next_serial();
    let managers: Vec<_> = state.output_management.managers.clone();
    for manager in managers {
        if manager.is_alive() {
            manager.done(serial);
        }
    }
}

/// Announce a newly-connected output to every bound manager and bump the
/// serial.  Call this after the output is fully initialised and mapped.
///
/// Only the tty/udev backend hotplugs outputs; the winit backend has a single
/// static output, so this is dead code there.
#[cfg(feature = "tty")]
pub fn broadcast_head_added(state: &mut crate::state::ShoestringWm, output: &Output) {
    let dh = state.display_handle.clone();
    let managers: Vec<_> = state.output_management.managers.clone();
    for manager in managers {
        if manager.is_alive() {
            if let Some(head) = announce_head(&manager, output, &dh) {
                state.output_management.heads.push(head);
            }
        }
    }
    broadcast_done(state);
}

/// Send `finished` on every head that belongs to `output`, remove them from
/// the tracking list, and bump the serial.  Call this before unmapping the
/// output.
///
/// Tty/udev backend only (output removal is a DRM hotplug event); dead code
/// under winit.
#[cfg(feature = "tty")]
pub fn broadcast_head_removed(state: &mut crate::state::ShoestringWm, output: &Output) {
    let heads = std::mem::take(&mut state.output_management.heads);
    let mut kept = Vec::with_capacity(heads.len());
    for head in heads {
        let is_match = head
            .data::<HeadData>()
            .map(|d| d.output.name() == output.name())
            .unwrap_or(false);
        if is_match {
            if head.is_alive() {
                head.finished();
            }
        } else {
            kept.push(head);
        }
    }
    state.output_management.heads = kept;
    broadcast_done(state);
}

fn wl_transform(
    t: Transform,
) -> smithay::reexports::wayland_server::protocol::wl_output::Transform {
    use smithay::reexports::wayland_server::protocol::wl_output::Transform as WlT;
    match t {
        Transform::Normal => WlT::Normal,
        Transform::_90 => WlT::_90,
        Transform::_180 => WlT::_180,
        Transform::_270 => WlT::_270,
        Transform::Flipped => WlT::Flipped,
        Transform::Flipped90 => WlT::Flipped90,
        Transform::Flipped180 => WlT::Flipped180,
        Transform::Flipped270 => WlT::Flipped270,
    }
}
