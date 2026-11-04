//! herdr-core terminals as panes: the connection's news applied to app state,
//! core panes attached and kept in step with their terminal, and opening one.
//!
//! The core owns the terminal, its life and its status; Herdr only shows and
//! types. Herdr never starts a core and never re-runs a terminal's program.

use std::collections::HashMap;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::app::actions::PaneStateUpdate;
use crate::app::App;
use crate::core_client::connection::{ConnectionEvent, CoreConnectionConfig, CoreConnectionHandle};
use crate::core_client::model::{
    agent_label, presentation, CoreChangeKind, CoreConnectionState, CoreMode, CorePaneBinding,
    CorePanePhase, CoreTerminalKey, CoreView,
};
use crate::core_client::protocol::{Notification, RpcError, Terminal};
use crate::detect::AgentState;
use crate::events::AppEvent;
use crate::layout::PaneId;
use crate::pane::core_stream::CoreAttachSpec;
use crate::terminal::TerminalId;

pub(crate) const CORE_SOURCE: &str = "herdr:core";
const NOTIFY_REPEAT: std::time::Duration = std::time::Duration::from_secs(30);

pub(crate) struct CoreRuntime {
    pub(crate) connection: Option<CoreConnectionHandle>,
    pub(crate) config: crate::config::CoreConfig,
    started_with: Option<crate::config::CoreConfig>,
    /// Unplaced terminals already notified about, by path, and when.
    notified: HashMap<String, Instant>,
    /// Core runtimes by terminal, so the per-loop visibility sync stays
    /// O(core panes) instead of walking every pane.
    runtime_panes: HashMap<TerminalId, PaneId>,
}

impl CoreRuntime {
    pub(crate) fn new(config: crate::config::CoreConfig) -> Self {
        Self {
            connection: None,
            config,
            started_with: None,
            notified: HashMap::new(),
            runtime_panes: HashMap::new(),
        }
    }
}

impl Drop for CoreRuntime {
    fn drop(&mut self) {
        if let Some(connection) = self.connection.take() {
            connection.shutdown();
        }
    }
}

/// Where a core pane is, found from its terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CorePaneLocation {
    pub ws_idx: usize,
    pub tab_idx: usize,
    pub pane_id: PaneId,
    pub terminal_id: TerminalId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CorePlacement {
    Existing,
    Tab,
    Split,
}

#[derive(Debug, Clone)]
pub(crate) struct CoreOpenRequest {
    pub endpoint_core_id: Option<String>,
    pub path: String,
    pub mode: CoreMode,
    pub placement: CorePlacement,
    pub target: Option<(usize, PaneId)>,
    pub direction: ratatui::layout::Direction,
    pub focus: bool,
    pub close_popup: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CoreOpenResult {
    pub ws_idx: usize,
    pub pane_id: PaneId,
    pub created: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CoreError {
    pub code: &'static str,
    pub message: String,
}

impl CoreError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// The core socket Herdr connects to: the configured path, else the core's
/// own rules for a session. Herdr's environment (`HC_*`) and the core's
/// config file are not read, so a Herdr started inside an hc terminal does
/// not follow that terminal's socket.
pub(crate) fn resolve_socket(config: &crate::config::CoreConfig) -> std::path::PathBuf {
    if let Some(socket) = config.socket.as_deref().filter(|socket| !socket.is_empty()) {
        return crate::worktree::expand_tilde_path(socket);
    }
    let session = if config.session.is_empty() {
        "default"
    } else {
        config.session.as_str()
    };
    let file = format!("{session}.sock");
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").filter(|dir| !dir.is_empty()) {
        return std::path::PathBuf::from(runtime)
            .join("herdr-core")
            .join(file);
    }
    #[cfg(unix)]
    {
        let run_user = std::path::PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() }));
        if run_user.is_dir() {
            return run_user.join("herdr-core").join(file);
        }
    }
    let state = std::env::var_os("XDG_STATE_HOME")
        .filter(|dir| !dir.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            crate::worktree::expand_tilde_path("~")
                .join(".local")
                .join("state")
        });
    state.join("herdr-core").join(session).join(file)
}

/// Whether two configs reach the core the same way. Only these settings
/// restart the connection; the others apply to what comes next.
fn same_connection(a: &crate::config::CoreConfig, b: &crate::config::CoreConfig) -> bool {
    a.enabled == b.enabled
        && a.session == b.session
        && a.socket == b.socket
        && a.reconnect_max_seconds == b.reconnect_max_seconds
}

impl App {
    /// Starts, restarts or stops the core connection to match the config.
    pub(crate) fn apply_core_config(&mut self, config: &crate::config::CoreConfig) {
        self.core.config = config.clone();
        if self
            .core
            .started_with
            .as_ref()
            .is_some_and(|started| same_connection(started, config))
        {
            return;
        }
        self.leave_core_connection();
        self.core.started_with = Some(config.clone());
        self.state.core = CoreView::default();
        self.start_core_connection(config);
        for location in self.core_pane_locations() {
            self.reflect_core_pane(&location, true);
        }
        self.state.mark_core_changed();
    }

    /// Detaches every core pane from the connection, then ends it. Dropping a
    /// pane's runtime closes its streams on that connection; the pane waits
    /// (interrupted) until the next connection checks its key and attaches it
    /// again. Nothing runs again.
    fn leave_core_connection(&mut self) {
        for location in self.core_pane_locations() {
            if self
                .terminal_runtimes
                .get(&location.terminal_id)
                .is_some_and(|runtime| runtime.is_core())
            {
                self.terminal_runtimes.remove(&location.terminal_id);
            }
            if let Some(binding) = self.core_binding_mut(&location.terminal_id) {
                if !matches!(binding.phase, CorePanePhase::Ended(_)) {
                    binding.phase = CorePanePhase::Interrupted;
                }
            }
        }
        self.core.runtime_panes.clear();
        if let Some(connection) = self.core.connection.take() {
            connection.shutdown();
        }
    }

    fn start_core_connection(&mut self, config: &crate::config::CoreConfig) {
        if !config.enabled {
            self.state.core.state = CoreConnectionState::Disabled;
            return;
        }
        if cfg!(windows) {
            self.state.core.state = CoreConnectionState::Unsupported;
            return;
        }
        let socket = resolve_socket(config);
        self.state.core.endpoint = Some(socket.clone());
        self.state.core.state = CoreConnectionState::Connecting;
        let events = self.event_tx.clone();
        let id = crate::core_client::connection::next_connection_id();
        let started = CoreConnectionHandle::start_as(
            id,
            CoreConnectionConfig {
                socket,
                reconnect_max: std::time::Duration::from_secs(config.reconnect_max_seconds.max(1)),
            },
            Box::new(move |event| {
                let _ = events.blocking_send(AppEvent::Core {
                    connection: id,
                    event: Box::new(event),
                });
            }),
        );
        match started {
            Ok(connection) => self.core.connection = Some(connection),
            Err(err) => {
                self.state.core.state = CoreConnectionState::Disconnected {
                    error: err.to_string(),
                }
            }
        }
    }

