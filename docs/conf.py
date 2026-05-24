"""Sphinx configuration for the shoestring-wm user guide.

This single conf.py drives the html and man builders. Man-page sources
for the sibling projects (shoestring-bar, shoestring-menu) live here too
so there is one place that owns prose documentation.
"""

project = "shoestring-wm"
author = "Brandon Arrendondo"
copyright = "2026, Brandon Arrendondo"
release = "0.1"
version = "0.1"

extensions = []
templates_path = []
exclude_patterns = ["_build"]

# -- HTML -------------------------------------------------------------------
html_theme = "alabaster"
html_static_path = []
html_title = "shoestring-wm"

# -- Man pages --------------------------------------------------------------
# (source, name, description, authors, section)
man_pages = [
    ("man/shoestring-wm.1",     "shoestring-wm",     "lightweight Wayland window manager",      [author], 1),
    ("man/shoestring-wm.5",     "shoestring-wm",     "shoestring-wm configuration file format", [author], 5),
    ("man/shoestring-bar.1",    "shoestring-bar",    "status bar for shoestring-wm",            [author], 1),
    ("man/shoestring-menu.1",   "shoestring-menu",   "dmenu-style launcher for shoestring-wm",  [author], 1),
    ("man/shoestring-ctl.1",    "shoestring-ctl",    "reference IPC client for shoestring-wm",  [author], 1),
]
man_show_urls = True
