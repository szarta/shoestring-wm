# 0002. Depend on upstream Smithay at a pinned commit; patch a single crate under `third_party/` instead of forking

## Status

Accepted (2026-10-19).

## Context

shoestring-wm is built on [Smithay](https://github.com/Smithay/smithay)
(MIT). Its last release, 0.7.0, dates from 2025-06-24, while `master`
keeps moving, so the workspace depends on Smithay by git URL at a pinned
commit (`Cargo.toml`, `crates/wlcs-shoestring/Cargo.toml`).

Twice a dependency inside that tree has blocked a platform:

- `pam-client2` built only against Linux-PAM, so `shoestring-lock` did
  not build on FreeBSD.
- On 2026-10-19 FreeBSD 15.1 shipped libdisplay-info 0.4, which the
  newest released `libdisplay-info-sys` rejects, breaking the build that
  `smithay-drm-extras` pulls in.

Neither problem was in Smithay itself, and neither needed a Smithay fork
to solve.

## Decision

- **Depend on upstream Smithay, pinned to a commit.** Every Smithay
  dependency names the same `rev`. Moving the pin is a deliberate change
  of its own, tested like any other.
- **When a crate in the dependency tree blocks us, patch that crate, not
  Smithay.** Vendor it under `third_party/<crate>/`, override it with
  `[patch.crates-io]` in the root `Cargo.toml`, and keep its published
  version so existing requirements still match. Prefer an upstream fix
  (a release, or a pending upstream change) over local edits.
- **Each vendored crate carries a `PATCH.md`** with the provenance ADR
  0001 requires, and a condition for dropping it.
- **Fork Smithay only when a change has to be in Smithay itself**, and
  upstream can't take it in time. The maintainer keeps a local clone of
  Smithay with full history for that case, with a branch at the current
  pin.

## Consequences

- **`[patch.crates-io]` stays the last section of `Cargo.toml`.** The
  FreeBSD ports cargo framework comments out every `git =` line after
  that header, so placing it above the Smithay dependencies breaks the
  port. The comment above the section says so.
- **Vendored crates are left out of the FreeBSD port's crate list.**
  `packaging/freebsd/Makefile.crates` and `distinfo` don't list a crate
  that `[patch.crates-io]` replaces. Dropping a vendor restores them.
- **Vendored code isn't held to our complexity metric.** The `knots`
  hook skips `third_party/`, since refactoring vendored code would only
  make it harder to diff against upstream. Every other hook still runs
  there.
- **A patch applies to the root workspace only.** `wlcs-shoestring` has
  its own workspace and lockfile; if it ever pulls in a vendored crate,
  it needs the same override.
- **Vendors are temporary.** Each `PATCH.md` says when to remove it.
  Check those conditions whenever the Smithay pin moves.
