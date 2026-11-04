//! A pane that shows a herdr-core terminal instead of owning a PTY.
//!
//! Rendering always comes from an observer stream (`observe`), so the core's
//! authoritative paints and in-stream RESIZE frames drive the emulator. A
//! control pane also holds a control stream used only to type, resize and
//! focus; its output is credited and dropped by the connection. Closing either
//! stream only detaches: the core terminal keeps running.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use bytes::Bytes;
use tokio::sync::{mpsc, Notify};

use super::{publish_terminal_bells, PaneTerminal, TerminalCompressionWake};
use crate::core_client::connection::{
    AttachRequest, CoreConnectionHandle, StreamEvent, StreamId, StreamRole, StreamSink,
};
use crate::core_client::protocol::{CloseReason, RpcError, Terminal};
use crate::core_client::resync::{RenderResync, ResyncAction, RESYNC_DEBOUNCE};
use crate::events::AppEvent;
use crate::layout::PaneId;
use crate::render_signal::RenderSignal;

pub(crate) use crate::core_client::model::CoreMode;

/// Where and how a core pane attaches.
#[derive(Debug, Clone)]
pub(crate) struct CoreAttachSpec {
    pub path: String,
    pub mode: CoreMode,
    /// False only when a person just opened it and it should take the size.
    pub resumed: bool,
    pub history: u32,
    pub display: Option<serde_json::Value>,
}

fn pack(a: u32, b: u32) -> u64 {
    (u64::from(a) << 32) | u64::from(b)
}

fn unpack(value: u64) -> (u32, u32) {
    ((value >> 32) as u32, value as u32)
}

/// State shared by the pane runtime (app thread) and its render thread.
pub(crate) struct CoreShared {
    pub(crate) mode: CoreMode,
    control: Mutex<Option<StreamId>>,
    render: Mutex<Option<StreamId>>,
    attached: AtomicBool,
    ended: AtomicBool,
    /// Whether every render attach asked for is done writing.
    pub(crate) settle: RenderSettle,
    visible: AtomicBool,
    focus_sent: AtomicBool,
    /// (cols, rows) the core grid has, which the emulator follows.
    grid: AtomicU64,
    /// (cols, rows) this pane would like (its rectangle).
    desired: AtomicU64,
    /// (width, height) of a cell in pixels.
    cell_px: AtomicU64,
}

impl CoreShared {
    fn new(mode: CoreMode, cols: u16, rows: u16) -> Self {
        Self {
            mode,
            control: Mutex::new(None),
            render: Mutex::new(None),
            attached: AtomicBool::new(false),
            ended: AtomicBool::new(false),
            visible: AtomicBool::new(false),
            focus_sent: AtomicBool::new(false),
            settle: RenderSettle::new(),
            grid: AtomicU64::new(pack(u32::from(cols), u32::from(rows))),
            desired: AtomicU64::new(pack(u32::from(cols), u32::from(rows))),
            cell_px: AtomicU64::new(0),
        }
    }

    pub(crate) fn grid(&self) -> (u16, u16) {
        let (cols, rows) = unpack(self.grid.load(Ordering::Acquire));
        (cols as u16, rows as u16)
    }

    pub(crate) fn cell_px(&self) -> (u32, u32) {
        unpack(self.cell_px.load(Ordering::Acquire))
    }

