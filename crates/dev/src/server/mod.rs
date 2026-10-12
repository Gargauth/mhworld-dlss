//! The TCP server: listener, connections (a reader thread and a writer
//! thread each), authentication, dispatch, the log pump and the execution
//! points.

mod capture;
mod methods;
mod probes;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::ffi::c_void;
use std::io::{self, BufWriter, Read};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Weak};
use std::thread;
use std::time::{Duration, Instant};

use mhw_core::frame::{CameraFrame, FrameInfo, JitterSample};
use mhw_core::math::{camera_cut_metric, CutMetric};
use parking_lot::Mutex;
use serde_json::{json, Value};

use crate::exec::{Ctx, Queue, Step};
use crate::frame::{write_frame, FrameReader};
use crate::game::Game;
use crate::logline::LogFilter;
use crate::proto::{self, ErrorCode, Method, Point, RpcError, RpcResult};
use crate::trace::{clock, Recorder};
use crate::{token, DevConfig, Host, SlotContext};

/// Absolute deadline, from accept, for a client to send a complete `auth`
/// line; a peer trickling bytes is dropped when it passes.
const AUTH_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest line read before authentication (`auth` is ~100 bytes).
pub(crate) const PRE_AUTH_LINE: usize = 4 * 1024;
/// Connections served at once, authenticated or not; more are closed on
/// accept.
pub(crate) const MAX_CONNECTIONS: usize = 8;
/// Frames queued for one client's writer; events beyond it are dropped.
const OUTBOX: usize = 1024;
/// How often the log pump looks at the ring.
const LOG_PUMP: Duration = Duration::from_millis(50);

/// One part of a response.
#[derive(Debug)]
pub(crate) struct Part {
    pub result: Value,
    pub binary: Option<Vec<u8>>,
}

impl Part {
    pub fn json(result: Value) -> Vec<Part> {
        vec![Part {
            result,
            binary: None,
        }]
    }
}

struct Outbound {
    msg: Value,
    binary: Option<Vec<u8>>,
}

/// The client socket; while `deadline` is set every read gets the time
/// left until it as its timeout, and fails with `TimedOut` past it.
struct Timed {
    stream: TcpStream,
    deadline: Option<Instant>,
}

impl Read for Timed {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(deadline) = self.deadline {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "authentication deadline passed",
                ));
            }
            self.stream.set_read_timeout(Some(left))?;
        }
        self.stream.read(buf)
    }
}

/// One of the [`MAX_CONNECTIONS`] slots, released when the client's
/// thread ends.
struct ConnSlot(Arc<AtomicUsize>);

impl ConnSlot {
    fn take(count: &Arc<AtomicUsize>, max: usize) -> Option<ConnSlot> {
        count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < max).then_some(n + 1)
            })
            .ok()
            .map(|_| ConnSlot(count.clone()))
    }
}

impl Drop for ConnSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

struct LogSub {
    filter: LogFilter,
    next: u64,
    /// Lines that passed the filter but were dropped on a full outbox,
    /// reported as `missed` on the next line delivered.
    dropped: u64,
}

/// What became of an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Sent {
    Queued,
    /// The outbox was full; counted in `Conn::dropped`.
    Dropped,
    /// The client is gone.
    Gone,
}

/// One client.
pub(crate) struct Conn {
    id: u64,
    out: SyncSender<Outbound>,
    alive: AtomicBool,
    log: Mutex<Option<LogSub>>,
    watches: Mutex<HashMap<String, Arc<AtomicBool>>>,
    /// Events dropped on a full outbox since the connection opened.
    dropped: AtomicU64,
    /// Watch events dropped since the last watch event delivered; that
    /// one carries the count as `dropped`.
    watch_dropped: AtomicU64,
}

impl Conn {
    /// Send a response frame (waits for room).
    fn respond(&self, msg: Value, binary: Option<Vec<u8>>) -> bool {
        self.alive.load(Ordering::Acquire) && self.out.send(Outbound { msg, binary }).is_ok()
    }

