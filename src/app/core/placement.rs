//! Where a core terminal shows when a person picks it: the pane already
//! showing it, else a tab in the workspace for its folder. Runs only when
//! asked, never while rendering: it may look at processes and folders.

use std::path::{Path, PathBuf};

use crate::app::App;
use crate::core_client::model::split_path;
use crate::layout::PaneId;
use crate::terminal::TerminalState;

/// The terminal a `hc attach <t>` command line names, if it is one that
/// types (not `--observe`). Without a target the client picked one from a
/// list, which only a core that reports viewers can tell.
pub(crate) fn hc_attach_target(argv: &[String]) -> Option<&str> {
    let program = Path::new(argv.first()?).file_name()?;
    if program != "hc" {
        return None;
    }
    let mut args = argv[1..].iter().map(String::as_str);
    if args.next()? != "attach" {
        return None;
    }
    let mut target = None;
    while let Some(arg) = args.next() {
        match arg {
            "--observe" => return None,
            "--history" => {
                args.next();
            }
            "--json" => {}
            _ if arg.starts_with("--history=") => {}
            _ if arg.starts_with('-') => {}
            _ if target.is_none() => target = Some(arg),
            _ => {}
        }
    }
    target
}

/// The same folder on disk (`/tmp` and `/private/tmp` are one), never the
/// same name in two places.
fn same_folder(a: &Path, b: &Path) -> bool {
    let real = |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    real(a) == real(b)
}

impl App {
    /// A local pane whose foreground program views `path` (a `hc attach` of
    /// it), found by the viewers the core reports, else by the command line.
    pub(super) fn pane_showing_core_terminal(&self, path: &str) -> Option<(usize, PaneId)> {
        if split_path(path).0.is_some() {
            return None;
        }
        let view = &self.state.core;
        let record = view.terminal(path)?;
        let reports_viewers = view
            .hello
            .as_ref()
            .is_some_and(|hello| hello.has_feature("viewers"));
        let viewer_pids: Vec<u32> = record
            .viewers
            .iter()
            .filter(|viewer| !viewer.observe)
            .filter_map(|viewer| viewer.pid)
            .collect();
        if reports_viewers && viewer_pids.is_empty() {
            return None;
        }
        let shows = |process: &crate::platform::ForegroundProcess| {
            if reports_viewers {
                return viewer_pids.contains(&process.pid);
            }
            process
                .argv
                .as_deref()
                .and_then(hc_attach_target)
                .is_some_and(|target| target == path || record.name.as_deref() == Some(target))
        };
        for (ws_idx, workspace) in self.state.workspaces.iter().enumerate() {
            for tab in &workspace.tabs {
                for (pane_id, pane) in &tab.panes {
                    let Some(runtime) = self.terminal_runtimes.get(&pane.attached_terminal_id)
                    else {
                        continue;
                    };
                    if runtime.is_core() {
                        continue;
                    }
                    let Some(job) = runtime.child_pid().and_then(crate::detect::foreground_job)
                    else {
                        continue;
                    };
                    if job.processes.iter().any(shows) {
                        return Some((ws_idx, *pane_id));
                    }
                }
            }
        }
        None
    }

    /// The workspace whose folder is `folder`: the active one when it is,
    /// else the first in order.
    fn workspace_for_folder(&self, folder: &Path) -> Option<usize> {
        let matches = |ws_idx: &usize| {
            self.state.workspaces[*ws_idx]
                .resolved_identity_cwd_from(&self.state.terminals, &self.terminal_runtimes)
                .is_some_and(|cwd| same_folder(&cwd, folder))
        };
        self.state
            .active
            .filter(|ws_idx| *ws_idx < self.state.workspaces.len())
            .filter(matches)
            .or_else(|| (0..self.state.workspaces.len()).find(matches))
    }

