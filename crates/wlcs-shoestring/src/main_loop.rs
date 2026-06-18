//! The compositor worker thread: constructs [`ShoestringWm`] headless and runs
//! a render/dispatch loop, translating [`WlcsEvent`]s from the WLCS runner into
//! real input/window operations. Adapted from smithay's
//! `wlcs_anvil/src/main_loop.rs`, but against shoestring-wm's concrete state
//! (no backend-generic `AnvilState`, no `clock`/`running`/`pointer` fields) and
//! smithay's `desktop::space::render_output` driven by a `DummyRenderer`.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    sync::Arc,
    time::Duration,
};

use smithay::{
    backend::{
        input::ButtonState,
        renderer::{
            damage::OutputDamageTracker,
            test::{DummyFramebuffer, DummyRenderer},
        },
    },
    input::pointer::{ButtonEvent, MotionEvent, RelativeMotionEvent},
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::{
        calloop::{
            channel::{Channel, Event as ChannelEvent},
            EventLoop,
        },
        wayland_server::{Client, Display, Resource},
    },
    utils::SERIAL_COUNTER as SCOUNTER,
    wayland::seat::WaylandFocus,
};

use shoestring_config::Config;
use shoestring_wm::drawing::PointerRenderElement;
use shoestring_wm::state::{ClientState, ShoestringWm};

use crate::WlcsEvent;

const OUTPUT_NAME: &str = "wlcs";

thread_local! {
    /// WLCS addresses windows by the client's local socket fd. We need that fd →
    /// [`Client`] mapping in `PositionWindow`, so it lives here rather than on
    /// `ShoestringWm` — the loop is single-threaded and both touch points are in
    /// [`handle_event`], so a `thread_local` keeps the test-only state out of the
    /// shared compositor struct.
    static CLIENTS: RefCell<HashMap<i32, Client>> = RefCell::new(HashMap::new());
    /// Flipped to `false` by `WlcsEvent::Exit`; the run loop polls it. Stands in
    /// for Anvil's `AnvilState::running` (which shoestring-wm has no equivalent of).
    static RUNNING: Cell<bool> = const { Cell::new(true) };
}

pub fn run(channel: Channel<WlcsEvent>) {
    // Opt-in tracing for debugging the harness: `SHOESTRING_WM_LOG`-style via
    // RUST_LOG. Ignored if a global subscriber is already set (per test run).
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("off")),
        )
        .with_writer(std::io::stderr)
        .try_init();

    let mut event_loop =
        EventLoop::<ShoestringWm>::try_new().expect("Failed to init the event loop.");

    let display = Display::<ShoestringWm>::new().expect("Failed to init display");
    let mut state = ShoestringWm::new_headless(&mut event_loop, display, Config::default());

    event_loop
        .handle()
        .insert_source(channel, move |event, &mut (), data| match event {
            ChannelEvent::Msg(evt) => handle_event(evt, data),
            ChannelEvent::Closed => handle_event(WlcsEvent::Exit, data),
        })
        .unwrap();

    let mut renderer = DummyRenderer;
    let mut framebuffer = DummyFramebuffer;

    let mode = Mode {
        size: (800, 600).into(),
        refresh: 60_000,
    };
    let output = Output::new(
        OUTPUT_NAME.to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "Shoestring".into(),
            model: "WLCS".into(),
            serial_number: "0".into(),
        },
    );
    let _global = output.create_global::<ShoestringWm>(&state.display_handle);
    output.change_current_state(Some(mode), None, None, Some((0, 0).into()));
    output.set_preferred(mode);
    state.space.map_output(&output, (0, 0));

    let mut damage_tracker = OutputDamageTracker::from_output(&output);

    while RUNNING.with(|r| r.get()) {
        // "Render" the scene into the dummy framebuffer. We draw no cursor (the
        // empty custom-element slice), but the pass still imports client buffers
        // and advances the damage tracker, matching what Anvil does so buffer
        // lifecycle behaves. WLCS asserts protocol, not pixels, so the result is
        // ignored exactly as the real backends ignore non-fatal render errors.
        let _ =
            smithay::desktop::space::render_output::<_, PointerRenderElement<DummyRenderer>, _, _>(
                &output,
                &mut renderer,
                &mut framebuffer,
                1.0,
                0,
                [&state.space],
                &[],
                &mut damage_tracker,
                [0.1, 0.1, 0.1, 1.0],
            );

        // Send frame callbacks so clients draw their next frame — toplevels
        // and layer-shell surfaces alike, matching the real backends. The
        // WLCS layer-shell suite blocks on `wl_surface.frame` before checking
        // surface position, so a layer surface that never gets one hangs.
        shoestring_wm::presentation::send_frames(
            &state.space,
            &output,
            state.start_time.elapsed(),
        );

        if event_loop
            .dispatch(Some(Duration::from_millis(16)), &mut state)
            .is_err()
        {
            RUNNING.with(|r| r.set(false));
        } else {
            state.space.refresh();
            // Re-evaluate the surface under a *stationary* pointer after this
            // dispatch batch (surfaces that mapped/unmapped/moved owe the
            // client enter/leave/motion). The real binary does this from its
            // event-loop callback in main.rs; the harness has its own loop, so
            // it calls the same shared method here — WlcsEvent::PositionWindow
            // teleports a window under the pointer without any client motion,
            // which the WLCS ClientSurfaceEvents / SurfacePointerMotion family
            // checks for.
            state.refresh_pointer_focus();
            state.popups.cleanup();
            state.display_handle.flush_clients().unwrap();
        }
    }
}

