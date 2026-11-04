//! `herdr core …`: herdr-core terminals through the running Herdr server.
//! Nothing here starts a core or runs `hc`.

use serde_json::Value;

use crate::api::client::ApiClient;
use crate::api::schema::{
    CoreAttentionAction, CoreAttentionParams, CorePlacementParam, CoreRunOpen, CoreStatusParams,
    CoreTerminalListParams, CoreTerminalOpenParams, CoreTerminalRunParams, CoreViewMode, Method,
    Request, SplitDirection,
};

const USAGE: &str = "\
usage:
  herdr core status [--json]
  herdr core list [--state S] [--machine M] [--cwd DIR] [--text T] [--label K=V]... [--placed|--unplaced] [--json]
  herdr core open <terminal> [--observe] [--tab|--split|--existing|--workspace] [--down] [--target PANE] [--no-focus]
                  [--endpoint-core-id ID] [--close-popup] [--json]
  herdr core run [--cwd DIR] [--name N] [--label K=V]... [--parent T] [--outlive-parent] [--keep] [--on MACHINE]
                 [--no-notify] [--line LINE] [--no-open] [--observe] [--tab|--split] [--no-focus]
                 [--endpoint-core-id ID] [--close-popup] [--json] [-- PROGRAM ARGS...|TEMPLATE TASK...]
  herdr core attention [next] [--tab|--split] [--close-popup] [--json]

Terminals are herdr-core (hc) terminals: `t7`, a name, or `machine/t7`.
Herdr only connects to a running core (`[core] enabled = true`); it never starts one.";

pub(super) fn run_core_command(args: &[String]) -> std::io::Result<i32> {
    let Some(command) = args.first().map(String::as_str) else {
        println!("{USAGE}");
        return Ok(2);
    };
    let rest = &args[1..];
    let parsed = match command {
        "status" => parse_flags(rest).map(|flags| (Method::CoreStatus(CoreStatusParams {}), flags)),
        "list" => parse_flags(rest).map(|flags| {
            let params = CoreTerminalListParams {
                state: flags.value("--state"),
                machine: flags.value("--machine"),
                cwd: flags.value("--cwd"),
                text: flags.value("--text"),
                labels: flags.labels(),
                placed: if flags.has("--placed") {
                    Some(true)
                } else if flags.has("--unplaced") {
                    Some(false)
                } else {
                    None
                },
            };
            (Method::CoreTerminalList(params), flags)
        }),
        "open" => parse_flags(rest).and_then(|flags| {
            let terminal = flags.positional.first().cloned()?;
            let params = CoreTerminalOpenParams {
                endpoint_core_id: flags.value("--endpoint-core-id"),
                terminal,
                mode: flags.mode(),
                placement: flags.placement(),
                target_pane_id: flags.value("--target"),
                direction: flags.has("--down").then_some(SplitDirection::Down),
                focus: flags.has("--no-focus").then_some(false),
                close_popup: flags.has("--close-popup"),
            };
            Some((Method::CoreTerminalOpen(params), flags))
        }),
        "run" => parse_flags(rest).map(|flags| {
            let open = (!flags.has("--no-open")).then(|| CoreRunOpen {
                mode: flags.mode(),
                placement: flags.placement(),
                focus: flags.has("--no-focus").then_some(false),
                close_popup: flags.has("--close-popup"),
            });
            let params = CoreTerminalRunParams {
                endpoint_core_id: flags.value("--endpoint-core-id"),
                argv: flags.trailing.clone(),
                line: flags.value("--line"),
                cwd: flags.value("--cwd").or_else(|| {
                    std::env::current_dir()
                        .ok()
                        .map(|dir| dir.display().to_string())
                }),
                name: flags.value("--name"),
                labels: flags.labels(),
                parent: flags.value("--parent"),
                outlive_parent: flags.has("--outlive-parent"),
                keep: flags.has("--keep"),
                on: flags.value("--on"),
                no_notify: flags.has("--no-notify"),
                open,
            };
            (Method::CoreTerminalRun(params), flags)
        }),
        "attention" => parse_flags(rest).map(|flags| {
            let next = flags.positional.first().map(String::as_str) == Some("next");
            let params = CoreAttentionParams {
                action: if next {
                    CoreAttentionAction::Next
                } else {
                    CoreAttentionAction::Count
                },
                placement: flags.placement(),
                close_popup: flags.has("--close-popup"),
            };
            (Method::CoreAttention(params), flags)
        }),
        "help" | "--help" | "-h" => {
            println!("{USAGE}");
            return Ok(0);
        }
        _ => None,
    };
    let Some((method, flags)) = parsed else {
        eprintln!("{USAGE}");
        return Ok(2);
    };
    let request = Request {
        id: format!("cli:core:{command}"),
        method,
    };
    let response = match ApiClient::local().request_value(&request) {
        Ok(value) => value,
        Err(err) => {
            eprintln!("herdr core {command}: {err}");
            return Ok(1);
        }
    };
    if let Some(error) = response.get("error") {
        if flags.has("--json") {
            println!("{response}");
        } else {
            eprintln!(
                "{}: {}",
                error.get("code").and_then(Value::as_str).unwrap_or("error"),
                error.get("message").and_then(Value::as_str).unwrap_or("")
            );
        }
        // A tab bar segment shows nothing rather than an error.
        return Ok(if command == "attention" { 0 } else { 1 });
    }
    let result = response.get("result").cloned().unwrap_or(Value::Null);
    if flags.has("--json") {
        println!("{result}");
        return Ok(0);
    }
    print_human(command, &result);
    Ok(0)
}

