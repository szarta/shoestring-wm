# Vendored `pipewire-sys` that builds against FreeBSD's pipewire

A copy of `pipewire-sys` 0.10.0 from
[pipewire-rs](https://gitlab.freedesktop.org/pipewire/pipewire-rs) (MIT),
pulled in via `[patch.crates-io]` in the workspace root `Cargo.toml`. The
source is the crates.io package, built from upstream commit
`5dd5dfad56bb0987e4d63af3d6f397e4a132dd9d`. The version is unchanged, so the
patch satisfies the `pipewire-sys = "0.10"` requirement of the `pipewire`
crate.

Vendored by Brandon Arrendondo on 2026-10-24.

## Why

`xdg-desktop-portal-shoestring` and `shoestring-mediad` use the `pipewire`
crate, whose `init()` and `deinit()` call `pw_sys::pw_init` and
`pw_sys::pw_deinit`. FreeBSD's libutil also exports a `pw_init`, so FreeBSD's
pipewire package renames the two functions to `pipewire_init` and
`pipewire_deinit` and keeps the old names only as `#define` macros in
`pipewire/pipewire.h`. Bindgen doesn't turn function-like macros into
functions, and upstream's allowlist only matches `pw_.*`, so on FreeBSD the
bindings have neither name and the `pipewire` crate fails to compile (E0425).
The rename is a FreeBSD packaging change (pipewire upstream issue 1819), and
pipewire-rs has no FreeBSD handling as of its latest commit `7102242`.

Before this vendor, the FreeBSD port patched both crates in its own build,
which a plain `cargo build` can't do, so CI excluded the two PipeWire crates
on FreeBSD.

## The change

Two hunks, each marked with a `shoestring-wm:` comment:

- `build.rs` also allowlists `pipewire_.*`. Upstream's headers declare no
  such function, so the Linux bindings are unchanged.
- `src/lib.rs`, on FreeBSD only, re-exports `pipewire_init` and
  `pipewire_deinit` as `pw_init` and `pw_deinit`, so the `pipewire` crate
  builds unchanged.

Everything else is the published 0.10.0 package. Its `Cargo.lock`,
`Cargo.toml.orig` and the `.cargo_vcs_info.json` / `.cargo-ok` markers were
not copied.

## When to drop it

When a released `pipewire-sys` binds `pipewire_init`/`pipewire_deinit` on
FreeBSD (or pipewire itself drops the rename), and `Cargo.lock` moves to it.
Also revisit it whenever the `pipewire` dependency moves past 0.10: a new
`pipewire-sys` needs re-vendoring with these two hunks, or dropping.