    fn control(&self) -> Option<StreamId> {
        *self
            .control
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn set_control(&self, id: Option<StreamId>) {
        *self
            .control
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = id;
    }

    fn render(&self) -> Option<StreamId> {
        *self
            .render
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn set_render(&self, id: Option<StreamId>) {
        *self
            .render
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = id;
    }

    pub(crate) fn accepts_input(&self) -> bool {
        self.mode == CoreMode::Control
            && self.attached.load(Ordering::Acquire)
            && !self.ended.load(Ordering::Acquire)
    }
}

/// Whether every render attach asked for so far was handled and nothing can
/// still write output. The app counts its requests; the render thread marks
/// the request it served once no stream or attach of it is left. An older
/// stream ending after a newer request never marks that request.
pub(crate) struct RenderSettle {
    requested: AtomicU64,
    idle: AtomicU64,
}

impl RenderSettle {
    /// The first attach is request 1, not yet served.
    pub(crate) fn new() -> Self {
        Self {
            requested: AtomicU64::new(1),
            idle: AtomicU64::new(0),
        }
    }

    /// A new render attach is asked for; returns its number.
    pub(crate) fn request(&self) -> u64 {
        self.requested.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// The render thread served request `generation` and holds no stream.
    pub(crate) fn idle_at(&self, generation: u64) {
        self.idle.fetch_max(generation, Ordering::AcqRel);
    }

    /// A request that never reached the render thread: nothing will write.
    pub(crate) fn abandon(&self) {
        self.idle
            .fetch_max(self.requested.load(Ordering::Acquire), Ordering::AcqRel);
    }

    pub(crate) fn settled(&self) -> bool {
        self.idle.load(Ordering::Acquire) >= self.requested.load(Ordering::Acquire)
    }
}

/// The pane's I/O side: what `PaneRuntimeIo::Core` does on the app thread.
pub(crate) struct CoreStreamIo {
    pub(crate) handle: CoreConnectionHandle,
    pub(crate) shared: Arc<CoreShared>,
    render_tx: std_mpsc::Sender<RenderMsg>,
}

impl CoreStreamIo {
    fn focus_once(&self, control: StreamId) {
        if !self.shared.focus_sent.swap(true, Ordering::AcqRel) {
            self.handle.focus(control, true);
        }
    }

    /// Keystrokes and pastes. Observers, pending and ended panes drop input
    /// quietly: an error here would warn on every key.
    pub(crate) fn send_input(&self, bytes: Bytes) {
        if !self.shared.accepts_input() {
            return;
        }
        if let Some(control) = self.shared.control() {
            self.focus_once(control);
            self.handle.input(control, bytes, false);
        }
    }

    /// Replies the emulator owes the program (query answers).
    pub(crate) fn send_terminal_response(&self, bytes: Bytes) {
        if bytes.is_empty() || !self.shared.accepts_input() {
            return;
        }
        if let Some(control) = self.shared.control() {
            self.handle.input(control, bytes, true);
        }
    }

    pub(crate) fn submit(
        &self,
        text: Bytes,
        enter: Bytes,
        delay: std::time::Duration,
    ) -> std::io::Result<std_mpsc::Receiver<std::io::Result<()>>> {
        if self.shared.mode == CoreMode::Observe {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "this core pane only observes",
            ));
        }
        if !self.shared.accepts_input() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "the core terminal is not attached",
            ));
        }
        let control = self.shared.control().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "the core terminal is not attached",
            )
        })?;
        self.focus_once(control);
        let handle = self.handle.clone();
        let (reply_tx, reply_rx) = std_mpsc::channel();
        crate::thread_spawn::spawn_named("herdr-core-submit", move || {
            handle.input(control, text, false);
            std::thread::sleep(delay);
            handle.input(control, enter, false);
            let _ = reply_tx.send(Ok(()));
        })?;
        Ok(reply_rx)
    }

    /// The pane's rectangle changed. The emulator follows the core grid, so
    /// only a shown control pane asks the core for its size, and only when
    /// the rectangle really changed (layout resizes run on every render).
    pub(crate) fn request_size(
        &self,
        rows: u16,
        cols: u16,
        cell_width_px: u32,
        cell_height_px: u32,
    ) {
        let desired = pack(u32::from(cols), u32::from(rows));
        let previous = self.shared.desired.swap(desired, Ordering::AcqRel);
        if cell_width_px > 0 && cell_height_px > 0 {
            self.shared
                .cell_px
                .store(pack(cell_width_px, cell_height_px), Ordering::Release);
        }
        if previous != desired {
            self.send_desired_size_if_owner();
        }
    }

    /// The pane gained or lost focus in Herdr. Gaining it takes the size back
    /// even when nothing was typed and the rectangle did not change.
    pub(crate) fn focus_changed(&self, gained: bool) {
        let Some(control) = self.shared.control() else {
            return;
        };
        if self.shared.mode != CoreMode::Control || !self.shared.accepts_input() {
            return;
        }
        if gained {
            self.send_desired_size_if_owner();
        }
        self.handle.focus(control, gained);
        self.shared.focus_sent.store(gained, Ordering::Release);
    }

    fn send_desired_size_if_owner(&self) {
        if self.shared.mode != CoreMode::Control || !self.shared.visible.load(Ordering::Acquire) {
            return;
        }
        let (cols, rows) = unpack(self.shared.desired.load(Ordering::Acquire));
        if let Some(control) = self.shared.control() {
            self.handle.resize(control, cols as u16, rows as u16);
        }
    }

    /// Shown to some client now or not. Coming into view takes the size back
    /// once (RESIZE + FOCUS); going out of view sends nothing.
    pub(crate) fn set_visible(&self, visible: bool) {
        let was = self.shared.visible.swap(visible, Ordering::AcqRel);
        if visible && !was && self.shared.accepts_input() {
            self.send_desired_size_if_owner();
            if let Some(control) = self.shared.control() {
                self.handle.focus(control, true);
                self.shared.focus_sent.store(true, Ordering::Release);
            }
        }
    }

    /// The core reported a new grid on the events channel.
    pub(crate) fn size_hint(&self) {
        let _ = self.render_tx.send(RenderMsg::SizeHint);
    }

    /// A snapshot says the core grid is `cols`x`rows`: when the emulator
    /// follows another grid, the render stream catches up as after a `size`.
    pub(crate) fn grid_hint(&self, cols: u16, rows: u16) {
        if cols == 0 || rows == 0 {
            return;
        }
        let wanted = (
            cols.max(super::MIN_PANE_COLS),
            rows.max(super::MIN_PANE_ROWS),
        );
        if self.shared.grid() != wanted {
            self.size_hint();
        }
    }

    pub(crate) fn reattach(&self, spec: CoreAttachSpec) {
        let generation = self.shared.settle.request();
        if self
            .render_tx
            .send(RenderMsg::Reattach(spec, generation))
            .is_err()
        {
            // No render thread: nothing will write for this request.
            self.shared.settle.abandon();
        }
    }

    pub(crate) fn shutdown(&self) {
        let _ = self.render_tx.send(RenderMsg::Shutdown);
        if let Some(control) = self.shared.control() {
            self.handle.close(control);
        }
        if let Some(render) = self.shared.render() {
            self.handle.close(render);
        }
        self.shared.set_control(None);
        self.shared.set_render(None);
    }
}