fn print_human(command: &str, result: &Value) {
    let text = |value: &Value, key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    match command {
        "status" => {
            let status = &result["status"];
            println!("state: {}", text(status, "state"));
            if let Some(error) = status.get("error").and_then(Value::as_str) {
                println!("error: {error}");
            }
            println!("endpoint: {}", text(status, "endpoint"));
            if let Some(core) = status.get("core") {
                println!("core: {} ({})", text(core, "name"), text(core, "id"));
            }
            println!("core version: {}", text(status, "version"));
            println!("features: {}", status["features"]);
            println!(
                "terminals: {} live, {} finished, {} need attention; core panes here: {}",
                status["live"], status["finished"], status["attention"], status["panes"]
            );
            println!("by state: {}", status["by_state"]);
            if let Some(machines) = status["machines"]
                .as_array()
                .filter(|list| !list.is_empty())
            {
                for machine in machines {
                    println!(
                        "core machine {}: {} ({} {})",
                        text(machine, "name"),
                        text(machine, "state"),
                        text(machine, "host"),
                        text(machine, "version")
                    );
                }
            }
            println!("herdr: {}", text(status, "herdr_version"));
        }
        "list" => {
            for terminal in result["terminals"].as_array().into_iter().flatten() {
                let panes = terminal["panes"].as_array().map_or(0, Vec::len);
                println!(
                    "{:<12} {:<20} {:<22} {:<4} {}",
                    text(terminal, "path"),
                    terminal.get("name").and_then(Value::as_str).unwrap_or("-"),
                    text(terminal, "label"),
                    if panes > 0 { "pane" } else { "-" },
                    text(terminal, "cwd")
                );
            }
        }
        "open" => {
            let opened = &result["opened"];
            println!(
                "{} {}",
                if opened["created"].as_bool() == Some(true) {
                    "opened"
                } else {
                    "focused"
                },
                text(&opened["pane"], "pane_id")
            );
        }
        "run" => {
            let path = text(&result["terminal"], "path");
            println!("started {path}");
            if let Some(line) = first_message_line(result, &path) {
                if result["first_message"]["sent"].as_bool() == Some(true) {
                    println!("{line}");
                } else {
                    eprintln!("{line}");
                }
            }
            if let Some(error) = result.get("open_error") {
                eprintln!(
                    "not opened ({}): {}; open it with `herdr core open`",
                    text(error, "code"),
                    text(error, "message")
                );
            }
        }
        "attention" => {
            let line = text(result, "line");
            if !line.is_empty() {
                println!("{line}");
            }
        }
        _ => println!("{result}"),
    }
}

