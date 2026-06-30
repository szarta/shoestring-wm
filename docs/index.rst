shoestring-wm
=============

shoestring-wm is a Rust Wayland compositor designed as a lightweight,
low-dependency replacement for an Openbox/X11 desktop. It pairs a small
floating-window WM with two sibling projects:

- **shoestring-bar** — a status bar that consumes the WM's IPC stream.
- **shoestring-menu** — a dmenu-style launcher for commands and bookmarks.

This is the user guide. For source-level architecture notes see
``docs/architecture.md`` in the repository.

.. toctree::
   :maxdepth: 2
   :caption: User Guide

   overview
   install
   running
   configuration
   bindings
   portals
   compatibility
   containers
   remote
   ipc

.. toctree::
   :maxdepth: 1
   :caption: Companion Tools

   bar
   menu

.. toctree::
   :maxdepth: 1
   :caption: Reference

   architecture

.. toctree::
   :hidden:
   :caption: Man pages

   man/shoestring-wm.1
   man/shoestring-wm.5
   man/shoestring-bar.1
   man/shoestring-menu.1
   man/shoestring-ctl.1
   man/shoestring-tasks.1
   man/shoestring-confirm.1
   man/shoestring-kill.1
   man/shoestring-lock.1
   man/shoestring-notify.1
   man/shoestring-region.1
   man/shoestring-screenshot.1

Indices
-------

* :ref:`genindex`
* :ref:`search`
