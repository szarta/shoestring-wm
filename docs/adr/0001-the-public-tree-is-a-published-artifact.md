# 0001. The public tree is a published artifact; it ships sanitized and records the provenance of what it borrows

## Status

Accepted (2026-10-19).

## Context

shoestring-wm is a public repository. Everything that reaches `origin` is
published: the source tree, the Sphinx manual, the release packages
(`.deb`, `.rpm`) and the FreeBSD port.

`AGENTS.md` ("Public files") already keeps task ids, task titles,
credentials, private host names, addresses, network layout and the
location of unfixed upstream defects out of public files. On 2026-10-19
a sweep still found a private build host's name in a source comment, the
manual, a CI comment and a vendored crate's notes, and a task-tracker
reference in the same notes. The vendored crates under `third_party/`
also recorded where their code came from, but not who vendored it or
when.

## Decision

**Every commit that reaches `origin` meets three conditions.**

1. **It leaks nothing private.**
   - The list in `AGENTS.md` ("Public files") applies. That list is kept
     there and nowhere else.
   - Also kept out: local filesystem paths from a real machine, and
     internal URLs.
   - Examples use obvious placeholders: `/home/you`, `remote-host`, "a
     FreeBSD machine".
   - The exception is authorship git already makes public: commit author
     names and e-mail addresses.
2. **What it borrows keeps its licence and records its provenance.** A
   file or directory derived from someone else's work keeps that work's
   licence notice. Next to it (a `PATCH.md` under `third_party/`, for
   example) it records:
   - what was changed, or that nothing was;
   - who brought it in, and when;
   - where the unmodified original is, down to a release or commit.

   When upstream ships no licence text, the notes say so, and the
   licence file used is the standard text for the licence the upstream
   declares.
3. **It discloses AI use in one place**: the "AI Assistance" section of
   `README.md`. Commit trailers are not the mechanism (`AGENTS.md`).

## Consequences

- **This is a personal project.** No employer's review, copyright or
  accountability process applies to it.
- **Fix leaks at the head.** A leak found in text already in public git
  history is fixed in a new commit. Whether to rewrite history is decided
  case by case, and is not the default.
- **Placeholders are fine.** Paths like `/home/tester` in tests and
  `/home/you` in docs are not leaks. A real path copied from a real
  machine is.
- **Provenance is recorded when code is vendored**, and updated when the
  vendored copy is refreshed or patched further.
- **Nothing in `AGENTS.md` is relaxed.** This ADR only adds conditions
  and the reasoning behind them.
