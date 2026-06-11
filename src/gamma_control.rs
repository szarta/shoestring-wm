//! State for `zwlr_gamma_control_v1` — privileged per-output gamma-ramp
//! control, the protocol night-light tools (wlsunset, gammastep) use to warm
//! the screen. Dispatch lives in [`crate::handlers::gamma_control`]; the DRM
//! glue (resolve output → CRTC, read/apply ramps) lives in
//! [`crate::backend::udev`] since it reaches into private udev state.
//!
//! KMS-only: gamma ramps are a CRTC property, so this whole module is gated on
//! the `tty` feature and only does anything for udev-backed outputs. Clients
//! that ask for gamma control on a non-KMS output get an immediate `failed`.

use std::os::fd::{AsRawFd, OwnedFd};

use smithay::backend::drm::DrmNode;
use smithay::reexports::drm::control::crtc;
use smithay::reexports::wayland_protocols_wlr::gamma_control::v1::server::zwlr_gamma_control_v1::ZwlrGammaControlV1;
use smithay::reexports::wayland_server::backend::GlobalId;

/// Global marker for `zwlr_gamma_control_manager_v1`. Mirrors
/// [`crate::screencopy::ScreencopyManagerData`].
pub struct GammaControlManagerData;

/// Per-resource data hung off each `zwlr_gamma_control_v1`. The output name is
/// the key back into [`GammaControlState::controls`], used by the `set_gamma`
/// and `destroyed` paths to find (and on release, restore) the right CRTC.
/// Empty for a control that was failed at creation (unknown output).
pub struct GammaControlData {
    pub output_name: String,
}

/// Live gamma-control state. At most one control may own an output at a time
/// (protocol exclusivity); `controls` is keyed by output name.
pub struct GammaControlState {
    /// Held for the manager global's lifetime.
    #[allow(dead_code)]
    pub manager_global: GlobalId,
    pub controls: std::collections::HashMap<String, GammaControl>,
}

/// Who currently owns an output's gamma ramp. Exclusivity spans both
/// surfaces: a wlr-gamma-control client and the WM's own IPC `set-gamma`
/// transfer ownership from each other (the previous owner is dropped, and a
/// client owner is `failed`).
pub enum GammaOwner {
    /// A `zwlr_gamma_control_v1` client resource.
    Client(ZwlrGammaControlV1),
    /// The WM itself, via the IPC `set-gamma` request.
    Ipc,
}

impl GammaOwner {
    /// True if this owner is the given client resource — used to ignore
    /// requests/destroys from a resource that has since been transferred away.
    pub fn is_client(&self, resource: &ZwlrGammaControlV1) -> bool {
        matches!(self, GammaOwner::Client(r) if r == resource)
    }
}

/// One owner's exclusive grip on an output's gamma ramp.
pub struct GammaControl {
    pub owner: GammaOwner,
    pub node: DrmNode,
    pub crtc: crtc::Handle,
    /// Entries per channel, as advertised to the client via `gamma_size`.
    pub size: u32,
    /// The CRTC's ramp from before this control (chain) took over, restored
    /// when the control is released. Carried forward across transfers so the
    /// *true* pre-client table is what we eventually put back.
    pub original: GammaRamp,
    /// The ramp the client last uploaded. Kept so we can re-apply it after a
    /// VT switch, which resets the CRTC's gamma to identity. `None` until the
    /// client's first `set_gamma`.
    pub current: Option<GammaRamp>,
}

/// R/G/B gamma ramps, each `size` `u16` entries.
#[derive(Clone)]
pub struct GammaRamp {
    pub red: Vec<u16>,
    pub green: Vec<u16>,
    pub blue: Vec<u16>,
}

impl GammaRamp {
    /// The linear ("no correction") ramp. Used as the restore target when we
    /// couldn't read the CRTC's pre-existing table — at boot the hardware ramp
    /// is already linear, so this is the right fallback in practice.
    pub fn identity(size: u32) -> Self {
        let n = size as usize;
        let denom = (n.saturating_sub(1)).max(1) as u64;
        let ramp: Vec<u16> = (0..n)
            .map(|i| ((i as u64 * u16::MAX as u64) / denom) as u16)
            .collect();
        Self {
            red: ramp.clone(),
            green: ramp.clone(),
            blue: ramp,
        }
    }
}

