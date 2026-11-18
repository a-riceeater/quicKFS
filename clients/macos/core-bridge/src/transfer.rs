// SPDX-License-Identifier: Apache-2.0
//! Background downloads and uploads with progress, retries, and resume.
//!
//! The app polls [`status`] rather than receiving callbacks, which keeps the
//! C ABI to plain request/response strings. Reads and writes are pipelined,
//! sized by [`Pipeline`] to the link the chunks are actually crossing, so
//! peak buffered data is the pipeline's in-flight bytes (at most
//! `max_depth x max_chunk`).
//!
//! A chunk that fails because the link stalled or dropped does not fail the
//! transfer: everything before it is already committed, so the transfer backs
//! off, lets the resilient layer reconnect, and carries on from the first
//! uncommitted byte. Only when nothing commits for the session's retry window
//! does it give up, and then it keeps its place (the hidden partial file for a
//! download, the written prefix for an upload) so [`resume`] can continue it.

use crate::{
    model::{BridgeError, BridgeResult, validate_name},
    pipeline::{Pipeline, Shape, Tuning},
    session::{READ_WRITE, Session, lookup},
};
use futures::{StreamExt, future::BoxFuture, stream::FuturesOrdered};
use quickfs_client_core::{
    ClientError, MAX_CLIENT_READ_SIZE, MAX_CLIENT_WRITE_SIZE, RemoteFilesystem,
};
use quickfs_protocol::{
    ErrorCode, FileAccess, FileHandle, FileOpenOptions, Name, NodeId, NodeKind,
};
use quickfs_transport_quic::TransportError;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    io::SeekFrom,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    // Tokio's clock, so tests can run retry backoff on paused time.
    time::Instant,
};
use uuid::Uuid;

const MAX_TRACKED_TRANSFERS: usize = 1_024;
/// Recent events kept per transfer for the app's detail view.
const MAX_EVENTS: usize = 64;
/// First retry waits this long. It must exceed the resilient layer's 2 s
/// reconnect backoff, or the retry just sees the cached reconnect failure.
const FIRST_RETRY_DELAY: Duration = Duration::from_millis(2_500);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);
/// Closing a handle on a link that just failed must not hold up the retry.
const CLOSE_GRACE: Duration = Duration::from_secs(5);
/// Files smaller than this read through one connection: opening the file on
/// every extra connection costs a round trip each, more than striping saves.
const STRIPE_MIN_BYTES: u64 = 8 * 1024 * 1024;
/// Resizes this close together are reported as one.
const SHAPE_SETTLE: Duration = Duration::from_secs(2);
/// A link that keeps nudging the size between two steps is reported at
/// most this often.
const SHAPE_REPORT_SPACING: Duration = Duration::from_secs(15);

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Clone, Debug)]
enum State {
    Running,
    Finished,
    /// Code, message, and whether [`resume`] can continue from where it stopped.
    Failed(String, String, bool),
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Level {
    Info,
    Warning,
    Error,
}

impl Level {
    fn name(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

struct Event {
    at: Duration,
    level: Level,
    message: String,
}

/// Where a stopped transfer can pick up again.
#[derive(Default)]
struct Checkpoint {
    /// Download steps, planned once so a resume does not walk the tree again.
    plan: Option<Arc<Vec<Planned>>>,
    /// Upload sources, sorted, with whether the source was a single file.
    files: Option<Arc<Vec<PathBuf>>>,
    /// Steps (download) or files (upload) fully done.
    next: usize,
    partial: Option<Partial>,
}

/// The file a transfer stopped inside of.
#[derive(Clone, Debug)]
enum Partial {
    Download {
        step: usize,
        path: PathBuf,
        revision: u64,
        committed: u64,
    },
    Upload {
        index: usize,
        node: NodeId,
        committed: u64,
    },
}

pub struct Progress {
    started: Instant,
    bytes: AtomicU64,
    total: AtomicU64,
    files: AtomicU64,
    files_total: AtomicU64,
    cancel: AtomicBool,
    /// The file being copied now, relative to the transfer's root.
    current: Mutex<String>,
    shape: Mutex<Shape>,
    /// The rate chunks are completing at, in bytes/s, when measured.
    link_rate: Mutex<Option<f64>>,
    retries: AtomicU64,
    last_commit: Mutex<Instant>,
    retry_at: Mutex<Option<Instant>>,
    events: Mutex<VecDeque<Event>>,
    /// In-flight bytes when the pipeline was last reported, so steady
    /// small adjustments do not flood the event list.
    logged_in_flight: AtomicU64,
    shape_logged_at: Mutex<Option<Instant>>,
    /// A resize not yet reported, and when the shape last changed: a burst
    /// of resizes is logged once, after it settles.
    pending_shape: Mutex<Option<(Shape, Option<f64>, Instant)>>,
    /// Connections replaced so far, to report reconnects as they happen.
    reconnects: AtomicU64,
    /// Connections the current file is striped across.
    connections: AtomicUsize,
    checkpoint: Mutex<Checkpoint>,
}

impl Progress {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            started: now,
            bytes: AtomicU64::new(0),
            total: AtomicU64::new(0),
            files: AtomicU64::new(0),
            files_total: AtomicU64::new(0),
            cancel: AtomicBool::new(false),
            current: Mutex::new(String::new()),
            shape: Mutex::new(Shape { depth: 0, chunk: 0 }),
            link_rate: Mutex::new(None),
            retries: AtomicU64::new(0),
            last_commit: Mutex::new(now),
            retry_at: Mutex::new(None),
            events: Mutex::new(VecDeque::new()),
            logged_in_flight: AtomicU64::new(0),
            shape_logged_at: Mutex::new(None),
            pending_shape: Mutex::new(None),
            reconnects: AtomicU64::new(u64::MAX),
            connections: AtomicUsize::new(1),
            checkpoint: Mutex::new(Checkpoint::default()),
        }
    }

    fn set_current(&self, name: impl Into<String>) {
        *lock(&self.current) = name.into();
    }

    fn check(&self) -> BridgeResult<()> {
        if self.cancel.load(Ordering::Acquire) {
            Err(BridgeError::Cancelled)
        } else {
            Ok(())
        }
    }

    fn event(&self, level: Level, message: impl Into<String>) {
        let mut events = lock(&self.events);
        if events.len() >= MAX_EVENTS {
            events.pop_front();
        }
        events.push_back(Event {
            at: self.started.elapsed(),
            level,
            message: message.into(),
        });
    }

    /// Bytes landed for good: count them and restart the retry window.
    /// Returns how long it had been since the previous commit.
    fn commit(&self, amount: u64) -> Duration {
        self.bytes.fetch_add(amount, Ordering::Relaxed);
        let gap = std::mem::replace(&mut *lock(&self.last_commit), Instant::now()).elapsed();
        self.settle_shape();
        gap
    }

    /// Report a connection the resilient layer replaced underneath us, which
    /// otherwise shows only as a pause.
    fn note_link(&self, filesystem: &dyn RemoteFilesystem, gap: Duration) {
        let Some(stats) = filesystem.link_stats() else {
            return;
        };
        let before = self.reconnects.swap(stats.reconnects, Ordering::Relaxed);
        if before != u64::MAX && stats.reconnects > before {
            self.event(
                Level::Warning,
                format!(
                    "Reconnected to the server after {} without data (connection {})",
                    seconds(gap),
                    stats.reconnects + 1
                ),
            );
        }
    }

    fn touch(&self) {
        *lock(&self.last_commit) = Instant::now();
    }

    fn since_commit(&self) -> Duration {
        lock(&self.last_commit).elapsed()
    }

    /// Publish the pipeline's shape. `report` logs it at once (the first
    /// shape, a cut after a failure); ordinary resizes are logged by
    /// [`Self::settle_shape`] once they stop changing.
    fn show_shape(&self, shape: Shape, rate: Option<f64>, report: Option<&str>) {
        *lock(&self.shape) = shape;
        if rate.is_some() {
            *lock(&self.link_rate) = rate;
        }
        match report {
            Some(why) => {
                *lock(&self.pending_shape) = None;
                self.log_shape(why, shape, rate);
            }
            None => *lock(&self.pending_shape) = Some((shape, rate, Instant::now())),
        }
    }

    fn settle_shape(&self) {
        let mut pending = lock(&self.pending_shape);
        let Some((shape, rate, changed)) = *pending else {
            return;
        };
        let spaced =
            lock(&self.shape_logged_at).is_none_or(|at| at.elapsed() >= SHAPE_REPORT_SPACING);
        if changed.elapsed() < SHAPE_SETTLE || !spaced {
            return;
        }
        *pending = None;
        drop(pending);
        // A step from 8 to 7 requests is noise; report halving or doubling.
        let logged = self.logged_in_flight.load(Ordering::Relaxed);
        let now = shape.in_flight();
        if now * 2 <= logged || now >= logged * 2 {
            self.log_shape("Settled", shape, rate);
        }
    }

    fn log_shape(&self, why: &str, shape: Shape, rate: Option<f64>) {
        self.logged_in_flight
            .store(shape.in_flight(), Ordering::Relaxed);
        *lock(&self.shape_logged_at) = Some(Instant::now());
        let rate = rate.map_or(String::new(), |rate| {
            format!(", link about {}/s", bytes(rate as u64))
        });
        let connections = self.connections.load(Ordering::Relaxed);
        let across = if connections > 1 {
            format!(" across {connections} connections")
        } else {
            String::new()
        };
        self.event(
            Level::Info,
            format!(
                "{why}: {} x {} in flight{across}{rate}",
                shape.depth,
                bytes(shape.chunk)
            ),
        );
    }

