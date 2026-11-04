//! Pure model of what one connected core reports: terminals here and on the
//! machines it added, how to name them across reconnects, and how their
//! status reads in Herdr. No I/O; the connection feeds it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::protocol::{HelloResult, MachineView, StatusResult, Terminal};
use crate::detect::AgentState;

/// A core terminal, forever: the id of the core that runs it and its address
/// there. Paths like `rohan/t7` are only how the connected core names it now.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub(crate) struct CoreTerminalKey {
    pub origin_core_id: String,
    pub origin_address: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CoreMode {
    /// Types, resizes and focuses (one per terminal in a Herdr server).
    Control,
    /// Watches only.
    Observe,
}

/// Where a core pane stands, beyond the terminal's own status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CorePanePhase {
    /// Waiting for a connection, a size, or the attach answer.
    Pending,
    Attached,
    /// The stream closed before the terminal record said why.
    Closing,
    /// A kept terminal lost its program; waiting for `restarted`.
    Lost,
    /// The line to the core dropped.
    Interrupted,
    /// The saved key no longer names a terminal of this core (or machine).
    Mismatch(String),
    NotFound(String),
    /// The program ended for good; the last screen stays.
    Ended(String),
}

impl CorePanePhase {
    pub(crate) fn placeholder(&self, path: &str) -> Option<String> {
        match self {
            Self::Pending => Some(format!("core: connecting to {path}…")),
            Self::Attached | Self::Closing => None,
            Self::Lost => Some(format!(
                "core: {path} lost its program; waiting for a new shell"
            )),
            Self::Interrupted => Some(format!("core: disconnected; {path} keeps running")),
            Self::Mismatch(reason) => Some(format!("core: {reason}")),
            Self::NotFound(reason) => Some(format!("core: {path}: {reason}")),
            Self::Ended(label) => Some(format!("core: {path} {label}")),
        }
    }
}

/// A pane showing a core terminal: which one, how, and how it is doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CorePaneBinding {
    pub key: CoreTerminalKey,
    pub path: String,
    pub mode: CoreMode,
    pub phase: CorePanePhase,
    /// Last state reported into Herdr for this pane, to skip repeats.
    pub reported: Option<(AgentState, String, Option<String>, String)>,
    /// How the program ended, once the core's record said so for good
    /// (`exited 0`): kept after the record is removed.
    pub end: Option<String>,
}

