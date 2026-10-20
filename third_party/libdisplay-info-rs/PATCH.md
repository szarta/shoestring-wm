# Vendored `libdisplay-info` crates with libdisplay-info 0.4 support

A copy of three crates from
[libdisplay-info-rs](https://github.com/Smithay/libdisplay-info-rs)
(MIT), pulled in via `[patch.crates-io]` in the workspace root
`Cargo.toml`:

| Crate | Version | Directory |
|---|---|---|
| `libdisplay-info` | 0.3.0 | `libdisplay-info/` |
| `libdisplay-info-sys` | 0.3.0 | `libdisplay-info-sys/` |
| `libdisplay-info-derive` | 0.1.1 | `libdisplay-info-derive/` |

The source is upstream's not-yet-released libdisplay-info 0.4 support,
commit `a2890463af474e4ec3f0037d54d53caba2b9f38b`, which sits on top of
the 0.3.0 release (`6a05eb0b`). The crate versions are unchanged,
so the patch satisfies the `libdisplay-info = "0.3.0"` requirement in
Smithay's `smithay-drm-extras`.

Vendored by Brandon Arrendondo on 2026-10-19.

## Why

`smithay-drm-extras` reads each monitor's make, model and serial from
its EDID through these crates (`src/backend/udev.rs`). The released
`libdisplay-info-sys` 0.3.0 caps the native library at `< 0.4.0` in its
pkg-config check, so the build fails on systems that ship
libdisplay-info 0.4, FreeBSD 15.1's packages among them.

## The change

Everything here is upstream code from that commit; nothing was
edited locally. Relative to 0.3.0, the hand-written part is small:

- `libdisplay-info-sys/build.rs` detects a native 0.4 library and
  selects the new `v0_4` bindings. `Cargo.toml` raises the pkg-config
  cap to `< 0.5.0`.
- `libdisplay-info-sys/src/auto.rs` adds aliases and thin wrappers for
  the symbols 0.4 renamed, plus getters for the VIC and DMT codes that
  0.4 wrapped in structs.
- `libdisplay-info-derive` gains a `#[get_with(fn)]` field attribute,
  which the safe wrappers use for those three fields.
- `libdisplay-info-sys/src/v0_4/` holds the bindgen output for 0.4.

Systems with an older libdisplay-info keep their old bindings.

Upstream's workspace `Cargo.toml`, CI config and `.gitignore` were not
vendored. Upstream ships no license text, only `license = "MIT"` in each
`Cargo.toml`, so `LICENSE` here is the standard MIT text naming the
author listed in those manifests and the project's contributors.

## Verification

Built and linted with `cargo clippy --workspace --all-targets -- -D
warnings` against libdisplay-info 0.1.1 and 0.4.0. Against 0.4.0, the
call path Smithay uses (`Info::parse_edid`, then `make`, `model`,
`serial`) returned the expected values for two real monitors.

## Dropping the vendor

Remove this directory and its three `[patch.crates-io]` entries once a
libdisplay-info-rs release supports 0.4 and the pinned Smithay revision
accepts it. Then restore the three crates in
`packaging/freebsd/Makefile.crates` and `distinfo` (`make makesum`
regenerates the latter).
