use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::*;
use crate::core_client::fake_core::{wait_for, FakeCore};
use crate::core_client::wire::FrameKind;

fn start(core: &FakeCore) -> (CoreConnectionHandle, Arc<Mutex<Vec<String>>>) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let log = events.clone();
    let handle = CoreConnectionHandle::start(
        CoreConnectionConfig {
            socket: core.socket.clone(),
            reconnect_max: Duration::from_millis(200),
        },
        Box::new(move |event| {
            let line = match event {
                ConnectionEvent::Connecting => "connecting".to_string(),
                ConnectionEvent::Connected {
                    hello,
                    seq,
                    terminals,
                } => {
                    format!(
                        "connected {} seq={seq} n={}",
                        hello.core.id,
                        terminals.len()
                    )
                }
                ConnectionEvent::Status(status) => format!("status {}", status.version),
                ConnectionEvent::Machines(machines) => format!("machines {}", machines.len()),
                ConnectionEvent::Notification(Notification::Event { kind, .. }) => {
                    format!("event {kind}")
                }
                ConnectionEvent::Notification(other) => format!("notification {other:?}"),
                ConnectionEvent::Disconnected { .. } => "disconnected".to_string(),
            };
            log.lock().unwrap().push(line);
        }),
    )
    .unwrap();
    (handle, events)
}

fn request(path: &str, role: StreamRole) -> AttachRequest {
    AttachRequest {
        path: path.into(),
        role,
        cols: 80,
        rows: 24,
        resumed: false,
        history: 0,
        display: None,
    }
}

fn sink(tx: mpsc::Sender<StreamEvent>) -> StreamSink {
    let tx = Mutex::new(tx);
    Arc::new(move |event| {
        let _ = tx.lock().unwrap().send(event);
    })
}

fn next_event(rx: &std::sync::mpsc::Receiver<StreamEvent>) -> StreamEvent {
    rx.recv_timeout(Duration::from_secs(5))
        .expect("stream event")
}

#[test]
fn connects_subscribes_and_credits_event_lines() {
    let core = FakeCore::start("core-a");
    let (handle, events) = start(&core);
    wait_for("connected", || {
        events
            .lock()
            .unwrap()
            .iter()
            .any(|line| line == "connected core-a seq=5 n=1")
            .then_some(())
    });
    let line = br#"{"jsonrpc":"2.0","method":"event","params":{"seq":6,"type":"size","terminal":"t1","data":{"cols":90,"rows":20}}}"#;
    let mut payload = line.to_vec();
    payload.push(b'\n');
    // Split across two frames: lines are reassembled.
    core.push(0, 1, FrameKind::Data, &payload[..10]);
    core.push(0, 1, FrameKind::Data, &payload[10..]);
    wait_for("event", || {
        events
            .lock()
            .unwrap()
            .iter()
            .any(|line| line == "event size")
            .then_some(())
    });
    let credited: u32 = wait_for("event credit", || {
        let total: u32 = core
            .frames(0)
            .iter()
            .filter(|frame| frame.channel == 1 && frame.kind == FrameKind::Credit)
            .map(|frame| u32::from_le_bytes(frame.payload[..4].try_into().unwrap()))
            .sum();
        (total as usize == payload.len()).then_some(total)
    });
    assert_eq!(credited as usize, payload.len());
    handle.shutdown();
}

#[test]
fn render_streams_forward_output_and_credit_only_what_was_consumed() {
    let core = FakeCore::start("core-a");
    let (handle, _) = start(&core);
    let (tx, rx) = mpsc::channel();
    let tx = sink(tx);
    let id = handle.attach(request("t1", StreamRole::Render), tx);
    let StreamEvent::Attached { result, .. } = next_event(&rx) else {
        panic!("expected attach");
    };
    let attached = (*result).unwrap();
    assert_eq!(attached.terminal.cols, 80);
    let StreamEvent::Data { bytes, .. } = next_event(&rx) else {
        panic!("expected paint");
    };
    assert_eq!(&bytes[..], b"\x18paint");
    let params = core.attach_params();
    assert_eq!(params[0].1["observe"], true);
    assert_eq!(params[0].1["resize_notifications"], true);

    assert!(core
        .frames(0)
        .iter()
        .all(|frame| frame.channel != attached.channel || frame.kind != FrameKind::Credit));
    handle.consumed(id, 6);
    wait_for("render credit", || {
        core.frames(0)
            .iter()
            .any(|frame| frame.channel == attached.channel && frame.kind == FrameKind::Credit)
            .then_some(())
    });

    core.push(
        0,
        attached.channel,
        FrameKind::Resize,
        &resize_payload(100, 30),
    );
    assert!(matches!(
        next_event(&rx),
        StreamEvent::Resize {
            cols: 100,
            rows: 30,
            ..
        }
    ));
    // Observers never type or resize.
    handle.input(id, Bytes::from_static(b"x"), false);
    handle.resize(id, 50, 10);
    core.push(
        0,
        attached.channel,
        FrameKind::Close,
        br#"{"reason":"exited","exit_code":3}"#,
    );
    let StreamEvent::Closed { reason, .. } = next_event(&rx) else {
        panic!("expected close");
    };
    assert_eq!(reason.unwrap().exit_code, Some(3));
    assert!(core
        .frames(0)
        .iter()
        .all(|frame| frame.channel != attached.channel
            || !matches!(frame.kind, FrameKind::Data | FrameKind::Resize)));
    handle.shutdown();
}

