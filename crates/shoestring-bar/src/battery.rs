//! Battery status source for the bar.
//!
//! [`auto_detect`] picks a platform-appropriate [`BatterySource`] at
//! startup (or returns `None` on systems without a recognised battery).
//! Sources are polled cheaply once per bar tick; the trait keeps the
//! rest of the bar ignorant of the per-OS read mechanism.
//!
//! Supported sources:
//!
//! - **Linux**: reads the first `/sys/class/power_supply/BAT*` entry
//!   (capacity + status files). Stateless — re-reads on every call.
//! - **FreeBSD**: `sysctlbyname` against `hw.acpi.battery.life` /
//!   `hw.acpi.battery.state` / `hw.acpi.battery.units`. Untested at
//!   the time of writing; pure `libc` so the dep tree stays clean.
//!
//! Other operating systems (and BSDs without an ACPI battery) return
//! `None` from `auto_detect`; the bar simply hides the indicator.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatteryState {
    Charging,
    Discharging,
    Full,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatteryReading {
    /// 0..=100, clamped from whatever the source produced.
    pub capacity: u8,
    pub state: BatteryState,
}

pub trait BatterySource: Send {
    fn read(&self) -> Option<BatteryReading>;
}

/// Pick the first source that detects a battery. `None` means either
/// no supported source for this OS, or no battery present.
pub fn auto_detect() -> Option<Box<dyn BatterySource>> {
    #[cfg(target_os = "linux")]
    {
        if let Some(s) = linux::Sysfs::detect() {
            return Some(Box::new(s));
        }
    }
    #[cfg(target_os = "freebsd")]
    {
        if let Some(s) = freebsd::Acpi::detect() {
            return Some(Box::new(s));
        }
    }
    None
}

/// Substitute `{pct}` and `{sign}` placeholders in `fmt`. Sign is `+`
/// when charging, `-` when discharging, empty otherwise. Anything else
/// in the template is passed through verbatim — no escaping needed,
/// the placeholders are full strings rather than printf-style %s.
///
/// At 100% capacity the sign is always suppressed: a battery sitting on
/// AC at full charge flickers its kernel status between `Full`,
/// `Charging`, and `Discharging` (the latter as it sheds a trickle
/// before topping back up), which otherwise makes the bar cycle between
/// `100%` and `100%-`/`100%+`. At full there is nothing to signal.
pub fn format_reading(reading: &BatteryReading, fmt: &str) -> String {
    let sign = if reading.capacity >= 100 {
        ""
    } else {
        match reading.state {
            BatteryState::Charging => "+",
            BatteryState::Discharging => "-",
            BatteryState::Full | BatteryState::Unknown => "",
        }
    };
    fmt.replace("{pct}", &reading.capacity.to_string())
        .replace("{sign}", sign)
}

/// Pick the indicator color based on capacity + state. Charging /
/// Full batteries always use `normal` — the visual warning is about
/// imminent depletion, not about being plugged in at low charge.
pub fn pick_color(
    reading: &BatteryReading,
    normal: u32,
    low: u32,
    low_threshold: u8,
    critical: u32,
    critical_threshold: u8,
) -> u32 {
    if matches!(reading.state, BatteryState::Charging | BatteryState::Full) {
        return normal;
    }
    if reading.capacity <= critical_threshold {
        critical
    } else if reading.capacity <= low_threshold {
        low
    } else {
        normal
    }
}

/// Parse the contents of `/sys/class/power_supply/BAT*/capacity`.
/// Public so tests can exercise it without sysfs present.
#[allow(dead_code)] // used by linux module + tests; dead on non-Linux build
pub fn parse_capacity_str(s: &str) -> Option<u8> {
    s.trim().parse::<u32>().ok().map(|v| v.min(100) as u8)
}

/// Parse the contents of `/sys/class/power_supply/BAT*/status`.
/// Public so tests can exercise it without sysfs present.
#[allow(dead_code)] // used by linux module + tests; dead on non-Linux build
pub fn parse_status_str(s: &str) -> BatteryState {
    match s.trim() {
        "Charging" => BatteryState::Charging,
        "Discharging" => BatteryState::Discharging,
        "Full" => BatteryState::Full,
        _ => BatteryState::Unknown,
    }
}

