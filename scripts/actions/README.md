# shoestring-wm action scripts

Tiny wrappers around common system actions (audio, brightness, logout)
so keybinds and `shoestring-menu` can call them without each callsite
hard-coding `wpctl` / `brightnessctl` / `pkill` flags.

| Script                        | What it does                                            | Requires        |
| ----------------------------- | ------------------------------------------------------- | --------------- |
| `shoestring-volume-up`        | Default sink +5% (capped at 100%, also unmutes)         | `wpctl`         |
| `shoestring-volume-down`      | Default sink -5%                                        | `wpctl`         |
| `shoestring-volume-mute`      | Toggle mute on default sink                             | `wpctl`         |
| `shoestring-mic-mute`         | Toggle mute on default source (mic)                     | `wpctl`         |
| `shoestring-brightness-up`    | Screen brightness +5%                                   | `brightnessctl` |
| `shoestring-brightness-down`  | Screen brightness -5% (floor 1% so screen never blacks) | `brightnessctl` |
| `shoestring-logout`           | SIGTERM the running `shoestring-wm` for clean shutdown  | `pkill`         |

Lock is not wrapped — invoke `shoestring-ctl lock` directly so the
compositor's session-lock protocol path is used.

## Install

The scripts are not installed automatically. Symlink or copy them
somewhere on `$PATH`:

```sh
ln -s "$PWD"/scripts/actions/shoestring-* ~/.local/bin/
```

Or reference them by absolute path from your config / keybind actions.
