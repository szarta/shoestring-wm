#[cfg(feature = "winit")]
pub mod winit;

#[cfg(feature = "headless")]
pub mod headless;

#[cfg(feature = "tty")]
pub mod udev;

use smithay::output::Scale;
use smithay::utils::Transform;

use shoestring_config::OutputTransform;

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

/// Map a configured [`OutputTransform`] onto smithay's [`Transform`]. Kept here
/// (rather than in the config crate) so `shoestring-config` stays free of a
/// smithay dependency.
pub fn transform_from_config(t: OutputTransform) -> Transform {
    match t {
        OutputTransform::Normal => Transform::Normal,
        OutputTransform::_90 => Transform::_90,
        OutputTransform::_180 => Transform::_180,
        OutputTransform::_270 => Transform::_270,
        OutputTransform::Flipped => Transform::Flipped,
        OutputTransform::Flipped90 => Transform::Flipped90,
        OutputTransform::Flipped180 => Transform::Flipped180,
        OutputTransform::Flipped270 => Transform::Flipped270,
    }
}

/// Render a smithay [`Transform`] as its `wlr-randr` / config name, for the IPC
/// `outputs` report. Inverse of [`transform_from_config`] over the names the
/// config accepts.
pub fn transform_name(t: Transform) -> &'static str {
    match t {
        Transform::Normal => "normal",
        Transform::_90 => "90",
        Transform::_180 => "180",
        Transform::_270 => "270",
        Transform::Flipped => "flipped",
        Transform::Flipped90 => "flipped-90",
        Transform::Flipped180 => "flipped-180",
        Transform::Flipped270 => "flipped-270",
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

    #[test]
    fn transform_config_covers_every_variant() {
        // Each config orientation maps to the matching smithay transform, and
        // its IPC name round-trips back to the config name.
        let cases = [
            (OutputTransform::Normal, Transform::Normal, "normal"),
            (OutputTransform::_90, Transform::_90, "90"),
            (OutputTransform::_180, Transform::_180, "180"),
            (OutputTransform::_270, Transform::_270, "270"),
            (OutputTransform::Flipped, Transform::Flipped, "flipped"),
            (
                OutputTransform::Flipped90,
                Transform::Flipped90,
                "flipped-90",
            ),
            (
                OutputTransform::Flipped180,
                Transform::Flipped180,
                "flipped-180",
            ),
            (
                OutputTransform::Flipped270,
                Transform::Flipped270,
                "flipped-270",
            ),
        ];
        for (cfg, smithay, name) in cases {
            assert_eq!(transform_from_config(cfg), smithay);
            assert_eq!(transform_name(smithay), name);
        }
    }
}
