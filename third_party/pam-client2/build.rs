//! Selects the PAM implementation flavour at compile time.
//!
//! Linux-PAM and OpenPAM diverge in a few FFI details that leak into this
//! crate's bindings-dependent code: the `pam_message.msg` pointer
//! mutability and the set of `PAM_*` return codes (OpenPAM has no
//! `PAM_CONV_AGAIN` / `PAM_INCOMPLETE`). `pam-sys2` already picks the
//! implementation per target in its own build script; we mirror that
//! decision here and expose it as a single `openpam` cfg so the rest of
//! the crate can gate on it concisely.
fn main() {
    println!("cargo:rustc-check-cfg=cfg(openpam)");

    // Matches pam-sys2's OpenPAM target set (see its build.rs). Linux and
    // Android use Linux-PAM; everything pam-sys2 supports otherwise is
    // OpenPAM.
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if matches!(
        target_os.as_str(),
        "freebsd" | "netbsd" | "macos" | "ios" | "dragonfly"
    ) {
        println!("cargo:rustc-cfg=openpam");
    }
}
