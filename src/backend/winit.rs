use std::time::Duration;

use anyhow::Result;
use smithay::{
    backend::{
        renderer::{
            damage::OutputDamageTracker, element::surface::WaylandSurfaceRenderElement,
            gles::GlesRenderer,
        },
        winit::{self, WinitEvent},
    },
    output::{Mode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::{
        calloop::EventLoop,
        winit::{dpi::PhysicalSize, window::Window as WinitWindow},
    },
    utils::{Rectangle, Transform},
};

use crate::state::ShoestringWm;

pub fn init_winit(event_loop: &mut EventLoop<'static, ShoestringWm>, state: &mut ShoestringWm) -> Result<()> {
    // Force a physical-pixel initial size. winit's default is LogicalSize(1280, 800)
    // which gets multiplied by its guessed scale factor (often 2x-2.75x on HiDPI
    // panels in X11 sessions where nothing else actually does per-app scaling),
    // creating a window larger than the visible screen and clipping the right half.
    let attrs = WinitWindow::default_attributes()
        .with_inner_size(PhysicalSize::new(1600, 1000))
        .with_title("shoestring-wm")
        .with_visible(true);
    let (mut backend, winit) = winit::init_from_attributes(attrs)
        .map_err(|e| anyhow::anyhow!("winit init failed: {e}"))?;

    let mode = Mode {
        size: backend.window_size(),
        refresh: 60_000,
    };
    // Inside an X11/winit dev session there is no per-app scaling — the
    // "physical" window size from winit is what the user sees, regardless of
    // winit's guessed scale factor (often 2x/2.75x on HiDPI panels). Forcing
    // scale=1 keeps clients rendering at the same DPI as the rest of the
    // user's X11 desktop. Real DRM outputs in M7 will use their advertised
    // scale.
    tracing::info!(
        window_size = ?backend.window_size(),
        raw_scale = backend.scale_factor(),
        "winit output created (forcing scale=1 for nested X11)",
    );
    let scale = Scale::Integer(1);

    let output = Output::new(
        "winit".to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "shoestring-wm".into(),
            model: "Winit".into(),
            serial_number: "Unknown".into(),
        },
    );
    let _global = output.create_global::<ShoestringWm>(&state.display_handle);
    output.change_current_state(
        Some(mode),
        Some(Transform::Flipped180),
        Some(scale),
        Some((0, 0).into()),
    );
    output.set_preferred(mode);

    state.space.map_output(&output, (0, 0));

    let mut damage_tracker = OutputDamageTracker::from_output(&output);

    event_loop
        .handle()
        .insert_source(winit, move |event, _, state| match event {
            WinitEvent::Resized { size, .. } => {
                tracing::debug!(?size, "winit window resized");
                output.change_current_state(
                    Some(Mode {
                        size,
                        refresh: 60_000,
                    }),
                    None,
                    None,
                    None,
                );
                state.space.map_output(&output, (0, 0));
            }
            WinitEvent::Input(event) => state.process_input_event(event),
            WinitEvent::Redraw => {
                let size = backend.window_size();
                let damage = Rectangle::from_size(size);

                {
                    let (renderer, mut framebuffer) = backend.bind().unwrap();
                    smithay::desktop::space::render_output::<
                        _,
                        WaylandSurfaceRenderElement<GlesRenderer>,
                        _,
                        _,
                    >(
                        &output,
                        renderer,
                        &mut framebuffer,
                        1.0,
                        0,
                        [&state.space],
                        &[],
                        &mut damage_tracker,
                        [0.1, 0.1, 0.1, 1.0],
                    )
                    .unwrap();
                }
                backend.submit(Some(&[damage])).unwrap();

                state.space.elements().for_each(|window| {
                    window.send_frame(
                        &output,
                        state.start_time.elapsed(),
                        Some(Duration::ZERO),
                        |_, _| Some(output.clone()),
                    )
                });

                state.space.refresh();
                state.popups.cleanup();
                let _ = state.display_handle.flush_clients();

                backend.window().request_redraw();
            }
            WinitEvent::CloseRequested => {
                state.loop_signal.stop();
            }
            _ => {}
        })
        .map_err(|e| anyhow::anyhow!("failed to insert winit source: {e}"))?;

    Ok(())
}