/// Approximate sRGB white point for a black-body color `temperature` (Kelvin),
/// using the Tanner Helland approximation (the same family redshift/wlsunset
/// use). Returns per-channel multipliers in `[0, 1]`; ~6500 K is neutral
/// (≈ white), lower is warmer (blue pulled down), higher is cooler.
fn whitepoint(temperature: u32) -> (f64, f64, f64) {
    let t = (temperature as f64) / 100.0;
    let clamp01 = |v: f64| v.clamp(0.0, 1.0);

    let red = if t <= 66.0 {
        1.0
    } else {
        clamp01(329.698_727_446 * (t - 60.0).powf(-0.133_204_759_2) / 255.0)
    };
    let green = if t <= 66.0 {
        clamp01((99.470_802_586_1 * t.ln() - 161.119_568_166_1) / 255.0)
    } else {
        clamp01(288.122_169_528_3 * (t - 60.0).powf(-0.075_514_849_2) / 255.0)
    };
    let blue = if t >= 66.0 {
        1.0
    } else if t <= 19.0 {
        0.0
    } else {
        clamp01((138.517_731_223_1 * (t - 10.0).ln() - 305.044_792_730_7) / 255.0)
    };
    (red, green, blue)
}

/// Build an R/G/B gamma ramp for `size` entries from a color `temperature`
/// (Kelvin), an overall `brightness` (`0..=1`) and a `gamma` exponent. With
/// `temperature = 6500`, `brightness = 1.0`, `gamma = 1.0` this is essentially
/// the identity ramp. Used by the IPC `set-gamma` path so no external
/// night-light daemon is needed.
pub fn ramp_from_temperature(
    size: u32,
    temperature: u32,
    brightness: f64,
    gamma: f64,
) -> GammaRamp {
    let (wr, wg, wb) = whitepoint(temperature);
    let n = size as usize;
    let denom = (n.saturating_sub(1)).max(1) as f64;
    let inv_gamma = 1.0 / gamma;

    let channel = |white: f64| -> Vec<u16> {
        (0..n)
            .map(|i| {
                let v = (i as f64 / denom).powf(inv_gamma);
                let scaled = (v * brightness * white * u16::MAX as f64).round();
                scaled.clamp(0.0, u16::MAX as f64) as u16
            })
            .collect()
    };

    GammaRamp {
        red: channel(wr),
        green: channel(wg),
        blue: channel(wb),
    }
}

/// Why reading a client's gamma table failed.
pub enum GammaReadError {
    /// The fd is smaller than `3 * size * 2` bytes — a malformed table. Maps
    /// to the protocol's `invalid_gamma` error.
    BadSize,
    /// The fd couldn't be stat'd or mapped. Maps to a `failed` event.
    Io,
}

