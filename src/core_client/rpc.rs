//! One update call on its own short plain-mode connection, for the API
//! thread (`core.terminal.run`). The core's identity is checked before
//! anything is sent, and nothing is ever sent twice: after a request has gone
//! out, a dropped or silent line is "unknown", not "not run".

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::time::Duration;

use serde_json::{json, Value};

use super::protocol::{encode_request, parse_incoming, HelloResult, Incoming, RpcError};
use crate::platform::connect_core_socket;

#[derive(Debug)]
pub(crate) enum CallOutcome {
    Done(Value),
    Refused(RpcError),
    /// Sent, but no answer came back: it may have run.
    Unknown(String),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CallError {
    /// Not sent: no core there, or it is another core.
    Unreachable(String),
    Mismatch {
        expected: String,
        found: String,
    },
}

fn read_line(reader: &mut BufReader<crate::platform::CoreSocket>) -> std::io::Result<Vec<u8>> {
    let mut line = Vec::new();
    let read = reader.read_until(b'\n', &mut line)?;
    if read == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "core closed the connection",
        ));
    }
    Ok(line)
}

pub(crate) fn call_once(
    socket: &Path,
    expected_core_id: &str,
    method: &str,
    params: Value,
    timeout: Duration,
) -> Result<CallOutcome, CallError> {
    let unreachable = |err: std::io::Error| CallError::Unreachable(err.to_string());
    let mut stream = connect_core_socket(socket).map_err(unreachable)?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(unreachable)?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(unreachable)?;
    let mut hello = encode_request(
        0,
        "hello",
        json!({"generation": super::protocol::PROTOCOL_GENERATION}),
    );
    hello.push(b'\n');
    stream.write_all(&hello).map_err(unreachable)?;
    let mut reader = BufReader::new(stream.try_clone().map_err(unreachable)?);
    let line = read_line(&mut reader).map_err(unreachable)?;
    let hello = match parse_incoming(&line) {
        Ok(Incoming::Response {
            result: Ok(value), ..
        }) => serde_json::from_value::<HelloResult>(value)
            .map_err(|err| CallError::Unreachable(err.to_string()))?,
        _ => return Err(CallError::Unreachable("core refused hello".into())),
    };
    if hello.core.id != expected_core_id {
        return Err(CallError::Mismatch {
            expected: expected_core_id.to_string(),
            found: hello.core.id,
        });
    }
    let mut request = encode_request(1, method, params);
    request.push(b'\n');
    stream.write_all(&request).map_err(unreachable)?;
    // From here on the core may have acted on it.
    if let Err(err) = stream.set_read_timeout(Some(timeout)) {
        return Ok(CallOutcome::Unknown(err.to_string()));
    }
    let line = match read_line(&mut reader) {
        Ok(line) => line,
        Err(err) => return Ok(CallOutcome::Unknown(err.to_string())),
    };
    Ok(match parse_incoming(&line) {
        Ok(Incoming::Response {
            result: Ok(value), ..
        }) => CallOutcome::Done(value),
        Ok(Incoming::Response {
            result: Err(error), ..
        }) => CallOutcome::Refused(error),
        Ok(Incoming::Notification(_)) | Err(_) => {
            CallOutcome::Unknown("the core's answer could not be read".into())
        }
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    fn fake(
        core_id: &'static str,
        answer: Option<&'static str>,
    ) -> (std::path::PathBuf, std::thread::JoinHandle<Vec<String>>) {
        let dir = std::env::temp_dir().join(format!(
            "hcr-{}-{core_id}-{}",
            std::process::id(),
            answer.is_some()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("c.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);
            let mut seen = Vec::new();
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            seen.push(line.clone());
            writer.write_all(format!("{{\"jsonrpc\":\"2.0\",\"id\":0,\"result\":{{\"generation\":1,\"core\":{{\"id\":\"{core_id}\",\"name\":\"x\"}}}}}}\n").as_bytes()).unwrap();
            line.clear();
            if reader.read_line(&mut line).unwrap_or(0) > 0 {
                seen.push(line.clone());
                if let Some(answer) = answer {
                    writer.write_all(answer.as_bytes()).unwrap();
                    writer.write_all(b"\n").unwrap();
                }
            }
            seen
        });
        (socket, handle)
    }

    #[test]
    fn checks_the_core_before_sending_anything() {
        let (socket, handle) = fake("other", Some(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#));
        let result = call_once(
            &socket,
            "expected",
            "terminal.run",
            json!({}),
            Duration::from_secs(1),
        );
        assert_eq!(
            result.unwrap_err(),
            CallError::Mismatch {
                expected: "expected".into(),
                found: "other".into()
            }
        );
        let seen = handle.join().unwrap();
        assert_eq!(seen.len(), 1, "only hello went out: {seen:?}");
    }

    #[test]
    fn a_silent_core_after_sending_is_unknown_not_failed() {
        let (socket, handle) = fake("core", None);
        let result = call_once(
            &socket,
            "core",
            "terminal.run",
            json!({"argv": ["sh"]}),
            Duration::from_millis(200),
        );
        assert!(matches!(result, Ok(CallOutcome::Unknown(_))), "{result:?}");
        let seen = handle.join().unwrap();
        assert!(seen[1].contains("terminal.run"));
    }

    #[test]
    fn answers_and_refusals_come_back() {
        let (socket, _) = fake(
            "core",
            Some(r#"{"jsonrpc":"2.0","id":1,"result":{"address":"t9"}}"#),
        );
        let Ok(CallOutcome::Done(value)) = call_once(
            &socket,
            "core",
            "terminal.run",
            json!({}),
            Duration::from_secs(1),
        ) else {
            panic!("expected an answer");
        };
        assert_eq!(value["address"], "t9");
        let (socket, _) = fake(
            "core2",
            Some(
                r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"name taken","data":{"kind":"conflict"}}}"#,
            ),
        );
        let Ok(CallOutcome::Refused(error)) = call_once(
            &socket,
            "core2",
            "terminal.run",
            json!({}),
            Duration::from_secs(1),
        ) else {
            panic!("expected a refusal");
        };
        assert_eq!(error.kind.as_deref(), Some("conflict"));
    }

    #[test]
    fn no_core_is_unreachable() {
        let result = call_once(
            Path::new("/nonexistent/hc.sock"),
            "core",
            "terminal.run",
            json!({}),
            Duration::from_secs(1),
        );
        assert!(matches!(result, Err(CallError::Unreachable(_))));
    }
}