#[test]
fn control_streams_type_resize_focus_and_drop_their_output() {
    let core = FakeCore::start("core-a");
    let (handle, _) = start(&core);
    let (tx, rx) = mpsc::channel();
    let tx = sink(tx);
    let id = handle.attach(request("t1", StreamRole::Control), tx);
    let StreamEvent::Attached { result, .. } = next_event(&rx) else {
        panic!("expected attach");
    };
    let attached = (*result).unwrap();
    let channel = attached.channel;
    assert_eq!(core.attach_params()[0].1["observe"], false);
    handle.input(id, Bytes::from_static(b"ls\r"), false);
    handle.input(id, Bytes::from_static(b"\x1b[?1;2c"), true);
    handle.resize(id, 120, 40);
    handle.focus(id, true);
    let frames = wait_for("control frames", || {
        let frames: Vec<Frame> = core
            .frames(0)
            .into_iter()
            .filter(|frame| frame.channel == channel)
            .collect();
        (frames.len() >= 5).then_some(frames)
    });
    // The paint was credited at once and never forwarded.
    assert_eq!(frames[0].kind, FrameKind::Credit);
    assert_eq!(&frames[1].payload[..], b"ls\r");
    // Without negotiated automatic_input the reply goes as DATA.
    assert_eq!(frames[2].kind, FrameKind::Data);
    assert_eq!(frames[3].kind, FrameKind::Resize);
    assert_eq!(parse_resize(&frames[3].payload), Some((120, 40)));
    assert_eq!(
        (frames[4].kind, &frames[4].payload[..]),
        (FrameKind::Focus, &[1u8][..])
    );
    assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());

    handle.close(id);
    wait_for("close", || {
        core.frames(0)
            .iter()
            .any(|frame| frame.channel == channel && frame.kind == FrameKind::Close)
            .then_some(())
    });
    // Our own detach is answered with an empty CLOSE, not reported as an end.
    assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
    handle.shutdown();
}

#[test]
fn failed_attaches_report_the_core_error_kind() {
    let core = FakeCore::start("core-a");
    let (handle, _) = start(&core);
    let (tx, rx) = mpsc::channel();
    let tx = sink(tx);
    handle.attach(request("missing", StreamRole::Render), tx);
    let StreamEvent::Attached { result, .. } = next_event(&rx) else {
        panic!("expected failure");
    };
    let error = (*result).unwrap_err();
    assert_eq!(error.kind.as_deref(), Some("not_found"));
    handle.shutdown();
}

#[test]
fn a_dropped_line_ends_streams_without_attaching_them_again_by_path() {
    let core = FakeCore::start("core-a");
    let (handle, events) = start(&core);
    let (tx, rx) = mpsc::channel();
    let tx = sink(tx);
    handle.attach(request("t1", StreamRole::Control), tx);
    assert!(matches!(
        next_event(&rx),
        StreamEvent::Attached { result, .. } if result.is_ok()
    ));
    core.drop_connection(0);
    assert!(matches!(next_event(&rx), StreamEvent::Interrupted { .. }));
    wait_for("reconnected", || {
        (events
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.starts_with("connected"))
            .count()
            >= 2)
            .then_some(())
    });
    // The owner decides whether t1 is still the same terminal (core id).
    assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
    assert_eq!(core.attach_params().len(), 1);

    let (tx, rx) = mpsc::channel();
    let tx = sink(tx);
    let mut again = request("t1", StreamRole::Control);
    again.resumed = true;
    handle.attach(again, tx);
    assert!(matches!(
        next_event(&rx),
        StreamEvent::Attached { result, .. } if result.is_ok()
    ));
    let attaches = core.attach_params();
    assert_eq!(
        (attaches[1].0, &attaches[1].1["resumed"]),
        (1, &json!(true))
    );
    handle.shutdown();
}

#[test]
fn attaching_while_disconnected_fails_at_once() {
    let dir = std::env::temp_dir().join(format!("hcf-offline-{}", std::process::id()));
    let handle = CoreConnectionHandle::start(
        CoreConnectionConfig {
            socket: dir.join("absent.sock"),
            reconnect_max: Duration::from_secs(5),
        },
        Box::new(|_| {}),
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(50));
    let (tx, rx) = mpsc::channel();
    let tx = sink(tx);
    handle.attach(request("t1", StreamRole::Render), tx);
    let StreamEvent::Attached { result, .. } = next_event(&rx) else {
        panic!("expected an immediate failure");
    };
    let error = (*result).unwrap_err();
    assert_eq!(error.kind.as_deref(), Some("disconnected"));
    handle.shutdown();
}

#[test]
fn a_missing_socket_stays_disconnected_without_starting_anything() {
    let dir = std::env::temp_dir().join(format!("hcf-missing-{}", std::process::id()));
    let events = Arc::new(Mutex::new(Vec::new()));
    let log = events.clone();
    let handle = CoreConnectionHandle::start(
        CoreConnectionConfig {
            socket: dir.join("absent.sock"),
            reconnect_max: Duration::from_millis(100),
        },
        Box::new(move |event| {
            if let ConnectionEvent::Disconnected { error } = event {
                log.lock().unwrap().push(error);
            }
        }),
    )
    .unwrap();
    wait_for("two failed attempts", || {
        (events.lock().unwrap().len() >= 2).then_some(())
    });
    assert!(!dir.join("absent.sock").exists());
    handle.shutdown();
}