/// Read a client-supplied gamma table from `fd`: `3 * size` little/​native-order
/// `u16`s, laid out as the red ramp, then green, then blue.
///
/// We `mmap` rather than `read` so a client that passes a pipe (or any fd that
/// would block) can't hang the compositor — `fstat` + `mmap` fail fast instead.
/// The table is host-native `u16` (same convention as wlroots), so we read it
/// with native byte order.
pub fn read_ramp_from_fd(fd: OwnedFd, size: u32) -> Result<GammaRamp, GammaReadError> {
    let count = size as usize * 3;
    let bytes = count * 2;
    let raw = fd.as_raw_fd();

    // SAFETY: zeroed `stat` is a valid initial value; fstat fully populates it.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(raw, &mut st) } != 0 {
        return Err(GammaReadError::Io);
    }
    if st.st_size < 0 || (st.st_size as usize) < bytes {
        return Err(GammaReadError::BadSize);
    }

    // SAFETY: PROT_READ/MAP_PRIVATE map of a stat'd fd of sufficient length.
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            bytes,
            libc::PROT_READ,
            libc::MAP_PRIVATE,
            raw,
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(GammaReadError::Io);
    }
    // SAFETY: mmap returned `bytes` of readable memory; we never outlive it.
    let slice = unsafe { std::slice::from_raw_parts(ptr as *const u8, bytes) };
    let mut all = vec![0u16; count];
    for (dst, chunk) in all.iter_mut().zip(slice.chunks_exact(2)) {
        *dst = u16::from_ne_bytes([chunk[0], chunk[1]]);
    }
    // SAFETY: unmapping exactly the region we mapped, after copying it out.
    unsafe {
        libc::munmap(ptr, bytes);
    }

    let mut red = all;
    let mut rest = red.split_off(size as usize);
    let blue = rest.split_off(size as usize);
    let green = rest;
    Ok(GammaRamp { red, green, blue })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::fd::FromRawFd;

    /// Anonymous in-memory fd holding the R/G/B ramps back to back, as a real
    /// gamma client (wlsunset etc.) supplies via a memfd.
    fn ramp_fd(channels: &[&[u16]]) -> OwnedFd {
        // SAFETY: memfd_create with a valid C string and no flags.
        let raw = unsafe { libc::memfd_create(c"gamma-test".as_ptr(), 0) };
        assert!(raw >= 0, "memfd_create failed");
        // SAFETY: we own the just-created fd.
        let mut f = unsafe { std::fs::File::from_raw_fd(raw) };
        for ch in channels {
            for v in *ch {
                f.write_all(&v.to_ne_bytes()).unwrap();
            }
        }
        f.flush().unwrap();
        OwnedFd::from(f)
    }

    #[test]
    fn read_ramp_splits_channels_in_order() {
        let red = [1u16, 2, 3, 4];
        let green = [10u16, 20, 30, 40];
        let blue = [100u16, 200, 300, 400];
        let fd = ramp_fd(&[&red, &green, &blue]);
        let ramp = read_ramp_from_fd(fd, 4).ok().expect("valid table");
        assert_eq!(ramp.red, red);
        assert_eq!(ramp.green, green);
        assert_eq!(ramp.blue, blue);
    }

    #[test]
    fn read_ramp_rejects_undersized_fd() {
        // Claims size 4 (needs 24 bytes) but supplies only 5 u16 (10 bytes).
        let fd = ramp_fd(&[&[1u16, 2], &[3u16, 4], &[5u16]]);
        assert!(matches!(
            read_ramp_from_fd(fd, 4),
            Err(GammaReadError::BadSize)
        ));
    }

    #[test]
    fn neutral_temperature_is_near_identity() {
        // 6500 K with unit brightness/gamma ≈ the identity ramp: endpoints
        // span the full range and the channels track each other.
        let r = ramp_from_temperature(256, 6500, 1.0, 1.0);
        assert_eq!(r.red[0], 0);
        assert_eq!(*r.red.last().unwrap(), u16::MAX);
        let top = *r.green.last().unwrap();
        assert!(top > 60000, "green near full at 6500K, got {top}");
    }

    #[test]
    fn warm_temperature_pulls_blue_down() {
        // At 3000 K the blue channel's top entry must be well below red's.
        let r = ramp_from_temperature(256, 3000, 1.0, 1.0);
        let red_top = *r.red.last().unwrap();
        let blue_top = *r.blue.last().unwrap();
        assert_eq!(red_top, u16::MAX);
        assert!(
            blue_top < red_top,
            "warm light dims blue: blue_top={blue_top} red_top={red_top}"
        );
    }

    #[test]
    fn brightness_scales_the_peak() {
        let full = ramp_from_temperature(256, 6500, 1.0, 1.0);
        let half = ramp_from_temperature(256, 6500, 0.5, 1.0);
        let f = *full.red.last().unwrap() as u32;
        let h = *half.red.last().unwrap() as u32;
        // Half brightness ≈ half the peak (allow rounding slack).
        assert!((h as i32 - (f / 2) as i32).abs() < 4, "f={f} h={h}");
    }

    #[test]
    fn identity_spans_the_full_u16_range() {
        let r = GammaRamp::identity(256);
        assert_eq!(r.red.len(), 256);
        assert_eq!(r.green.len(), 256);
        assert_eq!(r.blue.len(), 256);
        assert_eq!(r.red[0], 0);
        assert_eq!(*r.red.last().unwrap(), u16::MAX);
        // Monotonically non-decreasing.
        assert!(r.red.windows(2).all(|w| w[0] <= w[1]));
    }
}