    pub(crate) fn is_core_event(event: &AppEvent) -> bool {
        matches!(
            event,
            AppEvent::Core { .. }
                | AppEvent::CorePaneAttached { .. }
                | AppEvent::CorePaneStreamEnded { .. }
                | AppEvent::CoreNotify { .. }
        )
    }

    /// Core events the app handles before plain state does. News an ended
    /// connection still had queued is dropped: its panes already left it.
    pub(crate) fn handle_core_event(&mut self, event: AppEvent) -> Vec<PaneStateUpdate> {
        let current = self.core.connection.as_ref().map(CoreConnectionHandle::id);
        match event {
            AppEvent::Core { connection, event } if Some(connection) == current => {
                self.handle_core_connection_event(*event)
            }
            AppEvent::CorePaneAttached {
                connection,
                pane_id,
                result,
            } if Some(connection) == current => self.handle_core_pane_attached(pane_id, *result),
            AppEvent::CorePaneStreamEnded {
                connection,
                pane_id,
                reason,
                interrupted,
            } if Some(connection) == current => {
                self.handle_core_stream_ended(pane_id, reason, interrupted)
            }
            AppEvent::Core { .. }
            | AppEvent::CorePaneAttached { .. }
            | AppEvent::CorePaneStreamEnded { .. } => {
                tracing::debug!("dropped news of an ended core connection");
            }
            _ => {}
        }
        Vec::new()
    }

    pub(crate) fn core_pane_locations(&self) -> Vec<CorePaneLocation> {
        let mut found = Vec::new();
        for (ws_idx, workspace) in self.state.workspaces.iter().enumerate() {
            for (tab_idx, tab) in workspace.tabs.iter().enumerate() {
                for (pane_id, pane) in &tab.panes {
                    let is_core = self
                        .state
                        .terminals
                        .get(&pane.attached_terminal_id)
                        .is_some_and(|terminal| terminal.core.is_some());
                    if is_core {
                        found.push(CorePaneLocation {
                            ws_idx,
                            tab_idx,
                            pane_id: *pane_id,
                            terminal_id: pane.attached_terminal_id.clone(),
                        });
                    }
                }
            }
        }
        found
    }

    fn core_binding(&self, terminal_id: &TerminalId) -> Option<&CorePaneBinding> {
        self.state.terminals.get(terminal_id)?.core.as_ref()
    }

    fn core_binding_mut(&mut self, terminal_id: &TerminalId) -> Option<&mut CorePaneBinding> {
        self.state.terminals.get_mut(terminal_id)?.core.as_mut()
    }

    fn core_location_for_pane(&self, pane_id: PaneId) -> Option<CorePaneLocation> {
        self.core_pane_locations()
            .into_iter()
            .find(|location| location.pane_id == pane_id)
    }

    fn handle_core_connection_event(&mut self, event: ConnectionEvent) {
        match event {
            ConnectionEvent::Connecting => {
                // Machine names map to cores per connection: a remote key
                // waits for this connection's list, never the last one's.
                self.state.core.machines.clear();
                self.state.core.machines_known = false;
                if !matches!(
                    self.state.core.state,
                    CoreConnectionState::Disconnected { .. }
                ) {
                    self.state.core.state = CoreConnectionState::Connecting;
                }
            }
            ConnectionEvent::Connected {
                hello,
                seq,
                terminals,
            } => {
                self.state.core.hello = Some(hello);
                self.state.core.state = CoreConnectionState::Connected;
                self.state.core.clear_terminals();
                self.state.core.seq = 0;
                self.state.core.apply_snapshot(seq, terminals);
                self.reconcile_core_panes(true);
            }
            ConnectionEvent::Status(status) => self.state.core.server = Some(status),
            ConnectionEvent::Machines(machines) => {
                self.state.core.machines = machines;
                self.state.core.machines_known = true;
                self.reconcile_core_panes(false);
            }
            ConnectionEvent::Notification(Notification::Event {
                seq,
                kind,
                terminal,
                data,
                ..
            }) => {
                let before = terminal
                    .as_deref()
                    .and_then(|path| self.state.core.terminal(path))
                    .map(presentation);
                let change =
                    self.state
                        .core
                        .apply_event(seq, &kind, terminal.as_deref(), &data, now_ms());
                if let Some(change) = change {
                    self.handle_core_change(change.path, change.kind, before);
                }
            }
            ConnectionEvent::Notification(Notification::Missed { seq, terminals }) => {
                self.state.core.apply_snapshot(seq, terminals);
                self.resync_core_grids();
                self.reconcile_core_panes(false);
            }
            ConnectionEvent::Notification(_) => {}
            ConnectionEvent::Disconnected { error } => {
                self.state.core.state = CoreConnectionState::Disconnected { error };
                for location in self.core_pane_locations() {
                    if let Some(binding) = self.core_binding_mut(&location.terminal_id) {
                        if !matches!(binding.phase, CorePanePhase::Ended(_)) {
                            binding.phase = CorePanePhase::Interrupted;
                        }
                    }
                    self.reflect_core_pane(&location, true);
                }
            }
        }
        self.state.mark_core_changed();
    }

    /// A snapshot may carry a grid whose `size` event was missed. A render
    /// stream without in-stream RESIZE (0.1.16) is attached again to follow
    /// it; the control stream stays.
    fn resync_core_grids(&self) {
        for terminal_id in self.core.runtime_panes.keys() {
            let Some(binding) = self.core_binding(terminal_id) else {
                continue;
            };
            if binding.phase != CorePanePhase::Attached {
                continue;
            }
            let (Some(record), Some(runtime)) = (
                self.state.core.terminal(&binding.path),
                self.terminal_runtimes.get(terminal_id),
            ) else {
                continue;
            };
            runtime.core_grid_hint(record.cols, record.rows);
        }
    }

    fn handle_core_change(
        &mut self,
        path: String,
        kind: CoreChangeKind,
        before: Option<crate::core_client::model::CorePresentation>,
    ) {
        let panes: Vec<CorePaneLocation> = self
            .core_pane_locations()
            .into_iter()
            .filter(|location| {
                self.core_binding(&location.terminal_id)
                    .is_some_and(|binding| binding.path == path)
            })
            .collect();
        if let Some(machine) = crate::core_client::model::split_path(&path).0 {
            if !self.state.core.owns_machine(machine) {
                if let Some(connection) = &self.core.connection {
                    connection.refresh_machines();
                }
            }
        }
        for location in &panes {
            let phase = self
                .core_binding(&location.terminal_id)
                .map(|binding| binding.phase.clone());
            match kind {
                CoreChangeKind::Size => {
                    if let Some(runtime) = self.terminal_runtimes.get(&location.terminal_id) {
                        runtime.core_size_hint();
                    }
                }
                CoreChangeKind::Replaced | CoreChangeKind::Created => {
                    if matches!(phase, Some(CorePanePhase::Lost | CorePanePhase::Closing)) {
                        self.attach_core_pane(location, true);
                    }
                }
                CoreChangeKind::Finished | CoreChangeKind::Removed => {
                    if !matches!(phase, Some(CorePanePhase::Attached))
                        || kind == CoreChangeKind::Removed
                    {
                        let label = self.core_end_label(&path);
                        if let Some(binding) = self.core_binding_mut(&location.terminal_id) {
                            binding.phase = CorePanePhase::Ended(label);
                        }
                    }
                }
                CoreChangeKind::Lost => {
                    if let Some(binding) = self.core_binding_mut(&location.terminal_id) {
                        if matches!(
                            binding.phase,
                            CorePanePhase::Closing | CorePanePhase::Attached
                        ) {
                            binding.phase = CorePanePhase::Lost;
                        }
                    }
                }
                CoreChangeKind::Status | CoreChangeKind::Other => {}
            }
            if matches!(
                kind,
                CoreChangeKind::Other | CoreChangeKind::Created | CoreChangeKind::Replaced
            ) {
                self.sync_core_cwd(location, &path);
            }
            self.reflect_core_pane(location, false);
        }
        if panes.is_empty() {
            self.notify_unplaced_core_terminal(&path, kind, before);
        }
    }