    /// Send an event without waiting (game threads call this); a full
    /// outbox drops it (counted in `dropped`).
    pub fn event(&self, msg: Value) -> Sent {
        if !self.alive.load(Ordering::Acquire) {
            return Sent::Gone;
        }
        match self.out.try_send(Outbound { msg, binary: None }) {
            Ok(()) => Sent::Queued,
            Err(TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                Sent::Dropped
            }
            Err(TrySendError::Disconnected(_)) => Sent::Gone,
        }
    }

    /// Send a watch event, adding `dropped` when earlier watch events were
    /// lost. `false` once the client is gone.
    pub fn watch_event(&self, mut msg: Value) -> bool {
        let lost = self.watch_dropped.swap(0, Ordering::AcqRel);
        if lost > 0 {
            msg["dropped"] = json!(lost);
        }
        match self.event(msg) {
            Sent::Queued => true,
            Sent::Dropped => {
                self.watch_dropped.fetch_add(lost + 1, Ordering::AcqRel);
                true
            }
            Sent::Gone => false,
        }
    }

    /// Events dropped since the connection opened.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    fn close(&self) {
        self.alive.store(false, Ordering::Release);
        for (_, stop) in self.watches.lock().drain() {
            stop.store(true, Ordering::Release);
        }
        *self.log.lock() = None;
    }
}

/// What the last camera and slot points showed.
#[derive(Clone, Copy, Default)]
pub(crate) struct Stash {
    pub camera: Option<CameraFrame>,
    pub jitter: Option<JitterSample>,
    /// The camera change into `camera`, with its frame.
    pub cut: Option<(u64, CutMetric)>,
    pub slot: Option<FrameInfo>,
}

/// Results the capture service hands back, routed to the request waiting
/// for them.
pub(crate) struct Inbox<T> {
    waiting: Mutex<HashMap<u64, mpsc::Sender<T>>>,
}

impl<T> Default for Inbox<T> {
    fn default() -> Inbox<T> {
        Inbox {
            waiting: Mutex::new(HashMap::new()),
        }
    }
}

impl<T> Inbox<T> {
    pub fn register(&self, request: u64) -> Receiver<T> {
        let (tx, rx) = mpsc::channel();
        self.waiting.lock().insert(request, tx);
        rx
    }

    pub fn unregister(&self, request: u64) {
        self.waiting.lock().remove(&request);
    }

    pub fn is_waiting(&self) -> bool {
        !self.waiting.lock().is_empty()
    }

    /// Hand `item` to `request`'s waiter; dropped when nobody waits.
    pub fn deliver(&self, request: u64, item: T) {
        if let Some(tx) = self.waiting.lock().get(&request) {
            let _ = tx.send(item);
        }
    }
}

/// The server's state, shared by every thread.
pub(crate) struct Shared {
    pub config: DevConfig,
    token: String,
    pub host: Arc<dyn Host>,
    pub game: Arc<dyn Game>,
    pub queue: Queue,
    pub trace: Recorder,
    stash: Mutex<Stash>,
    last_frame: AtomicU64,
    conns: Mutex<Vec<Weak<Conn>>>,
    next_conn: AtomicU64,
    pub images: Inbox<crate::analysis::Image>,
    pub stats: Inbox<(u64, crate::analysis::ColourStats)>,
    next_request: AtomicU64,
    pub local_addr: SocketAddr,
    /// Clients being served (slots taken).
    serving: Arc<AtomicUsize>,
    /// [`AUTH_TIMEOUT`] in milliseconds; tests shorten it.
    pub(crate) auth_timeout_ms: AtomicU64,
}

/// `path` relative to `base` unless absolute.
pub(crate) fn resolve(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_owned()
    } else {
        base.join(path)
    }
}

