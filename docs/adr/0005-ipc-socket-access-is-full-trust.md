# 0005. Reaching the IPC socket is full trust; the gates are consent, not authorization

## Status

Accepted (2026-10-20).

## Context

The WM's IPC socket is its automation and observation surface. Some
requests are held behind runtime gates (automation, screen capture,
remote), and earlier docs described those gates as protecting a session
from "anything that reaches the socket". The code never worked that
way. Any client of the socket can send `set_automation`,
`set_screen_capture` or `set_remote` and then use what the gate held
back. The docs and the implementation had to be reconciled, one way or
the other.

Who can reach the socket, and what else that party can already do:

- The socket is `$XDG_RUNTIME_DIR/shoestring-wm-$WAYLAND_DISPLAY.sock`,
  mode 0600, normally inside a 0700 directory owned by the user. Only
  processes running as that user can connect. The WM warns at startup
  if the directory is open to anyone else.
- Any process running as that user can already read the user's files,
  `ptrace` the user's processes, and connect to the Wayland socket.
- Every Wayland client may use `zwlr_virtual_pointer_v1` and
  `zwp_virtual_keyboard_v1`, which are advertised without a gate so
  that wlrctl, ydotool and input methods work. A client can therefore
  synthesize clicks and keystrokes without the automation gate.

Separate authorization for changing a gate was considered: a
confirmation dialog when a gate is turned on over IPC, with startup
flags still enabling it for automation. It would not hold. A hostile
client could click "Allow" through the ungated virtual pointer, so a
real barrier would also mean gating virtual input per client, breaking
or allowlisting the tools above. Meanwhile it would add a prompt to
every agent that drives a live desktop.

Test automation, a primary use, needs a session it starts with the
gates already open, and a live session it can open them in.

## Decision

- **Any process that can connect to the IPC socket is trusted with the
  whole session**, including changing every gate. The socket's trust
  boundary is the user's uid, enforced by its file mode (0600) and its
  private directory.
- **The gates are consent and accident protection, not authorization.**
  They keep capture, input synthesis and remote access off until someone
  deliberately turns them on, make that state visible (indicators,
  `*_changed` events), and let it be turned off again. They do not
  defend against hostile code running as the same user.
- **Agents and test harnesses open the gates the ordinary way:** at
  launch with `--enable-automation` or `[general].automation_enabled`,
  or at runtime with `set_automation` / `shoestring-ctl automation on`.
  No extra step is required.
- **A session lock is enforced regardless of the gates.** While locked,
  requests that would reach one window past the lock
  (`capture_window`, `screenshot_window`, `inject_input_to_window`) are
  refused, open graft streams pause, and whole-output capture and global
  input meet the lock screen.

## Consequences

- Docs must describe the gates as consent switches. The authoritative
  description, including what each gate holds back and what stays open,
  is the "Trust model" section of `docs/ipc.rst`. Other docs link to it
  instead of restating it.
- Making a process unable to reach the socket is the way to keep it out:
  run it as another user, or in a container or sandbox without the
  user's `$XDG_RUNTIME_DIR`.
- New IPC requests that observe or drive the session belong behind a
  gate, so they stay off by default. A gate is not a reason to expose
  something to a process that shouldn't reach the socket at all.
- A real barrier against applications inside the session would need
  per-client gating of the virtual input protocols plus an
  interactive confirmation. That would be a new decision superseding
  this one, not a patch to it.