/// Decode FreeBSD's `hw.acpi.battery.state` bitmask. Public + always
/// compiled so we can unit-test it on Linux CI; the platform module
/// just calls into it.
///
/// Bits per `acpiio(4)` / `sysctl(8)`:
/// - `1` = discharging
/// - `2` = charging
/// - `4` = critical (orthogonal to the above; usually paired with bit 1)
/// - `7` = no battery info available
#[allow(dead_code)] // used by freebsd module + tests; dead on Linux build
pub fn decode_freebsd_state(bits: i32) -> BatteryState {
    if bits == 7 || bits < 0 {
        return BatteryState::Unknown;
    }
    if bits & 2 != 0 {
        return BatteryState::Charging;
    }
    if bits & 1 != 0 {
        return BatteryState::Discharging;
    }
    BatteryState::Full
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{
        parse_capacity_str, parse_status_str, BatteryReading, BatterySource, BatteryState,
    };
    use std::{fs, path::PathBuf};

    pub struct Sysfs {
        path: PathBuf,
    }

    impl Sysfs {
        pub fn detect() -> Option<Self> {
            let dir = fs::read_dir("/sys/class/power_supply").ok()?;
            for entry in dir.flatten() {
                let name = entry.file_name();
                let s = name.to_string_lossy();
                if s.starts_with("BAT") {
                    let path = entry.path();
                    // Sanity-check by reading capacity once at detect
                    // time; if the kernel exposes a BAT* dir without
                    // the file we expect (rare but possible on weird
                    // ACPI), skip it rather than producing dud reads.
                    if fs::read_to_string(path.join("capacity")).is_ok() {
                        return Some(Self { path });
                    }
                }
            }
            None
        }
    }

    impl BatterySource for Sysfs {
        fn read(&self) -> Option<BatteryReading> {
            let cap = fs::read_to_string(self.path.join("capacity")).ok()?;
            let capacity = parse_capacity_str(&cap)?;
            let state = fs::read_to_string(self.path.join("status"))
                .ok()
                .as_deref()
                .map(parse_status_str)
                .unwrap_or(BatteryState::Unknown);
            Some(BatteryReading { capacity, state })
        }
    }
}

#[cfg(target_os = "freebsd")]
mod freebsd {
    use super::{decode_freebsd_state, BatteryReading, BatterySource};
    use std::{ffi::CString, mem, os::raw::c_void, ptr};

    pub struct Acpi;

    impl Acpi {
        pub fn detect() -> Option<Self> {
            // `units == 0` is the documented "no battery" signal; we
            // also bail if life isn't readable at all (no ACPI
            // battery driver loaded).
            let units = read_int("hw.acpi.battery.units")?;
            if units == 0 {
                return None;
            }
            read_int("hw.acpi.battery.life")?;
            Some(Self)
        }
    }

    impl BatterySource for Acpi {
        fn read(&self) -> Option<BatteryReading> {
            let life = read_int("hw.acpi.battery.life")?;
            let bits = read_int("hw.acpi.battery.state").unwrap_or(0);
            Some(BatteryReading {
                capacity: life.clamp(0, 100) as u8,
                state: decode_freebsd_state(bits),
            })
        }
    }