    fn save_partial(&self, partial: Partial) {
        lock(&self.checkpoint).partial = Some(partial);
    }

    fn take_partial(&self) -> Option<Partial> {
        lock(&self.checkpoint).partial.take()
    }

    fn step_done(&self, next: usize) {
        let mut checkpoint = lock(&self.checkpoint);
        checkpoint.next = next;
        checkpoint.partial = None;
        drop(checkpoint);
        self.touch();
    }

    /// Wait out a transient failure. `Ok(false)` means the retry window is
    /// spent and the caller should fail with the error.
    async fn wait_to_retry(
        &self,
        tuning: &Tuning,
        failures: &mut u32,
        error: &BridgeError,
        what: &str,
    ) -> BridgeResult<bool> {
        let idle = self.since_commit();
        if idle >= tuning.retry_window {
            self.event(
                Level::Error,
                format!(
                    "{what}: {}. Nothing has moved for {}, giving up",
                    explain(error, tuning),
                    seconds(idle)
                ),
            );
            return Ok(false);
        }
        *failures += 1;
        let delay = FIRST_RETRY_DELAY
            .saturating_mul(1 << (*failures - 1).min(4))
            .min(MAX_RETRY_DELAY)
            .min(tuning.retry_window - idle + Duration::from_millis(100));
        self.retries.fetch_add(1, Ordering::Relaxed);
        self.event(
            Level::Warning,
            format!(
                "{what}: {}. Retrying in {} (attempt {})",
                explain(error, tuning),
                seconds(delay),
                *failures
            ),
        );
        let until = Instant::now() + delay;
        *lock(&self.retry_at) = Some(until);
        let waited = async {
            while Instant::now() < until {
                self.check()?;
                tokio::time::sleep(Duration::from_millis(200).min(until - Instant::now())).await;
            }
            self.check()
        }
        .await;
        *lock(&self.retry_at) = None;
        waited.map(|()| true)
    }
}

/// Run an idempotent request, retrying it through transient failures.
async fn retrying<T, F, Fut>(
    progress: &Progress,
    tuning: &Tuning,
    what: &str,
    mut operation: F,
) -> BridgeResult<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, ClientError>>,
{
    let mut failures = 0;
    loop {
        progress.check()?;
        match operation().await {
            Ok(value) => return Ok(value),
            Err(error) if transient(&error) => {
                let error = BridgeError::from(error);
                if !progress
                    .wait_to_retry(tuning, &mut failures, &error, what)
                    .await?
                {
                    return Err(error);
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// Failures of the link or a busy server, which a later attempt can get past.
/// `AmbiguousMutation` is left out: only callers that know their write is
/// idempotent may retry it.
fn transient(error: &ClientError) -> bool {
    matches!(
        error,
        ClientError::Transport(_)
            | ClientError::Offline
            | ClientError::Server(ErrorCode::Timeout | ErrorCode::Busy | ErrorCode::Offline, _)
    )
}

fn resumable(error: &BridgeError) -> bool {
    match error {
        BridgeError::Client(error) => {
            transient(error) || matches!(error, ClientError::AmbiguousMutation)
        }
        BridgeError::Transport(_) => true,
        _ => false,
    }
}

/// The resilient layer's own per-request budget ran out: data may have been
/// moving the whole time, just not fast enough for the request's size.
fn client_deadline(error: &BridgeError) -> bool {
    matches!(error, BridgeError::Client(ClientError::Server(ErrorCode::Timeout, message))
        if message == CLIENT_DEADLINE_MESSAGE)
}

/// What `ResilientFilesystem` reports when one operation outlives its budget.
const CLIENT_DEADLINE_MESSAGE: &str = "resilient operation deadline exceeded";

/// What went wrong, in terms of the link rather than the code path.
fn explain(error: &BridgeError, tuning: &Tuning) -> String {
    let stall = seconds(tuning.stall_timeout);
    if client_deadline(error) {
        return "one request took longer than its time limit, the link is too slow for its size"
            .into();
    }
    match error {
        BridgeError::Client(ClientError::Transport(TransportError::Timeout))
        | BridgeError::Transport(TransportError::Timeout) => {
            format!("no data moved for {stall}, the link stalled")
        }
        BridgeError::Client(ClientError::Transport(TransportError::Connection(error)))
        | BridgeError::Transport(TransportError::Connection(error)) => {
            format!("the connection dropped ({error})")
        }
        BridgeError::Client(ClientError::Transport(error)) | BridgeError::Transport(error) => {
            format!("the link failed ({error})")
        }
        BridgeError::Client(ClientError::Offline) => "could not reconnect to the server".into(),
        BridgeError::Client(ClientError::AmbiguousMutation) => {
            "the connection dropped with writes in flight".into()
        }
        BridgeError::Client(ClientError::Server(ErrorCode::Timeout, _)) => {
            "the request ran past its deadline".into()
        }
        BridgeError::Client(ClientError::Server(ErrorCode::Busy, _)) => "the server is busy".into(),
        other => other.to_string(),
    }
}

/// Advice for a failed transfer, shown under the error.
fn hint(error: &BridgeError, tuning: &Tuning) -> Option<String> {
    let stall = seconds(tuning.stall_timeout);
    let window = seconds(tuning.retry_window);
    if client_deadline(error) {
        return Some(if tuning.adaptive {
            format!(
                "Requests kept outliving their time limit even at the smallest size, and nothing \
                 landed for {window}. The link is barely moving data. Resume continues from \
                 where this stopped."
            )
        } else {
            format!(
                "Each request is {} with {} in flight, more than this link can move within a \
                 request's time limit. Turn on Size requests to the link (or lower Largest \
                 request) in the server's Transfers settings, then Resume.",
                bytes(tuning.max_chunk),
                tuning.max_depth
            )
        });
    }
    match error {
        BridgeError::Client(
            ClientError::Transport(_) | ClientError::Offline | ClientError::AmbiguousMutation,
        )
        | BridgeError::Transport(_) => Some(format!(
            "The link to the server kept stalling (no data for {stall} at a time) and did not \
             recover within {window}. A ping can look fine while the path is relayed or \
             losing packets in one direction; on ZeroTier, check that `zerotier-cli peers` \
             lists the server with a DIRECT path. Resume continues from where this stopped."
        )),
        BridgeError::Client(ClientError::Server(ErrorCode::Timeout | ErrorCode::Busy, _)) => {
            Some(format!(
                "The server stayed busy or slow for {window}. Resume continues from where \
                 this stopped."
            ))
        }
        BridgeError::Client(ClientError::StaleRevision) => Some(
            "The file changed on the server during the transfer, so the copy would mix two \
             versions. Start the transfer again."
                .into(),
        ),
        _ => None,
    }
}

type Job = Arc<dyn Fn(Arc<Progress>) -> BoxFuture<'static, BridgeResult<()>> + Send + Sync>;

struct Transfer {
    progress: Arc<Progress>,
    state: Mutex<State>,
    tuning: Tuning,
    job: Job,
    hint: Mutex<Option<String>>,
    /// The failure in plain words, for the app's headline.
    reason: Mutex<Option<String>>,
}

static TRANSFERS: Mutex<Option<HashMap<u64, Arc<Transfer>>>> = Mutex::new(None);
static NEXT_TRANSFER: AtomicU64 = AtomicU64::new(1);

fn with_transfers<T>(body: impl FnOnce(&mut HashMap<u64, Arc<Transfer>>) -> T) -> T {
    let mut guard = lock(&TRANSFERS);
    body(guard.get_or_insert_with(HashMap::new))
}

fn transfer(id: u64) -> BridgeResult<Arc<Transfer>> {
    with_transfers(|transfers| transfers.get(&id).cloned()).ok_or(BridgeError::UnknownTransfer)
}

/// Register a transfer and run `job` on the shared runtime. `job` is kept
/// so [`resume`] can run it again; each run continues from the checkpoint the
/// last one left in its [`Progress`].
pub fn start<F, Fut>(
    runtime: &tokio::runtime::Runtime,
    tuning: Tuning,
    job: F,
) -> BridgeResult<Value>
where
    F: Fn(Arc<Progress>) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = BridgeResult<()>> + Send + 'static,
{
    let record = Arc::new(Transfer {
        progress: Arc::new(Progress::new()),
        state: Mutex::new(State::Running),
        tuning,
        job: Arc::new(move |progress| Box::pin(job(progress))),
        hint: Mutex::new(None),
        reason: Mutex::new(None),
    });
    let id = NEXT_TRANSFER.fetch_add(1, Ordering::Relaxed);
    with_transfers(|transfers| {
        if transfers.len() >= MAX_TRACKED_TRANSFERS {
            transfers.retain(|_, transfer| matches!(lock_state(transfer), State::Running));
        }
        if transfers.len() >= MAX_TRACKED_TRANSFERS {
            return Err(BridgeError::invalid("too many transfers are running"));
        }
        transfers.insert(id, Arc::clone(&record));
        Ok(())
    })?;
    run(runtime, record);
    Ok(json!({ "transfer": id }))
}

fn run(runtime: &tokio::runtime::Runtime, record: Arc<Transfer>) {
    let future = (record.job)(Arc::clone(&record.progress));
    runtime.spawn(async move {
        let outcome = future.await;
        let progress = &record.progress;
        let state = match outcome {
            Ok(()) => {
                progress.event(Level::Info, "Finished");
                State::Finished
            }
            Err(BridgeError::Cancelled) => {
                discard_partial(progress).await;
                progress.event(Level::Warning, "Cancelled");
                State::Cancelled
            }
            Err(error) => {
                let resumable = resumable(&error);
                if !resumable {
                    discard_partial(progress).await;
                }
                *lock(&record.hint) = hint(&error, &record.tuning);
                *lock(&record.reason) = Some(sentence(&explain(&error, &record.tuning)));
                progress.event(Level::Error, format!("Failed: {error}"));
                State::Failed(error.code(), error.to_string(), resumable)
            }
        };
        *lock(&record.state) = state;
    });
}

fn lock_state(transfer: &Transfer) -> State {
    lock(&transfer.state).clone()
}

pub fn status(id: u64) -> BridgeResult<Value> {
    let transfer = transfer(id)?;
    let progress = &transfer.progress;
    let shape = *lock(&progress.shape);
    let retry_in = lock(&progress.retry_at).map(|at| at.saturating_duration_since(Instant::now()));
    let events: Vec<Value> = lock(&progress.events)
        .iter()
        .map(|event| {
            json!({
                "at_ms": event.at.as_millis() as u64,
                "level": event.level.name(),
                "message": event.message,
            })
        })
        .collect();
    let state = lock_state(&transfer);
    let mut reply = json!({
        "bytes": progress.bytes.load(Ordering::Relaxed),
        "total": progress.total.load(Ordering::Relaxed),
        "files": progress.files.load(Ordering::Relaxed),
        "files_total": progress.files_total.load(Ordering::Relaxed),
        "current": lock(&progress.current).clone(),
        "chunk_bytes": shape.chunk,
        "pipeline_depth": shape.depth,
        "adaptive": transfer.tuning.adaptive,
        "connections": progress.connections.load(Ordering::Relaxed),
        "link_rate": *lock(&progress.link_rate),
        "retries": progress.retries.load(Ordering::Relaxed),
        "since_commit_ms": progress.since_commit().as_millis() as u64,
        "retry_in_ms": retry_in.map(|delay| delay.as_millis() as u64),
        "stall_timeout_ms": transfer.tuning.stall_timeout.as_millis() as u64,
        "retry_window_ms": transfer.tuning.retry_window.as_millis() as u64,
        "events": events,
    });
    let (name, error) = match state {
        State::Running => ("running", None),
        State::Finished => ("finished", None),
        State::Cancelled => ("cancelled", None),
        State::Failed(code, message, resumable) => {
            reply["resumable"] = json!(resumable);
            ("failed", Some(json!({ "code": code, "message": message })))
        }
    };
    reply["state"] = json!(name);
    if let Some(error) = error {
        reply["error"] = error;
    }
    if let Some(hint) = lock(&transfer.hint).clone() {
        reply["hint"] = json!(hint);
    }
    if let Some(reason) = lock(&transfer.reason).clone() {
        reply["reason"] = json!(reason);
    }
    Ok(reply)
}

/// Continue a failed transfer from its checkpoint.
pub fn resume(runtime: &tokio::runtime::Runtime, id: u64) -> BridgeResult<Value> {
    let record = transfer(id)?;
    {
        let mut state = lock(&record.state);
        if !matches!(*state, State::Failed(_, _, true)) {
            return Err(BridgeError::invalid(
                "only a stopped transfer can be resumed",
            ));
        }
        *state = State::Running;
    }
    *lock(&record.hint) = None;
    *lock(&record.reason) = None;
    record.progress.event(Level::Info, "Resuming");
    record.progress.touch();
    run(runtime, Arc::clone(&record));
    Ok(json!({}))
}

/// Cancel a running transfer, or discard a stopped one's partial data.
pub async fn cancel(id: u64) -> BridgeResult<Value> {
    let record = transfer(id)?;
    record.progress.cancel.store(true, Ordering::Release);
    let stopped = {
        let mut state = lock(&record.state);
        let stopped = matches!(*state, State::Failed(_, _, true));
        if stopped {
            *state = State::Cancelled;
        }
        stopped
    };
    if stopped {
        discard_partial(&record.progress).await;
    }
    Ok(json!({}))
}

pub async fn forget(id: u64) -> Value {
    let removed = with_transfers(|transfers| match transfers.get(&id) {
        Some(transfer) if !matches!(lock_state(transfer), State::Running) => transfers.remove(&id),
        _ => None,
    });
    if let Some(record) = &removed {
        discard_partial(&record.progress).await;
    }
    json!({ "forgotten": removed.is_some() })
}

/// Delete a download's hidden partial file. An upload's written prefix is
/// already the remote file, as it was before resume existed, so it stays.
async fn discard_partial(progress: &Progress) {
    if let Some(Partial::Download { path, .. }) = progress.take_partial() {
        let _ = tokio::fs::remove_file(path).await;
    }
}

/// Capitalise an explanation for use as a headline.
fn sentence(text: &str) -> String {
    let mut characters = text.chars();
    characters.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(characters).collect()
    })
}

fn bytes(amount: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if amount < 1_000 {
        return format!("{amount} B");
    }
    let mut value = amount as f64 / 1_000.0;
    let mut unit = 0;
    while value >= 1_000.0 && unit + 1 < UNITS.len() {
        value /= 1_000.0;
        unit += 1;
    }
    let precision = if value < 10.0 { 1 } else { 0 };
    format!("{value:.precision$} {}", UNITS[unit])
}

fn seconds(duration: Duration) -> String {
    let total = duration.as_secs_f64();
    if total < 10.0 {
        format!("{total:.1} s")
    } else if total < 120.0 {
        format!("{} s", total.round() as u64)
    } else {
        format!("{} min", (total / 60.0).round() as u64)
    }
}

fn limit(capability: u64, client: u64) -> u64 {
    capability.max(1).min(client)
}

// ---------------------------------------------------------------------------
// Download
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum Planned {
    Directory(PathBuf),
    File { node: NodeId, path: PathBuf },
    Symlink { node: NodeId, path: PathBuf },
}

fn partial_path(destination: &Path) -> PathBuf {
    let name = destination
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    destination.with_file_name(format!(".{name}.{}.qkfs-partial", Uuid::new_v4().simple()))
}

/// Walk the remote tree into a list of steps and size the transfer.
async fn plan_download(
    filesystem: &dyn RemoteFilesystem,
    tuning: &Tuning,
    node: NodeId,
    destination: PathBuf,
    progress: &Progress,
) -> BridgeResult<Vec<Planned>> {
    let root = retrying(progress, tuning, "Reading the item", || {
        filesystem.get_metadata(node)
    })
    .await?;
    let mut plan = Vec::new();
    match root.kind {
        NodeKind::Directory => {
            let mut pending = vec![(node, destination.clone())];
            plan.push(Planned::Directory(destination));
            while let Some((directory, local)) = pending.pop() {
                let entries = retrying(progress, tuning, "Listing a folder", || {
                    filesystem.list_directory(directory)
                })
                .await?;
                for entry in entries {
                    let name = local_name(&entry.name)?;
                    let path = local.join(name);
                    match entry.kind {
                        NodeKind::Directory => {
                            plan.push(Planned::Directory(path.clone()));
                            pending.push((entry.node, path));
                        }
                        NodeKind::File => {
                            progress
                                .total
                                .fetch_add(entry.metadata.size, Ordering::Relaxed);
                            progress.files_total.fetch_add(1, Ordering::Relaxed);
                            plan.push(Planned::File {
                                node: entry.node,
                                path,
                            });
                        }
                        NodeKind::Symlink => plan.push(Planned::Symlink {
                            node: entry.node,
                            path,
                        }),
                        // Devices, sockets, and FIFOs have no portable local copy.
                        _ => {}
                    }
                }
            }
        }
        NodeKind::File => {
            progress.total.store(root.size, Ordering::Relaxed);
            progress.files_total.store(1, Ordering::Relaxed);
            plan.push(Planned::File {
                node,
                path: destination,
            });
        }
        NodeKind::Symlink => plan.push(Planned::Symlink {
            node,
            path: destination,
        }),
        _ => {
            return Err(BridgeError::invalid(
                "this kind of item cannot be downloaded",
            ));
        }
    }
    Ok(plan)
}

/// Copy a remote file or directory tree to `destination`, which must not
/// exist unless `overwrite` is set (files only; directories merge).
pub async fn download(
    session: Arc<Session>,
    node: NodeId,
    destination: PathBuf,
    overwrite: bool,
    progress: Arc<Progress>,
) -> BridgeResult<()> {
    let filesystem = session.filesystem.as_ref();
    let tuning = session.tuning;
    let saved = lock(&progress.checkpoint).plan.clone();
    let plan = match saved {
        Some(plan) => plan,
        None => {
            let plan = Arc::new(
                plan_download(filesystem, &tuning, node, destination.clone(), &progress).await?,
            );
            lock(&progress.checkpoint).plan = Some(Arc::clone(&plan));
            plan
        }
    };
    let first = lock(&progress.checkpoint).next;
    let read_limit = limit(session.capabilities.max_read_size, MAX_CLIENT_READ_SIZE);
    // Extra connections log in in the background; each joins the file being
    // read as soon as it is up, so nothing waits on the logins.
    let (extras_ready, extras) = tokio::sync::watch::channel(Vec::new());
    if tuning.connections > 1 {
        let session = Arc::clone(&session);
        let progress = Arc::clone(&progress);
        tokio::spawn(async move {
            let (opened, shortfall) = session.extra_connections(tuning.connections - 1).await;
            if let Some(shortfall) = shortfall {
                progress.event(
                    Level::Warning,
                    format!("Using fewer connections: {shortfall}"),
                );
            }
            let _ = extras_ready.send(opened);
        });
    }
    for (index, step) in plan.iter().enumerate().skip(first) {
        progress.check()?;
        match step {
            Planned::Directory(path) => match tokio::fs::create_dir(path).await {
                Ok(()) => {}
                Err(error)
                    if error.kind() == std::io::ErrorKind::AlreadyExists && path.is_dir() => {}
                Err(error) => return Err(error.into()),
            },
            Planned::Symlink { node, path } => {
                let target = retrying(&progress, &tuning, "Reading a link", || {
                    filesystem.read_link(*node)
                })
                .await?;
                create_local_symlink(&target, path)?;
            }
            Planned::File { node, path } => {
                let shown = match path.strip_prefix(&destination) {
                    Ok(relative) if !relative.as_os_str().is_empty() => relative,
                    _ => path.file_name().map(Path::new).unwrap_or(path),
                };
                progress.set_current(shown.display().to_string());
                let target = DownloadTarget {
                    step: index,
                    node: *node,
                    destination: path,
                    overwrite,
                };
                download_file(
                    &session.filesystem,
                    &extras,
                    &tuning,
                    &target,
                    read_limit,
                    &progress,
                )
                .await?;
                progress.files.fetch_add(1, Ordering::Relaxed);
            }
        }
        progress.step_done(index + 1);
    }
    Ok(())
}

fn local_name(name: &Name) -> BridgeResult<&std::ffi::OsStr> {
    use std::os::unix::ffi::OsStrExt;
    validate_name(name.as_bytes())?;
    Ok(std::ffi::OsStr::from_bytes(name.as_bytes()))
}

fn create_local_symlink(target: &[u8], path: &Path) -> BridgeResult<()> {
    use std::os::unix::ffi::OsStrExt;
    std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(target), path)?;
    Ok(())
}

struct DownloadTarget<'a> {
    step: usize,
    node: NodeId,
    destination: &'a Path,
    overwrite: bool,
}

/// Open the file on each extra connection. A connection that cannot open it,
/// or sees a different revision, sits this file out rather than failing it.
async fn open_lanes(
    extras: &[Arc<dyn RemoteFilesystem>],
    tuning: &Tuning,
    node: NodeId,
    revision: u64,
    read_limit: u64,
    progress: &Progress,
) -> Vec<Lane> {
    let opened = futures::future::join_all(extras.iter().map(|filesystem| async move {
        (Arc::clone(filesystem), filesystem.open_file(node).await)
    }))
    .await;
    let mut lanes = Vec::new();
    for (filesystem, result) in opened {
        match result {
            Ok((handle, lane_revision, _)) if lane_revision == revision => {
                lanes.push(Lane::new(filesystem, handle, tuning, read_limit));
            }
            Ok((handle, _, _)) => {
                let _ = filesystem.close_file(handle).await;
                progress.event(
                    Level::Warning,
                    "A connection saw a newer revision; it sits this file out",
                );
            }
            Err(error) => progress.event(
                Level::Warning,
                format!("A connection could not open the file ({error}); it sits this file out"),
            ),
        }
    }
    lanes
}

async fn download_file(
    primary: &Arc<dyn RemoteFilesystem>,
    extras: &tokio::sync::watch::Receiver<Vec<Arc<dyn RemoteFilesystem>>>,
    tuning: &Tuning,
    target: &DownloadTarget<'_>,
    read_limit: u64,
    progress: &Progress,
) -> BridgeResult<()> {
    let filesystem = primary.as_ref();
    let destination = target.destination;
    if !target.overwrite && tokio::fs::symlink_metadata(destination).await.is_ok() {
        return Err(BridgeError::Io(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("'{}' already exists", destination.display()),
        )));
    }
    let (handle, revision, size) = retrying(progress, tuning, "Opening the file", || {
        filesystem.open_file(target.node)
    })
    .await?;

    // Pick up a partial file a stopped attempt left, unless the remote file
    // changed since, in which case its bytes would mix two versions.
    let saved = match progress.take_partial() {
        Some(Partial::Download {
            step,
            path,
            revision: saved_revision,
            committed,
        }) if step == target.step => {
            if saved_revision == revision || revision == 0 {
                Some((path, committed))
            } else {
                progress.event(
                    Level::Warning,
                    "The file changed on the server since the last attempt, starting it over",
                );
                progress.bytes.fetch_sub(committed, Ordering::Relaxed);
                let _ = tokio::fs::remove_file(path).await;
                None
            }
        }
        other => {
            if let Some(other) = other {
                progress.save_partial(other);
            }
            None
        }
    };
    let (partial, mut committed, mut local) = match saved {
        Some((path, committed)) => {
            let mut local = tokio::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .await?;
            local.set_len(committed).await?;
            local.seek(SeekFrom::Start(committed)).await?;
            progress.event(
                Level::Info,
                format!("Resuming at {} of {}", bytes(committed), bytes(size)),
            );
            (path, committed, local)
        }
        None => {
            let path = partial_path(destination);
            let local = tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .await?;
            (path, 0, local)
        }
    };

    let remote = RemoteRead { revision, size };
    let mut lanes = vec![Lane::new(Arc::clone(primary), handle, tuning, read_limit)];
    let mut joiner = (size - committed >= STRIPE_MIN_BYTES).then(|| Joiner {
        extras: extras.clone(),
        joined: 0,
        node: target.node,
        limit: read_limit,
    });
    progress.connections.store(1, Ordering::Relaxed);
    let copied = copy_to_local(
        &mut lanes,
        joiner.as_mut(),
        tuning,
        &remote,
        &mut local,
        &mut committed,
        progress,
    )
    .await;
    for lane in lanes.iter().skip(1) {
        let _ = tokio::time::timeout(CLOSE_GRACE, lane.filesystem.close_file(lane.handle)).await;
    }
    let copied = match copied {
        Ok(()) => {
            async {
                local.flush().await?;
                local.sync_all().await?;
                Ok::<(), BridgeError>(())
            }
            .await
        }
        Err(error) => Err(error),
    };
    // Every byte is already read and checked against the revision, so a
    // close that fails on a dying link does not undo the copy.
    let _ = tokio::time::timeout(CLOSE_GRACE, filesystem.close_file(handle)).await;
    drop(local);
    match copied {
        Ok(()) => {
            if let Err(error) = tokio::fs::rename(&partial, destination).await {
                let _ = tokio::fs::remove_file(&partial).await;
                return Err(error.into());
            }
            Ok(())
        }
        Err(error) => {
            if committed > 0 && resumable(&error) {
                progress.save_partial(Partial::Download {
                    step: target.step,
                    path: partial,
                    revision,
                    committed,
                });
            } else {
                let _ = tokio::fs::remove_file(&partial).await;
            }
            Err(error)
        }
    }
}

struct RemoteRead {
    revision: u64,
    size: u64,
}

/// One connection a download reads through: the file's handle on it, its
/// own pipeline (each connection has its own congestion window, so its own
/// rate), and how many of its reads are still running.
struct Lane {
    filesystem: Arc<dyn RemoteFilesystem>,
    handle: FileHandle,
    pipeline: Pipeline,
    running: Arc<AtomicU64>,
}

impl Lane {
    fn new(
        filesystem: Arc<dyn RemoteFilesystem>,
        handle: FileHandle,
        tuning: &Tuning,
        limit: u64,
    ) -> Self {
        Self {
            filesystem,
            handle,
            pipeline: Pipeline::new(*tuning, limit),
            running: Arc::new(AtomicU64::new(0)),
        }
    }
}

/// Extra connections still to join a file's lanes as they come up.
struct Joiner {
    extras: tokio::sync::watch::Receiver<Vec<Arc<dyn RemoteFilesystem>>>,
    joined: usize,
    node: NodeId,
    limit: u64,
}

/// The lanes' shapes as one figure for status and the log: requests summed,
/// the first lane's chunk size, and the rate summed.
fn combined(lanes: &[Lane]) -> (Shape, Option<f64>) {
    let depth = lanes.iter().map(|lane| lane.pipeline.shape().depth).sum();
    let chunk = lanes.first().map_or(0, |lane| lane.pipeline.shape().chunk);
    let rates: Vec<f64> = lanes
        .iter()
        .filter_map(|lane| lane.pipeline.rate())
        .collect();
    let rate = (!rates.is_empty()).then(|| rates.iter().sum());
    (Shape { depth, chunk }, rate)
}

/// Read `remote` from `committed` to its end into `local`, which is
/// positioned at `committed`, striping chunks across `lanes`. Chunks are
/// written strictly in order, so after any failure `committed` is exactly
/// what is safely on disk; a lane that runs ahead of a slow one buffers at
/// most twice the total in flight before it waits.
async fn copy_to_local(
    lanes: &mut Vec<Lane>,
    mut joiner: Option<&mut Joiner>,
    tuning: &Tuning,
    remote: &RemoteRead,
    local: &mut tokio::fs::File,
    committed: &mut u64,
    progress: &Progress,
) -> BridgeResult<()> {
    let (shape, _) = combined(lanes);
    progress.show_shape(shape, None, Some("Starting"));
    let mut failures = 0;
    loop {
        let mut next = *committed;
        let mut queued = 0_u64;
        let mut inflight = FuturesOrdered::new();
        let (lane, offset, error) = loop {
            // Bring in extra connections that finished logging in. Compared
            // by count: the channel closes once its one value is sent.
            if let Some(joiner) = joiner.as_deref_mut() {
                let fresh: Vec<_> = joiner
                    .extras
                    .borrow()
                    .iter()
                    .skip(joiner.joined)
                    .cloned()
                    .collect();
                if !fresh.is_empty() {
                    joiner.joined += fresh.len();
                    let opened = open_lanes(
                        &fresh,
                        tuning,
                        joiner.node,
                        remote.revision,
                        joiner.limit,
                        progress,
                    )
                    .await;
                    if !opened.is_empty() {
                        lanes.extend(opened);
                        progress.connections.store(lanes.len(), Ordering::Relaxed);
                        let (shape, rate) = combined(lanes);
                        progress.show_shape(shape, rate, Some("Joined"));
                    }
                }
            }
            // Top each lane up to its own depth, round robin, within the
            // buffering budget.
            let budget = 2 * combined(lanes).0.in_flight();
            loop {
                let mut pushed = false;
                for (index, lane) in lanes.iter().enumerate() {
                    let shape = lane.pipeline.shape();
                    if next >= remote.size
                        || lane.running.load(Ordering::Acquire) >= shape.depth as u64
                    {
                        continue;
                    }
                    let length = shape.chunk.min(remote.size - next);
                    if queued > 0 && queued + length > budget {
                        continue;
                    }
                    let offset = next;
                    next += length;
                    queued += length;
                    pushed = true;
                    let (filesystem, handle) = (Arc::clone(&lane.filesystem), lane.handle);
                    let running = Arc::clone(&lane.running);
                    running.fetch_add(1, Ordering::AcqRel);
                    inflight.push_back(async move {
                        let started = Instant::now();
                        let read = filesystem
                            .read_range_versioned(handle, offset, length)
                            .await;
                        running.fetch_sub(1, Ordering::AcqRel);
                        (
                            index,
                            offset,
                            length,
                            started.elapsed(),
                            Instant::now(),
                            read,
                        )
                    });
                }
                if !pushed {
                    break;
                }
            }
            let Some((index, offset, expected, latency, finished, read)) = inflight.next().await
            else {
                return Ok(());
            };
            progress.check()?;
            queued -= expected;
            match read {
                Ok(read) => {
                    // A revision of zero means the implementation does not report one.
                    if read.revision != 0
                        && remote.revision != 0
                        && read.revision != remote.revision
                    {
                        return Err(ClientError::StaleRevision.into());
                    }
                    local.write_all(&read.data).await?;
                    let landed = read.data.len() as u64;
                    *committed += landed;
                    let gap = progress.commit(landed);
                    progress.note_link(lanes[0].filesystem.as_ref(), gap);
                    failures = 0;
                    if lanes[index]
                        .pipeline
                        .complete(landed, latency, finished.into_std())
                        .is_some()
                    {
                        let (shape, rate) = combined(lanes);
                        progress.show_shape(shape, rate, None);
                    }
                    if landed < expected {
                        // The file shrank after open; what was read is the whole file.
                        return Ok(());
                    }
                }
                Err(error) if transient(&error) => break (index, offset, error),
                Err(error) => return Err(error.into()),
            }
        };
        // Abandon the rest of the wave: reads are idempotent, and every byte
        // past `committed` is read again. Dropped reads never finish, so
        // their lanes' counts start over.
        drop(inflight);
        for lane in lanes.iter() {
            lane.running.store(0, Ordering::Release);
        }
        let error = BridgeError::from(error);
        if lanes[lane].pipeline.fail().is_some() {
            let (shape, _) = combined(lanes);
            progress.show_shape(shape, None, Some("Cut back after a failure"));
        }
        let through = if lanes.len() > 1 {
            format!(" (connection {})", lane + 1)
        } else {
            String::new()
        };
        let what = format!("Chunk at {}{through}", bytes(offset));
        if !progress
            .wait_to_retry(tuning, &mut failures, &error, &what)
            .await?
        {
            return Err(error);
        }
    }
}

// ---------------------------------------------------------------------------
// Upload
// ---------------------------------------------------------------------------

/// Copy a local file or directory tree into `parent` as `name`. Existing
/// files are replaced only when `replace` is set; existing directories merge.
pub async fn upload(
    session: Arc<Session>,
    parent: NodeId,
    source: PathBuf,
    name: Name,
    replace: bool,
    progress: Arc<Progress>,
) -> BridgeResult<()> {
    let filesystem = session.filesystem.as_ref();
    let tuning = session.tuning;
    let write_limit = limit(session.capabilities.max_write_size, MAX_CLIENT_WRITE_SIZE);
    let metadata = tokio::fs::symlink_metadata(&source).await?;
    if metadata.is_file() {
        progress.total.store(metadata.len(), Ordering::Relaxed);
        progress.files_total.store(1, Ordering::Relaxed);
        progress.set_current(String::from_utf8_lossy(name.as_bytes()).into_owned());
        let target = UploadTarget {
            index: 0,
            parent,
            name: &name,
            source: &source,
            replace,
        };
        upload_file(filesystem, &tuning, &target, write_limit, &progress).await?;
        progress.files.store(1, Ordering::Relaxed);
        progress.step_done(1);
        let _ = filesystem.sync_directory(parent).await;
        return Ok(());
    }
    if !metadata.is_dir() {
        return Err(BridgeError::invalid(
            "only files and folders can be uploaded",
        ));
    }

    // Size the whole tree first so progress is meaningful from the start. A
    // resumed upload keeps the first walk's list and totals.
    let saved = lock(&progress.checkpoint).files.clone();
    let files = match saved {
        Some(files) => files,
        None => {
            let mut files = Vec::new();
            let mut pending = vec![source.clone()];
            while let Some(directory) = pending.pop() {
                progress.check()?;
                let mut reader = tokio::fs::read_dir(&directory).await?;
                while let Some(entry) = reader.next_entry().await? {
                    let kind = entry.file_type().await?;
                    if kind.is_dir() {
                        pending.push(entry.path());
                    } else if kind.is_file() {
                        let size = entry.metadata().await?.len();
                        progress.total.fetch_add(size, Ordering::Relaxed);
                        progress.files_total.fetch_add(1, Ordering::Relaxed);
                        files.push(entry.path());
                    }
                }
            }
            files.sort();
            let files = Arc::new(files);
            lock(&progress.checkpoint).files = Some(Arc::clone(&files));
            files
        }
    };

    let root = ensure_directory(
        filesystem,
        &tuning,
        &progress,
        parent,
        name,
        mode_of(&metadata),
    )
    .await?;
    let mut directories: HashMap<PathBuf, NodeId> = HashMap::from([(source.clone(), root)]);
    let first = lock(&progress.checkpoint).next;
    for (index, file) in files.iter().enumerate().skip(first) {
        progress.check()?;
        let relative = file
            .strip_prefix(&source)
            .map_err(|_| BridgeError::invalid("upload source changed during the walk"))?;
        progress.set_current(relative.display().to_string());
        let mut remote = root;
        let mut local = source.clone();
        if let Some(parent_path) = relative.parent() {
            for component in parent_path.components() {
                local.push(component);
                remote = match directories.get(&local) {
                    Some(node) => *node,
                    None => {
                        let mode = mode_of(&tokio::fs::metadata(&local).await?);
                        let node = ensure_directory(
                            filesystem,
                            &tuning,
                            &progress,
                            remote,
                            os_name(component.as_os_str())?,
                            mode,
                        )
                        .await?;
                        directories.insert(local.clone(), node);
                        node
                    }
                };
            }
        }
        let file_name = os_name(
            file.file_name()
                .ok_or_else(|| BridgeError::invalid("upload file has no name"))?,
        )?;
        let target = UploadTarget {
            index,
            parent: remote,
            name: &file_name,
            source: file,
            replace,
        };
        upload_file(filesystem, &tuning, &target, write_limit, &progress).await?;
        progress.files.fetch_add(1, Ordering::Relaxed);
        progress.step_done(index + 1);
    }
    for node in directories.values() {
        let _ = filesystem.sync_directory(*node).await;
    }
    let _ = filesystem.sync_directory(parent).await;
    Ok(())
}

fn os_name(name: &std::ffi::OsStr) -> BridgeResult<Name> {
    use std::os::unix::ffi::OsStrExt;
    let bytes = name.as_bytes().to_vec();
    validate_name(&bytes)?;
    Ok(Name::new(bytes))
}

fn mode_of(metadata: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o7777
}

/// Create a directory, or use the one already there. Safe to retry: a create
/// that landed before the link failed shows up as an existing directory.
async fn ensure_directory(
    filesystem: &dyn RemoteFilesystem,
    tuning: &Tuning,
    progress: &Progress,
    parent: NodeId,
    name: Name,
    mode: u32,
) -> BridgeResult<NodeId> {
    let mut failures = 0;
    loop {
        progress.check()?;
        match filesystem
            .create_directory(parent, name.clone(), mode | 0o700)
            .await
        {
            Ok(metadata) => return Ok(metadata.node),
            Err(ClientError::Server(ErrorCode::AlreadyExists, _)) => {
                let node = lookup(filesystem, parent, &name).await?;
                if filesystem.get_metadata(node).await?.kind != NodeKind::Directory {
                    return Err(ClientError::Server(
                        ErrorCode::NotDirectory,
                        String::from_utf8_lossy(name.as_bytes()).into_owned(),
                    )
                    .into());
                }
                return Ok(node);
            }
            Err(error) if transient(&error) || matches!(error, ClientError::AmbiguousMutation) => {
                let error = BridgeError::from(error);
                if !progress
                    .wait_to_retry(tuning, &mut failures, &error, "Creating a folder")
                    .await?
                {
                    return Err(error);
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
}

struct UploadTarget<'a> {
    index: usize,
    parent: NodeId,
    name: &'a Name,
    source: &'a Path,
    replace: bool,
}

const REWRITE: FileOpenOptions = FileOpenOptions {
    access: FileAccess::ReadWrite,
    truncate: true,
    append: false,
};

/// Create (or, with `replace`, truncate) the remote file and return its node
/// and an open handle. A create whose reply was lost is retried; if it had
/// landed, the file it made is still empty, which is how it is recognised.
async fn create_remote(
    filesystem: &dyn RemoteFilesystem,
    tuning: &Tuning,
    progress: &Progress,
    target: &UploadTarget<'_>,
    mode: u32,
) -> BridgeResult<(NodeId, FileHandle)> {
    let mut failures = 0;
    let mut ambiguous = false;
    loop {
        progress.check()?;
        let error = match filesystem
            .create_file(target.parent, target.name.clone(), mode, READ_WRITE)
            .await
        {
            Ok(created) => return Ok((created.metadata.node, created.opened.handle)),
            Err(ClientError::Server(ErrorCode::AlreadyExists, message)) => {
                let node = lookup(filesystem, target.parent, target.name).await?;
                let ours = ambiguous && filesystem.get_metadata(node).await?.size == 0;
                if !target.replace && !ours {
                    return Err(ClientError::Server(ErrorCode::AlreadyExists, message).into());
                }
                match filesystem.open_file_with_options(node, REWRITE).await {
                    Ok(opened) => return Ok((node, opened.handle)),
                    // Truncating to empty again is harmless, so retry it too.
                    Err(error)
                        if transient(&error) || matches!(error, ClientError::AmbiguousMutation) =>
                    {
                        error
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            Err(error) if transient(&error) || matches!(error, ClientError::AmbiguousMutation) => {
                ambiguous = true;
                error
            }
            Err(error) => return Err(error.into()),
        };
        let error = BridgeError::from(error);
        if !progress
            .wait_to_retry(tuning, &mut failures, &error, "Creating the file")
            .await?
        {
            return Err(error);
        }
    }
}

async fn upload_file(
    filesystem: &dyn RemoteFilesystem,
    tuning: &Tuning,
    target: &UploadTarget<'_>,
    write_limit: u64,
    progress: &Progress,
) -> BridgeResult<()> {
    let mut local = tokio::fs::File::open(target.source).await?;
    let mode = mode_of(&local.metadata().await?) & 0o777;
    let (node, handle, committed) = match progress.take_partial() {
        Some(Partial::Upload {
            index,
            node,
            committed,
        }) if index == target.index => {
            progress.event(Level::Info, format!("Resuming at {}", bytes(committed)));
            (node, None, committed)
        }
        other => {
            if let Some(other) = other {
                progress.save_partial(other);
            }
            let (node, handle) = create_remote(filesystem, tuning, progress, target, mode).await?;
            (node, Some(handle), 0)
        }
    };
    let destination = WriteTarget {
        index: target.index,
        node,
        base: 0,
        sync: true,
    };
    let written = write_from_local(
        filesystem,
        tuning,
        &destination,
        handle,
        &mut local,
        committed,
        write_limit,
        progress,
    )
    .await?;
    let _ = tokio::time::timeout(CLOSE_GRACE, filesystem.close_file(written)).await;
    Ok(())
}

/// Where [`write_from_local`] writes: file bytes `n..` go to remote offset
/// `base + n`, and `sync` asks for durability before it returns.
struct WriteTarget {
    index: usize,
    node: NodeId,
    base: u64,
    sync: bool,
}

/// Write `local` from byte `committed` to its end into `target.node`,
/// pipelining positioned writes, then flush (and sync). Returns the handle
/// still open, for the caller to close.
///
/// Positioned writes are idempotent, so when the link fails mid-wave the
/// writer reopens the file (never truncating) and rewrites from the last
/// in-order acknowledged byte. That assumes no one else writes this range
/// meanwhile, which holds for a file this transfer created or truncated.
#[allow(clippy::too_many_arguments)]
async fn write_from_local(
    filesystem: &dyn RemoteFilesystem,
    tuning: &Tuning,
    target: &WriteTarget,
    mut handle: Option<FileHandle>,
    local: &mut tokio::fs::File,
    mut committed: u64,
    write_limit: u64,
    progress: &Progress,
) -> BridgeResult<FileHandle> {
    let mut pipeline = Pipeline::new(*tuning, write_limit);
    progress.show_shape(pipeline.shape(), None, Some("Starting"));
    let reopen = FileOpenOptions {
        access: FileAccess::ReadWrite,
        truncate: false,
        append: false,
    };
    let mut failures = 0;
    loop {
        let current = match handle.take() {
            Some(handle) => handle,
            None => {
                retrying(progress, tuning, "Reopening the file", || {
                    filesystem.open_file_with_options(target.node, reopen)
                })
                .await?
                .handle
            }
        };
        let outcome = write_wave(
            filesystem,
            current,
            target,
            local,
            &mut committed,
            &mut pipeline,
            progress,
        )
        .await;
        let error = match outcome {
            Ok(()) => {
                failures = 0;
                let finished = async {
                    filesystem.flush_file(current, None).await?;
                    if target.sync {
                        filesystem.sync_file(current, false).await?;
                    }
                    Ok::<(), ClientError>(())
                }
                .await;
                match finished {
                    Ok(()) => return Ok(current),
                    Err(error)
                        if transient(&error) || matches!(error, ClientError::AmbiguousMutation) =>
                    {
                        error
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            Err(WaveError::Retry(error)) => error,
            Err(WaveError::Fatal(error)) => {
                save_upload(progress, target, committed);
                return Err(error);
            }
        };
        let _ = tokio::time::timeout(CLOSE_GRACE, filesystem.close_file(current)).await;
        let error = BridgeError::from(error);
        if let Some(shape) = pipeline.fail() {
            progress.show_shape(shape, None, Some("Cut back after a failure"));
        }
        let what = format!("Write at {}", bytes(committed));
        match progress
            .wait_to_retry(tuning, &mut failures, &error, &what)
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                save_upload(progress, target, committed);
                return Err(error);
            }
            Err(cancelled) => return Err(cancelled),
        }
    }
}

fn save_upload(progress: &Progress, target: &WriteTarget, committed: u64) {
    progress.save_partial(Partial::Upload {
        index: target.index,
        node: target.node,
        committed,
    });
}

enum WaveError {
    Retry(ClientError),
    Fatal(BridgeError),
}

impl<E: Into<BridgeError>> From<E> for WaveError {
    fn from(error: E) -> Self {
        Self::Fatal(error.into())
    }
}

/// One pass of pipelined writes from `committed` to the end of `local`.
async fn write_wave(
    filesystem: &dyn RemoteFilesystem,
    handle: FileHandle,
    target: &WriteTarget,
    local: &mut tokio::fs::File,
    committed: &mut u64,
    pipeline: &mut Pipeline,
    progress: &Progress,
) -> Result<(), WaveError> {
    local.seek(SeekFrom::Start(*committed)).await?;
    let mut position = *committed;
    let mut exhausted = false;
    let mut inflight = FuturesOrdered::new();
    loop {
        let shape = pipeline.shape();
        while !exhausted && inflight.len() < shape.depth {
            progress.check()?;
            let capacity = usize::try_from(shape.chunk).unwrap_or(usize::MAX);
            let mut buffer = vec![0_u8; capacity];
            let mut filled = 0;
            while filled < capacity {
                let amount = local.read(&mut buffer[filled..]).await?;
                if amount == 0 {
                    exhausted = true;
                    break;
                }
                filled += amount;
            }
            if filled == 0 {
                break;
            }
            buffer.truncate(filled);
            let at = target
                .base
                .checked_add(position)
                .ok_or_else(|| BridgeError::invalid("write offset overflows"))?;
            position += filled as u64;
            inflight.push_back(async move {
                let started = Instant::now();
                let result = filesystem.write_range(handle, at, &buffer).await;
                (buffer.len() as u64, started, result)
            });
        }
        let Some((expected, started, result)) = inflight.next().await else {
            return Ok(());
        };
        progress.check()?;
        let result = match result {
            Ok(result) => result,
            Err(error) if transient(&error) || matches!(error, ClientError::AmbiguousMutation) => {
                return Err(WaveError::Retry(error));
            }
            Err(error) => return Err(error.into()),
        };
        if result.written != expected {
            return Err(BridgeError::invalid("the server reported a partial write").into());
        }
        *committed += result.written;
        let gap = progress.commit(result.written);
        progress.note_link(filesystem, gap);
        if let Some(shape) =
            pipeline.complete(result.written, started.elapsed(), Instant::now().into_std())
        {
            progress.show_shape(shape, pipeline.rate(), None);
        }
    }
}

/// The CLI `write` command: overwrite part of an existing remote file with a
/// local file's bytes at `offset`.
pub async fn write_at(
    session: Arc<Session>,
    node: NodeId,
    source: PathBuf,
    offset: u64,
    sync: bool,
    progress: Arc<Progress>,
) -> BridgeResult<()> {
    let filesystem = session.filesystem.as_ref();
    let tuning = session.tuning;
    if let Some(name) = source.file_name() {
        progress.set_current(name.to_string_lossy().into_owned());
    }
    let mut local = tokio::fs::File::open(&source).await?;
    progress
        .total
        .store(local.metadata().await?.len(), Ordering::Relaxed);
    progress.files_total.store(1, Ordering::Relaxed);
    let committed = match progress.take_partial() {
        Some(Partial::Upload { committed, .. }) => committed,
        _ => 0,
    };
    let target = WriteTarget {
        index: 0,
        node,
        base: offset,
        sync,
    };
    let write_limit = limit(session.capabilities.max_write_size, MAX_CLIENT_WRITE_SIZE);
    let handle = write_from_local(
        filesystem,
        &tuning,
        &target,
        None,
        &mut local,
        committed,
        write_limit,
        &progress,
    )
    .await?;
    let _ = tokio::time::timeout(CLOSE_GRACE, filesystem.close_file(handle)).await;
    progress.files.store(1, Ordering::Relaxed);
    progress.step_done(1);
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use quickfs_client_core::{CreatedFile, OpenedFile, RangeRead, WriteResult};
    use quickfs_protocol::{DirectoryEntry, FilesystemCapabilities, Metadata, ROOT_NODE};
    use std::sync::atomic::AtomicUsize;

    const MIB: u64 = 1024 * 1024;

    /// An in-memory server whose reads and writes fail on request, the way a
    /// stalled link makes them fail.
    #[derive(Default)]
    struct FlakyFs {
        files: Mutex<HashMap<NodeId, Vec<u8>>>,
        names: Mutex<HashMap<Vec<u8>, NodeId>>,
        handles: Mutex<HashMap<FileHandle, NodeId>>,
        /// Reads at or past this offset time out while `stall_reads` is set.
        stall_from: AtomicU64,
        stall_reads: AtomicBool,
        /// Fail this many reads (past `stall_from`) and then recover.
        failing_reads: AtomicUsize,
        /// Apply this many writes past `stall_from` but report the reply lost.
        lost_write_replies: AtomicUsize,
        /// Land the next create but report the reply lost.
        lost_create_reply: AtomicBool,
        reads: Mutex<Vec<u64>>,
        read_handles: Mutex<std::collections::HashSet<FileHandle>>,
        opens: AtomicUsize,
    }

    impl FlakyFs {
        fn with_file(node: NodeId, data: Vec<u8>) -> Self {
            let fs = Self::default();
            lock(&fs.files).insert(node, data);
            fs
        }

        fn meta(&self, node: NodeId) -> Metadata {
            let size = lock(&self.files)
                .get(&node)
                .map_or(0, |data| data.len() as u64);
            Metadata {
                node,
                kind: if node == ROOT_NODE {
                    NodeKind::Directory
                } else {
                    NodeKind::File
                },
                size,
                mode: 0o644,
                allocated_blocks: 0,
                revision: 7,
                accessed_unix_ms: 1,
                modified_unix_ms: 1,
                created_unix_ms: Some(1),
                backup_unix_ms: None,
                link_count: 1,
                device_major: 0,
                device_minor: 0,
            }
        }

        fn stalled() -> ClientError {
            ClientError::Transport(TransportError::Timeout)
        }

        fn reads_at(&self, offset: u64) -> usize {
            lock(&self.reads).iter().filter(|at| **at == offset).count()
        }
    }

    #[async_trait]
    impl RemoteFilesystem for FlakyFs {
        async fn ping(&self, nonce: u64) -> quickfs_client_core::Result<u64> {
            Ok(nonce)
        }
        async fn get_metadata(&self, node: NodeId) -> quickfs_client_core::Result<Metadata> {
            Ok(self.meta(node))
        }
        async fn list_directory(
            &self,
            _node: NodeId,
        ) -> quickfs_client_core::Result<Vec<DirectoryEntry>> {
            Ok(lock(&self.names)
                .iter()
                .map(|(name, node)| DirectoryEntry {
                    node: *node,
                    name: Name::new(name.clone()),
                    kind: NodeKind::File,
                    metadata: self.meta(*node),
                })
                .collect())
        }
        async fn open_file(
            &self,
            node: NodeId,
        ) -> quickfs_client_core::Result<(FileHandle, u64, u64)> {
            let opened = self
                .open_file_with_options(node, FileOpenOptions::READ_ONLY)
                .await?;
            Ok((opened.handle, opened.revision, opened.size))
        }
        async fn open_file_with_options(
            &self,
            node: NodeId,
            options: FileOpenOptions,
        ) -> quickfs_client_core::Result<OpenedFile> {
            self.opens.fetch_add(1, Ordering::SeqCst);
            if options.truncate {
                lock(&self.files).insert(node, Vec::new());
            }
            let handle = FileHandle(Uuid::new_v4());
            lock(&self.handles).insert(handle, node);
            Ok(OpenedFile {
                handle,
                revision: 7,
                size: self.meta(node).size,
            })
        }
        async fn create_file(
            &self,
            _parent: NodeId,
            name: Name,
            _mode: u32,
            options: FileOpenOptions,
        ) -> quickfs_client_core::Result<CreatedFile> {
            if lock(&self.names).contains_key(name.as_bytes()) {
                return Err(ClientError::Server(
                    ErrorCode::AlreadyExists,
                    "exists".into(),
                ));
            }
            let node = NodeId(Uuid::new_v4());
            lock(&self.files).insert(node, Vec::new());
            lock(&self.names).insert(name.as_bytes().to_vec(), node);
            if self.lost_create_reply.swap(false, Ordering::SeqCst) {
                return Err(ClientError::AmbiguousMutation);
            }
            let opened = self.open_file_with_options(node, options).await?;
            Ok(CreatedFile {
                metadata: self.meta(node),
                opened,
            })
        }
        async fn read_range(
            &self,
            handle: FileHandle,
            offset: u64,
            length: u64,
        ) -> quickfs_client_core::Result<Vec<u8>> {
            Ok(self
                .read_range_versioned(handle, offset, length)
                .await?
                .data)
        }
        async fn read_range_versioned(
            &self,
            handle: FileHandle,
            offset: u64,
            length: u64,
        ) -> quickfs_client_core::Result<RangeRead> {
            lock(&self.reads).push(offset);
            lock(&self.read_handles).insert(handle);
            // Yield once so concurrent reads interleave like real requests.
            tokio::task::yield_now().await;
            if offset >= self.stall_from.load(Ordering::SeqCst) {
                if self.stall_reads.load(Ordering::SeqCst) {
                    return Err(Self::stalled());
                }
                if self
                    .failing_reads
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                        left.checked_sub(1)
                    })
                    .is_ok()
                {
                    return Err(Self::stalled());
                }
            }
            let node = lock(&self.handles)[&handle];
            let files = lock(&self.files);
            let data = &files[&node];
            let start = (offset as usize).min(data.len());
            let end = (start + length as usize).min(data.len());
            Ok(RangeRead {
                revision: 7,
                data: data[start..end].to_vec(),
            })
        }
        async fn write_range(
            &self,
            handle: FileHandle,
            offset: u64,
            data: &[u8],
        ) -> quickfs_client_core::Result<WriteResult> {
            let node = lock(&self.handles)[&handle];
            let size = {
                let mut files = lock(&self.files);
                let file = files.get_mut(&node).unwrap();
                let end = offset as usize + data.len();
                if file.len() < end {
                    file.resize(end, 0);
                }
                file[offset as usize..end].copy_from_slice(data);
                file.len() as u64
            };
            if offset >= self.stall_from.load(Ordering::SeqCst)
                && self
                    .lost_write_replies
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                        left.checked_sub(1)
                    })
                    .is_ok()
            {
                return Err(ClientError::AmbiguousMutation);
            }
            Ok(WriteResult {
                written: data.len() as u64,
                revision: 7,
                size,
            })
        }
        async fn flush_file(
            &self,
            _handle: FileHandle,
            _lock_owner: Option<u64>,
        ) -> quickfs_client_core::Result<()> {
            Ok(())
        }
        async fn sync_file(
            &self,
            _handle: FileHandle,
            _data_only: bool,
        ) -> quickfs_client_core::Result<()> {
            Ok(())
        }
        async fn sync_directory(&self, _node: NodeId) -> quickfs_client_core::Result<()> {
            Ok(())
        }
        async fn close_file(&self, handle: FileHandle) -> quickfs_client_core::Result<()> {
            lock(&self.handles).remove(&handle);
            Ok(())
        }
    }

    fn session(fs: Arc<FlakyFs>, tuning: Tuning) -> Arc<Session> {
        Arc::new(Session::new(
            fs,
            FilesystemCapabilities {
                server_epoch: Uuid::nil(),
                writable: true,
                supports_locks: false,
                supports_atomic_rename: true,
                supports_directory_sync: true,
                supports_preallocation: false,
                supports_symlinks: false,
                supports_xattrs: false,
                supports_hard_links: false,
                supports_special_nodes: false,
                supports_copy_file_range: false,
                supports_seek_data_hole: false,
                supports_safe_ioctl: false,
                supports_poll: false,
                supports_bmap: false,
                supports_exchange_data: false,
                supports_volume_rename: false,
                supports_backup_time: false,
                supports_readdirplus: false,
                persistent_node_ids: true,
                restart_lock_replay: false,
                volume_name: "test".into(),
                max_read_size: 16 * MIB,
                max_write_size: 8 * MIB,
            },
            tuning,
        ))
    }

    fn content(length: u64) -> Vec<u8> {
        (0..length).map(|index| (index % 251) as u8).collect()
    }

    fn events(progress: &Progress) -> Vec<String> {
        lock(&progress.events)
            .iter()
            .map(|event| event.message.clone())
            .collect()
    }

    const FILE: NodeId = NodeId(Uuid::from_u128(42));

    #[tokio::test(start_paused = true)]
    async fn download_rides_out_stalls_without_rereading_committed_chunks() {
        let data = content(6 * MIB);
        let fs = Arc::new(FlakyFs::with_file(FILE, data.clone()));
        fs.stall_from.store(2 * MIB, Ordering::SeqCst);
        fs.failing_reads.store(3, Ordering::SeqCst);
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("copy.bin");
        let progress = Arc::new(Progress::new());
        download(
            session(Arc::clone(&fs), Tuning::default()),
            FILE,
            destination.clone(),
            false,
            Arc::clone(&progress),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), data);
        assert_eq!(fs.reads_at(0), 1, "committed chunks are never read again");
        // Reads failing together in one wave cost a single retry.
        assert!((1..=3).contains(&progress.retries.load(Ordering::Relaxed)));
        assert_eq!(progress.bytes.load(Ordering::Relaxed), 6 * MIB);
        let log = events(&progress);
        assert!(
            log.iter()
                .any(|line| line.contains("no data moved for 30 s")),
            "{log:?}"
        );
        assert!(
            log.iter()
                .any(|line| line.starts_with("Cut back after a failure")),
            "{log:?}"
        );
        // No hidden partial file is left behind.
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn download_stripes_across_connections_and_still_writes_in_order() {
        let data = content(20 * MIB + 77);
        let fs = Arc::new(FlakyFs::with_file(FILE, data.clone()));
        fs.stall_from.store(9 * MIB, Ordering::SeqCst);
        fs.failing_reads.store(2, Ordering::SeqCst);
        let tuning = Tuning {
            connections: 3,
            ..Tuning::default()
        };
        let session = session(Arc::clone(&fs), tuning);
        // Two extra connections, each its own handle onto the same server.
        session.add_extra_connection(fs.clone()).await;
        session.add_extra_connection(fs.clone()).await;
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("copy.bin");
        let progress = Arc::new(Progress::new());
        download(
            session,
            FILE,
            destination.clone(),
            false,
            Arc::clone(&progress),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), data);
        assert_eq!(fs.reads_at(0), 1);
        assert!((1..=2).contains(&progress.retries.load(Ordering::Relaxed)));
        // The file was opened on all three connections and all three read.
        assert_eq!(fs.opens.load(Ordering::SeqCst), 3);
        assert_eq!(lock(&fs.read_handles).len(), 3);
        assert_eq!(progress.connections.load(Ordering::Relaxed), 3);
        assert!(
            events(&progress)
                .iter()
                .any(|line| line.contains("across 3 connections"))
        );
        // Every extra handle was closed again.
        assert!(lock(&fs.handles).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn small_files_skip_striping() {
        let data = content(3 * MIB);
        let fs = Arc::new(FlakyFs::with_file(FILE, data.clone()));
        let tuning = Tuning {
            connections: 4,
            ..Tuning::default()
        };
        let session = session(Arc::clone(&fs), tuning);
        session.add_extra_connection(fs.clone()).await;
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("copy.bin");
        download(
            session,
            FILE,
            destination.clone(),
            false,
            Arc::new(Progress::new()),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), data);
        assert_eq!(fs.opens.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn download_gives_up_after_the_window_then_resumes_from_its_partial() {
        let data = content(5 * MIB);
        let fs = Arc::new(FlakyFs::with_file(FILE, data.clone()));
        fs.stall_from.store(3 * MIB, Ordering::SeqCst);
        fs.stall_reads.store(true, Ordering::SeqCst);
        let tuning = Tuning {
            retry_window: Duration::from_secs(60),
            ..Tuning::default()
        };
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("copy.bin");
        let progress = Arc::new(Progress::new());
        let session = session(Arc::clone(&fs), tuning);
        let error = download(
            Arc::clone(&session),
            FILE,
            destination.clone(),
            false,
            Arc::clone(&progress),
        )
        .await
        .unwrap_err();
        assert!(resumable(&error));
        assert!(hint(&error, &tuning).unwrap().contains("DIRECT path"));
        assert!(!destination.exists());
        let Some(Partial::Download {
            path, committed, ..
        }) = lock(&progress.checkpoint).partial.clone()
        else {
            panic!("the partial file is kept");
        };
        // Everything before the stall point landed; the file is exactly it.
        assert!((3 * MIB..5 * MIB).contains(&committed), "{committed}");
        assert_eq!(std::fs::metadata(&path).unwrap().len(), committed);

        // The link comes back: resume reads only what is missing.
        fs.stall_reads.store(false, Ordering::SeqCst);
        let reads_before = lock(&fs.reads).len();
        download(
            session,
            FILE,
            destination.clone(),
            false,
            Arc::clone(&progress),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), data);
        assert!(
            lock(&fs.reads)[reads_before..]
                .iter()
                .all(|offset| *offset >= committed)
        );
        assert_eq!(progress.bytes.load(Ordering::Relaxed), 5 * MIB);
        assert!(!path.exists());
        let resumed = format!("Resuming at {}", bytes(committed));
        assert!(
            events(&progress)
                .iter()
                .any(|line| line.starts_with(&resumed))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cancelling_a_stopped_download_deletes_its_partial() {
        let fs = Arc::new(FlakyFs::with_file(FILE, content(4 * MIB)));
        fs.stall_from.store(MIB, Ordering::SeqCst);
        fs.stall_reads.store(true, Ordering::SeqCst);
        let tuning = Tuning {
            retry_window: Duration::from_secs(10),
            ..Tuning::default()
        };
        let directory = tempfile::tempdir().unwrap();
        let progress = Arc::new(Progress::new());
        download(
            session(fs, tuning),
            FILE,
            directory.path().join("copy.bin"),
            false,
            Arc::clone(&progress),
        )
        .await
        .unwrap_err();
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        discard_partial(&progress).await;
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn upload_rewrites_from_the_last_acknowledged_byte_after_lost_replies() {
        let data = content(5 * MIB + 123);
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.bin");
        std::fs::write(&source, &data).unwrap();
        let fs = Arc::new(FlakyFs::default());
        fs.stall_from.store(2 * MIB, Ordering::SeqCst);
        fs.lost_write_replies.store(2, Ordering::SeqCst);
        let progress = Arc::new(Progress::new());
        upload(
            session(Arc::clone(&fs), Tuning::default()),
            ROOT_NODE,
            source,
            Name::from("copy.bin"),
            false,
            Arc::clone(&progress),
        )
        .await
        .unwrap();
        let node = lock(&fs.names)[b"copy.bin".as_slice()];
        assert_eq!(lock(&fs.files)[&node], data);
        assert_eq!(progress.bytes.load(Ordering::Relaxed), data.len() as u64);
        assert_eq!(progress.retries.load(Ordering::Relaxed), 2);
        // One handle from the create, one reopen per lost reply.
        assert_eq!(fs.opens.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn a_create_whose_reply_was_lost_is_recognised_and_reused() {
        let data = content(300_000);
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.bin");
        std::fs::write(&source, &data).unwrap();
        let fs = Arc::new(FlakyFs::default());
        fs.lost_create_reply.store(true, Ordering::SeqCst);
        upload(
            session(Arc::clone(&fs), Tuning::default()),
            ROOT_NODE,
            source,
            Name::from("copy.bin"),
            false,
            Arc::new(Progress::new()),
        )
        .await
        .unwrap();
        let node = lock(&fs.names)[b"copy.bin".as_slice()];
        assert_eq!(lock(&fs.files)[&node], data);
    }

    #[tokio::test(start_paused = true)]
    async fn an_existing_file_is_still_refused_without_replace() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.bin");
        std::fs::write(&source, b"new").unwrap();
        let fs = Arc::new(FlakyFs::with_file(FILE, b"old".to_vec()));
        lock(&fs.names).insert(b"copy.bin".to_vec(), FILE);
        let error = upload(
            session(Arc::clone(&fs), Tuning::default()),
            ROOT_NODE,
            source,
            Name::from("copy.bin"),
            false,
            Arc::new(Progress::new()),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code(), "AlreadyExists");
        assert_eq!(lock(&fs.files)[&FILE], b"old");
    }

    #[tokio::test(start_paused = true)]
    async fn a_burst_of_resizes_is_reported_once_it_settles() {
        let progress = Progress::new();
        let shape = |depth, chunk| Shape { depth, chunk };
        progress.show_shape(shape(2, MIB), None, Some("Starting"));
        for depth in [2, 4, 8, 7, 8] {
            progress.show_shape(shape(depth, 4 * MIB), Some(20e6), None);
            progress.commit(1);
        }
        assert_eq!(events(&progress), ["Starting: 2 x 1.0 MB in flight"]);
        tokio::time::advance(SHAPE_REPORT_SPACING).await;
        progress.commit(1);
        assert_eq!(
            events(&progress)[1],
            "Settled: 8 x 4.2 MB in flight, link about 20 MB/s"
        );
        // Small wobbles after that stay out of the log.
        progress.show_shape(shape(7, 4 * MIB), Some(18e6), None);
        tokio::time::advance(SHAPE_REPORT_SPACING).await;
        progress.commit(1);
        assert_eq!(events(&progress).len(), 2);
    }

    #[test]
    fn explanations_name_the_link_not_the_code_path() {
        let tuning = Tuning::default();
        let deadline = BridgeError::Client(ClientError::Server(
            ErrorCode::Timeout,
            CLIENT_DEADLINE_MESSAGE.into(),
        ));
        assert!(explain(&deadline, &tuning).contains("too slow for its size"));
        let fixed = Tuning {
            adaptive: false,
            ..tuning
        };
        assert!(
            hint(&deadline, &fixed)
                .unwrap()
                .contains("Size requests to the link")
        );
        let server =
            BridgeError::Client(ClientError::Server(ErrorCode::Timeout, "slow disk".into()));
        assert!(
            hint(&server, &tuning)
                .unwrap()
                .contains("server stayed busy")
        );
        let stalled = BridgeError::Client(ClientError::Transport(TransportError::Timeout));
        assert_eq!(
            explain(&stalled, &tuning),
            "no data moved for 30 s, the link stalled"
        );
        assert!(resumable(&stalled));
        let changed = BridgeError::Client(ClientError::StaleRevision);
        assert!(!resumable(&changed));
        assert!(
            hint(&changed, &tuning)
                .unwrap()
                .contains("changed on the server")
        );
        assert_eq!(bytes(3 * MIB), "3.1 MB");
        assert_eq!(
            sentence(&explain(
                &BridgeError::Client(ClientError::Offline),
                &tuning
            )),
            "Could not reconnect to the server"
        );
        assert_eq!(seconds(Duration::from_secs(300)), "5 min");
    }
}