impl Shared {
    /// Read or create the token, bind the listener and start the listener
    /// and log-pump threads.
    pub fn bind(
        config: DevConfig,
        host: Arc<dyn Host>,
        game: Arc<dyn Game>,
    ) -> io::Result<Arc<Shared>> {
        let token_path = resolve(&config.mod_folder, &config.token_file);
        let token = token::load_or_create(&token_path)?;
        let listener = TcpListener::bind(config.listen)?;
        let local_addr = listener.local_addr()?;
        let shared = Arc::new(Shared {
            config,
            token,
            host,
            game,
            queue: Queue::default(),
            trace: Recorder::default(),
            stash: Mutex::new(Stash::default()),
            last_frame: AtomicU64::new(0),
            conns: Mutex::new(Vec::new()),
            next_conn: AtomicU64::new(1),
            images: Inbox::default(),
            stats: Inbox::default(),
            next_request: AtomicU64::new(1),
            local_addr,
            serving: Arc::new(AtomicUsize::new(0)),
            auth_timeout_ms: AtomicU64::new(AUTH_TIMEOUT.as_millis() as u64),
        });
        let accept = shared.clone();
        thread::Builder::new()
            .name("mhw-dev-listen".into())
            .spawn(move || accept.listen(listener))?;
        let pump = Arc::downgrade(&shared);
        thread::Builder::new()
            .name("mhw-dev-log".into())
            .spawn(move || log_pump(pump))?;
        log::info!(target: "dev", "dev server listening on {local_addr} (token file {})", token_path.display());
        Ok(shared)
    }

    fn listen(self: Arc<Self>, listener: TcpListener) {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let Some(slot) = ConnSlot::take(&self.serving, MAX_CONNECTIONS) else {
                        log::warn!(target: "dev", "refusing a client: {MAX_CONNECTIONS} already connected");
                        let _ = stream.shutdown(Shutdown::Both);
                        continue;
                    };
                    let shared = self.clone();
                    let spawned =
                        thread::Builder::new()
                            .name("mhw-dev-client".into())
                            .spawn(move || {
                                shared.serve(stream);
                                drop(slot);
                            });
                    if let Err(error) = spawned {
                        log::warn!(target: "dev", "cannot serve a client: {error}");
                    }
                }
                Err(error) => {
                    log::warn!(target: "dev", "accept failed: {error}");
                    thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }

    /// A fresh id for a capture-service request.
    pub fn request_id(&self) -> u64 {
        self.next_request.fetch_add(1, Ordering::Relaxed)
    }

    /// The last frame number seen at the `frame` point.
    pub fn last_frame(&self) -> u64 {
        self.last_frame.load(Ordering::Relaxed)
    }

    /// The camera, jitter and slot frame info last seen.
    pub fn stash(&self) -> Stash {
        *self.stash.lock()
    }

    fn serve(self: Arc<Self>, stream: TcpStream) {
        let peer = stream
            .peer_addr()
            .map_or_else(|_| "?".to_owned(), |a| a.to_string());
        if let Err(error) = self.serve_inner(stream) {
            log::debug!(target: "dev", "client {peer}: {error}");
        }
        log::info!(target: "dev", "client {peer} disconnected");
    }

    fn serve_inner(self: &Arc<Self>, stream: TcpStream) -> io::Result<()> {
        let _ = stream.set_nodelay(true);
        let (tx, rx) = mpsc::sync_channel(OUTBOX);
        let writer = stream.try_clone()?;
        thread::Builder::new()
            .name("mhw-dev-write".into())
            .spawn(move || write_loop(writer, rx))?;
        let conn = Arc::new(Conn {
            id: self.next_conn.fetch_add(1, Ordering::Relaxed),
            out: tx,
            alive: AtomicBool::new(true),
            log: Mutex::new(None),
            watches: Mutex::new(HashMap::new()),
            dropped: AtomicU64::new(0),
            watch_dropped: AtomicU64::new(0),
        });
        let timeout = Duration::from_millis(self.auth_timeout_ms.load(Ordering::Relaxed));
        let mut reader = FrameReader::requests(Timed {
            stream,
            deadline: Some(Instant::now() + timeout),
        });
        reader.set_max_line(PRE_AUTH_LINE);
        if !self.authenticate(&conn, &mut reader) {
            conn.close();
            return Ok(());
        }
        reader.set_max_line(crate::frame::MAX_LINE);
        reader.get_mut().deadline = None;
        reader.get_ref().stream.set_read_timeout(None)?;
        self.conns.lock().push(Arc::downgrade(&conn));
        log::info!(target: "dev", "client {} authenticated", conn.id);
        let result = self.request_loop(&conn, &mut reader);
        conn.close();
        self.conns
            .lock()
            .retain(|c| c.strong_count() > 0 && !Weak::ptr_eq(c, &Arc::downgrade(&conn)));
        result
    }