    fn core_end_label(&self, path: &str) -> String {
        self.state
            .core
            .terminal(path)
            .map(|record| presentation(record).label)
            .unwrap_or_else(|| "removed".into())
    }

    /// A local terminal's folder becomes the pane's cwd (new panes and
    /// worktree actions follow it). Remote folders never leave the record.
    fn sync_core_cwd(&mut self, location: &CorePaneLocation, path: &str) {
        if crate::core_client::model::split_path(path).0.is_some() {
            return;
        }
        let Some(record) = self.state.core.terminal(path) else {
            return;
        };
        let cwd = record
            .foreground
            .as_ref()
            .map(|foreground| foreground.cwd.clone())
            .filter(|cwd| !cwd.is_empty())
            .unwrap_or_else(|| record.cwd.clone());
        let cwd = std::path::PathBuf::from(cwd);
        if !cwd.is_absolute() || !cwd.is_dir() {
            return;
        }
        if let Some(runtime) = self.terminal_runtimes.get(&location.terminal_id) {
            runtime.set_core_reported_cwd(Some(cwd.clone()));
        }
        if let Some(terminal) = self.state.terminals.get_mut(&location.terminal_id) {
            terminal.cwd = cwd;
        }
    }

    fn notify_unplaced_core_terminal(
        &mut self,
        path: &str,
        kind: CoreChangeKind,
        before: Option<crate::core_client::model::CorePresentation>,
    ) {
        let Some(record) = self.state.core.terminal(path) else {
            return;
        };
        let now = presentation(record);
        let notify = &self.core.config.notify;
        let wanted = match kind {
            CoreChangeKind::Status => {
                let entered = now.attention && !before.as_ref().is_some_and(|b| b.attention);
                let is_error = now.label == "error";
                entered && ((is_error && notify.error) || (!is_error && notify.blocked))
            }
            CoreChangeKind::Finished => notify.finished,
            _ => false,
        };
        if !wanted {
            return;
        }
        let instant = Instant::now();
        if self.core.notified.get(path).is_some_and(|at| {
            instant.duration_since(*at) < NOTIFY_REPEAT && kind == CoreChangeKind::Status
        }) {
            return;
        }
        self.core.notified.insert(path.to_string(), instant);
        let name = record.name.clone().unwrap_or_else(|| path.to_string());
        let body = now.message.clone().unwrap_or_default();
        let _ = self.event_tx.try_send(AppEvent::CoreNotify {
            title: format!("hc {name}: {}", now.label),
            body,
        });
    }

    /// Re-checks every core pane against the current connection. With
    /// `full`, every pane's state is reported again and attaches are retried.
    fn reconcile_core_panes(&mut self, full: bool) {
        let runtimes = &self.terminal_runtimes;
        self.core
            .runtime_panes
            .retain(|terminal_id, _| runtimes.get(terminal_id).is_some_and(|rt| rt.is_core()));
        let connected = self.state.core.state == CoreConnectionState::Connected;
        for location in self.core_pane_locations() {
            let Some(binding) = self.core_binding(&location.terminal_id).cloned() else {
                continue;
            };
            if connected {
                match self.state.core.path_for_key(&binding.key) {
                    None => {
                        let remote = self.state.core.endpoint_core_id()
                            != Some(binding.key.origin_core_id.as_str());
                        // A remote key waits for this connection's machine list.
                        if !remote || self.state.core.machines_known {
                            if let Some(binding) = self.core_binding_mut(&location.terminal_id) {
                                binding.phase = CorePanePhase::Mismatch(format!(
                                    "{} is not a terminal of this core (state wiped or machine gone); open it again from the list",
                                    binding.path
                                ));
                            }
                        }
                    }
                    Some(path) => {
                        let record_live = self
                            .state
                            .core
                            .terminal(&path)
                            .map(|record| (record.state == "live", record.reason.is_none()));
                        let needs_attach = match binding.phase {
                            CorePanePhase::Pending
                            | CorePanePhase::Interrupted
                            | CorePanePhase::Mismatch(_) => true,
                            CorePanePhase::Lost | CorePanePhase::Closing => {
                                record_live == Some((true, true))
                            }
                            CorePanePhase::Attached
                            | CorePanePhase::NotFound(_)
                            | CorePanePhase::Ended(_) => false,
                        };
                        if needs_attach {
                            self.attach_core_pane(&location, true);
                        }
                    }
                }
            }
            self.reflect_core_pane(&location, full);
        }
    }

    /// Attaches (or attaches again) a core pane's streams, after checking that
    /// its key still names a terminal of this connection.
    pub(crate) fn attach_core_pane(&mut self, location: &CorePaneLocation, resumed: bool) {
        let Some(connection) = self.core.connection.clone() else {
            return;
        };
        if self.state.core.state != CoreConnectionState::Connected {
            return;
        }
        let Some(binding) = self.core_binding(&location.terminal_id).cloned() else {
            return;
        };
        let Some(path) = self.state.core.path_for_key(&binding.key) else {
            if let Some(binding) = self.core_binding_mut(&location.terminal_id) {
                binding.phase = CorePanePhase::Mismatch(format!(
                    "{} is not a terminal of this core",
                    binding.path
                ));
            }
            return;
        };
        let (rows, cols) = self
            .state
            .new_pane_size(crate::ui::NewPanePlacement::Existing {
                ws_idx: location.ws_idx,
                pane: location.pane_id,
            });
        let sized = rows > 0 && cols > 0;
        let spec = CoreAttachSpec {
            path: path.clone(),
            mode: binding.mode,
            resumed: resumed || !sized,
            history: self.core.config.history_lines,
            display: None,
        };
        if let Some(binding) = self.core_binding_mut(&location.terminal_id) {
            binding.path = path;
            binding.phase = CorePanePhase::Pending;
        }
        if let Some(runtime) = self.terminal_runtimes.get(&location.terminal_id) {
            if runtime.is_core() {
                runtime.core_reattach(spec);
                return;
            }
        }
        let (rows, cols) = if sized { (rows, cols) } else { (24, 80) };
        match crate::terminal::TerminalRuntime::attach_core(
            location.pane_id,
            rows,
            cols,
            self.state.pane_scrollback_limit_bytes,
            self.state.host_terminal_theme,
            self.pane_host_terminal_appearance(),
            connection,
            spec,
            self.event_tx.clone(),
            self.render_notify.clone(),
            self.render_dirty.clone(),
        ) {
            Ok(runtime) => {
                self.terminal_runtimes
                    .insert(location.terminal_id.clone(), runtime);
                self.core
                    .runtime_panes
                    .insert(location.terminal_id.clone(), location.pane_id);
                self.render_dirty.request_generic();
                self.render_notify.notify_one();
            }
            Err(err) => {
                if let Some(binding) = self.core_binding_mut(&location.terminal_id) {
                    binding.phase = CorePanePhase::NotFound(err.to_string());
                }
            }
        }
    }

