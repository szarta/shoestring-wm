# shoestring-menu

A dmenu-style launcher for
[shoestring-wm](https://github.com/szarta/shoestring-wm). Wayland
layer-shell panel, pure `wayland-client`, two modes (**commands** and
**bookmarks**) sharing the same UI.

## Build & run

The menu is a workspace member of shoestring-wm. `cargo build
--release --workspace` from the repo root builds it with everything
else; to build just the menu:

```sh
cargo build --release -p shoestring-menu
shoestring-menu                    # commands mode
shoestring-menu --mode bookmarks   # bookmarks mode
```

Default bindings shipped by `shoestring-wm --write-default-config`:

- `Super+P` → `shoestring-menu` (commands)
- `Super+B` → `shoestring-menu --mode bookmarks`

Inside the menu: type to filter, `Up`/`Down` to navigate, `Enter` to
launch, `Esc` to cancel.

## Source files

Both modes read from the WM's config directory by default:

| Mode      | Default source                                          |
|-----------|---------------------------------------------------------|
| commands  | `$XDG_CONFIG_HOME/shoestring-wm/executables`            |
| bookmarks | `$XDG_CONFIG_HOME/shoestring-wm/bookmarks`              |

Override either with `--source PATH`.

**commands** format: one command per line. Blank lines and `#` comments
are skipped. The selection is whitespace-split and exec'd detached.

**bookmarks** format: markdown bullets containing `[label](url)`. The
entire line is displayed (so tags and URLs are fuzzy-searchable) and the
URL is opened via `xdg-open`. Example:

```markdown
- [Smithay docs](https://smithay.github.io/smithay/) <!-- TAGS: wayland rust -->
- [niri](https://github.com/YaLTeR/niri) <!-- TAGS: wayland reference -->
```

## Environment

| Variable                | Purpose                                          |
|-------------------------|--------------------------------------------------|
| `WAYLAND_DISPLAY`       | required                                         |
| `XDG_CONFIG_HOME`/`HOME`| resolve default source paths                     |
| `SHOESTRING_MENU_FONT`  | override TTF font path                           |
| `SHOESTRING_MENU_LOG`   | append tracing output to this file (TTY debug)   |
| `RUST_LOG`              | tracing filter, default `info`                   |

## Documentation

The full reference is in the shoestring-wm repo:

- [shoestring-menu page](https://github.com/szarta/shoestring-wm/blob/main/docs/menu.rst)
- `man shoestring-menu` after installing the shoestring-wm man pages.

## License

MIT — see [LICENSE](LICENSE).
