# shoestring-bar

A lightweight Wayland status bar for
[shoestring-wm](https://github.com/szarta/shoestring-wm). Pure
`wayland-client` — no Smithay, no GTK, no system bus, no fontconfig.

Renders workspaces, the open-window list, the currently focused window,
and a clock; attaches to the bottom edge of an output via
`zwlr_layer_shell_v1`. Window data comes from
`ext-foreign-toplevel-list-v1`; workspace and focus state come from
shoestring-wm's IPC stream.

## Build & run

```sh
cargo install --path .
shoestring-bar
```

The bar binds to `$WAYLAND_DISPLAY`. No CLI flags; configure via env:

| Variable                | Purpose                                                |
|-------------------------|--------------------------------------------------------|
| `WAYLAND_DISPLAY`       | required                                               |
| `SHOESTRING_WM_SOCKET`  | optional — enables workspace + focus updates from IPC  |
| `SHOESTRING_BAR_FONT`   | override TTF font path (skips bundled search)          |
| `RUST_LOG`              | tracing filter, default `info`                         |

Without `SHOESTRING_WM_SOCKET` the bar still renders the window list and
the clock — just no workspace or focus highlighting.

## Documentation

Full user guide lives in the shoestring-wm repo:

- [shoestring-bar page](https://github.com/szarta/shoestring-wm/blob/main/docs/bar.rst)
- `man shoestring-bar` after installing the shoestring-wm man pages.

## Status

v1 is rendering-complete but configuration-minimal: position (bottom),
height (24 px), and colors are baked in. Tracked roadmap items include
configurable clock format, position, and colors, plus multi-output
support.

## License

MIT — see [LICENSE](LICENSE).