    fn handle_core_pane_attached(&mut self, pane_id: PaneId, result: Result<Terminal, RpcError>) {
        let Some(location) = self.core_location_for_pane(pane_id) else {
            return;
        };
        let Some((expected_key, expected)) = self
            .core_binding(&location.terminal_id)
            .map(|binding| (binding.key.clone(), Some(binding.path.clone())))
        else {
            return;
        };
        // The same address and the same terminal identity on this connection:
        // a machine name that now names another core is not the saved pane.
        let same_terminal = |record: &Terminal| {
            expected.as_deref() == Some(record.address.as_str())
                && self.state.core.key_for_path(&record.address).as_ref() == Some(&expected_key)
        };
        let phase = match result {
            Ok(record) if same_terminal(&record) => CorePanePhase::Attached,
            Ok(record) => CorePanePhase::Mismatch(format!(
                "the core answered for {} instead of {}",
                record.address,
                expected.unwrap_or_default()
            )),
            // A program that already ended keeps its "ended" label after a
            // restart or handoff, rather than reading as missing.
            Err(_)
                if expected
                    .as_deref()
                    .and_then(|path| self.state.core.terminal(path))
                    .is_some_and(|record| record.state == "finished") =>
            {
                CorePanePhase::Ended(self.core_end_label(expected.as_deref().unwrap_or_default()))
            }
            Err(error) => match error.kind.as_deref() {
                Some("disconnected") => CorePanePhase::Interrupted,
                Some("unavailable") => CorePanePhase::Lost,
                _ => CorePanePhase::NotFound(error.message),
            },
        };
        let drop_runtime = matches!(
            phase,
            CorePanePhase::Mismatch(_) | CorePanePhase::NotFound(_)
        );
        if let Some(binding) = self.core_binding_mut(&location.terminal_id) {
            binding.phase = phase;
        }
        if drop_runtime {
            // Detach; the placeholder explains why.
            self.terminal_runtimes.remove(&location.terminal_id);
        }
        self.reflect_core_pane(&location, true);
        self.state.mark_core_changed();
        self.render_dirty.request_generic();
        self.render_notify.notify_one();
    }

    fn handle_core_stream_ended(
        &mut self,
        pane_id: PaneId,
        reason: Option<crate::core_client::protocol::CloseReason>,
        interrupted: bool,
    ) {
        let Some(location) = self.core_location_for_pane(pane_id) else {
            return;
        };
        let path = self
            .core_binding(&location.terminal_id)
            .map(|binding| binding.path.clone())
            .unwrap_or_default();
        // The record, not the close reason, says whether this is the end:
        // kept terminals stay live through a loss (holder_lost, signaled).
        let phase = if interrupted {
            CorePanePhase::Interrupted
        } else {
            match self.state.core.terminal(&path) {
                None => CorePanePhase::Ended(
                    reason
                        .map(|reason| reason.reason)
                        .filter(|reason| !reason.is_empty())
                        .unwrap_or_else(|| "removed".into()),
                ),
                Some(record) if record.state == "finished" => {
                    CorePanePhase::Ended(presentation(record).label)
                }
                Some(record) if record.reason.is_some() => CorePanePhase::Lost,
                Some(_) => CorePanePhase::Closing,
            }
        };
        if let Some(binding) = self.core_binding_mut(&location.terminal_id) {
            binding.phase = phase;
        }
        self.reflect_core_pane(&location, false);
        self.state.mark_core_changed();
    }

    /// Reports a core pane's state into Herdr's agent state through the
    /// internal event channel, so the server's notification forwarding sees
    /// it like any other report.
    pub(crate) fn reflect_core_pane(&mut self, location: &CorePaneLocation, force: bool) {
        let Some(binding) = self.core_binding(&location.terminal_id).cloned() else {
            return;
        };
        let record = self.state.core.terminal(&binding.path).cloned();
        let record = record.as_ref();
        let (state, label, message) = match (&binding.phase, record) {
            (CorePanePhase::Interrupted, _) => {
                (AgentState::Unknown, "core disconnected".to_string(), None)
            }
            (CorePanePhase::Lost, _) => (
                AgentState::Unknown,
                "lost: waiting for shell".to_string(),
                None,
            ),
            (CorePanePhase::Mismatch(reason) | CorePanePhase::NotFound(reason), _) => (
                AgentState::Unknown,
                "core: not found".to_string(),
                Some(reason.clone()),
            ),
            (CorePanePhase::Ended(label), _) => (AgentState::Unknown, label.clone(), None),
            (_, Some(record)) => {
                let shown = presentation(record);
                (shown.state, shown.label, shown.message)
            }
            (_, None) => (AgentState::Unknown, "core connecting".to_string(), None),
        };
        let agent = record
            .map(agent_label)
            .map(|label| {
                crate::app::api_helpers::normalize_reported_agent_label(&label).unwrap_or(label)
            })
            .unwrap_or_else(|| "hc".to_string());
        let reported = (state, label.clone(), message.clone(), agent.clone());
        if !force && binding.reported.as_ref() == Some(&reported) {
            return;
        }
        let state_word = match state {
            AgentState::Idle => "idle",
            AgentState::Working => "working",
            AgentState::Blocked => "blocked",
            AgentState::Unknown => "unknown",
        };
        let display = record
            .and_then(|record| record.name.clone())
            .map(|name| format!("{name} {}", binding.path))
            .unwrap_or_else(|| binding.path.clone());
        let state_sent = self.event_tx.try_send(AppEvent::HookStateReported {
            pane_id: location.pane_id,
            source: CORE_SOURCE.to_string(),
            agent_label: agent.clone(),
            state,
            message: message.or_else(|| Some(label.clone())),
            seq: None,
            session_ref: None,
        });
        let mut state_labels = HashMap::new();
        state_labels.insert(state_word.to_string(), label);
        let metadata_sent = self.event_tx.try_send(AppEvent::HookMetadataReported {
            pane_id: location.pane_id,
            source: CORE_SOURCE.to_string(),
            agent_label: Some(agent),
            applies_to_source: None,
            title: None,
            display_agent: Some(display),
            state_labels,
            clear_title: false,
            clear_display_agent: false,
            clear_state_labels: false,
            seq: None,
            ttl: None,
        });
        // Remember what was shown only once it is queued, so a full event
        // queue is retried on the next change instead of leaving a stale state.
        if state_sent.is_ok() && metadata_sent.is_ok() {
            if let Some(binding) = self.core_binding_mut(&location.terminal_id) {
                binding.reported = Some(reported);
            }
        } else {
            tracing::warn!(pane = ?location.pane_id, "core pane state not queued; will retry");
        }
    }