enum RenderMsg {
    Stream(StreamEvent),
    SizeHint,
    /// Attach again; the number is the app's request (see `RenderSettle`).
    Reattach(CoreAttachSpec, u64),
    Shutdown,
}

/// Everything the render thread needs to write into the pane's emulator,
/// the same way the PTY reader does, without touching the `PaneRuntime`.
pub(super) struct CoreRenderContext {
    pub(super) pane_id: PaneId,
    pub(super) terminal: Arc<PaneTerminal>,
    pub(super) response_writer: mpsc::Sender<Bytes>,
    pub(super) content_write_lock: Arc<Mutex<()>>,
    pub(super) content_seq: Arc<AtomicU64>,
    pub(super) compression_wake: TerminalCompressionWake,
    pub(super) events: mpsc::Sender<AppEvent>,
    pub(super) render_notify: Arc<Notify>,
    pub(super) render_dirty: Arc<RenderSignal>,
    pub(super) runtime: tokio::runtime::Handle,
}

impl CoreRenderContext {
    /// One chunk of program output, as the PTY reader handles it, minus cwd
    /// reports (a core terminal's folder comes from the core's record) and
    /// screen-based agent detection (the core reports status).
    fn write_output(&self, bytes: &[u8]) -> Vec<Bytes> {
        let guard = self
            .content_write_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.content_seq.fetch_add(1, Ordering::AcqRel);
        let result = self
            .terminal
            .process_pty_bytes(self.pane_id, 0, bytes, &self.response_writer);
        self.content_seq.fetch_add(1, Ordering::Release);
        drop(guard);
        self.compression_wake.wake();
        publish_terminal_bells(self.pane_id, result.terminal_bells, &self.events);
        let title_requested =
            result.terminal_title_changed && self.render_dirty.request_terminal_title(self.pane_id);
        let render_requested = result.request_render && self.render_dirty.request_pty(self.pane_id);
        if title_requested || render_requested {
            self.render_notify.notify_one();
        }
        if let Some(delay) = result.render_delay {
            let render_notify = self.render_notify.clone();
            let render_dirty = self.render_dirty.clone();
            let pane_id = self.pane_id;
            self.runtime.spawn(async move {
                tokio::time::sleep(delay).await;
                if render_dirty.request_pty(pane_id) {
                    render_notify.notify_one();
                }
            });
        }
        for content in result.clipboard_writes {
            let _ = self.events.try_send(AppEvent::ClipboardWrite { content });
        }
        result.terminal_responses
    }

