//! A control pane whose local core terminal ended for good becomes a shell
//! in the same place. Two facts must both hold: the core's record said the
//! program ended, and the pane wrote every byte its streams carried. Phases
//! alone prove neither.

use super::CorePaneLocation;
use crate::app::App;
use crate::core_client::model::{split_path, CoreMode};

impl App {
    /// The core's record says the program ended for good: remember how. A
    /// `removed` that follows a known end keeps that end.
    pub(super) fn note_core_end(&mut self, location: &CorePaneLocation, label: String) {
        if let Some(binding) = self.core_binding_mut(&location.terminal_id) {
            if binding.end.is_none() || binding.end.as_deref() == Some("removed") {
                binding.end = Some(label);
            }
        }
    }

    /// Turns the pane into a shell once its terminal ended and its output is
    /// all written; otherwise leaves it as it is.
    pub(super) fn settle_core_pane(&mut self, location: &CorePaneLocation) {
        let Some(binding) = self.core_binding(&location.terminal_id) else {
            return;
        };
        if binding.mode != CoreMode::Control
            || binding.end.is_none()
            || split_path(&binding.path).0.is_some()
        {
            return;
        }
        let written = self
            .terminal_runtimes
            .get(&location.terminal_id)
            .is_none_or(crate::terminal::TerminalRuntime::core_output_settled);
        if written {
            self.replace_ended_core_pane_with_shell(location);
        }
    }

    /// The ended program's pane becomes a shell where it worked, its last
    /// screen kept above the prompt. Nothing runs the program again, and
    /// focus stays where it is.
    fn replace_ended_core_pane_with_shell(&mut self, location: &CorePaneLocation) -> bool {
        let Some(binding) = self.core_binding(&location.terminal_id).cloned() else {
            return false;
        };
        let synced = self
            .state
            .terminals
            .get(&location.terminal_id)
            .map(|terminal| terminal.cwd.clone())
            .filter(|cwd| cwd.is_dir());
        let started = self
            .state
            .core
            .terminal(&binding.path)
            .map(|record| std::path::PathBuf::from(&record.cwd))
            .filter(|cwd| cwd.is_dir());
        let cwd = synced
            .or(started)
            .unwrap_or_else(|| crate::worktree::expand_tilde_path("~"));
        let old = self.terminal_runtimes.get(&location.terminal_id);
        let (rows, cols) = match self
            .state
            .new_pane_size(crate::ui::NewPanePlacement::Existing {
                ws_idx: location.ws_idx,
                pane: location.pane_id,
            }) {
            (rows, cols) if rows > 0 && cols > 0 => (rows, cols),
            _ => old
                .map(|runtime| runtime.current_size())
                .unwrap_or((24, 80)),
        };
        let mut history = old.and_then(last_screen).unwrap_or_default();
        history.push_str(&format!(
            "\r\n[hc {} {}]\r\n",
            binding.path,
            binding.end.as_deref().unwrap_or("ended")
        ));
        let Some(launch_env) = self.pane_launch_env(location.ws_idx, location.pane_id, Vec::new())
        else {
            return false;
        };
        let runtime = match crate::terminal::TerminalRuntime::spawn_with_initial_history(
            location.pane_id,
            rows,
            cols,
            cwd.clone(),
            self.state.pane_scrollback_limit_bytes,
            self.state.host_terminal_theme,
            self.pane_host_terminal_appearance(),
            crate::pane::PaneShellConfig::new(&self.state.default_shell, self.state.shell_mode),
            &launch_env,
            Some(&history),
            self.event_tx.clone(),
            self.render_notify.clone(),
            self.render_dirty.clone(),
        ) {
            Ok(runtime) => runtime,
            Err(err) => {
                tracing::warn!(path = %binding.path, err = %err, "could not start a shell where a core terminal ended");
                return false;
            }
        };
        // Dropping the core runtime closes what is left of its streams.
        self.terminal_runtimes
            .insert(location.terminal_id.clone(), runtime);
        self.core.runtime_panes.remove(&location.terminal_id);
        if let Some(terminal) = self.state.terminals.get_mut(&location.terminal_id) {
            terminal.core = None;
            terminal.cwd = cwd;
            terminal.clear_agent_runtime_identity_after_respawn();
        }
        self.state.mark_session_dirty();
        self.schedule_session_save();
        self.emit_pane_updated(location.ws_idx, location.pane_id);
        self.render_dirty.request_generic();
        self.render_notify.notify_one();
        true
    }
}

/// The ended program's main screen and recent history, as ANSI.
#[cfg(unix)]
fn last_screen(runtime: &crate::terminal::TerminalRuntime) -> Option<String> {
    runtime.handoff_history_ansi()
}