    /// Why API input to this pane would not reach its core terminal, if it
    /// is a core pane that cannot type now. A person's keys are dropped
    /// quietly; an API caller must not take that drop for a success.
    pub(crate) fn core_input_refusal(&self, ws_idx: usize, pane_id: PaneId) -> Option<CoreError> {
        let terminal_id = self.state.terminal_id_for_pane(ws_idx, pane_id)?;
        let binding = self.core_binding(&terminal_id)?;
        let path = &binding.path;
        if binding.mode == CoreMode::Observe {
            return Some(CoreError::new(
                "pane_observe_only",
                format!("this pane only observes {path}; open it to control it"),
            ));
        }
        let not_ready = |why: &str| {
            Some(CoreError::new(
                "pane_not_ready",
                format!("{path} is not attached ({why}); nothing was typed"),
            ))
        };
        match &binding.phase {
            CorePanePhase::Ended(label) => Some(CoreError::new(
                "pane_ended",
                format!("{path} has ended ({label})"),
            )),
            CorePanePhase::NotFound(reason) | CorePanePhase::Mismatch(reason) => {
                Some(CoreError::new("pane_ended", reason.clone()))
            }
            CorePanePhase::Attached => {
                if self.state.core.state != CoreConnectionState::Connected {
                    return not_ready(self.state.core.state.word());
                }
                let typing = self
                    .terminal_runtimes
                    .get(&terminal_id)
                    .is_some_and(crate::terminal::TerminalRuntime::core_accepts_input);
                if typing {
                    None
                } else {
                    not_ready("control stream pending")
                }
            }
            CorePanePhase::Pending => not_ready("pending"),
            CorePanePhase::Interrupted => not_ready("core disconnected"),
            CorePanePhase::Lost => not_ready("lost: waiting for shell"),
            CorePanePhase::Closing => not_ready("closing"),
        }
    }

    /// Shown to some client now: coming into view takes the size back once.
    /// Runs every loop, so it only touches core runtimes.
    pub(crate) fn sync_core_pane_visibility(&self, visible: &std::collections::HashSet<PaneId>) {
        for (terminal_id, pane_id) in &self.core.runtime_panes {
            if let Some(runtime) = self.terminal_runtimes.get(terminal_id) {
                runtime.set_core_visible(visible.contains(pane_id));
            }
        }
    }

    /// Opens a core terminal in a pane, or focuses the control pane that
    /// already shows it.
    pub(crate) fn open_core_terminal(
        &mut self,
        request: CoreOpenRequest,
    ) -> Result<CoreOpenResult, CoreError> {
        if self.state.core.state != CoreConnectionState::Connected {
            return Err(CoreError::new(
                "core_unavailable",
                format!("herdr-core is {}", self.state.core.state.word()),
            ));
        }
        if let Some(expected) = request.endpoint_core_id.as_deref() {
            if self.state.core.endpoint_core_id() != Some(expected) {
                return Err(CoreError::new(
                    "core_mismatch",
                    format!(
                        "Herdr is connected to core {}, not {expected}",
                        self.state.core.endpoint_core_id().unwrap_or("?")
                    ),
                ));
            }
        }
        let path = self
            .state
            .core
            .terminals()
            .find(|(path, record)| {
                *path == &request.path || record.name.as_deref() == Some(request.path.as_str())
            })
            .map(|(path, _)| path.clone())
            .unwrap_or_else(|| request.path.clone());
        if !self.state.core.owns_path(&path) {
            return Err(CoreError::new(
                "core_mismatch",
                format!("{path} is not a terminal of this core or its machines"),
            ));
        }
        let Some(key) = self.state.core.key_for_path(&path) else {
            return Err(CoreError::new(
                "core_mismatch",
                format!("{path} is not a terminal of this core or its machines"),
            ));
        };
        if request.mode == CoreMode::Control || request.placement == CorePlacement::Existing {
            if let Some(existing) = self.find_core_pane(&key, request.mode) {
                self.focus_core_pane(&existing);
                if request.close_popup {
                    self.close_popup_pane();
                }
                return Ok(CoreOpenResult {
                    ws_idx: existing.ws_idx,
                    pane_id: existing.pane_id,
                    created: false,
                });
            }
        }
        let cwd = self
            .state
            .core
            .terminal(&path)
            .filter(|_| crate::core_client::model::split_path(&path).0.is_none())
            .map(|record| std::path::PathBuf::from(&record.cwd))
            .filter(|cwd| cwd.is_dir())
            .unwrap_or_else(|| crate::worktree::expand_tilde_path("~"));
        let (ws_idx, pane_id, terminal) = self.create_core_pane_slot(&request, cwd)?;
        let mut terminal = terminal;
        terminal.core = Some(CorePaneBinding::new(key, path, request.mode));
        let terminal_id = terminal.id.clone();
        self.state.terminals.insert(terminal_id.clone(), terminal);
        self.state.mark_session_dirty();
        let tab_idx = self.state.workspaces[ws_idx]
            .find_tab_index_for_pane(pane_id)
            .unwrap_or(0);
        let location = CorePaneLocation {
            ws_idx,
            tab_idx,
            pane_id,
            terminal_id,
        };
        if request.focus {
            self.focus_core_pane(&location);
        }
        self.attach_core_pane(&location, !request.focus);
        self.reflect_core_pane(&location, true);
        if request.close_popup {
            self.close_popup_pane();
        }
        Ok(CoreOpenResult {
            ws_idx,
            pane_id,
            created: true,
        })
    }

    fn create_core_pane_slot(
        &mut self,
        request: &CoreOpenRequest,
        cwd: std::path::PathBuf,
    ) -> Result<(usize, PaneId, crate::terminal::TerminalState), CoreError> {
        let target = request.target.or_else(|| {
            let ws_idx = self.state.active?;
            let pane = self.state.workspaces.get(ws_idx)?.focused_pane_id()?;
            Some((ws_idx, pane))
        });
        let Some((ws_idx, target_pane)) = target else {
            return Err(CoreError::new(
                "workspace_not_found",
                "open a workspace first",
            ));
        };
        match request.placement {
            CorePlacement::Split => {
                let result = self.state.workspaces[ws_idx]
                    .split_pane_pending(target_pane, request.direction, None, cwd, request.focus)
                    .ok_or_else(|| CoreError::new("pane_not_found", "split target not found"))?;
                let (_, pane_id, terminal) =
                    result.map_err(|err| CoreError::new("split_failed", err.to_string()))?;
                Ok((ws_idx, pane_id, terminal))
            }
            CorePlacement::Tab | CorePlacement::Existing => {
                let (tab_idx, pane_id, terminal) =
                    self.state.workspaces[ws_idx].create_tab_pending(cwd);
                if request.focus {
                    self.state.workspaces[ws_idx].switch_tab(tab_idx);
                }
                Ok((ws_idx, pane_id, terminal))
            }
        }
    }

    fn find_core_pane(&self, key: &CoreTerminalKey, mode: CoreMode) -> Option<CorePaneLocation> {
        self.core_pane_locations().into_iter().find(|location| {
            self.core_binding(&location.terminal_id)
                .is_some_and(|binding| &binding.key == key && binding.mode == mode)
        })
    }

