use smithay::{
    backend::renderer::utils::on_commit_buffer_handler,
    desktop::Window,
    reexports::wayland_server::{
        protocol::{wl_buffer, wl_surface::WlSurface},
        Client, Resource,
    },
    wayland::{
        buffer::BufferHandler,
        compositor::{
            get_parent, is_sync_subsurface, with_states, CompositorClientState, CompositorHandler,
            CompositorState,
        },
        shell::xdg::XdgToplevelSurfaceData,
        shm::{ShmHandler, ShmState},
    },
};

use crate::{
    grabs,
    handlers::{layer_shell, xdg_shell},
    state::{ClientState, ShoestringWm},
};

impl CompositorHandler for ShoestringWm {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        // Two ClientData shapes carry per-client compositor state: our
        // own ClientState (regular wayland clients) and smithay's
        // XWaylandClientData (the Xwayland subprocess). Branch on which
        // one the client was created with — panicking would take the
        // whole compositor down on the first Xwayland commit.
        if let Some(state) = client.get_data::<ClientState>() {
            return &state.compositor_state;
        }
        if let Some(state) = client.get_data::<smithay::xwayland::XWaylandClientData>() {
            return &state.compositor_state;
        }
        panic!("client connected with unknown ClientData type");
    }

    fn new_surface(&mut self, surface: &WlSurface) {
        // Per-client resource attribution: bump the owning client's live
        // `wl_surface` count so a leak report can name the offender. The
        // owner name is memoised in ClientState, so the `/proc` lookup runs
        // at most once per client. See [`crate::metrics`].
        if let Some(client) = surface.client() {
            let owner = self.client_metrics_name(&client);
            self.metrics.track_surface(surface.id(), owner);
        }
    }

    fn destroyed(&mut self, surface: &WlSurface) {
        // Symmetric with `new_surface`: decrement the owning client's count.
        // Keyed by object id (not the client) so it balances even when the
        // dying surface's `client()` no longer resolves.
        self.metrics.untrack_surface(&surface.id());
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
        if !is_sync_subsurface(surface) {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            // Any mapped window kind (xdg or x11) might match: x11 surfaces
            // sit in the same Space and their commits land here too.
            let window = self
                .space
                .elements()
                .find(|w| crate::window_ext::matches_surface(w, &root))
                .cloned();
            if let Some(window) = window {
                window.on_commit();
                // Push any title/app_id changes the client just committed
                // out to ext-foreign-toplevel-list subscribers (bars, etc.).
                sync_foreign_toplevel(self, &window);
                // Per-app rules run once, on the first commit after map.
                self.try_apply_window_rules(&window);
                // First commit with real geometry — place the window
                // centered now that we know its actual size. Rules that
                // set an explicit position will have already cleared
                // the pending flag.
                self.try_recenter_pending(&window);
            }
        }

        layer_shell::handle_commit(self, surface);
        xdg_shell::handle_commit(&mut self.popups, &self.space, surface);
        grabs::resize_handle_commit(&mut self.space, surface);
        // Lock surfaces commit a buffer once they've acked our configure;
        // that's our signal to confirm the lock to the client.
        self.maybe_confirm_lock();
    }
}

impl BufferHandler for ShoestringWm {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for ShoestringWm {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

/// Read the toplevel's freshly-committed title/app_id and forward changes to
/// its ext-foreign-toplevel handle so bars and task switchers update live.
fn sync_foreign_toplevel(state: &mut ShoestringWm, window: &Window) {
    let Some(handle) = state.foreign_toplevels.get(window).cloned() else {
        return;
    };
    let Some(surface) = window.toplevel().map(|t| t.wl_surface().clone()) else {
        return;
    };
    let (title, app_id) = with_states(&surface, |states| {
        let attrs = states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .unwrap()
            .lock()
            .unwrap();
        (attrs.title.clone(), attrs.app_id.clone())
    });
    let mut changed = false;
    if let Some(title) = title.as_deref() {
        if handle.title() != title {
            handle.send_title(title);
            changed = true;
        }
    }
    if let Some(app_id) = app_id.as_deref() {
        if handle.app_id() != app_id {
            handle.send_app_id(app_id);
            changed = true;
        }
    }
    if changed {
        handle.send_done();
        // wlr-foreign-toplevel taskbars (above) see the raw client title; the
        // WM's own IPC consumers (bar, window-jump menu) see the *effective*
        // title, so an IPC-set name override isn't clobbered when the client
        // later changes its own title.
        let (effective_title, _) = crate::ipc::effective_title_app_id(state, window);
        state.emit_ipc(shoestring_ipc::Event::WindowTitleChanged {
            id: handle.identifier(),
            title: effective_title,
            app_id: app_id.unwrap_or_default(),
        });
    }
}
