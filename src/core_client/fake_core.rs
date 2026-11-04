//! A scripted stand-in for herdr-core, for tests of the connection and of
//! panes that show core terminals.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::wire::{encode_frame, Frame, FrameDecoder, FrameKind};

/// A scripted stand-in for herdr-core: answers hello, subscribe and attach,
/// records every frame the client sends, and lets the test push frames.
pub(crate) struct FakeCore {
    dir: std::path::PathBuf,
    pub(crate) socket: std::path::PathBuf,
    received: Arc<Mutex<Vec<(usize, Frame)>>>,
    attaches: Arc<Mutex<Vec<(usize, Value)>>>,
    writers: Arc<Mutex<Vec<UnixStream>>>,
}

impl FakeCore {
    pub(crate) fn start(core_id: &'static str) -> Self {
        Self::start_with(core_id, true)
    }

    /// `resize_notifications`: whether observers get in-stream RESIZE (0.1.17) or not (0.1.16).
    pub(crate) fn start_with(core_id: &'static str, resize_notifications: bool) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "hcf-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("c.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let received = Arc::new(Mutex::new(Vec::new()));
        let attaches = Arc::new(Mutex::new(Vec::new()));
        let writers = Arc::new(Mutex::new(Vec::new()));
        {
            let received = received.clone();
            let attaches = attaches.clone();
            let writers = writers.clone();
            std::thread::spawn(move || {
                for (index, stream) in listener.incoming().enumerate() {
                    let Ok(stream) = stream else { return };
                    let received = received.clone();
                    let attaches = attaches.clone();
                    writers.lock().unwrap().push(stream.try_clone().unwrap());
                    std::thread::spawn(move || {
                        serve(
                            index,
                            stream,
                            core_id,
                            resize_notifications,
                            received,
                            attaches,
                        );
                    });
                }
            });
        }
        Self {
            dir,
            socket,
            received,
            attaches,
            writers,
        }
    }

    pub(crate) fn push(&self, connection: usize, channel: u32, kind: FrameKind, payload: &[u8]) {
        let mut writers = self.writers.lock().unwrap();
        writers[connection]
            .write_all(&encode_frame(channel, kind, payload))
            .unwrap();
    }

    pub(crate) fn drop_connection(&self, connection: usize) {
        let writers = self.writers.lock().unwrap();
        let _ = writers[connection].shutdown(std::net::Shutdown::Both);
    }

    pub(crate) fn frames(&self, connection: usize) -> Vec<Frame> {
        self.received
            .lock()
            .unwrap()
            .iter()
            .filter(|(index, _)| *index == connection)
            .map(|(_, frame)| frame.clone())
            .collect()
    }

    pub(crate) fn attach_params(&self) -> Vec<(usize, Value)> {
        self.attaches.lock().unwrap().clone()
    }
}

impl Drop for FakeCore {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

static NEXT: AtomicU64 = AtomicU64::new(0);

fn serve(
    index: usize,
    stream: UnixStream,
    core_id: &str,
    resize_notifications: bool,
    received: Arc<Mutex<Vec<(usize, Frame)>>>,
    attaches: Arc<Mutex<Vec<(usize, Value)>>>,
) {
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    let mut hello = String::new();
    if reader.read_line(&mut hello).unwrap_or(0) == 0 {
        return;
    }
    let reply = json!({"jsonrpc": "2.0", "id": 0, "result": {
        "generation": 1, "version": "0.1.17", "core": {"id": core_id, "name": "fake"},
        "framed": true, "methods": [], "features": ["framed", "attach", "events", "resize_notifications"]}});
    writer.write_all(format!("{reply}\n").as_bytes()).unwrap();
    let mut decoder = FrameDecoder::default();
    let mut chunk = [0u8; 4096];
    let mut next_channel = 2u32;
    loop {
        while let Ok(Some(frame)) = decoder.next_frame() {
            received.lock().unwrap().push((index, frame.clone()));
            if frame.channel != 0 {
                if frame.kind == FrameKind::Close {
                    let _ = writer.write_all(&encode_frame(frame.channel, FrameKind::Close, &[]));
                }
                continue;
            }
            let request: Value = serde_json::from_slice(&frame.payload).unwrap();
            let id = request["id"].clone();
            let params = request["params"].clone();
            let result = match request["method"].as_str().unwrap() {
                "server.status" => {
                    json!({"core": {"id": core_id, "name": "fake"}, "version": "0.1.17"})
                }
                "machine.list" => json!({"machines": []}),
                "events.subscribe" => {
                    json!({"seq": 5, "channel": 1, "terminals": [{"address": "t1", "state": "live"}]})
                }
                "terminal.attach" => {
                    attaches.lock().unwrap().push((index, params.clone()));
                    if params["terminal"] == "missing" {
                        let error = json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": "no terminal", "data": {"kind": "not_found"}}});
                        writer
                            .write_all(&encode_frame(
                                0,
                                FrameKind::Data,
                                error.to_string().as_bytes(),
                            ))
                            .unwrap();
                        continue;
                    }
                    let channel = next_channel;
                    next_channel += 1;
                    let reply = json!({"jsonrpc": "2.0", "id": id, "result": {
                        "channel": channel, "resize_notifications": resize_notifications && params["observe"] == true,
                        "terminal": {"address": params["terminal"], "state": "live", "cols": 80, "rows": 24}}});
                    writer
                        .write_all(&encode_frame(
                            0,
                            FrameKind::Data,
                            reply.to_string().as_bytes(),
                        ))
                        .unwrap();
                    writer
                        .write_all(&encode_frame(channel, FrameKind::Data, b"\x18paint"))
                        .unwrap();
                    continue;
                }
                _ => json!({}),
            };
            let reply = json!({"jsonrpc": "2.0", "id": id, "result": result});
            writer
                .write_all(&encode_frame(
                    0,
                    FrameKind::Data,
                    reply.to_string().as_bytes(),
                ))
                .unwrap();
        }
        match reader.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(read) => decoder.push(&chunk[..read]),
        }
    }
}

pub(crate) fn wait_for<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}
