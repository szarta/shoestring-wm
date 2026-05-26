//! Config hot-reload: watch the user's TOML file via `notify`, debounce
//! rapid bursts (editors do write-truncate-rename sequences that fire
//! several events in milliseconds), and re-read the config when the dust
//! settles.
//!
//! The notify watcher runs on its own thread (that's what
//! `RecommendedWatcher` does internally); we bridge into the WM's
//! single-threaded calloop via `calloop::channel`. Every forwarded event
//! schedules — or re-schedules — a `Timer` that fires once after the
//! debounce window with no new events.
//!
//! On reload, we recompile the binding table and swap both the config and
//! the table atomically (the lookup path borrows neither, so a single
//! assignment is enough). An [`Event::ConfigReloaded`] is broadcast so
//! subscribers (e.g. the bar) can refresh anything derived from config.
//!
//! `[[window_rules]]` apply once on first commit after map (see
//! [`crate::window_rules`]); reload does *not* re-evaluate them against
//! already-mapped windows, matching the part-1 semantics.

use std::{path::PathBuf, time::Duration};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use smithay::reexports::calloop::{
    channel,
    timer::{TimeoutAction, Timer},
    LoopHandle,
};

use crate::state::ShoestringWm;

/// Time the watcher waits with no new filesystem events before firing the
/// reload. Long enough to coalesce an editor's write-truncate-rename burst,
/// short enough that a manual `:w` feels instant.
const DEBOUNCE: Duration = Duration::from_millis(150);

/// Held on `ShoestringWm` for the lifetime of the watcher; dropping the
/// `RecommendedWatcher` unregisters the inotify watch.
pub struct ConfigWatcher {
    #[allow(dead_code)]
    watcher: RecommendedWatcher,
}

impl ShoestringWm {
    /// Start watching `self.config_path` for changes. No-op (with a debug
    /// log) when the WM was launched without a config file. Failure is
    /// non-fatal — we log and continue without hot-reload rather than
    /// taking down the compositor.
    pub fn start_config_watcher(&mut self) {
        let Some(config_path) = self.config_path.clone() else {
            tracing::debug!("no config file in use; hot-reload disabled");
            return;
        };
        match install_watcher(config_path.clone(), self.loop_handle.clone()) {
            Ok(watcher) => {
                self.config_watcher = Some(ConfigWatcher { watcher });
                tracing::info!(path = %config_path.display(), "config hot-reload watcher started");
            }
            Err(e) => tracing::warn!(
                path = %config_path.display(),
                error = %e,
                "config watcher failed; hot-reload disabled",
            ),
        }
    }

    /// Re-read the config from `self.config_path`, recompile the binding
    /// table, and swap both. Emits [`Event::ConfigReloaded`] on success.
    /// Shared by the [`Action::ReloadConfig`] keybind path, the
    /// file-watcher's debounce timer, and `Request::ReloadConfig` from
    /// IPC. Internal logging happens here so non-IPC callers don't need
    /// to handle the result; the returned `Err` lets the IPC handler
    /// surface failure to the caller as `Response::Error`.
    pub fn reload_config_from_disk(&mut self) -> anyhow::Result<()> {
        let Some(path) = self.config_path.clone() else {
            tracing::warn!("config reload requested but no config file is in use");
            anyhow::bail!("no config file is in use (WM was launched without --config)");
        };
        match shoestring_config::load_from(&path) {
            Ok(cfg) => {
                let (table, warnings) = crate::binds::BindingTable::compile(&cfg);
                for w in warnings {
                    tracing::warn!(target: "shoestring_wm::config", "{w}");
                }
                self.config = cfg;
                self.bindings = table;
                tracing::info!(path = %path.display(), "config reloaded");
                self.emit_ipc(shoestring_ipc::Event::ConfigReloaded);
                Ok(())
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "config reload failed");
                Err(anyhow::anyhow!("{}: {e}", path.display()))
            }
        }
    }

    /// Notify-thread callback path: a filesystem event arrived. (Re-)arm
    /// the debounce timer; the previous one is removed so an unbroken
    /// stream of events keeps pushing the trigger further into the future.
    pub(crate) fn debounce_config_reload(&mut self) {
        if let Some(t) = self.pending_reload_token.take() {
            self.loop_handle.remove(t);
        }
        let timer = Timer::from_duration(DEBOUNCE);
        match self.loop_handle.insert_source(timer, |_, _, state| {
            state.pending_reload_token = None;
            let _ = state.reload_config_from_disk();
            TimeoutAction::Drop
        }) {
            Ok(token) => self.pending_reload_token = Some(token),
            Err(e) => tracing::warn!(error = %e, "insert config-reload debounce timer failed"),
        }
    }
}

fn install_watcher(
    config_path: PathBuf,
    loop_handle: LoopHandle<'static, ShoestringWm>,
) -> anyhow::Result<RecommendedWatcher> {
    // Watch the parent directory rather than the file itself: atomic-write
    // editors (vim, helix) replace the inode via rename, which detaches a
    // watch attached to the original. Filtering by file name on the
    // forwarded event re-narrows to just the config file.
    let parent = config_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("config path has no parent: {}", config_path.display()))?
        .to_path_buf();
    let file_name = config_path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("config path has no file name: {}", config_path.display()))?
        .to_owned();

    let (tx, rx) = channel::channel::<()>();

    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let event = match res {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, "config watcher error");
                return;
            }
        };
        // Only the mutating event kinds; ignore Access (read) and metadata-
        // only events that don't change file contents.
        if !matches!(
            event.kind,
            EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
        ) {
            return;
        }
        if !event
            .paths
            .iter()
            .any(|p| p.file_name() == Some(file_name.as_os_str()))
        {
            return;
        }
        // Send failure means the receiver (and the WM) is gone, in which
        // case the watcher is about to be dropped too — nothing to log.
        let _ = tx.send(());
    })?;

    watcher.watch(&parent, RecursiveMode::NonRecursive)?;

    loop_handle
        .insert_source(rx, |evt, _, state| {
            if matches!(evt, channel::Event::Msg(())) {
                state.debounce_config_reload();
            }
        })
        .map_err(|e| anyhow::anyhow!("insert config-watcher channel source: {e}"))?;

    Ok(watcher)
}
