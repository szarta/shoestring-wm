use smithay::{
    backend::renderer::utils::on_commit_buffer_handler,
    desktop::Window,
    reexports::wayland_server::{
        Client,
        protocol::{wl_buffer, wl_surface::WlSurface},
    },
    wayland::{
        buffer::BufferHandler,
        compositor::{
            CompositorClientState, CompositorHandler, CompositorState, get_parent,
            is_sync_subsurface, with_states,
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
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
        if !is_sync_subsurface(surface) {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            let window = self
                .space
                .elements()
                .find(|w| w.toplevel().unwrap().wl_surface() == &root)
                .cloned();
            if let Some(window) = window {
                window.on_commit();
                // Push any title/app_id changes the client just committed
                // out to ext-foreign-toplevel-list subscribers (bars, etc.).
                sync_foreign_toplevel(self, &window);
            }
        }

        layer_shell::handle_commit(self, surface);
        xdg_shell::handle_commit(&mut self.popups, &self.space, surface);
        grabs::resize_handle_commit(&mut self.space, surface);
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
    }
}