    /// Changes the emulator to the core's grid with the same steps as a PTY
    /// pane resize. The emulator is never rebuilt.
    fn apply_grid(&self, shared: &CoreShared, cols: u16, rows: u16) -> Vec<Bytes> {
        let cols = cols.max(super::MIN_PANE_COLS);
        let rows = rows.max(super::MIN_PANE_ROWS);
        let previous = shared
            .grid
            .swap(pack(u32::from(cols), u32::from(rows)), Ordering::AcqRel);
        if previous == pack(u32::from(cols), u32::from(rows)) {
            return Vec::new();
        }
        let (cell_width, cell_height) = shared.cell_px();
        let guard = self
            .content_write_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.content_seq.fetch_add(1, Ordering::AcqRel);
        let responses = self.terminal.resize(rows, cols, cell_width, cell_height);
        self.content_seq.fetch_add(1, Ordering::Release);
        drop(guard);
        self.compression_wake.wake();
        if self.render_dirty.request_pty(self.pane_id) {
            self.render_notify.notify_one();
        }
        responses
    }

    fn send_event(&self, event: AppEvent) {
        if self.events.blocking_send(event).is_err() {
            tracing::debug!(pane = self.pane_id.raw(), "core pane event after app exit");
        }
    }
}

/// Starts the render thread and the first attaches; returns the pane's I/O.
pub(super) fn start(
    context: CoreRenderContext,
    handle: CoreConnectionHandle,
    spec: CoreAttachSpec,
    cols: u16,
    rows: u16,
) -> std::io::Result<CoreStreamIo> {
    let shared = Arc::new(CoreShared::new(spec.mode, cols, rows));
    let (render_tx, render_rx) = std_mpsc::channel();
    let thread_shared = shared.clone();
    let thread_handle = handle.clone();
    let sink_tx = Mutex::new(render_tx.clone());
    let sink: StreamSink = Arc::new(move |event| {
        let _ = sink_tx
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .send(RenderMsg::Stream(event));
    });
    let pane = context.pane_id.raw();
    crate::thread_spawn::spawn_named(&format!("herdr-core-{pane}"), move || {
        RenderThread {
            context,
            shared: thread_shared,
            handle: thread_handle,
            sink,
            path: String::new(),
            current: None,
            pending: None,
            resync: None,
            reported_end: false,
            generation: 1,
        }
        .run(render_rx, spec);
    })?;
    Ok(CoreStreamIo {
        handle,
        shared,
        render_tx,
    })
}

struct RenderThread {
    context: CoreRenderContext,
    shared: Arc<CoreShared>,
    handle: CoreConnectionHandle,
    sink: StreamSink,
    path: String,
    /// The render stream whose output the emulator shows.
    current: Option<StreamId>,
    /// A fresh render stream being attached to replace `current`.
    pending: Option<StreamId>,
    /// Present when the core cannot announce grid changes in the stream.
    resync: Option<RenderResync>,
    reported_end: bool,
    /// The app's render-attach request being served (see `RenderSettle`).
    generation: u64,
}

