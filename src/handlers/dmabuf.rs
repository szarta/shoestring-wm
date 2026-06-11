//! `zwp_linux_dmabuf_v1` handler. The global itself is created lazily in the
//! udev backend (see [`crate::backend::udev`]) once a GPU reports its render
//! formats; this just wires the [`DmabufHandler`] callbacks. Dispatch is
//! covered by the blanket `delegate_dispatch2!(ShoestringWm)` — the dmabuf
//! protocol objects use Smithay's `Dispatch2`/`GlobalDispatch2` traits.

use smithay::{
    backend::allocator::dmabuf::Dmabuf,
    wayland::dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier},
};

use crate::state::ShoestringWm;

impl DmabufHandler for ShoestringWm {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        // Confirm our renderer can actually import the buffer before letting
        // the client create a wl_buffer from it. A buffer that fails here
        // would only blow up later at use time with a less helpful error.
        if self.import_dmabuf_test(&dmabuf) {
            let _ = notifier.successful::<ShoestringWm>();
        } else {
            notifier.failed();
        }
    }
}