    fn authenticate(&self, conn: &Conn, reader: &mut FrameReader<Timed>) -> bool {
        let frame = match reader.read_frame() {
            Ok(Some(frame)) => frame,
            Ok(None) => return false,
            Err(error) => {
                log::warn!(target: "dev", "dropping an unauthenticated client: {error}");
                conn.respond(
                    proto::error(&Value::Null, &RpcError::bad_request(error.to_string())),
                    None,
                );
                return false;
            }
        };
        let id = frame.msg.get("id").cloned().unwrap_or(Value::Null);
        let given = frame
            .msg
            .get("params")
            .and_then(|p| p.get("token"))
            .and_then(Value::as_str);
        let is_auth = frame.msg.get("method").and_then(Value::as_str) == Some(Method::Auth.name());
        if is_auth && given.is_some_and(|t| token::matches(t, &self.token)) {
            conn.respond(proto::ok(&id, self.auth_info(), false), None);
            return true;
        }
        log::warn!(target: "dev", "dev client failed to authenticate");
        let error = RpcError::new(
            ErrorCode::Unauthorized,
            "auth with the token from [Dev] TokenFile first",
        );
        conn.respond(proto::error(&id, &error), None);
        false
    }

    pub fn auth_info(&self) -> Value {
        json!({"build": "dev", "game_build": self.game.game_build(), "features": ["dev"]})
    }

    fn request_loop(
        self: &Arc<Self>,
        conn: &Arc<Conn>,
        reader: &mut FrameReader<Timed>,
    ) -> io::Result<()> {
        loop {
            let frame = match reader.read_frame() {
                Ok(Some(frame)) => frame,
                Ok(None) => return Ok(()),
                Err(error) => {
                    let recoverable = error.recoverable();
                    conn.respond(
                        proto::error(&Value::Null, &RpcError::bad_request(error.to_string())),
                        None,
                    );
                    if recoverable {
                        continue;
                    }
                    return Err(io::Error::other(error.to_string()));
                }
            };
            if !conn.is_alive() {
                return Ok(());
            }
            let request = match proto::parse_request(&frame.msg) {
                Ok(request) => request,
                Err((id, error)) => {
                    conn.respond(proto::error(&id, &error), None);
                    continue;
                }
            };
            log::debug!(target: "dev", method = request.method.name(), id = request.id.as_i64().unwrap_or(-1); "request");
            match self.handle(conn, &request) {
                Ok(parts) => {
                    let last = parts.len().saturating_sub(1);
                    for (i, part) in parts.into_iter().enumerate() {
                        conn.respond(proto::ok(&request.id, part.result, i < last), part.binary);
                    }
                }
                Err(error) => {
                    log::debug!(target: "dev", method = request.method.name(), code = error.code.as_str(); "request failed");
                    conn.respond(proto::error(&request.id, &error), None);
                }
            }
        }
    }

    /// Queue `sample` at `point` until it has produced `count` values,
    /// then return them; `timeout` if the point does not come often enough.
    /// `Ok(None)` from `sample` skips an occurrence.
    pub fn collect<T: Send + 'static>(
        &self,
        point: Point,
        count: usize,
        timeout: Duration,
        mut sample: impl FnMut(&Ctx<'_>) -> RpcResult<Option<T>> + Send + 'static,
    ) -> RpcResult<Vec<T>> {
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancelled = cancel.clone();
        let mut out = Vec::with_capacity(count);
        self.queue.push(
            point,
            Box::new(move |ctx| {
                if cancelled.load(Ordering::Acquire) {
                    return Step::Done;
                }
                match sample(ctx) {
                    Ok(Some(value)) => {
                        out.push(value);
                        if out.len() < count {
                            return Step::More;
                        }
                        let _ = tx.send(Ok(std::mem::take(&mut out)));
                        Step::Done
                    }
                    Ok(None) => Step::More,
                    Err(error) => {
                        let _ = tx.send(Err(error));
                        Step::Done
                    }
                }
            }),
        );
        rx.recv_timeout(timeout).unwrap_or_else(|_| {
            cancel.store(true, Ordering::Release);
            Err(RpcError::timeout(format!(
                "the {} point did not come {count} times within {:.1} s (is the game rendering?)",
                point.as_str(),
                timeout.as_secs_f64()
            )))
        })
    }