    fn focus_core_pane(&mut self, location: &CorePaneLocation) {
        self.state
            .focus_pane_in_workspace(location.ws_idx, location.pane_id);
        self.state.mark_active_tab_seen();
        self.state.mode = crate::app::Mode::Terminal;
    }

    /// The next terminal needing a person: its pane if it has one, else a
    /// new pane.
    pub(crate) fn open_next_core_attention(
        &mut self,
        placement: CorePlacement,
        close_popup: bool,
    ) -> Result<Option<CoreOpenResult>, CoreError> {
        let Some(path) = self.state.core.attention().into_iter().next() else {
            return Ok(None);
        };
        self.open_core_terminal(CoreOpenRequest {
            endpoint_core_id: None,
            path,
            mode: CoreMode::Control,
            placement: if placement == CorePlacement::Existing {
                CorePlacement::Tab
            } else {
                placement
            },
            target: None,
            direction: ratatui::layout::Direction::Horizontal,
            focus: true,
            close_popup,
        })
        .map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{
        CoreAttentionAction, CoreAttentionParams, CoreTerminalListParams, CoreTerminalOpenParams,
        CoreTerminalRunParams,
    };
    use crate::core_client::protocol::{CoreInfo, HelloResult, MachineView, Status};

    fn test_app() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state
            .workspaces
            .push(crate::workspace::Workspace::test_new("w"));
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        app
    }

    fn record(path: &str, state: &str, status: Option<(&str, Option<&str>)>) -> Terminal {
        Terminal {
            address: path.into(),
            name: Some(format!("n-{path}")),
            state: state.into(),
            cwd: "/tmp".into(),
            status: status.map(|(word, kind)| Status {
                state: word.into(),
                kind: kind.map(str::to_string),
                app: "claude-code".into(),
                ..Status::default()
            }),
            status_ms: Some(1),
            ..Terminal::default()
        }
    }

    fn connect(app: &mut App, terminals: Vec<Terminal>) {
        app.state.core.state = CoreConnectionState::Connected;
        app.state.core.hello = Some(HelloResult {
            generation: 1,
            core: CoreInfo {
                id: "core-a".into(),
                name: "mbp".into(),
            },
            ..HelloResult::default()
        });
        app.state.core.machines = vec![MachineView {
            name: "rb".into(),
            id: "core-b".into(),
            ..MachineView::default()
        }];
        app.state.core.apply_snapshot(1, terminals);
    }

    fn bind(app: &mut App, path: &str, mode: CoreMode) -> CorePaneLocation {
        let ws = &app.state.workspaces[0];
        let pane_id = ws.tabs[0].root_pane;
        let terminal_id = ws.tabs[0].panes[&pane_id].attached_terminal_id.clone();
        let key = app.state.core.key_for_path(path).unwrap();
        app.state.terminals.get_mut(&terminal_id).unwrap().core =
            Some(CorePaneBinding::new(key, path.into(), mode));
        CorePaneLocation {
            ws_idx: 0,
            tab_idx: 0,
            pane_id,
            terminal_id,
        }
    }

    fn drain(app: &mut App) -> Vec<AppEvent> {
        let mut events = Vec::new();
        while let Ok(event) = app.event_rx.try_recv() {
            events.push(event);
        }
        events
    }

    fn phase(app: &App, location: &CorePaneLocation) -> CorePanePhase {
        app.state.terminals[&location.terminal_id]
            .core
            .as_ref()
            .unwrap()
            .phase
            .clone()
    }

    #[tokio::test]
    async fn core_state_reaches_herdr_through_the_internal_event_channel() {
        let mut app = test_app();
        connect(&mut app, vec![record("t1", "live", Some(("error", None)))]);
        let location = bind(&mut app, "t1", CoreMode::Control);
        drain(&mut app);
        app.reflect_core_pane(&location, true);
        let events = drain(&mut app);
        let state = events.iter().find_map(|event| match event {
            AppEvent::HookStateReported {
                source,
                state,
                agent_label,
                ..
            } => Some((source.clone(), *state, agent_label.clone())),
            _ => None,
        });
        assert_eq!(
            state,
            Some((
                CORE_SOURCE.to_string(),
                AgentState::Blocked,
                "claude".to_string()
            ))
        );
        let labels = events.iter().find_map(|event| match event {
            AppEvent::HookMetadataReported { state_labels, .. } => Some(state_labels.clone()),
            _ => None,
        });
        assert_eq!(labels.unwrap()["blocked"], "error");
        // Unchanged state is not reported again unless forced.
        app.reflect_core_pane(&location, false);
        assert!(drain(&mut app).is_empty());
        app.state.assert_invariants_for_test();
    }

    #[tokio::test]
    async fn a_state_the_full_event_queue_dropped_is_sent_on_the_next_pass() {
        let mut app = test_app();
        connect(
            &mut app,
            vec![record("t1", "live", Some(("working", None)))],
        );
        let location = bind(&mut app, "t1", CoreMode::Control);
        drain(&mut app);
        while app
            .event_tx
            .try_send(AppEvent::CoreNotify {
                title: "fill".into(),
                body: String::new(),
            })
            .is_ok()
        {}
        app.reflect_core_pane(&location, false);
        drain(&mut app);
        // Nothing reached the queue, so the same state is not taken as shown.
        app.reflect_core_pane(&location, false);
        let state = drain(&mut app).into_iter().find_map(|event| match event {
            AppEvent::HookStateReported { state, .. } => Some(state),
            _ => None,
        });
        assert_eq!(state, Some(AgentState::Working));
    }

    #[tokio::test]
    async fn the_record_not_the_close_reason_decides_if_a_terminal_ended() {
        let mut app = test_app();
        let mut lost = record("t1", "live", None);
        lost.reason = Some("holder_lost".into());
        lost.keep = true;
        let mut done = record("t2", "finished", None);
        done.reason = Some("exited".into());
        done.exit_code = Some(3);
        connect(&mut app, vec![lost, done, record("t3", "live", None)]);
        let close = |reason: &str| {
            Some(crate::core_client::protocol::CloseReason {
                reason: reason.into(),
                ..Default::default()
            })
        };
        let location = bind(&mut app, "t1", CoreMode::Control);
        app.handle_core_stream_ended(location.pane_id, close("holder_lost"), false);
        assert_eq!(phase(&app, &location), CorePanePhase::Lost);
        app.handle_core_stream_ended(location.pane_id, close("x"), true);
        assert_eq!(phase(&app, &location), CorePanePhase::Interrupted);

        let location = bind(&mut app, "t2", CoreMode::Control);
        app.handle_core_stream_ended(location.pane_id, close("exited"), false);
        assert_eq!(
            phase(&app, &location),
            CorePanePhase::Ended("exited 3".into())
        );

        let location = bind(&mut app, "t3", CoreMode::Control);
        app.handle_core_stream_ended(location.pane_id, close("signaled"), false);
        assert_eq!(phase(&app, &location), CorePanePhase::Closing);
        // The event that follows decides: a loss waits for `restarted`.
        app.handle_core_connection_event(ConnectionEvent::Notification(Notification::Event {
            seq: 2,
            kind: "lost".into(),
            terminal: Some("t3".into()),
            machine: None,
            data: serde_json::json!({"reason": "signaled"}),
        }));
        assert_eq!(phase(&app, &location), CorePanePhase::Lost);
        // A finished event ends it; the pane stays and nothing runs again.
        app.handle_core_connection_event(ConnectionEvent::Notification(Notification::Event {
            seq: 3,
            kind: "finished".into(),
            terminal: Some("t3".into()),
            machine: None,
            data: serde_json::json!({"reason": "killed"}),
        }));
        assert_eq!(
            phase(&app, &location),
            CorePanePhase::Ended("killed".into())
        );
        assert_eq!(app.state.workspaces[0].tabs.len(), 1);
        app.state.assert_invariants_for_test();
    }

