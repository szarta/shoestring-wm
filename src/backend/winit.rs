use std::time::Duration;

use anyhow::Result;
use smithay::{
    backend::{
        renderer::{damage::OutputDamageTracker, gles::GlesRenderer},
        winit::{self, WinitEvent},
    },
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::{
        calloop::EventLoop,
        winit::{dpi::PhysicalSize, window::Window as WinitWindow},
    },
    utils::{Rectangle, Transform},
};

use crate::{backend::scale_from_config, state::ShoestringWm};

pub fn init_winit(
    event_loop: &mut EventLoop<'static, ShoestringWm>,
    state: &mut ShoestringWm,
) -> Result<()> {
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
    // winit's guessed scale factor (often 2x/2.75x on HiDPI panels). Use the
    // user-configured `general.output_scale` (default 1.0) so the nested
    // session matches whatever HiDPI scale the real session will use.
    let scale = scale_from_config(state.config.general.output_scale);
    tracing::info!(
        window_size = ?backend.window_size(),
        raw_scale = backend.scale_factor(),
        configured_scale = state.config.general.output_scale,
        ?scale,
        "winit output created",
    );

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
    state.emit_ipc(shoestring_ipc::Event::OutputAdded(
        shoestring_ipc::OutputSummary {
            name: output.name(),
            width: mode.size.w,
            height: mode.size.h,
            scale: state.config.general.output_scale,
        },
    ));

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

                    // Re-pick the cursor frame for the configured scale (cursors
                    // are raster sprites — fractional values round up to the next
                    // whole pixel ratio). Read straight from config; per-output
                    // scaling would change this.
                    let scale_int = state.config.general.output_scale.ceil().max(1.0) as u32;
                    state.refresh_cursor_buffer(scale_int);
                    let locked = state.is_locked();
                    // Cursor only when unlocked; the lock surface owns the
                    // visible content while locked.
                    let cursor_elements: Vec<crate::drawing::PointerRenderElement<GlesRenderer>> =
                        if !locked {
                            if let Some((pe, location, hotspot)) = state.cursor_render_snapshot() {
                                let scale: smithay::utils::Scale<f64> =
                                    output.current_scale().fractional_scale().into();
                                let physical_location: smithay::utils::Point<
                                    i32,
                                    smithay::utils::Physical,
                                > = smithay::utils::Point::<f64, smithay::utils::Physical>::from((
                                    (location.x - hotspot.0 as f64) * scale.x,
                                    (location.y - hotspot.1 as f64) * scale.y,
                                ))
                                .to_i32_round();
                                use smithay::backend::renderer::element::AsRenderElements;
                                pe.render_elements(renderer, physical_location, scale, 1.0)
                            } else {
                                Vec::new()
                            }
                        } else {
                            // Surface elements for the lock surface (if any)
                            // ride the same custom_elements slot — they
                            // accept WaylandSurfaceRenderElement via the
                            // Surface variant of PointerRenderElement.
                            let scale: smithay::utils::Scale<f64> =
                                output.current_scale().fractional_scale().into();
                            let lock_surface = state.lock_surface_for(&output);
                            crate::handlers::session_lock::lock_render_elements(
                                lock_surface.as_ref(),
                                renderer,
                                scale,
                            )
                        };
                    // Empty space while locked so no client content leaks.
                    let empty_space =
                        smithay::desktop::Space::<smithay::desktop::Window>::default();
                    let render_space = if locked { &empty_space } else { &state.space };
                    let clear = if locked {
                        [0.0, 0.0, 0.0, 1.0]
                    } else {
                        [0.1, 0.1, 0.1, 1.0]
                    };

                    // Fulfil any pending wlr-screencopy captures for this
                    // output first. The helper binds an offscreen GLES
                    // texture as the renderer's framebuffer, so doing it
                    // before render_output ensures the window framebuffer is
                    // the LAST bind, which is what backend.submit() requires
                    // (otherwise eglSwapBuffersWithDamageKHR hits BAD_SURFACE).
                    crate::screencopy::process_pending(
                        &mut state.screencopy,
                        &state.space,
                        &output,
                        renderer,
                        &cursor_elements,
                    );

                    smithay::desktop::space::render_output::<
                        _,
                        crate::drawing::PointerRenderElement<GlesRenderer>,
                        _,
                        _,
                    >(
                        &output,
                        renderer,
                        &mut framebuffer,
                        1.0,
                        0,
                        [render_space],
                        &cursor_elements,
                        &mut damage_tracker,
                        clear,
                    )
                    .unwrap();
                }
                backend.submit(Some(&[damage])).unwrap();

                // Keep wp_fractional_scale clients on this output in sync with
                // its current scale (no-op when unchanged).
                crate::scale::send_preferred_scale(&state.space, &output);

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