    /// A pane waiting for a core terminal, in a tab of the workspace for
    /// `folder`, or as the only tab of a new workspace for it. Returns where,
    /// and whether the workspace is new (its events go out once the pane's
    /// terminal is registered).
    pub(super) fn core_workspace_slot(
        &mut self,
        folder: PathBuf,
        focus: bool,
    ) -> (usize, PaneId, TerminalState, bool) {
        if let Some(ws_idx) = self.workspace_for_folder(&folder) {
            let (tab_idx, pane_id, terminal) =
                self.state.workspaces[ws_idx].create_tab_pending(folder);
            if focus {
                self.state.workspaces[ws_idx].switch_tab(tab_idx);
            }
            return (ws_idx, pane_id, terminal, false);
        }
        let (workspace, pane_id, terminal) = crate::workspace::Workspace::new_pending(
            folder,
            self.event_tx.clone(),
            self.render_notify.clone(),
            self.render_dirty.clone(),
        );
        self.state.workspaces.push(workspace);
        let ws_idx = self.state.workspaces.len() - 1;
        self.state.remove_alias_shadowed_by_new_pane(pane_id);
        crate::logging::workspace_created(&self.state.workspaces[ws_idx].id, pane_id.raw());
        if focus || self.state.active.is_none() {
            self.state.switch_workspace(ws_idx);
        }
        (ws_idx, pane_id, terminal, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::core::tests::{connect, record, test_app};
    use crate::app::core::{CoreOpenRequest, CorePlacement};
    use crate::core_client::connection::ConnectionEvent;
    use crate::core_client::model::CoreMode;
    use crate::core_client::protocol::Notification;

    fn folder(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("herdr-core-place-{}", std::process::id()))
            .join(name);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn agent(path: &str, cwd: &std::path::Path) -> crate::core_client::protocol::Terminal {
        let mut terminal = record(path, "live", None);
        terminal.template = Some("astra".into());
        terminal.cwd = cwd.display().to_string();
        terminal
    }

    fn show(path: &str, focus: bool) -> CoreOpenRequest {
        CoreOpenRequest {
            endpoint_core_id: None,
            path: path.into(),
            mode: CoreMode::Control,
            placement: CorePlacement::Workspace,
            target: None,
            direction: ratatui::layout::Direction::Horizontal,
            focus,
            close_popup: false,
        }
    }

    fn set_root_cwd(app: &mut crate::app::App, ws_idx: usize, cwd: &std::path::Path) {
        let ws = &app.state.workspaces[ws_idx];
        let root = ws.tabs[0].root_pane;
        let terminal_id = ws.tabs[0].panes[&root].attached_terminal_id.clone();
        app.state.terminals.get_mut(&terminal_id).unwrap().cwd = cwd.to_path_buf();
    }

    #[tokio::test]
    async fn a_picked_agent_opens_in_a_tab_of_the_workspace_for_its_folder() {
        let mut app = test_app();
        let (a, x_app, y_app, z) = (folder("a"), folder("x/app"), folder("y/app"), folder("z"));
        set_root_cwd(&mut app, 0, &y_app);
        app.state
            .workspaces
            .push(crate::workspace::Workspace::test_new("a"));
        app.state.ensure_test_terminals();
        set_root_cwd(&mut app, 1, &a);
        connect(
            &mut app,
            vec![agent("t1", &a), agent("t2", &x_app), agent("t3", &z)],
        );

        // The workspace for its folder gets a tab, and it is shown.
        let first = app.open_core_terminal(show("t1", true)).unwrap();
        assert!(first.created);
        assert_eq!(first.ws_idx, 1);
        assert_eq!(app.state.workspaces[1].tabs.len(), 2);
        assert_eq!(app.state.active, Some(1));
        let tab = app.state.workspaces[1]
            .find_tab_index_for_pane(first.pane_id)
            .unwrap();
        assert_eq!(app.state.workspaces[1].active_tab, tab);

        // Picking it again goes to the same tab.
        let again = app.open_core_terminal(show("t1", true)).unwrap();
        assert_eq!(
            (again.created, again.ws_idx, again.pane_id),
            (false, 1, first.pane_id)
        );
        assert_eq!(app.state.workspaces[1].tabs.len(), 2);

        // Another folder with the same name is another workspace, made for it.
        let other = app.open_core_terminal(show("t2", true)).unwrap();
        assert!(other.created);
        assert_eq!(other.ws_idx, 2);
        let made = &app.state.workspaces[2];
        assert_eq!(made.tabs.len(), 1);
        assert_eq!(made.tabs[0].root_pane, other.pane_id);
        assert_eq!(made.identity_cwd, x_app);
        assert_eq!(app.state.active, Some(2));
        assert_eq!(app.state.workspaces[0].tabs.len(), 1);

        // Without focus nothing moves to the new workspace.
        let quiet = app.open_core_terminal(show("t3", false)).unwrap();
        assert!(quiet.created);
        assert_eq!(quiet.ws_idx, 3);
        assert_eq!(app.state.active, Some(2));
        app.state.assert_invariants_for_test();
    }

    #[test]
    fn hc_attach_command_lines_name_their_target() {
        let argv = |line: &str| line.split(' ').map(str::to_string).collect::<Vec<_>>();
        for (line, target) in [
            ("hc attach t43", Some("t43")),
            (
                "/nix/store/x-herdr-core/bin/hc attach --history 200 rohan/t7",
                Some("rohan/t7"),
            ),
            ("hc attach --history=5 t2", Some("t2")),
            ("hc attach --json t3", Some("t3")),
            ("hc attach --observe t1", None),
            ("hc attach", None),
            ("hc", None),
            ("hc run attach", None),
            ("sh -c hc attach t1", None),
        ] {
            assert_eq!(hc_attach_target(&argv(line)), target, "{line}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_pane_whose_program_views_the_terminal_is_where_it_shows() {
        let mut app = test_app();
        app.state.default_shell = "/bin/sh".into();
        let home = folder("viewed");
        let ws_idx = app
            .create_workspace_with_options(home.clone(), false)
            .unwrap();
        let root = app.state.workspaces[ws_idx].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[ws_idx].tabs[0].panes[&root]
            .attached_terminal_id
            .clone();
        let shell = app
            .terminal_runtimes
            .get(&terminal_id)
            .unwrap()
            .child_pid()
            .unwrap();
        let mut viewed = agent("t1", &home);
        viewed.viewers = vec![crate::core_client::protocol::Viewer {
            pid: Some(shell),
            observe: false,
        }];
        connect(&mut app, vec![viewed]);
        app.state.core.hello.as_mut().unwrap().features = vec!["viewers".into()];

        let shown = app.open_core_terminal(show("t1", true)).unwrap();
        assert_eq!(
            (shown.created, shown.ws_idx, shown.pane_id),
            (false, ws_idx, root)
        );
        assert_eq!(app.state.workspaces[ws_idx].tabs.len(), 1);

        // Once the core says it left, nothing ties the pane to it.
        app.handle_core_connection_event(ConnectionEvent::Notification(Notification::Event {
            seq: 2,
            kind: "viewers".into(),
            terminal: Some("t1".into()),
            machine: None,
            data: serde_json::json!({"viewers": []}),
        }));
        let opened = app.open_core_terminal(show("t1", true)).unwrap();
        assert!(opened.created);
        assert_ne!(opened.pane_id, root);
        crate::app::api::test_support::shutdown_test_runtimes(&mut app);
    }
}