    #[tokio::test]
    async fn a_saved_key_of_another_core_is_not_attached_by_address() {
        let mut app = test_app();
        connect(&mut app, vec![record("t1", "live", None)]);
        let location = bind(&mut app, "t1", CoreMode::Control);
        app.state
            .terminals
            .get_mut(&location.terminal_id)
            .unwrap()
            .core
            .as_mut()
            .unwrap()
            .key = CoreTerminalKey {
            origin_core_id: "wiped".into(),
            origin_address: "t1".into(),
        };
        // The machine list can arrive before `Connected`; either way the pane
        // of another core is marked, not left waiting.
        let machines = app.state.core.machines.clone();
        app.handle_core_connection_event(ConnectionEvent::Machines(machines));
        app.reconcile_core_panes(true);
        assert!(matches!(phase(&app, &location), CorePanePhase::Mismatch(_)));
        let listed = app.core_terminal_infos(&CoreTerminalListParams::default());
        assert!(
            listed.iter().all(|terminal| terminal.panes.is_empty()),
            "{listed:?}"
        );
        assert!(app.terminal_runtimes.get(&location.terminal_id).is_none());
        // A renamed machine is found again by its core id.
        let remote_key = CoreTerminalKey {
            origin_core_id: "core-b".into(),
            origin_address: "t4".into(),
        };
        app.state.core.machines[0].name = "minas".into();
        assert_eq!(
            app.state.core.path_for_key(&remote_key).as_deref(),
            Some("minas/t4")
        );
    }

    #[tokio::test]
    async fn opening_checks_the_connection_the_core_and_the_machine() {
        let mut app = test_app();
        let open = |terminal: &str, endpoint: Option<&str>| CoreTerminalOpenParams {
            endpoint_core_id: endpoint.map(str::to_string),
            terminal: terminal.into(),
            mode: None,
            placement: None,
            target_pane_id: None,
            direction: None,
            focus: None,
            close_popup: false,
        };
        let code = |response: String| {
            serde_json::from_str::<serde_json::Value>(&response).unwrap()["error"]["code"]
                .as_str()
                .unwrap_or("ok")
                .to_string()
        };
        assert_eq!(
            code(app.handle_core_terminal_open("1".into(), open("t1", None))),
            "core_unavailable"
        );
        connect(
            &mut app,
            vec![record("t1", "live", Some(("blocked", Some("permission"))))],
        );
        assert_eq!(
            code(app.handle_core_terminal_open("2".into(), open("t1", Some("core-b")))),
            "core_mismatch"
        );
        assert_eq!(
            code(app.handle_core_terminal_open("3".into(), open("elsewhere/t1", None))),
            "core_mismatch"
        );

        let status = app.core_status_info();
        assert_eq!(
            (status.state.as_str(), status.live, status.attention),
            ("connected", 1, 1)
        );
        let attention = app.handle_core_attention(
            "4".into(),
            CoreAttentionParams {
                action: CoreAttentionAction::Count,
                placement: None,
                close_popup: false,
            },
        );
        let attention: serde_json::Value = serde_json::from_str(&attention).unwrap();
        assert_eq!(attention["result"]["line"], "hc !1");
        let list = app.core_terminal_infos(&CoreTerminalListParams {
            placed: Some(true),
            ..Default::default()
        });
        assert!(list.is_empty());
        let list = app.core_terminal_infos(&CoreTerminalListParams::default());
        assert_eq!(
            (list[0].label.as_str(), list[0].origin_core_id.as_str()),
            ("blocked: permission", "core-a")
        );
    }

    #[tokio::test]
    async fn an_unplaced_terminal_that_gets_blocked_is_notified() {
        let mut app = test_app();
        connect(&mut app, vec![record("t7", "live", None)]);
        drain(&mut app);
        app.handle_core_connection_event(ConnectionEvent::Notification(Notification::Event {
            seq: 2,
            kind: "status".into(),
            terminal: Some("t7".into()),
            machine: None,
            data: serde_json::json!({
                "status": {"state": "blocked", "kind": "permission", "app": "claude-code"},
                "reported": true,
            }),
        }));
        let notes: Vec<String> = drain(&mut app)
            .into_iter()
            .filter_map(|event| match event {
                AppEvent::CoreNotify { title, .. } => Some(title),
                _ => None,
            })
            .collect();
        assert_eq!(notes, ["hc n-t7: blocked: permission"]);
    }

    #[tokio::test]
    async fn a_remote_key_waits_for_this_connections_machine_list() {
        let mut app = test_app();
        connect(&mut app, vec![record("t1", "live", None)]);
        let remote_key = CoreTerminalKey {
            origin_core_id: "core-b".into(),
            origin_address: "t4".into(),
        };
        let location = bind(&mut app, "rb/t4", CoreMode::Control);
        let hello = app.state.core.hello.clone().unwrap();
        app.handle_core_connection_event(ConnectionEvent::Disconnected {
            error: "gone".into(),
        });
        app.handle_core_connection_event(ConnectionEvent::Connecting);
        // The new session's subscribe can answer before its machine.list.
        app.handle_core_connection_event(ConnectionEvent::Connected {
            hello,
            seq: 2,
            terminals: vec![record("rb/t4", "live", None)],
        });
        assert_eq!(app.state.core.path_for_key(&remote_key), None);
        assert_eq!(phase(&app, &location), CorePanePhase::Interrupted);
        // On this connection `rb` is another core: the saved pane is not it.
        app.handle_core_connection_event(ConnectionEvent::Machines(vec![MachineView {
            name: "rb".into(),
            id: "core-c".into(),
            ..MachineView::default()
        }]));
        assert_eq!(app.state.core.path_for_key(&remote_key), None);
        assert!(matches!(phase(&app, &location), CorePanePhase::Mismatch(_)));
    }

    #[tokio::test]
    async fn an_attach_answer_is_taken_only_for_the_same_terminal_identity() {
        let mut app = test_app();
        connect(&mut app, vec![record("rb/t4", "live", None)]);
        let location = bind(&mut app, "rb/t4", CoreMode::Control);
        // Meanwhile `rb` came to name another core with its own `t4`.
        app.state.core.machines[0].id = "core-c".into();
        app.handle_core_pane_attached(location.pane_id, Ok(record("rb/t4", "live", None)));
        assert!(matches!(phase(&app, &location), CorePanePhase::Mismatch(_)));
    }