    fn pump_captures(&self) {
        let Some(service) = self.game.capture() else {
            return;
        };
        if self.images.is_waiting() {
            for image in service.poll() {
                self.images.deliver(image.request, image);
            }
        }
        if self.stats.is_waiting() {
            for (request, frame, stats) in service.poll_stats() {
                self.stats.deliver(request, (frame, stats));
            }
        }
    }

    pub fn on_frame(&self, frame: u64) {
        self.last_frame.store(frame, Ordering::Relaxed);
        self.trace.record("frame", frame, clock::now);
        self.queue.run(&Ctx::Frame { frame });
        self.pump_captures();
    }

    pub fn on_camera(&self, camera: &CameraFrame, jitter: &JitterSample) {
        {
            let mut s = self.stash.lock();
            if let Some(prev) = &s.camera {
                s.cut = Some((camera.frame, camera_cut_metric(prev, camera)));
            }
            s.camera = Some(*camera);
            s.jitter = Some(*jitter);
        }
        self.trace.record("camera", camera.frame, clock::now);
        self.queue.run(&Ctx::Camera { camera, jitter });
    }

    pub fn on_slot(&self, list: *mut c_void, slot: &SlotContext) {
        self.stash.lock().slot = Some(slot.frame);
        self.trace.record("slot", slot.frame.frame, clock::now);
        self.queue.run(&Ctx::Slot { list, slot });
    }

    pub fn on_present(&self, frame: u64) {
        self.trace.record("present", frame, clock::now);
        self.queue.run(&Ctx::Present { frame });
        self.pump_captures();
    }

    /// Connections still open.
    pub fn connections(&self) -> Vec<Arc<Conn>> {
        self.conns
            .lock()
            .iter()
            .filter_map(Weak::upgrade)
            .filter(|c| c.is_alive())
            .collect()
    }
}

fn write_loop(stream: TcpStream, rx: Receiver<Outbound>) {
    let mut out = BufWriter::new(&stream);
    while let Ok(frame) = rx.recv() {
        if write_frame(&mut out, &frame.msg, frame.binary.as_deref()).is_err() {
            break;
        }
    }
    drop(out);
    let _ = stream.shutdown(Shutdown::Both);
}

/// Streams new ring lines to subscribed clients.
fn log_pump(shared: Weak<Shared>) {
    use mhw_core::log::ring;
    loop {
        thread::sleep(LOG_PUMP);
        let Some(shared) = shared.upgrade() else {
            return;
        };
        let subscribed: Vec<Arc<Conn>> = shared
            .connections()
            .into_iter()
            .filter(|c| c.log.lock().is_some())
            .collect();
        let Some(from) = subscribed
            .iter()
            .filter_map(|c| c.log.lock().as_ref().map(|s| s.next))
            .min()
        else {
            continue;
        };
        let entries = ring::since(from);
        let Some(last) = entries.last().map(|e| e.seq) else {
            continue;
        };
        for conn in subscribed {
            let mut sub = conn.log.lock();
            let Some(sub) = sub.as_mut() else {
                continue;
            };
            // Lines that fell out of the ring before we read them, plus
            // lines dropped on a full outbox earlier.
            let mut missed = entries
                .first()
                .map_or(0, |e| e.seq.saturating_sub(sub.next))
                + sub.dropped;
            let next = sub.next;
            for e in entries
                .iter()
                .filter(|e| e.seq >= next && sub.filter.passes(e.level, &e.target))
            {
                let mut event = crate::logline::event(e.level, &e.target, &e.line, e.seq);
                if missed > 0 {
                    event["missed"] = json!(missed);
                }
                match conn.event(event) {
                    Sent::Queued => missed = 0,
                    Sent::Dropped => missed += 1,
                    Sent::Gone => break,
                }
            }
            sub.dropped = missed;
            sub.next = last + 1;
        }
    }
}