impl CorePaneBinding {
    pub(crate) fn new(key: CoreTerminalKey, path: String, mode: CoreMode) -> Self {
        Self {
            key,
            path,
            mode,
            phase: CorePanePhase::Pending,
            reported: None,
            end: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum CoreConnectionState {
    #[default]
    Disabled,
    Unsupported,
    Connecting,
    Connected,
    Disconnected {
        error: String,
    },
}

impl CoreConnectionState {
    pub(crate) fn word(&self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Unsupported => "unsupported",
            Self::Connecting => "connecting",
            Self::Connected => "connected",
            Self::Disconnected { .. } => "disconnected",
        }
    }
}

/// What changed for one terminal, so the app can update panes and notify.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CoreChange {
    pub path: String,
    pub kind: CoreChangeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CoreChangeKind {
    Created,
    Replaced,
    Removed,
    Status,
    Finished,
    Lost,
    Size,
    Other,
}

/// How a core terminal reads in Herdr (plan D4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CorePresentation {
    pub state: AgentState,
    pub label: String,
    pub message: Option<String>,
    pub attention: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CoreFilter {
    /// A core word (`working`, `blocked`, ...), `attention`, `finished` or `no_status`.
    pub state: Option<String>,
    /// `local` or a machine name.
    pub machine: Option<String>,
    pub cwd_prefix: Option<String>,
    /// Case-insensitive substring of address, name, title, template or status message.
    pub text: Option<String>,
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct CoreView {
    pub state: CoreConnectionState,
    pub endpoint: Option<std::path::PathBuf>,
    pub hello: Option<HelloResult>,
    pub server: Option<StatusResult>,
    pub machines: Vec<MachineView>,
    /// The machine list arrived on this connection (an empty list counts).
    pub machines_known: bool,
    pub seq: u64,
    terminals: BTreeMap<String, Terminal>,
}

/// `rohan/t7` -> (`Some("rohan")`, `t7`); `t7` -> (`None`, `t7`).
pub(crate) fn split_path(path: &str) -> (Option<&str>, &str) {
    match path.rsplit_once('/') {
        Some((machine, address)) if !machine.is_empty() => (Some(machine), address),
        _ => (None, path),
    }
}

fn data_string(data: &Value, key: &str) -> Option<String> {
    data.get(key).and_then(Value::as_str).map(str::to_string)
}

fn data_i32(data: &Value, key: &str) -> Option<i32> {
    data.get(key)
        .and_then(Value::as_i64)
        .and_then(|value| i32::try_from(value).ok())
}

impl CoreView {
    pub(crate) fn endpoint_core_id(&self) -> Option<&str> {
        self.hello
            .as_ref()
            .map(|hello| hello.core.id.as_str())
            .filter(|id| !id.is_empty())
    }

    fn machine_by_name(&self, name: &str) -> Option<&MachineView> {
        self.machines.iter().find(|machine| machine.name == name)
    }

    pub(crate) fn key_for_path(&self, path: &str) -> Option<CoreTerminalKey> {
        let (machine, address) = split_path(path);
        if address.is_empty() {
            return None;
        }
        let origin_core_id = match machine {
            None => self.endpoint_core_id()?.to_string(),
            Some(name) => self
                .machine_by_name(name)
                .map(|machine| machine.id.clone())
                .filter(|id| !id.is_empty())?,
        };
        Some(CoreTerminalKey {
            origin_core_id,
            origin_address: address.to_string(),
        })
    }

    pub(crate) fn path_for_key(&self, key: &CoreTerminalKey) -> Option<String> {
        if self.endpoint_core_id() == Some(key.origin_core_id.as_str()) {
            return Some(key.origin_address.clone());
        }
        self.machines
            .iter()
            .find(|machine| machine.id == key.origin_core_id)
            .map(|machine| format!("{}/{}", machine.name, key.origin_address))
    }

    /// Whether `path` (a terminal, a `parent` or an `on` machine) belongs to
    /// this connection: here, or on a machine this core added.
    pub(crate) fn owns_path(&self, path: &str) -> bool {
        match split_path(path) {
            (None, address) => !address.is_empty() && self.endpoint_core_id().is_some(),
            (Some(machine), address) => !address.is_empty() && self.owns_machine(machine),
        }
    }

    pub(crate) fn owns_machine(&self, machine: &str) -> bool {
        self.machine_by_name(machine).is_some()
    }

    pub(crate) fn terminal(&self, path: &str) -> Option<&Terminal> {
        self.terminals.get(path)
    }

    pub(crate) fn terminals(&self) -> impl Iterator<Item = (&String, &Terminal)> {
        self.terminals.iter()
    }

    pub(crate) fn clear_terminals(&mut self) {
        self.terminals.clear();
    }

    pub(crate) fn apply_snapshot(&mut self, seq: u64, terminals: Vec<Terminal>) -> Vec<CoreChange> {
        let mut next = BTreeMap::new();
        for terminal in terminals {
            next.insert(terminal.address.clone(), terminal);
        }
        let mut changes = Vec::new();
        for path in self.terminals.keys() {
            if !next.contains_key(path) {
                changes.push(CoreChange {
                    path: path.clone(),
                    kind: CoreChangeKind::Removed,
                });
            }
        }
        for (path, terminal) in &next {
            let kind = match self.terminals.get(path) {
                None => CoreChangeKind::Created,
                Some(previous) if previous == terminal => continue,
                Some(_) => CoreChangeKind::Replaced,
            };
            changes.push(CoreChange {
                path: path.clone(),
                kind,
            });
        }
        self.terminals = next;
        self.seq = seq;
        changes
    }

    /// Applies one event in order. Events at or below the last snapshot's
    /// `seq` are dropped (after `missed`).
    pub(crate) fn apply_event(
        &mut self,
        seq: u64,
        kind: &str,
        path: Option<&str>,
        data: &Value,
        now_ms: u64,
    ) -> Option<CoreChange> {
        if seq <= self.seq {
            return None;
        }
        self.seq = seq;
        let path = path?.to_string();
        let change = |kind| {
            Some(CoreChange {
                path: path.clone(),
                kind,
            })
        };
        match kind {
            "created" | "restarted" => {
                let mut terminal: Terminal = serde_json::from_value(data.clone()).ok()?;
                terminal.address = path.clone();
                let existed = self.terminals.insert(path.clone(), terminal).is_some();
                change(if existed {
                    CoreChangeKind::Replaced
                } else {
                    CoreChangeKind::Created
                })
            }
            "removed" => self
                .terminals
                .remove(&path)
                .and_then(|_| change(CoreChangeKind::Removed)),
            "labels" => {
                let labels: BTreeMap<String, String> = data
                    .get("labels")
                    .cloned()
                    .and_then(|labels| serde_json::from_value(labels).ok())
                    .unwrap_or_default();
                if let Some(terminal) = self.terminals.get_mut(&path) {
                    terminal.labels = labels;
                    return change(CoreChangeKind::Other);
                }
                let mut terminal: Terminal =
                    serde_json::from_value(data.get("record")?.clone()).ok()?;
                terminal.address = path.clone();
                terminal.labels = labels;
                self.terminals.insert(path.clone(), terminal);
                change(CoreChangeKind::Created)
            }
            _ => {
                let terminal = self.terminals.get_mut(&path)?;
                match kind {
                    "status" => {
                        let status = data
                            .get("status")
                            .cloned()
                            .and_then(|status| serde_json::from_value(status).ok());
                        let word_changed = terminal.status.as_ref().map(|s| s.state.as_str())
                            != status
                                .as_ref()
                                .map(|s: &super::protocol::Status| s.state.as_str());
                        if word_changed || terminal.status_ms.is_none() {
                            terminal.status_ms = Some(now_ms);
                        }
                        terminal.status = status;
                        if let Some(reported) = data.get("reported").and_then(Value::as_bool) {
                            terminal.reported = reported;
                        }
                        change(CoreChangeKind::Status)
                    }
                    "finished" | "lost" => {
                        terminal.reason = data_string(data, "reason");
                        terminal.exit_code = data_i32(data, "exit_code");
                        terminal.signal = data_i32(data, "signal");
                        terminal.foreground = None;
                        if kind == "finished" {
                            terminal.state = "finished".into();
                            terminal.finished_ms = data.get("finished_ms").and_then(Value::as_u64);
                            change(CoreChangeKind::Finished)
                        } else {
                            terminal.pid = None;
                            change(CoreChangeKind::Lost)
                        }
                    }
                    "keep" => {
                        terminal.keep = data.get("keep").and_then(Value::as_bool).unwrap_or(false);
                        change(CoreChangeKind::Other)
                    }
                    "turn" => {
                        terminal.turn = data
                            .get("turn")
                            .and_then(Value::as_u64)
                            .unwrap_or(terminal.turn);
                        change(CoreChangeKind::Other)
                    }
                    "title" => {
                        terminal.title = data_string(data, "title").unwrap_or_default();
                        change(CoreChangeKind::Other)
                    }
                    "size" => {
                        let read = |key| {
                            data.get(key)
                                .and_then(Value::as_u64)
                                .and_then(|value| u16::try_from(value).ok())
                        };
                        terminal.cols = read("cols").unwrap_or(terminal.cols);
                        terminal.rows = read("rows").unwrap_or(terminal.rows);
                        change(CoreChangeKind::Size)
                    }
                    "foreground" => {
                        terminal.foreground = serde_json::from_value(data.clone()).ok();
                        change(CoreChangeKind::Other)
                    }
                    "viewers" => {
                        terminal.viewers = data
                            .get("viewers")
                            .cloned()
                            .and_then(|viewers| serde_json::from_value(viewers).ok())
                            .unwrap_or_default();
                        change(CoreChangeKind::Other)
                    }
                    _ => None,
                }
            }
        }
    }

    /// Terminals needing a human, oldest report first.
    pub(crate) fn attention(&self) -> Vec<String> {
        let mut paths: Vec<(&String, &Terminal)> = self
            .terminals
            .iter()
            .filter(|(_, terminal)| presentation(terminal).attention)
            .collect();
        paths.sort_by_key(|(path, terminal)| {
            (terminal.status_ms.unwrap_or(u64::MAX), (*path).clone())
        });
        paths.into_iter().map(|(path, _)| path.clone()).collect()
    }

    pub(crate) fn filter(&self, filter: &CoreFilter) -> Vec<String> {
        let text = filter.text.as_ref().map(|text| text.to_lowercase());
        self.terminals
            .iter()
            .filter(|(path, terminal)| {
                let shown = presentation(terminal);
                let state_ok = match filter.state.as_deref() {
                    None => true,
                    Some("attention") => shown.attention,
                    Some("finished") => terminal.state == "finished",
                    Some("no_status") => terminal.state != "finished" && terminal.status.is_none(),
                    Some(word) => {
                        terminal.state != "finished"
                            && terminal
                                .status
                                .as_ref()
                                .is_some_and(|status| status.state == word)
                    }
                };
                let machine_ok = match filter.machine.as_deref() {
                    None => true,
                    Some("local") => split_path(path).0.is_none(),
                    Some(name) => split_path(path).0 == Some(name),
                };
                let cwd_ok = filter.cwd_prefix.as_deref().is_none_or(|prefix| {
                    terminal.cwd.starts_with(prefix)
                        || terminal
                            .foreground
                            .as_ref()
                            .is_some_and(|foreground| foreground.cwd.starts_with(prefix))
                });
                let text_ok = text.as_deref().is_none_or(|needle| {
                    [
                        Some(path.as_str()),
                        terminal.name.as_deref(),
                        Some(terminal.title.as_str()),
                        terminal.template.as_deref(),
                        terminal.status.as_ref().map(|status| status.msg.as_str()),
                    ]
                    .into_iter()
                    .flatten()
                    .any(|value| value.to_lowercase().contains(needle))
                });
                let labels_ok = filter
                    .labels
                    .iter()
                    .all(|(key, value)| terminal.labels.get(key) == Some(value));
                state_ok && machine_ok && cwd_ok && text_ok && labels_ok
            })
            .map(|(path, _)| path.clone())
            .collect()
    }
}

pub(crate) fn presentation(terminal: &Terminal) -> CorePresentation {
    let unknown = |label: String| CorePresentation {
        state: AgentState::Unknown,
        label,
        message: None,
        attention: false,
    };
    if terminal.state == "finished" {
        let reason = terminal.reason.clone().unwrap_or_else(|| "finished".into());
        return unknown(match (reason.as_str(), terminal.exit_code) {
            ("exited", Some(code)) => format!("exited {code}"),
            _ => reason,
        });
    }
    if terminal.reason.is_some() {
        return unknown("lost: waiting for shell".into());
    }
    let Some(status) = &terminal.status else {
        return unknown("no status".into());
    };
    let message = Some(status.msg.clone()).filter(|msg| !msg.is_empty());
    let (state, label, attention) = match status.state.as_str() {
        "working" => (AgentState::Working, "working".to_string(), false),
        "blocked" => (
            AgentState::Blocked,
            match status.kind.as_deref() {
                Some(kind) if !kind.is_empty() => format!("blocked: {kind}"),
                _ => "blocked".to_string(),
            },
            true,
        ),
        "error" => (AgentState::Blocked, "error".to_string(), true),
        "idle" => (AgentState::Idle, "idle".to_string(), false),
        "done" => (AgentState::Idle, "turn done".to_string(), false),
        other => (AgentState::Unknown, other.to_string(), false),
    };
    CorePresentation {
        state,
        label,
        message,
        attention,
    }
}

/// Whether a core terminal runs a coding agent: started from a template,
/// reporting its own status, or with a known agent in front. Helpers (builds,
/// waits, watchers) are none of these.
pub(crate) fn is_agent_terminal(terminal: &Terminal) -> bool {
    terminal
        .template
        .as_deref()
        .is_some_and(|template| !template.is_empty())
        || terminal.status.is_some()
        || terminal
            .foreground
            .as_ref()
            .is_some_and(|foreground| crate::detect::identify_agent(&foreground.name).is_some())
}

/// How a listed core terminal's state reads in an agent list.
pub(crate) fn listed_status(terminal: &Terminal) -> crate::api::schema::AgentStatus {
    use crate::api::schema::AgentStatus;
    if terminal
        .status
        .as_ref()
        .is_some_and(|status| status.state == "done")
    {
        return AgentStatus::Done;
    }
    match presentation(terminal).state {
        AgentState::Idle => AgentStatus::Idle,
        AgentState::Working => AgentStatus::Working,
        AgentState::Blocked => AgentStatus::Blocked,
        AgentState::Unknown => AgentStatus::Unknown,
    }
}

/// The agent name Herdr shows for a core terminal: the program's own `app`,
/// else the template it was started from, else `hc`.
pub(crate) fn agent_label(terminal: &Terminal) -> String {
    terminal
        .status
        .as_ref()
        .map(|status| status.app.clone())
        .filter(|app| !app.is_empty())
        .or_else(|| {
            terminal
                .template
                .clone()
                .filter(|template| !template.is_empty())
        })
        .unwrap_or_else(|| "hc".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core_client::protocol::{CoreInfo, Status};
    use serde_json::json;

    fn view() -> CoreView {
        CoreView {
            state: CoreConnectionState::Connected,
            hello: Some(HelloResult {
                generation: 1,
                core: CoreInfo {
                    id: "local-id".into(),
                    name: "mbp".into(),
                },
                ..HelloResult::default()
            }),
            machines: vec![MachineView {
                name: "rohan".into(),
                id: "rohan-id".into(),
                state: "connected".into(),
                ..MachineView::default()
            }],
            ..CoreView::default()
        }
    }

    fn terminal(address: &str) -> Terminal {
        Terminal {
            address: address.into(),
            state: "live".into(),
            cwd: "/work/app".into(),
            ..Terminal::default()
        }
    }

    fn with_status(mut terminal: Terminal, state: &str, kind: Option<&str>) -> Terminal {
        terminal.reported = true;
        terminal.status = Some(Status {
            state: state.into(),
            kind: kind.map(str::to_string),
            app: "claude-code".into(),
            msg: "doing".into(),
            ..Status::default()
        });
        terminal
    }

    #[test]
    fn keys_use_the_origin_core_id_and_survive_machine_renames() {
        let mut view = view();
        assert_eq!(split_path("rohan/t7"), (Some("rohan"), "t7"));
        assert_eq!(split_path("t7"), (None, "t7"));
        let local = view.key_for_path("t7").unwrap();
        let remote = view.key_for_path("rohan/t7").unwrap();
        assert_eq!(local.origin_core_id, "local-id");
        assert_eq!(remote.origin_core_id, "rohan-id");
        assert_eq!(remote.origin_address, "t7");
        assert_ne!(
            local, remote,
            "the same address on two cores is two terminals"
        );
        assert_eq!(view.key_for_path("gone/t7"), None);

        view.machines[0].name = "minas".into();
        assert_eq!(view.path_for_key(&remote).as_deref(), Some("minas/t7"));
        assert_eq!(view.path_for_key(&local).as_deref(), Some("t7"));
        let other_core = CoreTerminalKey {
            origin_core_id: "wiped-id".into(),
            origin_address: "t7".into(),
        };
        assert_eq!(view.path_for_key(&other_core), None);
    }

    #[test]
    fn ownership_covers_terminals_parents_and_machines_of_this_connection() {
        let view = view();
        assert!(view.owns_path("t3"));
        assert!(view.owns_path("rohan/t3"));
        assert!(!view.owns_path("elsewhere/t3"));
        assert!(view.owns_machine("rohan"));
        assert!(!view.owns_machine("elsewhere"));
    }

    #[test]
    fn events_apply_as_the_core_readme_says() {
        let mut view = view();
        let changes = view.apply_snapshot(10, vec![terminal("t1"), terminal("rohan/t2")]);
        assert_eq!(changes.len(), 2);
        assert_eq!(view.seq, 10);

        let created = serde_json::to_value(terminal("t3")).unwrap();
        assert_eq!(
            view.apply_event(11, "created", Some("t3"), &created, 0)
                .unwrap()
                .kind,
            CoreChangeKind::Created
        );
        view.apply_event(12, "status", Some("t3"), &json!({"status": {"state": "working", "app": "codex"}, "records": {}, "reported": true}), 5_000);
        let t3 = view.terminal("t3").unwrap();
        assert_eq!(t3.status.as_ref().unwrap().state, "working");
        assert!(t3.reported);
        assert_eq!(t3.status_ms, Some(5_000));

        view.apply_event(
            13,
            "turn",
            Some("t3"),
            &json!({"turn": 4, "state": "done"}),
            0,
        );
        view.apply_event(14, "title", Some("t3"), &json!({"title": "review"}), 0);
        view.apply_event(
            15,
            "labels",
            Some("t3"),
            &json!({"labels": {"agents.role": "main"}}),
            0,
        );
        view.apply_event(16, "size", Some("t3"), &json!({"cols": 100, "rows": 30}), 0);
        view.apply_event(
            17,
            "foreground",
            Some("t3"),
            &json!({"pid": 9, "name": "node", "argv": ["node"], "cwd": "/work/app/sub"}),
            0,
        );
        view.apply_event(18, "keep", Some("t3"), &json!({"keep": true}), 0);
        let t3 = view.terminal("t3").unwrap();
        assert_eq!(
            (t3.turn, t3.title.as_str(), t3.cols, t3.rows),
            (4, "review", 100, 30)
        );
        assert_eq!(t3.labels["agents.role"], "main");
        assert_eq!(t3.foreground.as_ref().unwrap().cwd, "/work/app/sub");
        assert!(t3.keep);

        let lost = view
            .apply_event(
                19,
                "lost",
                Some("t3"),
                &json!({"reason": "signaled", "exit_code": null, "signal": 9, "finished_ms": null}),
                0,
            )
            .unwrap();
        assert_eq!(lost.kind, CoreChangeKind::Lost);
        let t3 = view.terminal("t3").unwrap();
        assert_eq!(t3.state, "live");
        assert_eq!(t3.pid, None);
        assert!(t3.foreground.is_none());

        let restarted = serde_json::to_value(terminal("t3")).unwrap();
        assert_eq!(
            view.apply_event(20, "restarted", Some("t3"), &restarted, 0)
                .unwrap()
                .kind,
            CoreChangeKind::Replaced
        );

        view.apply_event(
            21,
            "status",
            Some("t3"),
            &json!({"status": null, "records": {}, "reported": true}),
            0,
        );
        let finished = view
            .apply_event(
                22,
                "finished",
                Some("t3"),
                &json!({"reason": "exited", "exit_code": 3, "signal": null, "finished_ms": 99}),
                0,
            )
            .unwrap();
        assert_eq!(finished.kind, CoreChangeKind::Finished);
        let t3 = view.terminal("t3").unwrap();
        assert_eq!(
            (t3.state.as_str(), t3.exit_code, t3.finished_ms),
            ("finished", Some(3), Some(99))
        );
        assert!(t3.status.is_none());

        assert_eq!(
            view.apply_event(23, "removed", Some("t3"), &json!({}), 0)
                .unwrap()
                .kind,
            CoreChangeKind::Removed
        );
        assert!(view.terminal("t3").is_none());
        assert_eq!(
            view.apply_event(24, "doc", None, &json!({"name": "x", "version": 1}), 0),
            None
        );
        assert_eq!(view.seq, 24);
    }

    #[test]
    fn labels_events_with_a_record_add_a_newly_matching_terminal() {
        let mut view = view();
        view.apply_snapshot(1, vec![]);
        let record = serde_json::to_value(terminal("t9")).unwrap();
        let change = view.apply_event(
            2,
            "labels",
            Some("t9"),
            &json!({"labels": {"a": "b"}, "record": record}),
            0,
        );
        assert_eq!(change.unwrap().kind, CoreChangeKind::Created);
        assert_eq!(view.terminal("t9").unwrap().labels["a"], "b");
    }

    #[test]
    fn a_missed_snapshot_replaces_everything_and_drops_older_events() {
        let mut view = view();
        view.apply_snapshot(5, vec![terminal("t1"), terminal("t2")]);
        let changes = view.apply_snapshot(57, vec![terminal("t2"), terminal("t4")]);
        assert!(changes
            .iter()
            .any(|c| c.path == "t1" && c.kind == CoreChangeKind::Removed));
        assert!(changes
            .iter()
            .any(|c| c.path == "t4" && c.kind == CoreChangeKind::Created));
        assert!(view.terminal("t1").is_none());
        assert_eq!(
            view.apply_event(57, "title", Some("t2"), &json!({"title": "old"}), 0),
            None
        );
        assert_eq!(view.terminal("t2").unwrap().title, "");
        assert!(view
            .apply_event(58, "title", Some("t2"), &json!({"title": "new"}), 0)
            .is_some());
    }

    #[test]
    fn presentation_keeps_core_words_apart() {
        let cases = [
            (
                with_status(terminal("t1"), "working", None),
                AgentState::Working,
                "working",
                false,
            ),
            (
                with_status(terminal("t1"), "blocked", Some("permission")),
                AgentState::Blocked,
                "blocked: permission",
                true,
            ),
            (
                with_status(terminal("t1"), "error", None),
                AgentState::Blocked,
                "error",
                true,
            ),
            (
                with_status(terminal("t1"), "idle", None),
                AgentState::Idle,
                "idle",
                false,
            ),
            (
                with_status(terminal("t1"), "done", None),
                AgentState::Idle,
                "turn done",
                false,
            ),
            (
                with_status(terminal("t1"), "dreaming", None),
                AgentState::Unknown,
                "dreaming",
                false,
            ),
            (terminal("t1"), AgentState::Unknown, "no status", false),
        ];
        for (terminal, state, label, attention) in cases {
            let shown = presentation(&terminal);
            assert_eq!(
                (shown.state, shown.label.as_str(), shown.attention),
                (state, label, attention)
            );
        }
        let mut finished = terminal("t1");
        finished.state = "finished".into();
        finished.reason = Some("exited".into());
        finished.exit_code = Some(3);
        assert_eq!(presentation(&finished).label, "exited 3");
        assert_eq!(presentation(&finished).state, AgentState::Unknown);
        finished.reason = Some("killed".into());
        finished.exit_code = None;
        assert_eq!(presentation(&finished).label, "killed");
        let mut lost = terminal("t1");
        lost.reason = Some("signaled".into());
        assert_eq!(presentation(&lost).label, "lost: waiting for shell");

        assert_eq!(
            agent_label(&with_status(terminal("t1"), "idle", None)),
            "claude-code"
        );
        let mut templated = terminal("t1");
        templated.template = Some("reviewer".into());
        assert_eq!(agent_label(&templated), "reviewer");
        assert_eq!(agent_label(&terminal("t1")), "hc");
    }

    #[test]
    fn attention_lists_blocked_and_errored_terminals_oldest_first() {
        let mut view = view();
        let mut old_block = with_status(terminal("t2"), "blocked", Some("question"));
        old_block.status_ms = Some(100);
        let mut new_error = with_status(terminal("rohan/t1"), "error", None);
        new_error.status_ms = Some(300);
        let mut working = with_status(terminal("t3"), "working", None);
        working.status_ms = Some(50);
        view.apply_snapshot(1, vec![new_error, working, old_block]);
        assert_eq!(
            view.attention(),
            vec!["t2".to_string(), "rohan/t1".to_string()]
        );
    }

    #[test]
    fn filters_combine_state_machine_cwd_text_and_labels() {
        let mut view = view();
        let mut a = with_status(terminal("t1"), "blocked", Some("permission"));
        a.name = Some("main-1a2b".into());
        a.labels.insert("agents.role".into(), "main".into());
        let mut b = with_status(terminal("rohan/t2"), "working", None);
        b.cwd = "/srv/other".into();
        let mut c = terminal("t3");
        c.state = "finished".into();
        view.apply_snapshot(1, vec![a, b, c]);

        let by = |filter: CoreFilter| view.filter(&filter);
        assert_eq!(
            by(CoreFilter {
                state: Some("attention".into()),
                ..Default::default()
            }),
            vec!["t1"]
        );
        assert_eq!(
            by(CoreFilter {
                state: Some("finished".into()),
                ..Default::default()
            }),
            vec!["t3"]
        );
        assert_eq!(
            by(CoreFilter {
                machine: Some("rohan".into()),
                ..Default::default()
            }),
            vec!["rohan/t2"]
        );
        assert_eq!(
            by(CoreFilter {
                machine: Some("local".into()),
                ..Default::default()
            }),
            vec!["t1", "t3"]
        );
        assert_eq!(
            by(CoreFilter {
                cwd_prefix: Some("/srv".into()),
                ..Default::default()
            }),
            vec!["rohan/t2"]
        );
        assert_eq!(
            by(CoreFilter {
                text: Some("MAIN".into()),
                ..Default::default()
            }),
            vec!["t1"]
        );
        let mut labels = BTreeMap::new();
        labels.insert("agents.role".to_string(), "main".to_string());
        assert_eq!(
            by(CoreFilter {
                labels,
                ..Default::default()
            }),
            vec!["t1"]
        );
    }
}
