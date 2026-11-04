//! The subset of the herdr-core API Herdr uses (protocol generation 1).
//!
//! Records keep the core's own words as strings and default every field, so a
//! newer core that adds fields or values never breaks the connection.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub(crate) const PROTOCOL_GENERATION: u32 = 1;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct CoreInfo {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct HelloResult {
    pub generation: u32,
    pub version: String,
    pub core: CoreInfo,
    pub framed: bool,
    pub methods: Vec<String>,
    pub features: Vec<String>,
}

impl HelloResult {
    pub(crate) fn has_feature(&self, feature: &str) -> bool {
        self.features.iter().any(|value| value == feature)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Status {
    pub state: String,
    pub kind: Option<String>,
    pub progress: Option<u8>,
    pub app: String,
    pub title: String,
    pub msg: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Foreground {
    pub pid: i64,
    pub name: String,
    pub argv: Vec<String>,
    pub cwd: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Terminal {
    pub address: String,
    pub name: Option<String>,
    pub labels: BTreeMap<String, String>,
    pub argv: Vec<String>,
    pub cwd: String,
    pub state: String,
    pub created_ms: u64,
    pub finished_ms: Option<u64>,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub reason: Option<String>,
    pub status: Option<Status>,
    pub status_ms: Option<u64>,
    pub reported: bool,
    pub turn: u64,
    pub title: String,
    pub template: Option<String>,
    pub parent: Option<String>,
    pub cols: u16,
    pub rows: u16,
    pub foreground: Option<Foreground>,
    pub keep: bool,
    pub pid: Option<u32>,
    /// Who views it now (a core that reports viewers, feature `viewers`).
    pub viewers: Vec<Viewer>,
}

/// One viewer of a core terminal: its client's process on the core's own
/// machine (unknown for remote and Herdr's own streams) and whether it only
/// watches.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Viewer {
    pub pid: Option<u32>,
    pub observe: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct MachineView {
    pub name: String,
    pub id: String,
    pub host: String,
    pub session: Option<String>,
    pub state: String,
    pub target: String,
    pub temporary: bool,
    pub version: String,
    pub since_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct MachineListResult {
    pub machines: Vec<MachineView>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct StatusResult {
    pub core: CoreInfo,
    pub version: String,
    pub pid: u32,
    pub uptime_ms: u64,
    pub live: u64,
    pub finished: u64,
    pub installed_by: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct SubscribeResult {
    pub seq: u64,
    pub terminals: Vec<Terminal>,
    pub channel: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct AttachResult {
    pub channel: u32,
    pub terminal: Terminal,
    pub resize_notifications: bool,
    pub automatic_input: bool,
}

/// Why an attach stream closed, from the core's CLOSE payload. An empty
/// payload answers our own detach.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct CloseReason {
    pub reason: String,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub finished_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RpcError {
    pub code: i64,
    pub message: String,
    pub kind: Option<String>,
    pub hint: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Notification {
    Event {
        seq: u64,
        kind: String,
        terminal: Option<String>,
        machine: Option<String>,
        data: Value,
    },
    Heartbeat {
        seq: u64,
    },
    Missed {
        seq: u64,
        terminals: Vec<Terminal>,
    },
    Other {
        method: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Incoming {
    Response {
        id: u64,
        result: Result<Value, RpcError>,
    },
    Notification(Notification),
}

/// One JSON-RPC request line (without the trailing newline).
pub(crate) fn encode_request(id: u64, method: &str, params: Value) -> Vec<u8> {
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    // A `Value` built from JSON always serializes.
    serde_json::to_vec(&request).unwrap_or_default()
}

#[derive(Deserialize)]
struct RawMessage {
    #[serde(default)]
    id: Option<Value>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Value,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<RawError>,
}

#[derive(Deserialize)]
struct RawError {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    message: String,
    #[serde(default)]
    data: Value,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawEvent {
    seq: u64,
    #[serde(rename = "type")]
    kind: String,
    terminal: Option<String>,
    machine: Option<String>,
    data: Value,
    terminals: Vec<Terminal>,
}

pub(crate) fn parse_incoming(bytes: &[u8]) -> Result<Incoming, serde_json::Error> {
    let raw: RawMessage = serde_json::from_slice(bytes)?;
    if let Some(method) = raw.method {
        let params: RawEvent = serde_json::from_value(raw.params)?;
        let notification = match method.as_str() {
            "event" => Notification::Event {
                seq: params.seq,
                kind: params.kind,
                terminal: params.terminal,
                machine: params.machine,
                data: params.data,
            },
            "heartbeat" => Notification::Heartbeat { seq: params.seq },
            "missed" => Notification::Missed {
                seq: params.seq,
                terminals: params.terminals,
            },
            _ => Notification::Other { method },
        };
        return Ok(Incoming::Notification(notification));
    }
    let id = raw.id.as_ref().and_then(Value::as_u64).unwrap_or(0);
    let result = match raw.error {
        Some(error) => Err(RpcError {
            code: error.code,
            message: error.message,
            kind: error
                .data
                .get("kind")
                .and_then(Value::as_str)
                .map(str::to_string),
            hint: error
                .data
                .get("hint")
                .and_then(Value::as_str)
                .map(str::to_string),
        }),
        None => Ok(raw.result.unwrap_or(Value::Null)),
    };
    Ok(Incoming::Response { id, result })
}

pub(crate) fn parse_close_reason(payload: &[u8]) -> Option<CloseReason> {
    if payload.is_empty() {
        return None;
    }
    Some(
        serde_json::from_slice(payload).unwrap_or_else(|_| CloseReason {
            reason: String::from_utf8_lossy(payload).into_owned(),
            ..CloseReason::default()
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_result_reads_features_and_ignores_unknown_fields() {
        let line = br#"{"jsonrpc":"2.0","id":1,"result":{"generation":1,"version":"0.1.17","core":{"id":"bfd5","name":"mbp"},"framed":true,"methods":["hello"],"features":["framed","attach","events","status","shell","resize_notifications"],"future":{"x":1}}}"#;
        let Incoming::Response { id, result } = parse_incoming(line).unwrap() else {
            panic!("expected a response");
        };
        assert_eq!(id, 1);
        let hello: HelloResult = serde_json::from_value(result.unwrap()).unwrap();
        assert_eq!(hello.version, "0.1.17");
        assert_eq!(hello.core.id, "bfd5");
        assert!(hello.has_feature("resize_notifications"));
        assert!(!hello.has_feature("automatic_input"));
    }

    #[test]
    fn errors_keep_the_stable_kind() {
        let line = br#"{"error":{"code":-32000,"data":{"hint":"see `hc ls --all`","kind":"not_found"},"message":"no terminal \"nosuch\""},"id":3,"jsonrpc":"2.0"}"#;
        let Incoming::Response { id, result } = parse_incoming(line).unwrap() else {
            panic!("expected a response");
        };
        assert_eq!(id, 3);
        let error = result.unwrap_err();
        assert_eq!(error.code, -32000);
        assert_eq!(error.kind.as_deref(), Some("not_found"));
        assert_eq!(error.hint.as_deref(), Some("see `hc ls --all`"));
    }

    #[test]
    fn notifications_cover_events_heartbeats_and_missed() {
        let event = br#"{"jsonrpc":"2.0","method":"event","params":{"data":{"cols":100,"rows":30},"seq":4,"terminal":"t1","type":"size"}}"#;
        match parse_incoming(event).unwrap() {
            Incoming::Notification(Notification::Event {
                seq,
                kind,
                terminal,
                data,
                ..
            }) => {
                assert_eq!(seq, 4);
                assert_eq!(kind, "size");
                assert_eq!(terminal.as_deref(), Some("t1"));
                assert_eq!(data["cols"], 100);
            }
            other => panic!("unexpected {other:?}"),
        }
        let doc = br#"{"jsonrpc":"2.0","method":"event","params":{"seq":43,"type":"doc","data":{"name":"herdr-app","version":2},"machine":"rohan"}}"#;
        match parse_incoming(doc).unwrap() {
            Incoming::Notification(Notification::Event {
                terminal, machine, ..
            }) => {
                assert_eq!(terminal, None);
                assert_eq!(machine.as_deref(), Some("rohan"));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(
            parse_incoming(br#"{"jsonrpc":"2.0","method":"heartbeat","params":{"seq":42}}"#)
                .unwrap(),
            Incoming::Notification(Notification::Heartbeat { seq: 42 })
        );
        match parse_incoming(br#"{"jsonrpc":"2.0","method":"missed","params":{"seq":57,"terminals":[{"address":"t7","state":"live","brand_new":true}]}}"#).unwrap() {
            Incoming::Notification(Notification::Missed { seq, terminals }) => {
                assert_eq!(seq, 57);
                assert_eq!(terminals[0].address, "t7");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn requests_are_single_json_rpc_lines() {
        let line = encode_request(9, "terminal.list", serde_json::json!({"all": true}));
        assert!(!line.contains(&b'\n'));
        let value: Value = serde_json::from_slice(&line).unwrap();
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["id"], 9);
        assert_eq!(value["method"], "terminal.list");
        assert_eq!(value["params"]["all"], true);
    }

    #[test]
    fn close_payload_is_empty_for_a_detach_ack_and_json_for_an_exit() {
        assert_eq!(parse_close_reason(b""), None);
        let reason = parse_close_reason(
            br#"{"exit_code":3,"finished_ms":1793758327024,"reason":"exited","signal":null}"#,
        )
        .unwrap();
        assert_eq!(reason.reason, "exited");
        assert_eq!(reason.exit_code, Some(3));
    }
}