impl RenderThread {
    fn run(mut self, rx: std_mpsc::Receiver<RenderMsg>, spec: CoreAttachSpec) {
        self.attach(spec);
        loop {
            let msg = match self.resync.as_ref().and_then(RenderResync::deadline) {
                Some(deadline) => {
                    let wait = deadline.saturating_duration_since(Instant::now());
                    match rx.recv_timeout(wait) {
                        Ok(msg) => Some(msg),
                        Err(std_mpsc::RecvTimeoutError::Timeout) => None,
                        Err(std_mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                }
                None => match rx.recv() {
                    Ok(msg) => Some(msg),
                    Err(_) => return,
                },
            };
            match msg {
                None => self.tick(),
                Some(RenderMsg::Shutdown) => return,
                Some(RenderMsg::Stream(event)) => self.stream(event),
                Some(RenderMsg::SizeHint) => {
                    if let Some(resync) = self.resync.as_mut() {
                        resync.on_size_hint(Instant::now());
                    }
                }
                Some(RenderMsg::Reattach(spec, generation)) => {
                    self.generation = generation;
                    self.attach(spec);
                }
            }
        }
    }

    fn attach(&mut self, spec: CoreAttachSpec) {
        self.path = spec.path.clone();
        self.reported_end = false;
        self.shared.ended.store(false, Ordering::Release);
        self.shared.attached.store(false, Ordering::Release);
        self.shared.focus_sent.store(false, Ordering::Release);
        let (cols, rows) = unpack(self.shared.desired.load(Ordering::Acquire));
        if spec.mode == CoreMode::Control {
            let id = self.handle.attach(
                AttachRequest {
                    path: spec.path.clone(),
                    role: StreamRole::Control,
                    cols: cols as u16,
                    rows: rows as u16,
                    resumed: spec.resumed,
                    history: 0,
                    display: spec.display.clone(),
                },
                self.sink.clone(),
            );
            self.shared.set_control(Some(id));
        }
        self.current = None;
        self.pending = Some(self.attach_render(&spec.path, spec.history, spec.display));
    }

    fn attach_render(
        &self,
        path: &str,
        history: u32,
        display: Option<serde_json::Value>,
    ) -> StreamId {
        let (cols, rows) = unpack(self.shared.desired.load(Ordering::Acquire));
        self.handle.attach(
            AttachRequest {
                path: path.to_string(),
                role: StreamRole::Render,
                cols: cols as u16,
                rows: rows as u16,
                resumed: true,
                history,
                display,
            },
            self.sink.clone(),
        )
    }

    fn tick(&mut self) {
        let Some(resync) = self.resync.as_mut() else {
            return;
        };
        // Only a stream still open is replaced: after its end, a leftover
        // timer must not start an attach for a request already served.
        if resync.on_tick(Instant::now()) == ResyncAction::Reattach && self.current.is_some() {
            let path = self.path.clone();
            self.pending = Some(self.attach_render(&path, 0, None));
        }
    }

    fn respond(&self, responses: Vec<Bytes>) {
        if self.shared.mode != CoreMode::Control {
            return;
        }
        if let Some(control) = self.shared.control() {
            for response in responses.into_iter().filter(|bytes| !bytes.is_empty()) {
                self.handle.input(control, response, true);
            }
        }
    }

    fn stream(&mut self, event: StreamEvent) {
        match event {
            StreamEvent::Attached { id, result } => self.attached(id, *result),
            StreamEvent::Data { id, bytes } => {
                let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
                if Some(id) == self.current {
                    let responses = self.context.write_output(&bytes);
                    self.respond(responses);
                }
                // Credit after the bytes are written out (or dropped).
                self.handle.consumed(id, len);
            }
            StreamEvent::Resize { id, cols, rows } => {
                if Some(id) == self.current {
                    let responses = self.context.apply_grid(&self.shared, cols, rows);
                    self.respond(responses);
                }
            }
            StreamEvent::Closed { id, reason } => {
                if Some(id) == self.current {
                    self.end(reason, false);
                }
            }
            StreamEvent::Interrupted { id } => {
                if Some(id) == self.current
                    || Some(id) == self.pending
                    || Some(id) == self.shared.control()
                {
                    self.end(None, true);
                }
            }
        }
    }

    fn attached(
        &mut self,
        id: StreamId,
        result: Result<crate::core_client::protocol::AttachResult, RpcError>,
    ) {
        if Some(id) == self.shared.control() {
            match result {
                Ok(_) => self.shared.attached.store(true, Ordering::Release),
                Err(error) => {
                    self.shared.set_control(None);
                    self.report_attach(Err(error));
                }
            }
            return;
        }
        if Some(id) != self.pending {
            return;
        }
        self.pending = None;
        match result {
            Ok(attached) => {
                if let Some(old) = self.current.replace(id) {
                    self.handle.close(old);
                }
                self.shared.set_render(Some(id));
                let responses = self.context.apply_grid(
                    &self.shared,
                    attached.terminal.cols,
                    attached.terminal.rows,
                );
                self.respond(responses);
                match self.resync.as_mut() {
                    Some(resync) if resync.is_reattaching() => resync.on_reattached(Instant::now()),
                    Some(_) => {}
                    None if !attached.resize_notifications => {
                        self.resync = Some(RenderResync::new(RESYNC_DEBOUNCE));
                    }
                    None => {}
                }
                if self.shared.mode == CoreMode::Observe {
                    self.shared.attached.store(true, Ordering::Release);
                }
                self.report_attach(Ok(attached.terminal));
            }
            Err(error) => {
                if let Some(resync) = self.resync.as_mut() {
                    if resync.is_reattaching() && self.current.is_some() {
                        resync.on_reattach_failed();
                        return;
                    }
                }
                if self.current.is_none() {
                    // No render stream came of this request: nothing will write.
                    self.shared.settle.idle_at(self.generation);
                }
                self.report_attach(Err(error));
            }
        }
    }

    fn report_attach(&self, result: Result<Terminal, RpcError>) {
        self.context.send_event(AppEvent::CorePaneAttached {
            connection: self.handle.id(),
            pane_id: self.context.pane_id,
            result: Box::new(result),
        });
    }

    fn end(&mut self, reason: Option<CloseReason>, interrupted: bool) {
        self.shared.attached.store(false, Ordering::Release);
        if !interrupted {
            self.shared.ended.store(true, Ordering::Release);
        }
        self.current = None;
        self.pending = None;
        self.shared.set_render(None);
        if let Some(control) = self.shared.control() {
            if !interrupted {
                self.handle.close(control);
            }
        }
        self.shared.set_control(None);
        // Every byte of this request's streams is written: say so before the
        // app hears of the end.
        self.shared.settle.idle_at(self.generation);
        if self.reported_end {
            return;
        }
        self.reported_end = true;
        self.context.send_event(AppEvent::CorePaneStreamEnded {
            connection: self.handle.id(),
            pane_id: self.context.pane_id,
            reason,
            interrupted,
        });
    }
}

#[cfg(test)]
mod settle_tests {
    use super::RenderSettle;

    #[test]
    fn an_older_stream_ending_never_settles_a_newer_attach() {
        let settle = RenderSettle::new();
        assert!(!settle.settled(), "the first attach is still out");
        settle.idle_at(1);
        assert!(settle.settled());

        // A reattach is asked for; the old stream's end is handled after it.
        let second = settle.request();
        assert_eq!(second, 2);
        assert!(!settle.settled());
        settle.idle_at(1);
        assert!(!settle.settled(), "an old end settled a newer attach");
        settle.idle_at(2);
        assert!(settle.settled());

        // A request the render thread never got writes nothing.
        settle.request();
        assert!(!settle.settled());
        settle.abandon();
        assert!(settle.settled());
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::core_client::connection::{ConnectionEvent, CoreConnectionConfig};
    use crate::core_client::fake_core::{wait_for, FakeCore};
    use crate::core_client::wire::{parse_resize, resize_payload, FrameKind};
    use crate::pane::PaneRuntime;

    struct Harness {
        core: FakeCore,
        handle: CoreConnectionHandle,
        events: mpsc::Receiver<AppEvent>,
        events_tx: mpsc::Sender<AppEvent>,
    }

    fn harness(resize_notifications: bool) -> Harness {
        let core = FakeCore::start_with("core-a", resize_notifications);
        let connected = Arc::new(AtomicBool::new(false));
        let flag = connected.clone();
        let handle = CoreConnectionHandle::start(
            CoreConnectionConfig {
                socket: core.socket.clone(),
                reconnect_max: Duration::from_millis(200),
            },
            Box::new(move |event| {
                if matches!(event, ConnectionEvent::Connected { .. }) {
                    flag.store(true, Ordering::Release);
                }
            }),
        )
        .unwrap();
        wait_for("connected", || {
            connected.load(Ordering::Acquire).then_some(())
        });
        let (events_tx, events) = mpsc::channel(64);
        Harness {
            core,
            handle,
            events,
            events_tx,
        }
    }

    fn pane(h: &Harness, mode: CoreMode, resumed: bool) -> PaneRuntime {
        PaneRuntime::attach_core(
            PaneId::from_raw(7),
            24,
            80,
            0,
            crate::terminal_theme::TerminalTheme::default(),
            None,
            h.handle.clone(),
            CoreAttachSpec {
                path: "t1".into(),
                mode,
                resumed,
                history: 0,
                display: None,
            },
            h.events_tx.clone(),
            Arc::new(Notify::new()),
            Arc::new(RenderSignal::new()),
        )
        .unwrap()
    }

    fn attaches(h: &Harness) -> Vec<serde_json::Value> {
        h.core
            .attach_params()
            .into_iter()
            .map(|(_, params)| params)
            .collect()
    }

    /// Channel numbers the fake gave, in attach order (it counts from 2).
    fn channel_of(h: &Harness, observe: bool, nth: usize) -> u32 {
        let index = attaches(h)
            .iter()
            .enumerate()
            .filter(|(_, params)| params["observe"] == observe)
            .nth(nth)
            .map(|(index, _)| index)
            .unwrap();
        2 + index as u32
    }

    fn frames_on(h: &Harness, channel: u32) -> Vec<crate::core_client::wire::Frame> {
        h.core
            .frames(0)
            .into_iter()
            .filter(|frame| frame.channel == channel)
            .collect()
    }

    fn wait_attached(events: &mut mpsc::Receiver<AppEvent>) {
        wait_for("CorePaneAttached", || match events.try_recv() {
            Ok(AppEvent::CorePaneAttached { result, .. }) => result.is_ok().then_some(()),
            _ => None,
        });
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_control_pane_renders_the_observer_and_types_on_the_control_stream() {
        let mut h = harness(true);
        let runtime = pane(&h, CoreMode::Control, false);
        wait_attached(&mut h.events);
        let params = attaches(&h);
        assert_eq!(params.len(), 2);
        assert_eq!(
            (
                params[0]["observe"].as_bool(),
                params[0]["resumed"].as_bool()
            ),
            (Some(false), Some(false))
        );
        assert_eq!(
            (
                params[1]["observe"].as_bool(),
                params[1]["resize_notifications"].as_bool()
            ),
            (Some(true), Some(true))
        );
        let control = channel_of(&h, false, 0);
        let render = channel_of(&h, true, 0);
        wait_for("render paint", || {
            runtime.visible_text().contains("paint").then_some(())
        });

        runtime.try_send_bytes(Bytes::from_static(b"ls\r")).unwrap();
        wait_for("typed", || {
            frames_on(&h, control)
                .iter()
                .any(|frame| frame.kind == FrameKind::Data && &frame.payload[..] == b"ls\r")
                .then_some(())
        });
        // Focus goes once, before the first keystroke.
        assert!(frames_on(&h, control)
            .iter()
            .any(|frame| frame.kind == FrameKind::Focus));
        assert!(frames_on(&h, render)
            .iter()
            .all(|frame| frame.kind != FrameKind::Data && frame.kind != FrameKind::Resize));

        // Hidden: a new rectangle is remembered, not sent; the emulator keeps the core grid.
        runtime.resize(30, 100, 8, 16);
        std::thread::sleep(Duration::from_millis(100));
        assert!(frames_on(&h, control)
            .iter()
            .all(|frame| frame.kind != FrameKind::Resize));
        assert_eq!(runtime.current_size(), (24, 80));

        // Shown: the size is taken back once.
        let core_io = runtime.core_io().unwrap();
        core_io.set_visible(true);
        core_io.set_visible(true);
        wait_for("resize on show", || {
            frames_on(&h, control)
                .iter()
                .find(|frame| frame.kind == FrameKind::Resize)
                .and_then(|frame| parse_resize(&frame.payload))
        });
        let resizes = |h: &Harness| {
            frames_on(h, control)
                .iter()
                .filter(|f| f.kind == FrameKind::Resize)
                .count()
        };
        assert_eq!(resizes(&h), 1);
        // Focus back on a shown pane takes the size back without typing.
        assert!(runtime.try_send_focus_event(crate::ghostty::FocusEvent::Gained));
        wait_for("resize on focus", || (resizes(&h) == 2).then_some(()));

        // The core's in-stream RESIZE drives the emulator grid.
        h.core
            .push(0, render, FrameKind::Resize, &resize_payload(90, 20));
        wait_for("grid", || {
            (runtime.current_size() == (20, 90)).then_some(())
        });

        drop(runtime);
        wait_for("detach both", || {
            let closed = |channel| {
                frames_on(&h, channel)
                    .iter()
                    .any(|f| f.kind == FrameKind::Close)
            };
            (closed(control) && closed(render)).then_some(())
        });
        h.handle.shutdown();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_observer_never_types_resizes_or_answers() {
        let mut h = harness(true);
        let runtime = pane(&h, CoreMode::Observe, true);
        wait_attached(&mut h.events);
        assert_eq!(attaches(&h).len(), 1, "observers hold no control stream");
        runtime.try_send_bytes(Bytes::from_static(b"x")).unwrap();
        runtime.resize(40, 120, 8, 16);
        runtime.core_io().unwrap().set_visible(true);
        assert!(runtime
            .queue_user_input_submission(
                Bytes::from_static(b"x"),
                Bytes::from_static(b"\r"),
                Duration::ZERO,
                None
            )
            .is_err());
        std::thread::sleep(Duration::from_millis(100));
        let render = channel_of(&h, true, 0);
        assert!(frames_on(&h, render)
            .iter()
            .all(|frame| matches!(frame.kind, FrameKind::Credit | FrameKind::Close)));
        h.handle.shutdown();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn without_resize_notifications_a_size_hint_rebuilds_only_the_render_stream() {
        let mut h = harness(false);
        let runtime = pane(&h, CoreMode::Control, true);
        wait_attached(&mut h.events);
        let control = channel_of(&h, false, 0);
        let old_render = channel_of(&h, true, 0);
        // Old output after a grid change elsewhere: dropped once the new stream paints.
        h.core.push(0, old_render, FrameKind::Data, b"stale");
        runtime.core_io().unwrap().size_hint();
        runtime.core_io().unwrap().size_hint();
        wait_for("second render attach", || {
            (attaches(&h).len() == 3).then_some(())
        });
        wait_attached(&mut h.events);
        let new_render = channel_of(&h, true, 1);
        wait_for("old render closed", || {
            frames_on(&h, old_render)
                .iter()
                .any(|f| f.kind == FrameKind::Close)
                .then_some(())
        });
        assert!(
            frames_on(&h, control)
                .iter()
                .all(|f| f.kind != FrameKind::Close),
            "control stays"
        );
        // Output on the old channel after the switch never reaches the screen.
        h.core.push(0, old_render, FrameKind::Data, b"zzlate");
        h.core.push(0, new_render, FrameKind::Data, b"fresh");
        wait_for("fresh output", || {
            runtime.visible_text().contains("fresh").then_some(())
        });
        assert!(!runtime.visible_text().contains("zzlate"));
        runtime.try_send_bytes(Bytes::from_static(b"q")).unwrap();
        wait_for("typing still on control", || {
            frames_on(&h, control)
                .iter()
                .any(|f| f.kind == FrameKind::Data && &f.payload[..] == b"q")
                .then_some(())
        });
        h.handle.shutdown();
    }
}