fn handle_event(event: WlcsEvent, state: &mut ShoestringWm) {
    match event {
        WlcsEvent::Exit => RUNNING.with(|r| r.set(false)),
        WlcsEvent::NewClient { stream, client_id } => {
            let client = state
                .display_handle
                .insert_client(stream, Arc::new(ClientState::default()))
                .expect("Failed to insert client");
            CLIENTS.with(|c| c.borrow_mut().insert(client_id, client));
        }
        WlcsEvent::PositionWindow {
            client_id,
            surface_id,
            location,
        } => {
            let client = CLIENTS.with(|c| c.borrow().get(&client_id).cloned());
            let toplevel = state.space.elements().find(|w| {
                if let Some(surface) = w.wl_surface() {
                    state.display_handle.get_client(surface.id()).ok() == client
                        && surface.id().protocol_id() == surface_id
                } else {
                    false
                }
            });
            if let Some(toplevel) = toplevel.cloned() {
                state.space.map_element(toplevel, location, false);
            }
        }
        // pointer inputs
        WlcsEvent::NewPointer { .. } => {}
        WlcsEvent::PointerMoveAbsolute { location, .. } => {
            let serial = SCOUNTER.next_serial();
            let under = state.surface_under(location);
            let time = state.start_time.elapsed().as_millis() as u32;
            let ptr = state.seat.get_pointer().unwrap();
            ptr.motion(
                state,
                under.clone(),
                &MotionEvent {
                    location,
                    serial,
                    time,
                },
            );
            ptr.frame(state);
            // Keep the focus cache in sync with what the client just saw, so
            // the per-iteration refresh_pointer_focus() (called from the run
            // loop) doesn't re-send this motion as a phantom event.
            state.last_pointer_focus = under;
        }
        WlcsEvent::PointerMoveRelative { delta, .. } => {
            let ptr = state.seat.get_pointer().unwrap();
            let location = ptr.current_location() + delta;
            let serial = SCOUNTER.next_serial();
            let under = state.surface_under(location);
            let time = state.start_time.elapsed().as_millis() as u32;
            let utime = state.start_time.elapsed().as_micros() as u64;
            ptr.motion(
                state,
                under.clone(),
                &MotionEvent {
                    location,
                    serial,
                    time,
                },
            );
            ptr.relative_motion(
                state,
                under.clone(),
                &RelativeMotionEvent {
                    delta,
                    delta_unaccel: delta,
                    utime,
                },
            );
            ptr.frame(state);
            state.last_pointer_focus = under;
        }
        WlcsEvent::PointerButtonDown { button_id, .. } => {
            let serial = SCOUNTER.next_serial();
            let ptr = state.seat.get_pointer().unwrap();
            if !ptr.is_grabbed() {
                let under = state
                    .space
                    .element_under(ptr.current_location())
                    .map(|(w, _)| w.clone());
                if let Some(window) = under.as_ref() {
                    state.space.raise_element(window, true);
                }
                // shoestring-wm's keyboard focus target is the WlSurface, not the
                // window (see SeatHandler::KeyboardFocus), so resolve it here.
                let focus = under
                    .as_ref()
                    .and_then(shoestring_wm::window_ext::focus_surface);
                state
                    .seat
                    .get_keyboard()
                    .unwrap()
                    .set_focus(state, focus, serial);
            }
            let time = state.start_time.elapsed().as_millis() as u32;
            ptr.button(
                state,
                &ButtonEvent {
                    button: button_id as u32,
                    state: ButtonState::Pressed,
                    serial,
                    time,
                },
            );
            ptr.frame(state);
        }
        WlcsEvent::PointerButtonUp { button_id, .. } => {
            let serial = SCOUNTER.next_serial();
            let time = state.start_time.elapsed().as_millis() as u32;
            let ptr = state.seat.get_pointer().unwrap();
            ptr.button(
                state,
                &ButtonEvent {
                    button: button_id as u32,
                    state: ButtonState::Released,
                    serial,
                    time,
                },
            );
            ptr.frame(state);
        }
        WlcsEvent::PointerRemoved { .. } => {}
        // touch inputs — shoestring-wm has no touch handling yet; WLCS touch
        // tests land on the skip-list.
        WlcsEvent::NewTouch { .. } => {}
        WlcsEvent::TouchDown { .. } => {}
        WlcsEvent::TouchMove { .. } => {}
        WlcsEvent::TouchUp { .. } => {}
        WlcsEvent::TouchRemoved { .. } => {}
    }
}