    fn read_int(name: &str) -> Option<i32> {
        let cname = CString::new(name).ok()?;
        let mut val: i32 = 0;
        let mut len = mem::size_of::<i32>();
        let rc = unsafe {
            libc::sysctlbyname(
                cname.as_ptr(),
                &mut val as *mut i32 as *mut c_void,
                &mut len,
                ptr::null_mut(),
                0,
            )
        };
        (rc == 0).then_some(val)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(capacity: u8, state: BatteryState) -> BatteryReading {
        BatteryReading { capacity, state }
    }

    #[test]
    fn format_default_template() {
        let fmt = "{pct}%{sign}";
        assert_eq!(
            format_reading(&reading(85, BatteryState::Discharging), fmt),
            "85%-"
        );
        assert_eq!(
            format_reading(&reading(85, BatteryState::Charging), fmt),
            "85%+"
        );
        assert_eq!(
            format_reading(&reading(100, BatteryState::Full), fmt),
            "100%"
        );
        // At full charge the sign is suppressed regardless of the
        // (flickering) reported state — no "100%+"/"100%-" churn.
        assert_eq!(
            format_reading(&reading(100, BatteryState::Charging), fmt),
            "100%"
        );
        assert_eq!(
            format_reading(&reading(100, BatteryState::Discharging), fmt),
            "100%"
        );
        assert_eq!(
            format_reading(&reading(50, BatteryState::Unknown), fmt),
            "50%"
        );
    }

    #[test]
    fn format_drops_sign_when_template_omits_it() {
        let fmt = "{pct}%";
        assert_eq!(
            format_reading(&reading(40, BatteryState::Discharging), fmt),
            "40%"
        );
    }

    #[test]
    fn format_passes_other_text_through() {
        assert_eq!(
            format_reading(&reading(72, BatteryState::Discharging), "BAT {pct}%{sign}"),
            "BAT 72%-"
        );
    }

    #[test]
    fn parse_capacity_clamps_and_rejects_garbage() {
        assert_eq!(parse_capacity_str("0\n"), Some(0));
        assert_eq!(parse_capacity_str("85"), Some(85));
        assert_eq!(parse_capacity_str("  100  "), Some(100));
        // Some kernel drivers report 101 transiently; clamp instead of
        // dropping the read.
        assert_eq!(parse_capacity_str("105"), Some(100));
        assert_eq!(parse_capacity_str(""), None);
        assert_eq!(parse_capacity_str("foo"), None);
    }

    #[test]
    fn parse_status_recognises_canonical_values() {
        assert_eq!(parse_status_str("Charging\n"), BatteryState::Charging);
        assert_eq!(parse_status_str("Discharging"), BatteryState::Discharging);
        assert_eq!(parse_status_str("Full"), BatteryState::Full);
        assert_eq!(parse_status_str("Not charging"), BatteryState::Unknown);
        assert_eq!(parse_status_str(""), BatteryState::Unknown);
    }

    #[test]
    fn freebsd_state_bitmask() {
        assert_eq!(decode_freebsd_state(1), BatteryState::Discharging);
        assert_eq!(decode_freebsd_state(2), BatteryState::Charging);
        // Critical-while-discharging: bit 1 still wins for our purposes.
        assert_eq!(decode_freebsd_state(5), BatteryState::Discharging);
        // No state bits set = idle/full.
        assert_eq!(decode_freebsd_state(0), BatteryState::Full);
        // Sentinel.
        assert_eq!(decode_freebsd_state(7), BatteryState::Unknown);
        assert_eq!(decode_freebsd_state(-1), BatteryState::Unknown);
    }

    #[test]
    fn color_picker_respects_thresholds() {
        const NORMAL: u32 = 0xFFFFFFFF;
        const LOW: u32 = 0xFFFFAA55;
        const CRIT: u32 = 0xFFFF5555;

        let pick = |cap, state| pick_color(&reading(cap, state), NORMAL, LOW, 20, CRIT, 10);

        assert_eq!(pick(80, BatteryState::Discharging), NORMAL);
        assert_eq!(pick(20, BatteryState::Discharging), LOW);
        assert_eq!(pick(11, BatteryState::Discharging), LOW);
        assert_eq!(pick(10, BatteryState::Discharging), CRIT);
        assert_eq!(pick(0, BatteryState::Discharging), CRIT);

        // Charging / full never warn even at low capacity.
        assert_eq!(pick(5, BatteryState::Charging), NORMAL);
        assert_eq!(pick(5, BatteryState::Full), NORMAL);
        // Unknown gets the same treatment as Discharging — fail loud
        // rather than hide a low battery behind an unknown state.
        assert_eq!(pick(5, BatteryState::Unknown), CRIT);
    }
}
