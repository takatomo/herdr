//! One connection to a running herdr-core, kept on its own threads.
//!
//! The manager thread owns the socket for writing and all state; a reader
//! thread decodes frames and hands them back. Streams (attach channels) are
//! registered by id. A dropped line ends every stream with `Interrupted`; the
//! owner attaches again after `Connected`, once it has checked that the core
//! (its id) and the terminal's path still name the same terminal. The manager
//! never starts a core: a missing socket only means "disconnected, retry later".

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use serde_json::{json, Value};

use super::protocol::{
    encode_request, parse_close_reason, parse_incoming, AttachResult, CloseReason, HelloResult,
    Incoming, MachineListResult, MachineView, Notification, RpcError, StatusResult,
    SubscribeResult, Terminal, PROTOCOL_GENERATION,
};
use super::wire::{
    credit_payload, encode_frame, parse_resize, resize_payload, Frame, FrameDecoder, FrameKind,
};
use crate::platform::{connect_core_socket, CoreSocket};

pub(crate) type StreamId = u64;
/// Where a stream's events go; called on the manager thread, so it must not block.
pub(crate) type StreamSink = Arc<dyn Fn(StreamEvent) + Send + Sync>;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const FIRST_RETRY: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamRole {
    /// Types, resizes and focuses; its output is credited and dropped.
    Control,
    /// Watches only (`observe`); its output feeds the pane's emulator.
    Render,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AttachRequest {
    pub path: String,
    pub role: StreamRole,
    pub cols: u16,
    pub rows: u16,
    /// Back after a dropped line or restored: do not take the size.
    pub resumed: bool,
    pub history: u32,
    pub display: Option<Value>,
}

#[derive(Debug)]
pub(crate) enum StreamEvent {
    Attached {
        id: StreamId,
        // Boxed: data events are the hot path and must stay small.
        result: Box<Result<AttachResult, RpcError>>,
    },
    Data {
        id: StreamId,
        bytes: Bytes,
    },
    Resize {
        id: StreamId,
        cols: u16,
        rows: u16,
    },
    /// The core ended the stream (program exit, loss, ...).
    Closed {
        id: StreamId,
        reason: Option<CloseReason>,
    },
    /// The line dropped; the stream is over. Attach again after `Connected`.
    Interrupted {
        id: StreamId,
    },
}

#[derive(Debug)]
pub(crate) enum ConnectionEvent {
    Connecting,
    Connected {
        hello: HelloResult,
        seq: u64,
        terminals: Vec<Terminal>,
    },
    Status(StatusResult),
    Machines(Vec<MachineView>),
    Notification(Notification),
    Disconnected {
        error: String,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct CoreConnectionConfig {
    pub socket: PathBuf,
    pub reconnect_max: Duration,
}

enum Command {
    Attach {
        id: StreamId,
        request: AttachRequest,
        sink: StreamSink,
    },
    Input {
        id: StreamId,
        bytes: Bytes,
        automatic: bool,
    },
    Resize {
        id: StreamId,
        cols: u16,
        rows: u16,
    },
    Focus {
        id: StreamId,
        focused: bool,
    },
    Facts {
        id: StreamId,
        display: Value,
    },
    Consumed {
        id: StreamId,
        bytes: u32,
    },
    Close {
        id: StreamId,
    },
    RefreshMachines,
    Shutdown,
}

enum Msg {
    Command(Command),
    Frame { session: u64, frame: Frame },
    ReaderEnded { session: u64, error: String },
}

static NEXT_CONNECTION: AtomicU64 = AtomicU64::new(1);

/// A number for a new connection, unique in this process. News carries it so
/// that what an ended connection still had queued is not taken for the next.
pub(crate) fn next_connection_id() -> u64 {
    NEXT_CONNECTION.fetch_add(1, Ordering::Relaxed)
}

/// Cheap to clone; every clone talks to the same manager thread.
#[derive(Clone)]
pub(crate) struct CoreConnectionHandle {
    tx: Sender<Msg>,
    next_id: Arc<AtomicU64>,
    id: u64,
}

impl CoreConnectionHandle {
    #[cfg(test)]
    pub(crate) fn start(
        config: CoreConnectionConfig,
        on_event: Box<dyn Fn(ConnectionEvent) + Send>,
    ) -> std::io::Result<Self> {
        Self::start_as(next_connection_id(), config, on_event)
    }

    /// Starts connection `id` (from [`next_connection_id`]).
    pub(crate) fn start_as(
        id: u64,
        config: CoreConnectionConfig,
        on_event: Box<dyn Fn(ConnectionEvent) + Send>,
    ) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let manager_tx = tx.clone();
        crate::thread_spawn::spawn_named("herdr-core-client", move || {
            Manager::new(config, manager_tx, on_event).run(rx);
        })?;
        Ok(Self {
            tx,
            next_id: Arc::new(AtomicU64::new(1)),
            id,
        })
    }

    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    fn send(&self, command: Command) {
        let _ = self.tx.send(Msg::Command(command));
    }

    pub(crate) fn attach(&self, request: AttachRequest, sink: StreamSink) -> StreamId {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.send(Command::Attach { id, request, sink });
        id
    }

    pub(crate) fn input(&self, id: StreamId, bytes: Bytes, automatic: bool) {
        self.send(Command::Input {
            id,
            bytes,
            automatic,
        });
    }

    pub(crate) fn resize(&self, id: StreamId, cols: u16, rows: u16) {
        self.send(Command::Resize { id, cols, rows });
    }

    pub(crate) fn focus(&self, id: StreamId, focused: bool) {
        self.send(Command::Focus { id, focused });
    }

    pub(crate) fn facts(&self, id: StreamId, display: Value) {
        self.send(Command::Facts { id, display });
    }

    pub(crate) fn consumed(&self, id: StreamId, bytes: u32) {
        self.send(Command::Consumed { id, bytes });
    }

    pub(crate) fn close(&self, id: StreamId) {
        self.send(Command::Close { id });
    }

    pub(crate) fn refresh_machines(&self) {
        self.send(Command::RefreshMachines);
    }

    pub(crate) fn shutdown(&self) {
        self.send(Command::Shutdown);
    }
}

struct StreamEntry {
    request: AttachRequest,
    sink: StreamSink,
    channel: Option<u32>,
    automatic_input: bool,
    closing: bool,
}

enum Pending {
    Status,
    Machines,
    Subscribe,
    Attach(StreamId),
}

struct Session {
    number: u64,
    socket: CoreSocket,
    hello: HelloResult,
    next_rpc: u64,
    pending: HashMap<u64, Pending>,
    events_channel: Option<u32>,
    event_lines: Vec<u8>,
    channels: HashMap<u32, StreamId>,
}

enum SessionEnd {
    Shutdown,
    Lost(String),
}

struct Manager {
    config: CoreConnectionConfig,
    self_tx: Sender<Msg>,
    on_event: Box<dyn Fn(ConnectionEvent) + Send>,
    streams: HashMap<StreamId, StreamEntry>,
    sessions: u64,
}

impl Manager {
    fn new(
        config: CoreConnectionConfig,
        self_tx: Sender<Msg>,
        on_event: Box<dyn Fn(ConnectionEvent) + Send>,
    ) -> Self {
        Self {
            config,
            self_tx,
            on_event,
            streams: HashMap::new(),
            sessions: 0,
        }
    }

    fn run(mut self, rx: Receiver<Msg>) {
        let mut retry = FIRST_RETRY;
        loop {
            (self.on_event)(ConnectionEvent::Connecting);
            let error = match self.connect() {
                Ok(session) => {
                    retry = FIRST_RETRY;
                    match self.run_session(session, &rx) {
                        SessionEnd::Shutdown => return,
                        SessionEnd::Lost(error) => error,
                    }
                }
                Err(error) => error.to_string(),
            };
            (self.on_event)(ConnectionEvent::Disconnected { error });
            if self.wait_offline(&rx, retry) {
                return;
            }
            retry = (retry * 2).min(self.config.reconnect_max.max(FIRST_RETRY));
        }
    }

    /// Waits before the next attempt, refusing attaches meanwhile. Returns
    /// true on shutdown.
    fn wait_offline(&mut self, rx: &Receiver<Msg>, wait: Duration) -> bool {
        let deadline = std::time::Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return false;
            }
            match rx.recv_timeout(left) {
                Ok(Msg::Command(Command::Shutdown)) | Err(RecvTimeoutError::Disconnected) => {
                    return true
                }
                Ok(Msg::Command(Command::Attach { id, sink, .. })) => {
                    sink(StreamEvent::Attached {
                        id,
                        result: Box::new(Err(disconnected_error())),
                    });
                }
                Ok(_) | Err(RecvTimeoutError::Timeout) => {}
            }
        }
    }

    fn connect(&mut self) -> std::io::Result<Session> {
        let mut socket = connect_core_socket(&self.config.socket)?;
        socket.set_write_timeout(Some(WRITE_TIMEOUT))?;
        let reader = socket.try_clone()?;
        let mut hello_line = encode_request(
            0,
            "hello",
            json!({"generation": PROTOCOL_GENERATION, "framed": true, "resize_notifications": true}),
        );
        hello_line.push(b'\n');
        socket.write_all(&hello_line)?;
        let (hello, leftover, reader) = read_hello(reader)?;
        if hello.generation != PROTOCOL_GENERATION || !hello.framed {
            return Err(std::io::Error::other(format!(
                "core speaks protocol generation {} (framed: {}); Herdr needs generation {PROTOCOL_GENERATION}",
                hello.generation, hello.framed
            )));
        }
        self.sessions += 1;
        let number = self.sessions;
        let tx = self.self_tx.clone();
        crate::thread_spawn::spawn_named("herdr-core-reader", move || {
            read_frames(reader, leftover, number, tx);
        })?;
        Ok(Session {
            number,
            socket,
            hello,
            next_rpc: 1,
            pending: HashMap::new(),
            events_channel: None,
            event_lines: Vec::new(),
            channels: HashMap::new(),
        })
    }

    fn run_session(&mut self, mut session: Session, rx: &Receiver<Msg>) -> SessionEnd {
        let started = (|| {
            rpc(&mut session, Pending::Status, "server.status", json!({}))?;
            rpc(&mut session, Pending::Machines, "machine.list", json!({}))?;
            rpc(
                &mut session,
                Pending::Subscribe,
                "events.subscribe",
                json!({}),
            )?;
            Ok::<(), std::io::Error>(())
        })();
        if let Err(error) = started {
            return self.end_session(session, error.to_string());
        }
        loop {
            let msg = match rx.recv() {
                Ok(msg) => msg,
                Err(_) => {
                    let _ = session.socket.shutdown(std::net::Shutdown::Both);
                    return SessionEnd::Shutdown;
                }
            };
            let result = match msg {
                Msg::Command(Command::Shutdown) => {
                    let _ = session.socket.shutdown(std::net::Shutdown::Both);
                    return SessionEnd::Shutdown;
                }
                Msg::Command(command) => self.handle_command(&mut session, command),
                Msg::Frame {
                    session: number,
                    frame,
                } if number == session.number => self.handle_frame(&mut session, frame),
                Msg::ReaderEnded {
                    session: number,
                    error,
                } if number == session.number => {
                    return self.end_session(session, error);
                }
                Msg::Frame { .. } | Msg::ReaderEnded { .. } => Ok(()),
            };
            if let Err(error) = result {
                return self.end_session(session, error.to_string());
            }
        }
    }

    fn end_session(&mut self, session: Session, error: String) -> SessionEnd {
        let _ = session.socket.shutdown(std::net::Shutdown::Both);
        for (id, entry) in self.streams.drain() {
            if !entry.closing {
                (entry.sink)(StreamEvent::Interrupted { id });
            }
        }
        SessionEnd::Lost(error)
    }

    fn handle_command(&mut self, session: &mut Session, command: Command) -> std::io::Result<()> {
        match command {
            Command::Attach { id, request, sink } => {
                send_attach(session, id, &request)?;
                self.streams.insert(
                    id,
                    StreamEntry {
                        request,
                        sink,
                        channel: None,
                        automatic_input: false,
                        closing: false,
                    },
                );
            }
            Command::Input {
                id,
                bytes,
                automatic,
            } => {
                if let Some((channel, entry)) = self.live(id) {
                    if entry.request.role == StreamRole::Control {
                        let kind = if automatic && entry.automatic_input {
                            FrameKind::AutomaticInput
                        } else {
                            FrameKind::Data
                        };
                        write_frame(session, channel, kind, &bytes)?;
                    }
                }
            }
            Command::Resize { id, cols, rows } => {
                let Some(entry) = self.streams.get_mut(&id) else {
                    return Ok(());
                };
                entry.request.cols = cols;
                entry.request.rows = rows;
                if let (Some(channel), StreamRole::Control) = (entry.channel, entry.request.role) {
                    write_frame(
                        session,
                        channel,
                        FrameKind::Resize,
                        &resize_payload(cols, rows),
                    )?;
                }
            }
            Command::Focus { id, focused } => {
                if let Some((channel, entry)) = self.live(id) {
                    if entry.request.role == StreamRole::Control {
                        write_frame(session, channel, FrameKind::Focus, &[u8::from(focused)])?;
                    }
                }
            }
            Command::Facts { id, display } => {
                if let Some(entry) = self.streams.get_mut(&id) {
                    entry.request.display = Some(display.clone());
                }
                if let Some((channel, _)) = self.live(id) {
                    let payload = serde_json::to_vec(&display).unwrap_or_default();
                    write_frame(session, channel, FrameKind::Facts, &payload)?;
                }
            }
            Command::Consumed { id, bytes } => {
                if let Some((channel, _)) = self.live(id) {
                    write_frame(session, channel, FrameKind::Credit, &credit_payload(bytes))?;
                }
            }
            Command::Close { id } => {
                let Some(entry) = self.streams.get_mut(&id) else {
                    return Ok(());
                };
                match entry.channel {
                    Some(channel) => {
                        entry.closing = true;
                        write_frame(session, channel, FrameKind::Close, &[])?;
                    }
                    // Attach still in flight: close as soon as it answers.
                    None => entry.closing = true,
                }
            }
            Command::RefreshMachines => {
                rpc(session, Pending::Machines, "machine.list", json!({}))?;
            }
            Command::Shutdown => {}
        }
        Ok(())
    }

    fn live(&self, id: StreamId) -> Option<(u32, &StreamEntry)> {
        let entry = self.streams.get(&id)?;
        if entry.closing {
            return None;
        }
        Some((entry.channel?, entry))
    }

    fn handle_frame(&mut self, session: &mut Session, frame: Frame) -> std::io::Result<()> {
        if frame.channel == 0 {
            if frame.kind == FrameKind::Data {
                if let Ok(Incoming::Response { id, result }) = parse_incoming(&frame.payload) {
                    self.handle_response(session, id, result)?;
                }
            }
            return Ok(());
        }
        if Some(frame.channel) == session.events_channel {
            if frame.kind == FrameKind::Data {
                consume_event_lines(session, &frame.payload, &*self.on_event);
                write_frame(
                    session,
                    frame.channel,
                    FrameKind::Credit,
                    &credit_payload(frame.payload.len() as u32),
                )?;
            }
            return Ok(());
        }
        let Some(id) = session.channels.get(&frame.channel).copied() else {
            if frame.kind == FrameKind::Data {
                // A stream we already left: keep the core from stalling on it.
                write_frame(
                    session,
                    frame.channel,
                    FrameKind::Credit,
                    &credit_payload(frame.payload.len() as u32),
                )?;
            }
            return Ok(());
        };
        let Some(entry) = self.streams.get(&id) else {
            session.channels.remove(&frame.channel);
            return Ok(());
        };
        match frame.kind {
            FrameKind::Data => {
                if entry.request.role == StreamRole::Control || entry.closing {
                    write_frame(
                        session,
                        frame.channel,
                        FrameKind::Credit,
                        &credit_payload(frame.payload.len() as u32),
                    )?;
                } else {
                    (entry.sink)(StreamEvent::Data {
                        id,
                        bytes: frame.payload,
                    });
                }
            }
            FrameKind::Resize => {
                if let Some((cols, rows)) = parse_resize(&frame.payload) {
                    if !entry.closing {
                        (entry.sink)(StreamEvent::Resize { id, cols, rows });
                    }
                }
            }
            FrameKind::Close => {
                session.channels.remove(&frame.channel);
                if let Some(entry) = self.streams.remove(&id) {
                    if !entry.closing {
                        (entry.sink)(StreamEvent::Closed {
                            id,
                            reason: parse_close_reason(&frame.payload),
                        });
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_response(
        &mut self,
        session: &mut Session,
        rpc_id: u64,
        result: Result<Value, RpcError>,
    ) -> std::io::Result<()> {
        let Some(pending) = session.pending.remove(&rpc_id) else {
            return Ok(());
        };
        match pending {
            Pending::Status => {
                if let Some(status) = result
                    .ok()
                    .and_then(|value| serde_json::from_value::<StatusResult>(value).ok())
                {
                    (self.on_event)(ConnectionEvent::Status(status));
                }
            }
            Pending::Machines => {
                if let Some(list) = result
                    .ok()
                    .and_then(|value| serde_json::from_value::<MachineListResult>(value).ok())
                {
                    (self.on_event)(ConnectionEvent::Machines(list.machines));
                }
            }
            Pending::Subscribe => {
                let subscribed: SubscribeResult = match result {
                    Ok(value) => serde_json::from_value(value).map_err(std::io::Error::other)?,
                    Err(error) => {
                        return Err(std::io::Error::other(format!(
                            "events.subscribe failed: {}",
                            error.message
                        )))
                    }
                };
                session.events_channel = subscribed.channel;
                (self.on_event)(ConnectionEvent::Connected {
                    hello: session.hello.clone(),
                    seq: subscribed.seq,
                    terminals: subscribed.terminals,
                });
            }
            Pending::Attach(id) => {
                let attached = result.and_then(|value| {
                    serde_json::from_value::<AttachResult>(value).map_err(|error| RpcError {
                        code: 0,
                        message: error.to_string(),
                        kind: Some("invalid_reply".into()),
                        hint: None,
                    })
                });
                let Some(entry) = self.streams.get_mut(&id) else {
                    if let Ok(result) = attached {
                        write_frame(session, result.channel, FrameKind::Close, &[])?;
                    }
                    return Ok(());
                };
                match attached {
                    Ok(result) => {
                        session.channels.insert(result.channel, id);
                        entry.channel = Some(result.channel);
                        entry.automatic_input =
                            session.hello.has_feature("automatic_input") && result.automatic_input;
                        if entry.closing {
                            write_frame(session, result.channel, FrameKind::Close, &[])?;
                        } else {
                            (entry.sink)(StreamEvent::Attached {
                                id,
                                result: Box::new(Ok(result)),
                            });
                        }
                    }
                    Err(error) => {
                        if let Some(entry) = self.streams.remove(&id) {
                            if !entry.closing {
                                (entry.sink)(StreamEvent::Attached {
                                    id,
                                    result: Box::new(Err(error)),
                                });
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

fn disconnected_error() -> RpcError {
    RpcError {
        code: 0,
        message: "not connected to herdr-core".into(),
        kind: Some("disconnected".into()),
        hint: None,
    }
}

fn rpc(
    session: &mut Session,
    pending: Pending,
    method: &str,
    params: Value,
) -> std::io::Result<()> {
    let id = session.next_rpc;
    session.next_rpc += 1;
    session.pending.insert(id, pending);
    let payload = encode_request(id, method, params);
    write_frame(session, 0, FrameKind::Data, &payload)
}

fn send_attach(
    session: &mut Session,
    id: StreamId,
    request: &AttachRequest,
) -> std::io::Result<()> {
    let observe = request.role == StreamRole::Render;
    let mut params = json!({
        "terminal": request.path,
        "cols": request.cols,
        "rows": request.rows,
        "observe": observe,
        "resumed": request.resumed,
        "history": request.history,
        "resize_notifications": observe,
    });
    if let Some(display) = &request.display {
        params["display"] = display.clone();
    }
    rpc(session, Pending::Attach(id), "terminal.attach", params)
}

fn write_frame(
    session: &mut Session,
    channel: u32,
    kind: FrameKind,
    payload: &[u8],
) -> std::io::Result<()> {
    session
        .socket
        .write_all(&encode_frame(channel, kind, payload))
}

fn consume_event_lines(
    session: &mut Session,
    payload: &[u8],
    on_event: &(dyn Fn(ConnectionEvent) + Send),
) {
    session.event_lines.extend_from_slice(payload);
    while let Some(end) = session.event_lines.iter().position(|byte| *byte == b'\n') {
        let line: Vec<u8> = session.event_lines.drain(..=end).collect();
        if let Ok(Incoming::Notification(notification)) = parse_incoming(&line[..end]) {
            on_event(ConnectionEvent::Notification(notification));
        }
    }
}

fn read_hello(mut reader: CoreSocket) -> std::io::Result<(HelloResult, Vec<u8>, CoreSocket)> {
    reader.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let end = loop {
        if let Some(end) = buf.iter().position(|byte| *byte == b'\n') {
            break end;
        }
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "core closed the connection during hello",
            ));
        }
        buf.extend_from_slice(&chunk[..read]);
    };
    let leftover = buf.split_off(end + 1);
    let hello = match parse_incoming(&buf[..end]).map_err(std::io::Error::other)? {
        Incoming::Response {
            result: Ok(value), ..
        } => serde_json::from_value::<HelloResult>(value).map_err(std::io::Error::other)?,
        Incoming::Response {
            result: Err(error), ..
        } => {
            return Err(std::io::Error::other(format!(
                "core refused hello: {}",
                error.message
            )))
        }
        Incoming::Notification(_) => {
            return Err(std::io::Error::other(
                "core sent a notification before hello",
            ))
        }
    };
    reader.set_read_timeout(None)?;
    Ok((hello, leftover, reader))
}

fn read_frames(mut reader: CoreSocket, leftover: Vec<u8>, session: u64, tx: Sender<Msg>) {
    let mut decoder = FrameDecoder::default();
    decoder.push(&leftover);
    let mut chunk = vec![0u8; 64 * 1024];
    let error = 'read: loop {
        loop {
            match decoder.next_frame() {
                Ok(Some(frame)) => {
                    if tx.send(Msg::Frame { session, frame }).is_err() {
                        return;
                    }
                }
                Ok(None) => break,
                Err(error) => break 'read error.to_string(),
            }
        }
        match reader.read(&mut chunk) {
            Ok(0) => break "core closed the connection".to_string(),
            Ok(read) => decoder.push(&chunk[..read]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => break error.to_string(),
        }
    };
    let _ = tx.send(Msg::ReaderEnded { session, error });
}

#[cfg(all(test, unix))]
#[path = "connection_tests.rs"]
mod tests;
