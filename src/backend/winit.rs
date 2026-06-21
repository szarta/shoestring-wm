use anyhow::Result;
use smithay::{
    backend::{
        renderer::{
            damage::OutputDamageTracker, element::surface::WaylandSurfaceRenderElement,
            gles::GlesRenderer,
        },
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
    // Window title for the nested winit window. Defaults to "shoestring-wm",
    // but $SHOESTRING_WM_WINIT_TITLE overrides it so a specific nested instance
    // is identifiable on the parent compositor (the default collides when more
    // than one nested WM is running — rename to disambiguate, then resolve it
    // to a pid via the parent's `find_windows` → `WindowSummary::pid`).
    let title =
        std::env::var("SHOESTRING_WM_WINIT_TITLE").unwrap_or_else(|_| "shoestring-wm".to_string());
    tracing::info!(%title, "winit window title");
    let attrs = WinitWindow::default_attributes()
        .with_inner_size(PhysicalSize::new(1600, 1000))
        .with_title(title)
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
            // Nested winit cannot drive VRR.
            adaptive_sync: false,
            // The output's `Flipped180` is GL y-flip compensation, not a
            // user-facing orientation — report the nested output as upright.
            transform: "normal".to_string(),
        },
    ));
    // Register the output with wlr-output-management, exactly as the udev
    // backend does on connector add. No managers are bound this early, so this
    // just bumps the serial off zero — without it a fresh winit session would
    // answer the first manager bind with `done(0)`, which clients (and the
    // output_management integration test) read as "no done received".
    crate::output_management::broadcast_head_added(state, &output);
    crate::ext_workspace::broadcast_output_enter(state, &output);

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

                let render_states = {
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
                    let clear = if locked {
                        [0.0, 0.0, 0.0, 1.0]
                    } else {
                        [0.1, 0.1, 0.1, 1.0]
                    };

                    // A fullscreen window on this output is rendered alone,
                    // dropping the bar/layer surfaces it covers. While locked we
                    // render an empty scene so no client content leaks.
                    let fullscreen = if locked {
                        None
                    } else {
                        crate::layout::fullscreen_window_on(&state.space, &state.layout, &output)
                    };
                    // Overlay elements that ride above the window stack: the
                    // cursor (or lock surface) plus the server-side window
                    // borders. Built once so the capture path composites the
                    // exact same overlays the screen shows.
                    let border_elements =
                        state.output_border_elements(&output, locked, fullscreen.as_ref());

                    // F3-style diagnostics overlay (its own top-most slot, not
                    // a CaptureOverlay — kept out of screenshots). Refresh the
                    // buffer now, before the renderer touches it below. Hidden
                    // while locked: nothing but the lock surface should show.
                    let show_overlay = !locked && state.is_diag_overlay_output(&output);
                    if show_overlay {
                        state.refresh_diag_overlay(&output);
                    }
                    let overlays: Vec<crate::drawing::CaptureOverlay<GlesRenderer>> = cursor_elements
                        .into_iter()
                        .map(crate::drawing::CaptureOverlay::Pointer)
                        .chain(
                            border_elements
                                .into_iter()
                                .map(crate::drawing::CaptureOverlay::Border),
                        )
                        .collect();

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
                        &overlays,
                    );

                    let space_elements = if locked {
                        Vec::new()
                    } else {
                        crate::drawing::output_space_elements(
                            renderer,
                            &state.space,
                            &output,
                            fullscreen.as_ref(),
                        )
                    };
                    // Overlays (cursor on top, then borders) above the space
                    // stack — render_output draws first-element-first.
                    let mut elements: Vec<
                        crate::drawing::OutputRenderElements<
                            GlesRenderer,
                            WaylandSurfaceRenderElement<GlesRenderer>,
                        >,
                    > = Vec::with_capacity(overlays.len() + space_elements.len());
                    elements.extend(
                        overlays
                            .into_iter()
                            .map(crate::drawing::CaptureOverlay::into_output_element),
                    );
                    elements.extend(
                        space_elements
                            .into_iter()
                            .map(crate::drawing::OutputRenderElements::Space),
                    );
                    // Overlay on top of everything (index 0 = drawn first).
                    if show_overlay {
                        let scale_int =
                            (output.current_scale().fractional_scale().ceil() as i32).max(1);
                        if let Some(el) = state.diag_overlay.element(renderer, scale_int) {
                            elements.insert(0, crate::drawing::OutputRenderElements::Overlay(el));
                        }
                    }
                    let render_start = std::time::Instant::now();
                    let result = damage_tracker
                        .render_output(renderer, &mut framebuffer, 0, &elements, clear)
                        .unwrap();
                    state
                        .metrics
                        .record_frame(render_start.elapsed().as_micros() as u64);
                    result.states
                };
                backend.submit(Some(&[damage])).unwrap();

                // wp_presentation, best effort: the nested winit backend has no
                // hardware vblank, so mark the frame presented at submit time
                // against the same monotonic clock the global advertises. No
                // HwClock/HwCompletion flags and an unknown refresh — honest
                // about the lack of a real page-flip timestamp. Without this,
                // clients that request feedback would wait forever.
                {
                    use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind;
                    crate::presentation::update_primary_scanout_output(
                        &state.space,
                        &output,
                        &render_states,
                    );
                    let mut feedback = crate::presentation::take_presentation_feedback(
                        &state.space,
                        &output,
                        &render_states,
                    );
                    feedback.presented(
                        state.clock.now(),
                        smithay::wayland::presentation::Refresh::Unknown,
                        0,
                        Kind::Vsync,
                    );
                }

                // Keep wp_fractional_scale clients on this output in sync with
                // its current scale (no-op when unchanged).
                crate::scale::send_preferred_scale(&state.space, &output);

                crate::presentation::send_frames(&state.space, &output, state.start_time.elapsed());

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
