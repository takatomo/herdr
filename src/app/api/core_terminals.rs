//! JSON API for herdr-core terminals: status, list, open and attention.
//! `core.terminal.run` runs on the API connection's thread (see
//! `crate::api::core_run`) and comes back here only to open the pane.

use std::collections::BTreeMap;

use crate::api::schema::{
    CoreAttentionAction, CoreAttentionParams, CoreIdentityInfo, CoreMachineInfo, CoreOpenedInfo,
    CorePaneRef, CorePlacementParam, CoreStatusInfo, CoreTerminalInfo, CoreTerminalListParams,
    CoreTerminalOpenParams, CoreViewMode, ResponseResult, SplitDirection,
};
use crate::app::core::{CoreError, CoreOpenRequest, CoreOpenResult, CorePlacement};
use crate::app::App;
use crate::core_client::model::{
    presentation, split_path, CoreConnectionState, CoreFilter, CoreMode, CorePanePhase,
};

use super::responses::{encode_error, encode_success};

fn mode_from(mode: Option<CoreViewMode>) -> CoreMode {
    match mode {
        Some(CoreViewMode::Observe) => CoreMode::Observe,
        _ => CoreMode::Control,
    }
}

fn mode_to(mode: CoreMode) -> CoreViewMode {
    match mode {
        CoreMode::Control => CoreViewMode::Control,
        CoreMode::Observe => CoreViewMode::Observe,
    }
}

fn phase_word(phase: &CorePanePhase) -> &'static str {
    match phase {
        CorePanePhase::Pending => "pending",
        CorePanePhase::Attached => "attached",
        CorePanePhase::Closing => "closing",
        CorePanePhase::Lost => "lost",
        CorePanePhase::Interrupted => "interrupted",
        CorePanePhase::Mismatch(_) => "mismatch",
        CorePanePhase::NotFound(_) => "not_found",
        CorePanePhase::Ended(_) => "ended",
    }
}

impl App {
    fn core_placement(&self, placement: Option<CorePlacementParam>) -> CorePlacement {
        match placement {
            Some(CorePlacementParam::Existing) => CorePlacement::Existing,
            Some(CorePlacementParam::Tab) => CorePlacement::Tab,
            Some(CorePlacementParam::Split) => CorePlacement::Split,
            Some(CorePlacementParam::Workspace) => CorePlacement::Workspace,
            None => match self.core.config.open_placement {
                crate::config::CoreOpenPlacement::Tab => CorePlacement::Tab,
                crate::config::CoreOpenPlacement::Split => CorePlacement::Split,
            },
        }
    }

    pub(crate) fn core_status_info(&self) -> CoreStatusInfo {
        let view = &self.state.core;
        let mut live = 0;
        let mut finished = 0;
        let mut by_state = BTreeMap::new();
        for (_, terminal) in view.terminals() {
            if terminal.state == "finished" {
                finished += 1;
                continue;
            }
            live += 1;
            let word = terminal
                .status
                .as_ref()
                .map(|status| status.state.clone())
                .unwrap_or_else(|| "no_status".into());
            *by_state.entry(word).or_insert(0) += 1;
        }
        let attention = view.attention();
        let oldest_attention_ms = attention
            .first()
            .and_then(|path| view.terminal(path))
            .and_then(|terminal| terminal.status_ms);
        CoreStatusInfo {
            enabled: self.core.config.enabled,
            state: view.state.word().to_string(),
            error: match &view.state {
                CoreConnectionState::Disconnected { error } => Some(error.clone()),
                _ => None,
            },
            endpoint: view
                .endpoint
                .as_ref()
                .map(|path| path.display().to_string()),
            core: view.hello.as_ref().map(|hello| CoreIdentityInfo {
                id: hello.core.id.clone(),
                name: hello.core.name.clone(),
            }),
            version: view.hello.as_ref().map(|hello| hello.version.clone()),
            generation: view.hello.as_ref().map(|hello| hello.generation),
            features: view
                .hello
                .as_ref()
                .map(|hello| hello.features.clone())
                .unwrap_or_default(),
            machines: view
                .machines
                .iter()
                .map(|machine| CoreMachineInfo {
                    name: machine.name.clone(),
                    id: machine.id.clone(),
                    host: machine.host.clone(),
                    state: machine.state.clone(),
                    version: machine.version.clone(),
                })
                .collect(),
            live,
            finished,
            by_state,
            attention: attention.len(),
            oldest_attention_ms,
            panes: self.core_pane_locations().len(),
            herdr_version: crate::build_info::version().to_string(),
        }
    }

    fn core_terminal_panes(&self) -> BTreeMap<String, Vec<CorePaneRef>> {
        let mut panes: BTreeMap<String, Vec<CorePaneRef>> = BTreeMap::new();
        for location in self.core_pane_locations() {
            let Some(binding) = self
                .state
                .terminals
                .get(&location.terminal_id)
                .and_then(|terminal| terminal.core.as_ref())
            else {
                continue;
            };
            let Some(pane_id) = self.public_pane_id(location.ws_idx, location.pane_id) else {
                continue;
            };
            // By key: a pane whose saved terminal is gone is not listed under
            // whatever now has the same address.
            let Some(path) = self.state.core.path_for_key(&binding.key) else {
                continue;
            };
            panes.entry(path).or_default().push(CorePaneRef {
                pane_id,
                mode: mode_to(binding.mode),
                phase: phase_word(&binding.phase).to_string(),
            });
        }
        panes
    }

