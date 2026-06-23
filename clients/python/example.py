#!/usr/bin/env python3
"""Tiny demo of the shoestring-wm Python client.

Run inside a shoestring-wm session (so $SHOESTRING_WM_SOCKET is set):

    python3 example.py
"""

from __future__ import annotations

import shoestring_ipc


def main() -> None:
    with shoestring_ipc.Client() as wm:
        ws = wm.workspaces()
        print(f"active workspace: {ws['active']} of {ws['count']}")

        windows = wm.windows()["windows"]
        print(f"{len(windows)} window(s):")
        for win in windows:
            print(f"  [{win['workspace']}] {win['app_id']}: {win['title']}")

        # Tail a few events, then stop.
        print("watching events (Ctrl-C to stop)...")
        for event in wm.events():
            print(f"  event: {event}")


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        pass
