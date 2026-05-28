# shoestring-notify

A lightweight desktop notification daemon for
[shoestring-wm](https://github.com/szarta/shoestring-wm). Pure
`wayland-client` + `rustbus` — no Smithay, no GTK, no libdbus, no
fontconfig.

Implements `org.freedesktop.Notifications` on the session bus and
renders notification popups via `zwlr_layer_shell_v1`.

## Status

Bootstrap stub. The crate compiles, the binary prints its version and
exits. Real DBus wiring, layer-shell surfaces, and rendering land in
follow-up milestones.

## Build & run

```sh
cargo install --path .
shoestring-notify
```

## License

MIT — see [LICENSE](LICENSE).
