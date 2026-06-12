# Vendored, patched `pam-client2` 0.5.5

A near-verbatim copy of [`pam-client2`
0.5.5](https://crates.io/crates/pam-client2) (MPL-2.0,
<https://gitlab.com/LeChatP/rust-pam-client/>), pulled in via
`[patch.crates-io]` in the workspace root `Cargo.toml`, with a small set
of changes that make it **build on OpenPAM platforms** (FreeBSD, NetBSD,
macOS, iOS, DragonFly).

`shoestring-lock` authenticates the unlock via PAM through `pam-client2`.
Unpatched, the crate only compiles against Linux-PAM, which made
`shoestring-lock` the one workspace crate that would not build on FreeBSD
(verified on dev-106, FreeBSD 15.0).

## The two incompatibilities

`pam-client2` is written against Linux-PAM and `pam-sys2` faithfully
exposes the *other* PAM flavour on the BSDs/macOS — so the crate hits two
compile errors there:

1. **`pam_message.msg` pointer mutability.** Linux-PAM declares it
   `const char *`, OpenPAM declares it `char *` (`*mut c_char`).
   `pam-client2`'s conversation FFI (`src/ffi.rs`) hardcodes `&*const
   c_char` in the `msg_content_*` helpers → ~8 `E0308` mismatched-mutability
   errors.

2. **Linux-PAM-only return codes.** `PAM_CONV_AGAIN` and `PAM_INCOMPLETE`
   are Linux-PAM extensions; OpenPAM has no such codes, so the
   `ErrorCode` enum and its `repr()` / `from_repr()` mappings (`src/lib.rs`,
   `src/error.rs`) reference constants that don't exist → `E0425`.

The PAM handle pointers were *already* portable upstream — `PamHandle`
provides both `From<PamHandle> for *mut/*const RawPamHandle` — so these two
are the only divergences in the surface `shoestring-lock` exercises.

## The change

A new `build.rs` sets a single `openpam` cfg, mirroring `pam-sys2`'s own
target selection (`freebsd | netbsd | macos | ios | dragonfly`). Then:

- `src/ffi.rs` — a `MsgContentPtr` alias (`*mut c_char` under `openpam`,
  `*const c_char` otherwise) replaces the hardcoded `*const c_char` in the
  three `msg_content_*` helper signatures. Bodies are untouched (`*mut →
  *const` coerces at the `CStr::from_ptr` / `.cast()` call sites).
- `src/lib.rs`, `src/error.rs` — the `CONV_AGAIN` / `INCOMPLETE` enum
  variants and every match arm that names them are `#[cfg(not(openpam))]`.
  Shimming fake values was rejected: it would make `from_repr` mis-map a
  real OpenPAM code.
- `src/ffi.rs`, `src/resp_buf.rs`, `src/context.rs` — items used **only**
  by the Linux-only binary/radio/`PAM_XAUTHDATA` paths
  (`msg_content_to_bin`, `msg_content_to_cstr`, `put_binary`, the `c_char`
  / `slice` imports) are `#[cfg(target_os = "linux")]`, so the crate is
  warning-clean on OpenPAM under `clippy -D warnings`.

On Linux nothing changes: `openpam` is unset, so the code is byte-for-byte
upstream. Verified with `cargo clippy --workspace --all-targets -- -D
warnings` on Linux and `cargo clippy -p shoestring-lock … -D warnings` +
debug/release builds on FreeBSD 15.0.

Non-essential packaging files (`Cargo.lock`, `Cargo.toml.orig`, CI config)
were dropped from the vendor; `LICENSE` (MPL-2.0) is retained as required.
`Cargo.toml` differs from upstream only in `build = false` →
`build = "build.rs"`.

## Upstream

This should be reported/offered upstream so the fork can be dropped:

- Project: <https://gitlab.com/LeChatP/rust-pam-client/>
- The complete source diff is in `openpam-portability.patch` (next to this
  file). It applies cleanly to a pristine 0.5.5 checkout; the only piece
  not in the patch is the new `build.rs` + the `build` key in `Cargo.toml`.
- A fully upstreamable change would additionally port the `#[cfg(test)]`
  conversation tests, which build `PamMessage { msg: text.as_ptr(), .. }`
  assuming `*const`. Those tests are not compiled when the crate is
  consumed as a dependency, so they are left untouched in this vendor.

See the shoestring-wm task tracker (item 141) for context.
