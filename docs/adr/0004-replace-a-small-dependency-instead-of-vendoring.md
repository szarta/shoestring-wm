# 0004. Replace a small dependency instead of vendoring it

## Status

Accepted (2026-10-20). Amends [ADR 0002](0002-patch-a-crate-instead-of-forking-smithay.md).

## Context

ADR 0002 says to vendor and patch a crate under `third_party/` when it
blocks a platform, and to drop the vendor once upstream carries the fix.
`shoestring-lock` kept such a vendor of `pam-client2` so it would build
on FreeBSD, with no date by which it could be dropped.

`shoestring-lock` used four things from it: a conversation handler,
`pam_start`, `pam_authenticate` and `pam_acct_mgmt`. Carrying about
4,200 vendored lines, plus `pam-sys2` under them, for that was more
than writing the calls ourselves.

## Decision

- **When we use only a small, stable part of a crate, and a vendored
  copy has no near-term way out, replace it with our own code.**
  "Small" means the replacement is short enough to review in one
  sitting and has its own tests.
- **Vendoring (ADR 0002) stays the default** when we use a substantial
  part of the crate, when the fix is likely to land upstream soon, or
  when the crate sits deep in the dependency tree (such as the
  `libdisplay-info` crates under `smithay-drm-extras`), where we can't
  swap it out.
- **Our replacement takes platform differences from the platforms'
  headers.** For PAM, `PAM_CONV_ERR` is 19 on Linux-PAM and 6 on
  OpenPAM, so `crates/shoestring-lock/src/pam.rs` sets it per platform
  and fails to compile on a platform it doesn't know.

## Consequences

- `third_party/pam-client2` and its `[patch.crates-io]` entry are gone,
  and `shoestring-lock` links `libpam` directly with no PAM crate.
- Security-sensitive replacements need the same care as the code they
  replace: the PAM module checks every allocation, wipes the password
  and any reply it frees itself, and has tests for each conversation
  path, plus an ignored test that runs a real `pam_unix` conversation.
- This doesn't justify rewriting a dependency just to avoid it.
  Replacement is for a small surface with no other way out; otherwise
  ADR 0002 applies.