    pub(crate) fn core_terminal_infos(
        &self,
        params: &CoreTerminalListParams,
    ) -> Vec<CoreTerminalInfo> {
        let view = &self.state.core;
        let filter = CoreFilter {
            state: params.state.clone(),
            machine: params.machine.clone(),
            cwd_prefix: params.cwd.clone(),
            text: params.text.clone(),
            labels: params.labels.clone().into_iter().collect(),
        };
        let mut panes = self.core_terminal_panes();
        view.filter(&filter)
            .into_iter()
            .filter_map(|path| {
                let terminal = view.terminal(&path)?;
                let key = view.key_for_path(&path)?;
                let shown = presentation(terminal);
                let panes = panes.remove(&path).unwrap_or_default();
                if params
                    .placed
                    .is_some_and(|placed| placed == panes.is_empty())
                {
                    return None;
                }
                let status = terminal.status.as_ref();
                Some(CoreTerminalInfo {
                    machine: split_path(&path).0.map(str::to_string),
                    path,
                    origin_core_id: key.origin_core_id,
                    origin_address: key.origin_address,
                    name: terminal.name.clone(),
                    state: terminal.state.clone(),
                    status: status.map(|status| status.state.clone()),
                    kind: status.and_then(|status| status.kind.clone()),
                    message: shown.message,
                    label: shown.label,
                    attention: shown.attention,
                    app: status
                        .map(|status| status.app.clone())
                        .filter(|app| !app.is_empty()),
                    template: terminal.template.clone(),
                    cwd: terminal.cwd.clone(),
                    labels: terminal.labels.clone(),
                    parent: terminal.parent.clone(),
                    turn: terminal.turn,
                    title: terminal.title.clone(),
                    created_ms: terminal.created_ms,
                    status_ms: terminal.status_ms,
                    finished_ms: terminal.finished_ms,
                    exit_code: terminal.exit_code,
                    reason: terminal.reason.clone(),
                    panes,
                })
            })
            .collect()
    }

    fn core_opened_info(&self, opened: CoreOpenResult) -> Option<CoreOpenedInfo> {
        Some(CoreOpenedInfo {
            pane: self.pane_info(opened.ws_idx, opened.pane_id)?,
            created: opened.created,
        })
    }

    pub(super) fn handle_core_status(&mut self, id: String) -> String {
        encode_success(
            id,
            ResponseResult::CoreStatus {
                status: self.core_status_info(),
            },
        )
    }

    pub(super) fn handle_core_terminal_list(
        &mut self,
        id: String,
        params: CoreTerminalListParams,
    ) -> String {
        encode_success(
            id,
            ResponseResult::CoreTerminalList {
                terminals: self.core_terminal_infos(&params),
            },
        )
    }

    pub(crate) fn handle_core_terminal_open(
        &mut self,
        id: String,
        params: CoreTerminalOpenParams,
    ) -> String {
        let target = match params.target_pane_id.as_deref() {
            Some(pane) => match self.parse_pane_id(pane) {
                Some(target) => Some(target),
                None => return encode_error(id, "pane_not_found", format!("no pane {pane}")),
            },
            None => None,
        };
        let request = CoreOpenRequest {
            endpoint_core_id: params.endpoint_core_id,
            path: params.terminal,
            mode: mode_from(params.mode),
            placement: self.core_placement(params.placement),
            target,
            direction: match params.direction {
                Some(SplitDirection::Down) => ratatui::layout::Direction::Vertical,
                _ => ratatui::layout::Direction::Horizontal,
            },
            focus: params.focus.unwrap_or(true),
            close_popup: params.close_popup,
        };
        match self.open_core_terminal(request) {
            Ok(opened) => match self.core_opened_info(opened) {
                Some(opened) => encode_success(id, ResponseResult::CoreTerminalOpened { opened }),
                None => encode_error(id, "pane_not_found", "the opened pane is gone"),
            },
            Err(CoreError { code, message }) => encode_error(id, code, message),
        }
    }

    pub(crate) fn handle_core_attention(
        &mut self,
        id: String,
        params: CoreAttentionParams,
    ) -> String {
        let count = self.state.core.attention().len();
        let line = match self.state.core.state {
            CoreConnectionState::Connected if count > 0 => format!("hc !{count}"),
            CoreConnectionState::Connected | CoreConnectionState::Disabled => String::new(),
            _ => "hc ?".to_string(),
        };
        let opened = match params.action {
            CoreAttentionAction::Count => None,
            CoreAttentionAction::Next => {
                let placement = self.core_placement(params.placement);
                match self.open_next_core_attention(placement, params.close_popup) {
                    Ok(opened) => opened.and_then(|opened| self.core_opened_info(opened)),
                    Err(CoreError { code, message }) => return encode_error(id, code, message),
                }
            }
        };
        encode_success(
            id,
            ResponseResult::CoreAttention {
                count,
                line,
                opened,
            },
        )
    }
}
