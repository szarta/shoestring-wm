#[cfg(feature = "winit")]
pub mod winit;

#[cfg(feature = "tty")]
pub mod udev;

use smithay::output::Scale;

/// Resolved variable-refresh-rate state for an output, stored in the smithay
/// [`Output`](smithay::output::Output)'s user data so the IPC layer can report
/// it without reaching into a feature-gated backend. The bool is what the WM
/// actually enabled — `true` only when the config opted in *and* the connector
/// advertised support. The winit backend never inserts this, so nested outputs
/// report `false`.
#[derive(Debug, Clone, Copy)]
pub struct VrrState(pub bool);

/// Pick the right `Scale` variant for a configured `output_scale`. Whole
/// numbers (within a small epsilon) go through `Integer` so plain `wl_output`
/// clients get the natural value; fractional values use `Fractional`. Legacy
/// `wl_output.scale` clients see the rounded integer, while wp_fractional_scale
/// clients are handed the exact ratio via
/// [`crate::scale::send_preferred_scale`] so they can render pixel-crisp.
pub fn scale_from_config(scale: f64) -> Scale {
    let rounded = scale.round();
    if (scale - rounded).abs() < 1e-6 && rounded >= 1.0 {
        Scale::Integer(rounded as i32)
    } else {
        Scale::Fractional(scale)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_scales_become_integer() {
        assert!(matches!(scale_from_config(1.0), Scale::Integer(1)));
        assert!(matches!(scale_from_config(2.0), Scale::Integer(2)));
    }

    #[test]
    fn fractional_scales_pass_through() {
        match scale_from_config(1.5) {
            Scale::Fractional(v) => assert!((v - 1.5).abs() < 1e-9),
            other => panic!("expected Fractional, got {other:?}"),
        }
    }
}
