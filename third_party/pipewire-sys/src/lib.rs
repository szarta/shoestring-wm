// Copyright The pipewire-rs Contributors.
// SPDX-License-Identifier: MIT

#[allow(non_upper_case_globals)]
#[allow(non_camel_case_types)]
#[allow(non_snake_case)]
#[allow(unpredictable_function_pointer_comparisons)]
#[allow(unnecessary_transmutes)]
#[allow(clippy::all)]
mod bindings {
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}
pub use bindings::*;

// shoestring-wm: FreeBSD's pipewire package renames pw_init/pw_deinit to
// pipewire_init/pipewire_deinit (pw_init clashes with libutil) and keeps the
// old names only as macros, which bindgen can't see. Give them back their
// usual names so the `pipewire` crate builds unchanged. See PATCH.md.
#[cfg(target_os = "freebsd")]
pub use bindings::{pipewire_deinit as pw_deinit, pipewire_init as pw_init};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init() {
        unsafe {
            pw_init(std::ptr::null_mut(), std::ptr::null_mut());
            pw_deinit();
        }
    }
}
