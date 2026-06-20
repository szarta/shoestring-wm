//! `xdg_foreign_v2` — cross-process window parenting.
//!
//! The protocol has two halves. A client `export`s one of its toplevels
//! through `zxdg_exporter_v2`, getting back an opaque string *handle*. It
//! passes that handle to a *different process* by some out-of-band channel
//! (env var, D-Bus, command line). The second client feeds the handle to
//! `zxdg_importer_v2.import_toplevel` and can then `set_parent_of` to make
//! one of *its* toplevels a child of the exported (foreign) surface.
//!
//! The motivating consumer is `xdg-desktop-portal`: a file-chooser, print,
//! or screenshot dialog actually runs in the portal process, not the app
//! that triggered it, so without xdg-foreign it can't establish the
//! parent/child link a same-process dialog gets for free via
//! `xdg_toplevel.set_parent`. Wiring this up is what stops portals (and
//! toolkits that probe for the globals) from logging "compositor doesn't
//! support xdg-foreign" and falling back to an unparented dialog.
//!
//! There's almost nothing to do here: smithay implements the entire
//! protocol — handle minting, the export/import bookkeeping, handle
//! revocation when either side goes away, and the validity checks on
//! `set_parent_of` — and rides our blanket `delegate_dispatch2!`. When a
//! cross-process parent is set it writes the parent into the child's
//! `XdgToplevelSurfaceData` and calls `XdgShellHandler::parent_changed`,
//! exactly as a same-client `set_parent` would. We deliberately do *not*
//! override `parent_changed`: shoestring floats and centers dialogs
//! regardless of parentage, so cross-process parenting needs no special
//! layout handling beyond what same-process parenting already (doesn't) get
//! — keeping the two paths consistent. The only thing this module supplies
//! is the handler glue that hands smithay back its [`XdgForeignState`].

use smithay::wayland::xdg_foreign::{XdgForeignHandler, XdgForeignState};

use crate::state::ShoestringWm;

impl XdgForeignHandler for ShoestringWm {
    fn xdg_foreign_state(&mut self) -> &mut XdgForeignState {
        &mut self.xdg_foreign_state
    }
}
