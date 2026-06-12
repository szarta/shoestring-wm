# FreeBSD port for shoestring-wm

This is a FreeBSD ports skeleton (`USES=cargo`) — the FreeBSD analogue of
the `.deb` / `.rpm` built from `Cargo.toml`'s packaging metadata. It lets
FreeBSD build a real `pkg` from the same release tag.

Pre-1.0 it lives **in-repo** and is exercised by CI (a `poudriere`-built
package). It is **not** yet submitted to the official FreeBSD ports tree;
that is a 1.0 task (it goes through Bugzilla review).

## Files

| File             | Hand-written | What it is                                            |
| ---------------- | :----------: | ----------------------------------------------------- |
| `Makefile`       | yes          | The port recipe (deps, build/install logic)           |
| `Makefile.crates`| generated    | `CARGO_CRATES=` — every pinned dependency, incl. the smithay git rev |
| `distinfo`       | generated    | SHA256 + size of the source tarball and every crate   |
| `pkg-plist`      | yes          | Exact list of installed files                         |
| `pkg-descr`      | yes          | Package description                                    |

## Regenerating the generated files

Run against a checkout of the **FreeBSD ports tree**. Drop this directory
in as `x11-wm/shoestring-wm`, then from there:

```sh
# 1. Read Cargo.lock and emit the dependency list (crates.io + git crates).
make cargo-crates > Makefile.crates

# 2. Fetch every distfile and record its checksum + size.
make makesum

# 3. Validate the file list against a real staged install.
make stage && make check-plist
```

`make cargo-crates` / `makesum` fetch the source tarball from
`codeload.github.com` by tag (`vDISTVERSION`). To regenerate against an
**untagged** commit (e.g. while iterating before a release is cut),
override the tag with the commit hash:

```sh
make GH_TAGNAME=<commit-sha> cargo-crates
make GH_TAGNAME=<commit-sha> makesum
```

The crate checksums are reproducible and tag-independent; only the single
source-tarball line in `distinfo` depends on the exact commit. CI refreshes
that line (`make makesum`) against the checkout it is building, so the
committed `distinfo` need only be correct for humans and for the eventual
ports-tree submission.

## Why a few things differ from the .deb/.rpm

- **Linker path** is handled by `USES=...localbase:ldflags` instead of the
  `RUSTFLAGS=-L/usr/local/lib` the source-build docs use.
- **The session wrapper** (`shoestring-wm-session`) is rewritten to POSIX
  `sh` at `post-patch` so it runs without `bash` (not installed by default
  on FreeBSD).
- **The locker PAM policy** is `resources/pam/shoestring-lock.freebsd`
  (delegates to `security/unix-selfauth-helper`), installed as a `@sample`.
