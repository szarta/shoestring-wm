//! Opt-in Tracy profiler integration (`profile-with-tracy` feature).
//!
//! Niri ships a Tracy feature for diagnosing frame drops; this is the same
//! idea, kept deliberately small. When the feature is **off** (the default),
//! everything here compiles to nothing: [`frame_mark`] is an empty function,
//! [`crate::profile_scope`] expands to no statements, and `tracing-tracy` is
//! not even a dependency. So a normal build pays zero cost and grows no deps.
//!
//! When the feature is **on**, [`tracy_layer`] returns a `tracing` layer that
//! forwards spans to a running [Tracy](https://github.com/wolfpld/tracy)
//! capture. We mark instrumented regions with [`crate::profile_scope`] (a
//! `trace`-level span on the `shoestring_wm::profile` target) and call
//! [`frame_mark`] once per presented frame, so Tracy shows a zone per render
//! step and a frame boundary per vblank. We depend only on `tracing-tracy` and
//! reach the Tracy client through its `tracing_tracy::client` re-export, so the
//! client version can never skew from the layer's.

/// Mark the end of a frame for Tracy's frame timeline. No-op without the
/// feature. Call once per presented frame, after submit.
#[cfg(feature = "profile-with-tracy")]
#[inline]
pub fn frame_mark() {
    tracing_tracy::client::frame_mark();
}

/// No-op stand-in compiled in when the Tracy feature is off.
#[cfg(not(feature = "profile-with-tracy"))]
#[inline(always)]
pub fn frame_mark() {}

/// Build the Tracy `tracing` layer and start the client. The caller filters
/// it (see `init_tracing` in `main.rs`) so Tracy captures our profile spans
/// without the stderr log following them.
#[cfg(feature = "profile-with-tracy")]
pub fn tracy_layer() -> tracing_tracy::TracyLayer<tracing_tracy::DefaultConfig> {
    // Idempotent: returns the already-running client if one exists.
    tracing_tracy::client::Client::start();
    tracing_tracy::TracyLayer::default()
}

/// Default per-layer filter for the Tracy layer: capture our own spans at full
/// detail (the `profile_scope!` zones live under `shoestring_wm`) plus general
/// `info`, without dragging in smithay/calloop trace spam. Override with
/// `SHOESTRING_WM_TRACY` (same syntax as `RUST_LOG`).
#[cfg(feature = "profile-with-tracy")]
pub fn tracy_filter() -> tracing_subscriber::EnvFilter {
    tracing_subscriber::EnvFilter::try_from_env("SHOESTRING_WM_TRACY")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,shoestring_wm=trace"))
}

/// Open a Tracy zone for the current scope, named by a string literal. Expands
/// to a `trace`-level `tracing` span (entered for the rest of the enclosing
/// block) that the Tracy layer turns into a zone; expands to nothing without
/// the feature, so instrumented hot paths cost nothing in a normal build.
///
/// ```ignore
/// fn render(&mut self) {
///     crate::profile_scope!("render_surface");
///     // ... timed until the function returns ...
/// }
/// ```
#[cfg(feature = "profile-with-tracy")]
#[macro_export]
macro_rules! profile_scope {
    ($name:literal) => {
        let _shoestring_profile_span =
            ::tracing::trace_span!(target: "shoestring_wm::profile", $name).entered();
    };
}

/// No-op form compiled in when the Tracy feature is off.
#[cfg(not(feature = "profile-with-tracy"))]
#[macro_export]
macro_rules! profile_scope {
    ($name:literal) => {};
}
