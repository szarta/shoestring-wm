# 0003. Every workspace crate shares the workspace version

## Status

Accepted (2026-10-20).

## Context

Most workspace crates inherit `version.workspace = true`, but five set
their own version: `shoestring-bar`, `shoestring-menu` and
`shoestring-notify`, which started as separate repositories and were
merged in with their version numbers intact, and `shoestring-mediad`
and `xdg-desktop-portal-shoestring`, which were added later at 0.1.0
following the same pattern. Each was meant to be bumped when it changed,
and was, once, before v0.4.0.

That stopped after v0.4.0. By v0.7.1 `shoestring-bar` had 13 source
commits since its last bump and the portal backend had 5, while both
still reported their old versions. Nothing outside the package uses
those numbers: the release builds one `.deb`/`.rpm` (and one FreeBSD
port) carrying the workspace version. They show up only in a helper's
`--version` output and in the notify daemon's D-Bus server-info reply,
where a 0.7.x package reporting 0.1.0 or 0.2.0 is simply wrong.

Separate versions might also have looked like a way to let people build
only the helpers they need. They aren't: that is decided by what gets
built, not by the version number.

## Decision

- **Every workspace member uses `version.workspace = true`.** The
  `[workspace.package]` version in the root `Cargo.toml` is the only
  version, and `inv bump-version` moves it.
- **A new crate inherits it from the start.** Don't give a workspace
  member its own version, even one that began as a separate project.
- **`wlcs-shoestring` is the exception.** It is excluded from the
  workspace, so it can't inherit `workspace.package`, and it is a test
  plugin that is never shipped (`publish = false`). It keeps its own
  version.

## Consequences

- **A crate's version says which release it came from, not how much it
  changed.** Release notes and `git log` are where per-helper changes
  are recorded.
- **Minimal builds are unaffected.** Each helper still builds and
  installs on its own (`cargo install --path crates/<helper>`), and the
  FreeBSD port's `SCREENCAST` option still leaves out the PipeWire-linked
  crates. Any finer choice of what to build belongs in features, package
  options or the install list, not in per-crate versions.
- **If a helper is ever published on its own** (for example to
  crates.io), with a release cycle separate from the WM's, revisit this
  for that crate in a new ADR.