    #[cfg(unix)]
    async fn pump_until(app: &mut App, what: &str, mut done: impl FnMut(&App) -> bool) {
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        loop {
            while let Ok(event) = app.event_rx.try_recv() {
                if App::is_core_event(&event) {
                    app.handle_core_event(event);
                }
            }
            if done(app) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// Connects `app` to a fake core and opens its `t1` in a control pane.
    #[cfg(unix)]
    async fn open_fake_t1(
        app: &mut App,
        core: &crate::core_client::fake_core::FakeCore,
    ) -> (crate::config::CoreConfig, CorePaneLocation) {
        let config = crate::config::CoreConfig {
            enabled: true,
            socket: Some(core.socket.to_string_lossy().into_owned()),
            ..crate::config::CoreConfig::default()
        };
        app.apply_core_config(&config);
        pump_until(app, "connected", |app| {
            app.state.core.state == CoreConnectionState::Connected
        })
        .await;
        let opened = app
            .open_core_terminal(CoreOpenRequest {
                endpoint_core_id: None,
                path: "t1".into(),
                mode: CoreMode::Control,
                placement: CorePlacement::Tab,
                target: None,
                direction: ratatui::layout::Direction::Horizontal,
                focus: true,
                close_popup: false,
            })
            .unwrap();
        let location = app.core_location_for_pane(opened.pane_id).unwrap();
        pump_until(app, "attached", |app| {
            phase(app, &location) == CorePanePhase::Attached
        })
        .await;
        assert_eq!(core.attach_params().len(), 2);
        (config, location)
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_core_config_reload_keeps_or_moves_core_panes_to_the_new_connection() {
        use crate::core_client::fake_core::FakeCore;
        let first = FakeCore::start("core-a");
        let mut app = test_app();
        let (mut config, location) = open_fake_t1(&mut app, &first).await;
        let old_connection = app.core.connection.as_ref().unwrap().id();

        // Settings that do not pick the core keep the connection and the pane.
        config.notify.finished = !config.notify.finished;
        config.history_lines += 1;
        app.apply_core_config(&config);
        assert_eq!(app.state.core.state, CoreConnectionState::Connected);
        assert!(app.state.core.hello.is_some());
        assert_eq!(phase(&app, &location), CorePanePhase::Attached);
        assert!(app.terminal_runtimes.get(&location.terminal_id).is_some());

        // Another socket: the pane leaves the old connection and attaches on
        // the new one, without running anything again.
        let second = FakeCore::start("core-a");
        config.socket = Some(second.socket.to_string_lossy().into_owned());
        app.apply_core_config(&config);
        assert_eq!(phase(&app, &location), CorePanePhase::Interrupted);
        assert!(app.terminal_runtimes.get(&location.terminal_id).is_none());
        // What the old connection still had queued is not taken for the new one.
        app.handle_core_event(AppEvent::CorePaneAttached {
            connection: old_connection,
            pane_id: location.pane_id,
            result: Box::new(Ok(record("t1", "live", None))),
        });
        app.handle_core_event(AppEvent::Core {
            connection: old_connection,
            event: Box::new(ConnectionEvent::Disconnected {
                error: "old".into(),
            }),
        });
        assert_eq!(phase(&app, &location), CorePanePhase::Interrupted);
        assert_ne!(
            app.state.core.state,
            CoreConnectionState::Disconnected {
                error: "old".into()
            }
        );
        pump_until(&mut app, "attached again", |app| {
            phase(app, &location) == CorePanePhase::Attached
        })
        .await;
        assert_eq!(second.attach_params().len(), 2);
        assert_eq!(first.attach_params().len(), 2);
        assert!(app.terminal_runtimes.get(&location.terminal_id).is_some());
        app.state.assert_invariants_for_test();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_missed_snapshot_with_another_grid_rebuilds_the_render_stream() {
        use crate::core_client::fake_core::FakeCore;
        // 0.1.16: observers get no in-stream RESIZE.
        let core = FakeCore::start_with("core-a", false);
        let mut app = test_app();
        let (_, location) = open_fake_t1(&mut app, &core).await;
        let snapshot = |cols, rows| {
            ConnectionEvent::Notification(Notification::Missed {
                seq: 9,
                terminals: vec![Terminal {
                    cols,
                    rows,
                    ..record("t1", "live", None)
                }],
            })
        };
        // The grid the pane already follows: nothing to do.
        app.handle_core_connection_event(snapshot(80, 24));
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(core.attach_params().len(), 2);
        // A grid whose `size` event was missed: a fresh observer, same control.
        app.handle_core_connection_event(snapshot(100, 30));
        pump_until(&mut app, "render attached again", |_| {
            core.attach_params().len() == 3
        })
        .await;
        let (_, params) = core.attach_params().pop().unwrap();
        assert_eq!(params["observe"], true);
        assert_eq!(phase(&app, &location), CorePanePhase::Attached);
    }

    #[test]
    fn terminals_started_from_herdr_get_only_its_terminal_identity() {
        let params = crate::api::core_run::run_params(&CoreTerminalRunParams {
            argv: vec!["reviewer".into(), "review it".into()],
            name: Some("reviewer-1a2b".into()),
            ..Default::default()
        });
        let env = params["env"].as_object().unwrap();
        let mut keys: Vec<&str> = env.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["COLORTERM", "TERM", "TERM_PROGRAM", "TERM_PROGRAM_VERSION"]
        );
        assert_eq!(params["name"], "reviewer-1a2b");
        assert!(params.get("parent").is_none());
    }

    #[test]
    fn core_config_defaults_and_overrides() {
        let config: crate::config::Config = toml::from_str("").unwrap();
        assert!(!config.core.enabled);
        assert_eq!(config.core.history_lines, 200);
        let config: crate::config::Config = toml::from_str(
            "[core]\nenabled = true\nsession = \"work\"\nopen_placement = \"split\"\n[core.notify]\nfinished = true\n",
        )
        .unwrap();
        assert!(config.core.enabled && config.core.notify.finished && config.core.notify.blocked);
        assert_eq!(
            config.core.open_placement,
            crate::config::CoreOpenPlacement::Split
        );
        let socket = resolve_socket(&crate::config::CoreConfig {
            socket: Some("/tmp/x.sock".into()),
            ..config.core.clone()
        });
        assert_eq!(socket, std::path::PathBuf::from("/tmp/x.sock"));
        assert!(resolve_socket(&config.core).ends_with("work.sock"));
    }

    #[test]
    fn two_control_panes_for_one_terminal_break_the_invariants() {
        let mut state = crate::app::state::AppState::test_with_adversarial_identity_state();
        let key = CoreTerminalKey {
            origin_core_id: "c".into(),
            origin_address: "t1".into(),
        };
        for terminal in state.terminals.values_mut().take(2) {
            terminal.core = Some(CorePaneBinding::new(
                key.clone(),
                "t1".into(),
                CoreMode::Control,
            ));
        }
        assert_eq!(state.core_invariant_violations().len(), 1);
        let terminal = state.terminals.values_mut().next().unwrap();
        terminal.core.as_mut().unwrap().mode = CoreMode::Observe;
        terminal.launch_argv = Some(vec!["sh".into()]);
        assert_eq!(state.core_invariant_violations().len(), 1);
    }
}
