//! Live core agents that no Herdr pane shows yet: what an agent list offers
//! to open. Built for every client-shell projection pass, so it reads only
//! memory: no process or filesystem lookups.

use std::collections::HashSet;

use crate::app::App;
use crate::core_client::model::{
    agent_label, is_agent_terminal, listed_status, presentation, CoreConnectionState,
};
use crate::protocol::endpoint::EndpointCoreTerminal;

impl App {
    /// Live core agents without a pane, oldest first. Empty unless connected.
    pub(crate) fn unplaced_core_agents(&self) -> Vec<EndpointCoreTerminal> {
        let view = &self.state.core;
        if view.state != CoreConnectionState::Connected {
            return Vec::new();
        }
        let Some(endpoint_core_id) = view.endpoint_core_id() else {
            return Vec::new();
        };
        let placed: HashSet<_> = self
            .state
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.tabs.iter())
            .flat_map(|tab| tab.panes.values())
            .filter_map(|pane| {
                self.state
                    .terminals
                    .get(&pane.attached_terminal_id)?
                    .core
                    .as_ref()
            })
            .map(|binding| &binding.key)
            .collect();
        let mut listed: Vec<_> = view
            .terminals()
            .filter(|(_, terminal)| terminal.state == "live" && is_agent_terminal(terminal))
            .filter(|(path, _)| {
                view.key_for_path(path)
                    .is_some_and(|key| !placed.contains(&key))
            })
            .collect();
        listed.sort_by_key(|(path, terminal)| (terminal.created_ms, path.as_str()));
        listed
            .into_iter()
            .map(|(path, terminal)| {
                let label = agent_label(terminal);
                EndpointCoreTerminal {
                    path: path.clone(),
                    endpoint_core_id: endpoint_core_id.to_string(),
                    name: terminal.name.clone(),
                    agent: crate::app::api_helpers::normalize_reported_agent_label(&label)
                        .unwrap_or(label),
                    cwd: terminal.cwd.clone(),
                    agent_status: listed_status(terminal),
                    state_label: presentation(terminal).label,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use crate::api::schema::AgentStatus;
    use crate::app::core::tests::{bind, connect, record, test_app};
    use crate::core_client::connection::ConnectionEvent;
    use crate::core_client::model::{CoreConnectionState, CoreMode};
    use crate::core_client::protocol::Notification;

    #[tokio::test]
    async fn only_live_agents_without_a_pane_are_offered() {
        let mut app = test_app();
        let mut agent = record("t1", "live", None);
        agent.template = Some("astra".into());
        agent.cwd = "/work/a".into();
        agent.created_ms = 2;
        let mut reporting = record("t5", "live", Some(("working", None)));
        reporting.created_ms = 1;
        let helper = record("t2", "live", None);
        let mut finished = record("t3", "finished", None);
        finished.template = Some("astra".into());
        let mut shown = record("t4", "live", None);
        shown.template = Some("astra".into());
        connect(&mut app, vec![agent, helper, finished, shown, reporting]);
        bind(&mut app, "t4", CoreMode::Control);

        let listed = app.unplaced_core_agents();
        let paths: Vec<&str> = listed.iter().map(|agent| agent.path.as_str()).collect();
        assert_eq!(paths, ["t5", "t1"], "{listed:?}");
        assert_eq!(listed[1].endpoint_core_id, "core-a");
        assert_eq!(listed[1].cwd, "/work/a");
        assert_eq!(listed[1].agent, "astra");
        assert_eq!(listed[1].state_label, "no status");
        assert_eq!(listed[0].agent_status, AgentStatus::Working);

        app.handle_core_connection_event(ConnectionEvent::Notification(Notification::Event {
            seq: 9,
            kind: "status".into(),
            terminal: Some("t1".into()),
            machine: None,
            data: serde_json::json!({
                "status": {"state": "blocked", "kind": "permission", "app": "codex"},
                "reported": true,
            }),
        }));
        let t1 = app
            .unplaced_core_agents()
            .into_iter()
            .find(|agent| agent.path == "t1")
            .unwrap();
        assert_eq!(
            (t1.agent_status, t1.state_label.as_str(), t1.agent.as_str()),
            (AgentStatus::Blocked, "blocked: permission", "codex")
        );

        app.state.core.state = CoreConnectionState::Disconnected {
            error: "gone".into(),
        };
        assert!(app.unplaced_core_agents().is_empty());
    }
}