/// What happened to a template run's first message, if the core said.
fn first_message_line(result: &Value, path: &str) -> Option<String> {
    let first = result.get("first_message")?;
    if first["sent"].as_bool() == Some(true) {
        return Some(match first["turn"].as_u64() {
            Some(turn) => format!("first message sent (turn {turn})"),
            None => "first message sent".into(),
        });
    }
    let reason = first["reason"].as_str().unwrap_or("no reason given");
    Some(format!(
        "first message NOT sent ({reason}); {path} is running: check it with `herdr core open {path}` and send it there, do not run it again"
    ))
}

#[derive(Default)]
struct Flags {
    pairs: Vec<(String, String)>,
    switches: Vec<String>,
    positional: Vec<String>,
    trailing: Vec<String>,
}

const VALUE_FLAGS: &[&str] = &[
    "--state",
    "--machine",
    "--cwd",
    "--text",
    "--label",
    "--target",
    "--endpoint-core-id",
    "--name",
    "--parent",
    "--on",
    "--line",
];

fn parse_flags(args: &[String]) -> Option<Flags> {
    let mut flags = Flags::default();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            flags.trailing = iter.cloned().collect();
            break;
        }
        if VALUE_FLAGS.contains(&arg.as_str()) {
            let value = iter.next()?;
            flags.pairs.push((arg.clone(), value.clone()));
        } else if arg.starts_with("--") {
            flags.switches.push(arg.clone());
        } else {
            flags.positional.push(arg.clone());
        }
    }
    Some(flags)
}

impl Flags {
    fn value(&self, name: &str) -> Option<String> {
        self.pairs
            .iter()
            .rev()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    }

    fn has(&self, name: &str) -> bool {
        self.switches.iter().any(|switch| switch == name)
    }

    fn labels(&self) -> std::collections::HashMap<String, String> {
        self.pairs
            .iter()
            .filter(|(key, _)| key == "--label")
            .filter_map(|(_, value)| value.split_once('='))
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    fn mode(&self) -> Option<CoreViewMode> {
        self.has("--observe").then_some(CoreViewMode::Observe)
    }

    fn placement(&self) -> Option<CorePlacementParam> {
        if self.has("--split") {
            Some(CorePlacementParam::Split)
        } else if self.has("--tab") {
            Some(CorePlacementParam::Tab)
        } else if self.has("--existing") {
            Some(CorePlacementParam::Existing)
        } else if self.has("--workspace") {
            Some(CorePlacementParam::Workspace)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn flags_split_values_switches_positionals_and_the_program() {
        let flags = parse_flags(&args(&[
            "--name",
            "main-1",
            "--label",
            "agents.job=x",
            "--label",
            "agents.role=main",
            "--keep",
            "--",
            "claude",
            "--model",
            "x",
        ]))
        .unwrap();
        assert_eq!(flags.value("--name").as_deref(), Some("main-1"));
        assert_eq!(flags.labels().len(), 2);
        assert!(flags.has("--keep"));
        assert_eq!(flags.trailing, args(&["claude", "--model", "x"]));
        assert!(parse_flags(&args(&["--name"])).is_none());
    }

    #[test]
    fn a_first_message_the_core_did_not_send_is_said_not_hidden() {
        let answer = serde_json::json!({
            "address": "t3",
            "first_message": {"sent": false, "reason": "trust screen"},
        });
        let first = crate::api::core_run::first_message(&answer).unwrap();
        assert_eq!(
            (first.sent, first.reason.as_deref()),
            (false, Some("trust screen"))
        );
        assert!(
            crate::api::core_run::first_message(&serde_json::json!({"address": "t3"})).is_none()
        );
        let result = serde_json::json!({"terminal": {"path": "t3"}, "first_message": first});
        let line = first_message_line(&result, "t3").unwrap();
        assert!(
            line.contains("NOT sent (trust screen)") && line.contains("do not run it again"),
            "{line}"
        );
        let sent = serde_json::json!({"first_message": {"sent": true, "turn": 2}});
        assert_eq!(
            first_message_line(&sent, "t3").as_deref(),
            Some("first message sent (turn 2)")
        );
        assert!(first_message_line(&serde_json::json!({}), "t3").is_none());
    }
}