#[cfg(not(unix))]
fn last_screen(_runtime: &crate::terminal::TerminalRuntime) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use crate::app::core::tests::{bind, connect, record, test_app};
    use crate::app::core::CORE_SOURCE;
    use crate::core_client::model::CoreMode;
    use crate::detect::AgentState;
    use crate::events::AppEvent;

    fn close(reason: &str) -> Option<crate::core_client::protocol::CloseReason> {
        Some(crate::core_client::protocol::CloseReason {
            reason: reason.into(),
            ..Default::default()
        })
    }

    fn folder(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("herdr-core-ended-{}", std::process::id()))
            .join(name);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn exited(path: &str, cwd: &std::path::Path) -> crate::core_client::protocol::Terminal {
        let mut terminal = record(path, "finished", None);
        terminal.reason = Some("exited".into());
        terminal.exit_code = Some(0);
        terminal.cwd = cwd.display().to_string();
        terminal
    }

    #[tokio::test]
    async fn an_ended_control_pane_becomes_a_shell_in_its_folder() {
        let mut app = test_app();
        app.state.default_shell = crate::app::exiting_test_command().into();
        let dir = folder("done");
        connect(&mut app, vec![exited("t1", &dir)]);
        let location = bind(&mut app, "t1", CoreMode::Control);
        // Opening a core terminal gives its pane the terminal's folder.
        app.state
            .terminals
            .get_mut(&location.terminal_id)
            .unwrap()
            .cwd = dir.clone();
        app.handle_core_stream_ended(location.pane_id, close("exited"), false);

        let terminal = &app.state.terminals[&location.terminal_id];
        assert!(terminal.core.is_none(), "still a core pane");
        assert_eq!(terminal.cwd, dir);
        assert!(app
            .terminal_runtimes
            .get(&location.terminal_id)
            .is_some_and(|runtime| !runtime.is_core()));
        // A core report queued before the change does not make it an agent again.
        app.handle_internal_event(AppEvent::HookStateReported {
            pane_id: location.pane_id,
            source: CORE_SOURCE.into(),
            agent_label: "claude".into(),
            state: AgentState::Working,
            message: None,
            seq: None,
            session_ref: None,
        });
        assert!(app.state.terminals[&location.terminal_id]
            .effective_agent_label()
            .is_none());
        app.state.assert_invariants_for_test();
        crate::app::api::test_support::shutdown_test_runtimes(&mut app);
    }

    #[tokio::test]
    async fn panes_that_did_not_end_for_good_stay_core_panes() {
        let mut app = test_app();
        app.state.default_shell = crate::app::exiting_test_command().into();
        let dir = folder("kept");
        let mut lost = record("t3", "live", None);
        lost.reason = Some("holder_lost".into());
        lost.keep = true;
        connect(
            &mut app,
            vec![
                record("t2", "live", None),
                lost,
                exited("t5", &dir),
                exited("rb/t4", &dir),
            ],
        );
        for (path, mode, interrupted, why) in [
            ("t2", CoreMode::Control, true, "the line dropped"),
            ("t3", CoreMode::Control, false, "a kept program was lost"),
            ("t2", CoreMode::Control, false, "the record is still live"),
            ("t5", CoreMode::Observe, false, "an observer"),
            (
                "rb/t4",
                CoreMode::Control,
                false,
                "a terminal on another machine",
            ),
        ] {
            let location = bind(&mut app, path, mode);
            app.handle_core_stream_ended(location.pane_id, close("x"), interrupted);
            assert!(
                app.state.terminals[&location.terminal_id].core.is_some(),
                "{why}: became a shell"
            );
        }
    }

    #[cfg(unix)]
    mod with_a_core {
        use super::*;
        use crate::app::core::tests::{open_fake_t1, phase, pump_until};
        use crate::core_client::connection::ConnectionEvent;
        use crate::core_client::fake_core::FakeCore;
        use crate::core_client::model::{CorePaneBinding, CorePanePhase, CoreTerminalKey};
        use crate::core_client::protocol::Notification;
        use crate::core_client::wire::FrameKind;

        fn event(core: &FakeCore, seq: u64, kind: &str, data: serde_json::Value) {
            let line = serde_json::json!({"jsonrpc": "2.0", "method": "event",
                "params": {"seq": seq, "type": kind, "terminal": "t1", "data": data}});
            core.push(0, 1, FrameKind::Data, format!("{line}\n").as_bytes());
        }

        /// The render stream's channel (the fake numbers attaches from 2).
        fn render_channel(core: &FakeCore) -> u32 {
            let index = core
                .attach_params()
                .iter()
                .position(|(_, params)| params["observe"] == true)
                .unwrap();
            2 + index as u32
        }

        fn ran_nothing(core: &FakeCore) -> bool {
            core.frames(0)
                .iter()
                .all(|frame| !String::from_utf8_lossy(&frame.payload).contains("terminal.run"))
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn the_last_output_is_kept_when_the_end_is_known_before_it_arrives() {
            let core = FakeCore::start("core-a");
            let mut app = test_app();
            app.state.default_shell = crate::app::exiting_test_command().into();
            let (_, location) = open_fake_t1(&mut app, &core).await;
            let terminal_id = location.terminal_id.clone();
            event(
                &core,
                6,
                "finished",
                serde_json::json!({"reason": "exited", "exit_code": 0}),
            );
            event(&core, 7, "removed", serde_json::json!({}));
            pump_until(&mut app, "the end applied", |app| {
                app.state.core.terminal("t1").is_none()
            })
            .await;
            assert!(
                app.state.terminals[&terminal_id].core.is_some(),
                "switched before the last output"
            );

            let render = render_channel(&core);
            core.push(0, render, FrameKind::Data, b"last words\r\n");
            core.push(
                0,
                render,
                FrameKind::Close,
                br#"{"reason":"exited","exit_code":0}"#,
            );
            pump_until(&mut app, "a shell", |app| {
                app.state.terminals[&terminal_id].core.is_none()
            })
            .await;
            let text = app
                .terminal_runtimes
                .get(&terminal_id)
                .unwrap()
                .visible_text();
            assert!(text.contains("last words"), "{text}");
            assert!(text.contains("[hc t1 exited 0]"), "{text}");
            assert_eq!(core.attach_params().len(), 2);
            assert!(ran_nothing(&core));
            crate::app::api::test_support::shutdown_test_runtimes(&mut app);
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn an_end_learned_from_a_snapshot_after_the_stream_closed_switches_too() {
            let core = FakeCore::start("core-a");
            let mut app = test_app();
            app.state.default_shell = crate::app::exiting_test_command().into();
            let (_, location) = open_fake_t1(&mut app, &core).await;
            core.push(
                0,
                render_channel(&core),
                FrameKind::Close,
                br#"{"reason":"signaled"}"#,
            );
            pump_until(&mut app, "closing", |app| {
                phase(app, &location) == CorePanePhase::Closing
            })
            .await;
            let mut done = record("t1", "finished", None);
            done.reason = Some("exited".into());
            done.exit_code = Some(0);
            app.handle_core_connection_event(ConnectionEvent::Notification(Notification::Missed {
                seq: 9,
                terminals: vec![done],
            }));
            assert!(app.state.terminals[&location.terminal_id].core.is_none());
            assert!(ran_nothing(&core));
            crate::app::api::test_support::shutdown_test_runtimes(&mut app);
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_saved_pane_of_a_finished_terminal_becomes_a_shell_once() {
            let core = FakeCore::start_with_finished("core-a", "t9");
            let mut app = test_app();
            app.state.default_shell = crate::app::exiting_test_command().into();
            let root = app.state.workspaces[0].tabs[0].root_pane;
            let terminal_id = app.state.workspaces[0].tabs[0].panes[&root]
                .attached_terminal_id
                .clone();
            app.state.terminals.get_mut(&terminal_id).unwrap().core = Some(CorePaneBinding::new(
                CoreTerminalKey {
                    origin_core_id: "core-a".into(),
                    origin_address: "t9".into(),
                },
                "t9".into(),
                CoreMode::Control,
            ));
            let config = crate::config::CoreConfig {
                enabled: true,
                socket: Some(core.socket.to_string_lossy().into_owned()),
                ..crate::config::CoreConfig::default()
            };
            app.apply_core_config(&config);
            pump_until(&mut app, "a shell", |app| {
                app.state.terminals[&terminal_id].core.is_none()
            })
            .await;
            let text = app
                .terminal_runtimes
                .get(&terminal_id)
                .unwrap()
                .visible_text();
            assert!(text.contains("[hc t9 exited 0]"), "{text}");
            // Both answers (control and render) came back; nothing attached or ran again.
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            assert_eq!(core.attach_params().len(), 2);
            assert!(ran_nothing(&core));
            assert!(app.state.terminals[&terminal_id].core.is_none());
            crate::app::api::test_support::shutdown_test_runtimes(&mut app);
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_lost_program_alone_keeps_the_core_pane() {
            let core = FakeCore::start("core-a");
            let mut app = test_app();
            app.state.default_shell = crate::app::exiting_test_command().into();
            let (_, location) = open_fake_t1(&mut app, &core).await;
            event(
                &core,
                6,
                "lost",
                serde_json::json!({"reason": "holder_lost"}),
            );
            pump_until(&mut app, "lost applied", |app| {
                app.state
                    .core
                    .terminal("t1")
                    .is_some_and(|t| t.reason.is_some())
            })
            .await;
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            pump_until(&mut app, "events drained", |_| true).await;
            assert!(app.state.terminals[&location.terminal_id].core.is_some());
        }
    }
}
