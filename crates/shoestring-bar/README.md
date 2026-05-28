# shoestring-bar

A lightweight Wayland status bar for
[shoestring-wm](https://github.com/szarta/shoestring-wm). Pure
`wayland-client` — no Smithay, no GTK, no system bus, no fontconfig.

Renders workspaces, the open-window list, the currently focused window,
a battery indicator (when one is detected), and a clock; attaches to
the bottom edge of an output via `zwlr_layer_shell_v1`. Window data
comes from `ext-foreign-toplevel-list-v1`; workspace and focus state
come from shoestring-wm's IPC stream.

## Build & run

The bar is a workspace member of shoestring-wm. From the workspace
root, `cargo build --release --workspace` builds it alongside
everything else. To build just the bar:

```sh
cargo build --release -p shoestring-bar
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

## Configuration

Optional TOML at `$XDG_CONFIG_HOME/shoestring-bar/config.toml` (defaults
to `~/.config/shoestring-bar/config.toml`). A missing file is fine — the
bar runs with the same hardcoded defaults it shipped with.

Drop a starter file in place with:

```sh
shoestring-bar --write-default-config
```

That writes the schema below (with comments) to the resolved config path,
refusing to clobber an existing file unless `--force` is also passed.

```toml
[bar]
position    = "bottom"   # bottom | top
height      = 24
background  = "#222222"  # #RGB, #RRGGBB, or #AARRGGBB
foreground  = "#ffffff"
font        = "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"  # optional
font_size   = 14.0
show_workspaces = true   # set false to hide the box cluster + active name

[clock]
format = "%a %b %d  %H:%M"   # strftime(3) pattern
# format = "24h-short"        # alias for "%H:%M"
# format = "iso"              # alias for "%Y-%m-%d %H:%M:%S"

[battery]
show               = true            # auto-hidden when no battery present
format             = "{pct}%{sign}"  # {sign} is +/-/'' (charging/discharging/full)
low_threshold      = 20              # turns orange at or below this
critical_threshold = 10              # turns red at or below this
```

Battery sources: Linux reads the first `/sys/class/power_supply/BAT*`
entry; FreeBSD reads `hw.acpi.battery.{life,state,units}` via
`sysctlbyname`. Other OSes (and BSDs without ACPI batteries) hide
the indicator silently.

Font resolution order: `[bar].font` → `$SHOESTRING_BAR_FONT` → built-in
candidate paths.

## Status

v1 ships with TOML config for position, height, colors, font, and clock
format. Tracked roadmap items: configurable accent/dim colors and
multi-output support. Vertical bars (`left`/`right`) are not planned —
only horizontal bottom/top.

## License

MIT — see [LICENSE](LICENSE).
