//! A DevTools session with each `chromium-kiosk` app, over a pipe.
//!
//! What Chromium itself knows is what an operator needs when video stutters
//! or the GPU panel looks idle: which GPU it chose and whether each feature
//! runs on it (`SystemInfo.getInfo`), and, per media player, which decoder
//! was picked and whether it is the platform (hardware) one (the **Media**
//! domain). None of that is visible from outside the browser.
//!
//! Why `--remote-debugging-pipe` rather than `--remote-debugging-port`:
//! there is no socket and no port, so nothing on the network or the machine
//! can reach the browser through it, and there is nothing to allocate or
//! collide. Chromium reads requests from fd 3 and writes responses and
//! events to fd 4, which the supervisor wires up when it spawns the browser
//! ([`DevtoolsPipes`]).
//!
//! Measured on a test appliance (Google Chrome 151, 2026-10-12):
//!
//! - Frames are JSON objects terminated by a single NUL byte, both ways.
//!   Requests are `{"id","method","params"[,"sessionId"]}`; responses carry
//!   the same `id` and either `result` or `error`; events carry `method`,
//!   `params` and, when session-scoped, `sessionId`.
//! - Closing the write end of fd 3 makes Chromium exit with status 0
//!   ("Connection terminated while reading from pipe"). So the [`Client`]
//!   owns that write end for the whole life of the browser, *including*
//!   after its own reader has stopped: the task may end, the `Sender` may
//!   not. Dropping the client closes it, which is acceptable only because
//!   the supervisor drops it exactly when it is stopping or replacing the
//!   browser anyway.
//!
//! Cost profile: Chromium parks one reader thread on a blocking read. Suede
//! sends a handful of one-shot requests at launch (`Browser.getVersion`,
//! `SystemInfo.getInfo`, `Target.setDiscoverTargets`, `Target.getTargets`,
//! and an attach plus `Media.enable` per page), refreshes
//! `SystemInfo.getInfo` every [`REFRESH_INTERVAL`] (feature status changes
//! after a GPU process crash and fallback), and otherwise only listens: the
//! domain pushes events on player state changes, never per frame. The one
//! thing Suede asks a page itself is the playhead: once per
//! [`crate::diagnostics::SAMPLE_INTERVAL`] a single `Runtime.evaluate`
//! ([`PLAYHEAD_EXPRESSION`]) reads each media element's position, bounded
//! by [`EVALUATE_TIMEOUT_MS`] and never issued while the previous one for
//! that page is still outstanding.

use std::any::Any;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use futures::stream::{FuturesUnordered, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use utoipa::ToSchema;

/// The Chromium switch that opens the DevTools pipe on fds 3 and 4.
pub const PIPE_FLAG: &str = "--remote-debugging-pipe";

/// How long one request may take, write and response together.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// How often `SystemInfo.getInfo` is fetched again after the handshake.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(30);

/// The largest frame accepted from Chromium. A larger one is dropped and
/// logged rather than letting a runaway message grow the buffer for ever.
pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// Media players kept per app.
pub const MAX_PLAYERS: usize = 32;

/// How long a destroyed media player stays visible after its destroy event.
pub const DESTROYED_RETENTION_SECONDS: u64 = 60;

/// Warning and error messages kept per media player.
const MAX_MESSAGES: usize = 10;

/// Raised errors kept per media player.
const MAX_ERRORS: usize = 5;

/// Where the DevTools session with one browser stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub enum DevtoolsState {
    /// `browser_devtools = false`: the browser was launched without the pipe.
    Disabled,
    /// The pipe is open and no request has been answered yet.
    #[default]
    Connecting,
    /// At least one request has been answered.
    Connected,
    /// The pipe could not be created, or every handshake request failed.
    Failed,
    /// Chromium closed the pipe (it exited), or reading it failed.
    Closed,
}

/// [`DevtoolsState`] with the reason behind it.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DevtoolsStatus {
    pub state: DevtoolsState,
    /// Human-readable detail: why disabled/failed/closed, or null.
    pub detail: Option<String>,
}

impl DevtoolsStatus {
    /// A status with the given state and detail.
    pub fn new(state: DevtoolsState, detail: Option<String>) -> Self {
        Self { state, detail }
    }
}

/// What `Browser.getVersion` answered.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BrowserVersion {
    /// e.g. "Chrome/151.0.7922.71"
    pub product: String,
    pub protocol_version: String,
    pub js_version: Option<String>,
    pub user_agent: Option<String>,
}

/// One GPU as Chromium sees it (`SystemInfo.getInfo` `gpu.devices[]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BrowserGpuDevice {
    /// `vendorString`
    pub vendor: String,
    /// `deviceString`
    pub device: String,
    pub driver_vendor: Option<String>,
    pub driver_version: Option<String>,
}

/// A video decode or encode profile Chromium reports as accelerated.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoProfile {
    pub profile: String,
    pub max_width: u32,
    pub max_height: u32,
    /// 0 when Chromium reports no minimum (encode profiles never do).
    pub min_width: u32,
    pub min_height: u32,
}

/// Chromium's own view of its GPU, from `SystemInfo.getInfo`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BrowserGpuInfo {
    pub devices: Vec<BrowserGpuDevice>,
    /// `featureStatus`, sorted by key, e.g. `video_decode: enabled`.
    pub feature_status: BTreeMap<String, String>,
    pub video_decoding: Vec<VideoProfile>,
    pub video_encoding: Vec<VideoProfile>,
    pub driver_bug_workarounds: Vec<String>,
    pub command_line: Option<String>,
    /// Unix seconds this was last fetched.
    pub fetched_at: u64,
}

/// What the page's own media element says, read with `Runtime.evaluate`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Playhead {
    /// "video" | "audio"
    pub element: String,
    pub current_src: String,
    pub current_time_seconds: f64,
    /// null when the element reports Infinity or NaN (live streams).
    pub duration_seconds: Option<f64>,
    pub paused: bool,
    pub ended: bool,
    /// `HTMLMediaElement.readyState`, 0 to 4.
    pub ready_state: u8,
    /// `HTMLMediaElement.networkState`, 0 to 3.
    pub network_state: u8,
    pub playback_rate: f64,
    pub muted: bool,
    pub volume: f64,
    pub video_width: u32,
    pub video_height: u32,
    /// From `getVideoPlaybackQuality`, null for audio elements.
    pub total_frames: Option<u64>,
    pub dropped_frames: Option<u64>,
    /// Unix seconds the page answered.
    pub read_at: u64,
}

/// One entry of `kVideoTracks` or `kAudioTracks`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MediaTrack {
    /// Every key of the track object, sorted; nested objects serialized as
    /// compact JSON strings.
    pub fields: BTreeMap<String, String>,
}

/// A warning or error a media player logged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MediaMessage {
    pub level: String,
    pub message: String,
}

/// One media player, assembled from the Media domain's events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MediaPlayer {
    /// `playerId`
    pub id: String,
    /// Unix seconds the player was first seen by Suede.
    pub created_at: u64,
    /// `kFrameUrl`
    pub frame_url: Option<String>,
    /// The `url` of the last `kLoad` event.
    pub source_url: Option<String>,
    /// `kVideoDecoderName`, e.g. "VaapiVideoDecoder" or "FFmpegVideoDecoder".
    pub video_decoder: Option<String>,
    /// `kIsPlatformVideoDecoder`: true when the decoder is the platform's
    /// (hardware) one.
    pub video_decoder_platform: Option<bool>,
    /// `kAudioDecoderName`
    pub audio_decoder: Option<String>,
    /// `kIsPlatformAudioDecoder`
    pub audio_decoder_platform: Option<bool>,
    /// From the first `kVideoTracks` entry: its `profile` (e.g. "h264
    /// high") when present, else its `codec` (e.g. "h264").
    pub video_codec: Option<String>,
    /// `codec` of the first `kAudioTracks` entry.
    pub audio_codec: Option<String>,
    /// `kResolution`, else the last `kVideoSizeChanged` dimensions.
    pub resolution: Option<String>,
    /// `kMaxDuration`, else the last `kDurationChanged` duration.
    pub duration_seconds: Option<f64>,
    /// `kBitrate`
    pub bitrate: Option<u64>,
    /// `kIsStreaming`
    pub streaming: Option<bool>,
    /// `kRendererName`
    pub renderer: Option<String>,
    /// The last `kPipelineStateChange`, without the leading `k` ("Playing").
    pub pipeline_state: Option<String>,
    /// The last `pipeline_buffering_state.state`, e.g. "BUFFERING_HAVE_ENOUGH".
    pub buffering_state: Option<String>,
    /// true after `kPlay`, false after `kPause`/`kEnded`; null before either.
    pub playing: Option<bool>,
    /// Set by `kEnded`, cleared by a later `kPlay`.
    pub ended: bool,
    /// Set by `kWebMediaPlayerDestroyed`, or when the page the player
    /// belonged to went away.
    pub destroyed: bool,
    /// Unix seconds of the last event/property/message for this player.
    pub last_activity_at: u64,
    /// Last 10 messages at level "warning" or "error".
    pub messages: Vec<MediaMessage>,
    /// Last 5 raised errors, serialized JSON.
    pub errors: Vec<String>,
    /// Every property as last reported, sorted by name (raw internals).
    pub properties: BTreeMap<String, String>,
    /// Parsed from `kVideoTracks`.
    pub video_tracks: Vec<MediaTrack>,
    /// Parsed from `kAudioTracks`.
    pub audio_tracks: Vec<MediaTrack>,
    /// The page's media element matched to this player, if any and fresh.
    pub playhead: Option<Playhead>,
}

impl MediaPlayer {
    fn new(id: &str, now: u64) -> Self {
        Self {
            id: id.to_string(),
            created_at: now,
            frame_url: None,
            source_url: None,
            video_decoder: None,
            video_decoder_platform: None,
            audio_decoder: None,
            audio_decoder_platform: None,
            video_codec: None,
            audio_codec: None,
            resolution: None,
            duration_seconds: None,
            bitrate: None,
            streaming: None,
            renderer: None,
            pipeline_state: None,
            buffering_state: None,
            playing: None,
            ended: false,
            destroyed: false,
            last_activity_at: now,
            messages: Vec::new(),
            errors: Vec::new(),
            properties: BTreeMap::new(),
            video_tracks: Vec::new(),
            audio_tracks: Vec::new(),
            playhead: None,
        }
    }
}

/// Everything one [`Client`] has learned, as of the moment it is asked.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Internals {
    pub status: DevtoolsStatus,
    pub version: Option<BrowserVersion>,
    pub gpu: Option<BrowserGpuInfo>,
    /// Oldest first.
    pub players: Vec<MediaPlayer>,
    /// Media elements no player matched, in page then document order.
    pub unmatched_elements: Vec<Playhead>,
}

/// State shared between the [`Client`] handle and its task.
#[derive(Debug, Default)]
struct Shared {
    status: DevtoolsStatus,
    version: Option<BrowserVersion>,
    gpu: Option<BrowserGpuInfo>,
    media: MediaTracker,
}

/// Lock without ever panicking: a poisoned lock still holds usable data, and
/// telemetry is never worth taking the supervisor down for.
fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Timeouts and intervals, adjustable so tests need not wait seconds.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Timing {
    pub request_timeout: Duration,
    pub refresh_interval: Duration,
    pub playhead_interval: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            request_timeout: REQUEST_TIMEOUT,
            refresh_interval: REFRESH_INTERVAL,
            playhead_interval: crate::diagnostics::SAMPLE_INTERVAL,
        }
    }
}

/// The DevTools session with one running browser.
///
/// Holds the write end of the browser's fd 3 for as long as it lives, even
/// after its task has ended (see the module documentation for why).
/// Dropping it aborts the task and closes that write end, which makes
/// Chromium exit — so only drop it when the browser is going anyway.
pub struct Client {
    shared: Arc<Mutex<Shared>>,
    task: Option<JoinHandle<()>>,
    /// The writer, kept alive here independently of the task.
    _sender_owner: Option<Arc<dyn Any + Send + Sync>>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("status", &lock(&self.shared).status)
            .finish_non_exhaustive()
    }
}

impl Client {
    /// Start the session on the parent's ends of the two pipes: `sender`
    /// writes to Chromium's fd 3, `receiver` reads Chromium's fd 4. Must be
    /// called from within a Tokio runtime.
    #[cfg(unix)]
    pub fn spawn(
        app_id: String,
        sender: tokio::net::unix::pipe::Sender,
        receiver: tokio::net::unix::pipe::Receiver,
    ) -> Client {
        Self::with_io(app_id, receiver, sender, Timing::default())
    }

    /// A client with no session, recording why there is none — for a
    /// launch whose pipes could not be created.
    pub fn failed(detail: impl Into<String>) -> Client {
        let shared = Shared {
            status: DevtoolsStatus::new(DevtoolsState::Failed, Some(detail.into())),
            ..Shared::default()
        };
        Client {
            shared: Arc::new(Mutex::new(shared)),
            task: None,
            _sender_owner: None,
        }
    }

    /// A copy of everything learned so far. Destroyed media players past
    /// their retention are dropped on the way.
    pub fn internals(&self) -> Internals {
        let mut shared = lock(&self.shared);
        let (players, unmatched_elements) = shared.media.snapshot(crate::util::unix_now());
        Internals {
            status: shared.status.clone(),
            version: shared.version.clone(),
            gpu: shared.gpu.clone(),
            players,
            unmatched_elements,
        }
    }

    /// The session over any byte streams, which is what lets the tests play
    /// Chromium without one.
    pub(crate) fn with_io<R, W>(app_id: String, reader: R, writer: W, timing: Timing) -> Client
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let writer = Arc::new(tokio::sync::Mutex::new(writer));
        let task = tokio::spawn(run(app_id, reader, writer.clone(), shared.clone(), timing));
        Client {
            shared,
            task: Some(task),
            _sender_owner: Some(writer),
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

/// The parent's and the child's ends of the two DevTools pipes, before the
/// browser is spawned.
///
/// Pipe A carries requests: Suede keeps its `Sender`, Chromium reads its
/// other end as fd 3. Pipe B carries responses and events: Chromium writes
/// its end as fd 4, Suede keeps the `Receiver`.
#[cfg(unix)]
pub struct DevtoolsPipes {
    sender: tokio::net::unix::pipe::Sender,
    receiver: tokio::net::unix::pipe::Receiver,
    /// Becomes the child's fd 3. Blocking, because Chromium reads it from a
    /// thread with blocking reads.
    child_read: std::os::fd::OwnedFd,
    /// Becomes the child's fd 4.
    child_write: std::os::fd::OwnedFd,
}

#[cfg(unix)]
impl DevtoolsPipes {
    /// Create both pipes. Must be called from within a Tokio runtime.
    pub fn new() -> std::io::Result<Self> {
        let (sender, child_read) = tokio::net::unix::pipe::pipe()?;
        let (child_write, receiver) = tokio::net::unix::pipe::pipe()?;
        Ok(Self {
            sender,
            receiver,
            child_read: child_read.into_blocking_fd()?,
            child_write: child_write.into_blocking_fd()?,
        })
    }

    /// Spawn `command` with the child's ends on fds 3 and 4, then close the
    /// parent's copies of them. Returns the child and the parent's ends:
    /// the `Sender` writes to the child's fd 3, the `Receiver` reads its
    /// fd 4.
    pub fn spawn(
        self,
        command: &mut tokio::process::Command,
    ) -> std::io::Result<(
        tokio::process::Child,
        tokio::net::unix::pipe::Sender,
        tokio::net::unix::pipe::Receiver,
    )> {
        use std::os::fd::AsRawFd;
        let read = self.child_read.as_raw_fd();
        let write = self.child_write.as_raw_fd();
        // Safety: the closure runs in the forked child before `exec` and
        // calls only `fcntl` and `dup2`, both async-signal-safe; it touches
        // no allocator and no lock. `read` and `write` are open for the
        // whole of `spawn` because `self` owns them until it returns.
        unsafe {
            command.pre_exec(move || map_child_fds(read, write));
        }
        let child = command.spawn()?;
        // `self.child_read` and `self.child_write` close here: the child has
        // its own copies on 3 and 4, and the parent holding the read end of
        // pipe A or the write end of pipe B would hide the child's exit from
        // the reader (no EOF) and keep a dead pipe writable.
        Ok((child, self.sender, self.receiver))
    }
}

/// Put `read` on fd 3 and `write` on fd 4, in the forked child.
///
/// Both are first copied to 10 or above, so a source that happens to be
/// numbered 3 or 4 is not clobbered by the other `dup2`. The copies are
/// close-on-exec, as are the originals, so neither survives `exec`; `dup2`
/// clears close-on-exec on its target, so 3 and 4 do.
#[cfg(unix)]
fn map_child_fds(read: i32, write: i32) -> std::io::Result<()> {
    // Safety: plain fd syscalls on descriptors the parent kept open.
    unsafe {
        let read_high = libc::fcntl(read, libc::F_DUPFD_CLOEXEC, 10);
        if read_high < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let write_high = libc::fcntl(write, libc::F_DUPFD_CLOEXEC, 10);
        if write_high < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::dup2(read_high, 3) < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::dup2(write_high, 4) < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Framing

/// Splits the byte stream from Chromium into NUL-terminated frames.
#[derive(Debug, Default)]
pub(crate) struct FrameDecoder {
    buffer: Vec<u8>,
    /// Discarding the rest of an oversized frame, up to its NUL.
    skipping: bool,
    /// Oversized frames dropped since the last [`Self::take_dropped`].
    dropped: usize,
}

impl FrameDecoder {
    /// Feed bytes; returns every frame they complete, without its NUL.
    /// Empty frames are skipped.
    pub(crate) fn push(&mut self, mut bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();
        while let Some(end) = bytes.iter().position(|byte| *byte == 0) {
            let head = &bytes[..end];
            bytes = &bytes[end + 1..];
            if self.skipping {
                self.skipping = false;
            } else if self.buffer.len() + head.len() > MAX_FRAME_BYTES {
                self.dropped += 1;
            } else {
                self.buffer.extend_from_slice(head);
                if !self.buffer.is_empty() {
                    frames.push(std::mem::take(&mut self.buffer));
                }
            }
            self.buffer.clear();
        }
        if !self.skipping && !bytes.is_empty() {
            if self.buffer.len() + bytes.len() > MAX_FRAME_BYTES {
                self.buffer = Vec::new();
                self.skipping = true;
                self.dropped += 1;
            } else {
                self.buffer.extend_from_slice(bytes);
            }
        }
        frames
    }

    /// How many oversized frames were dropped since the last call.
    pub(crate) fn take_dropped(&mut self) -> usize {
        std::mem::take(&mut self.dropped)
    }
}

/// One request frame, NUL included.
pub(crate) fn encode_request(
    id: u64,
    method: &str,
    params: &Value,
    session: Option<&str>,
) -> Vec<u8> {
    let mut request = Map::new();
    request.insert("id".into(), json!(id));
    request.insert("method".into(), json!(method));
    request.insert("params".into(), params.clone());
    if let Some(session) = session {
        request.insert("sessionId".into(), json!(session));
    }
    let mut bytes = serde_json::to_vec(&Value::Object(request)).unwrap_or_default();
    bytes.push(0);
    bytes
}

// ---------------------------------------------------------------------------
// The session

type Writer<W> = Arc<tokio::sync::Mutex<W>>;

/// Request/response bookkeeping over the shared writer.
struct Connection<W> {
    writer: Writer<W>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>,
    next_id: AtomicU64,
    timeout: Duration,
}

impl<W: AsyncWrite + Unpin> Connection<W> {
    /// Send one request and wait for its answer, within the timeout.
    async fn call(
        &self,
        method: &str,
        params: Value,
        session: Option<&str>,
    ) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending_map().insert(id, tx);
        let frame = encode_request(id, method, &params, session);
        let exchange = async {
            {
                let mut writer = self.writer.lock().await;
                writer
                    .write_all(&frame)
                    .await
                    .map_err(|error| format!("write failed: {error}"))?;
                writer
                    .flush()
                    .await
                    .map_err(|error| format!("write failed: {error}"))?;
            }
            rx.await
                .map_err(|_| "the pipe closed before an answer".to_string())?
        };
        let outcome = match tokio::time::timeout(self.timeout, exchange).await {
            Ok(outcome) => outcome,
            Err(_) => Err(format!("timed out after {}s", self.timeout.as_secs_f64())),
        };
        self.pending_map().remove(&id);
        outcome
    }

    /// Hand a response to whoever is waiting for it. Returns whether it was
    /// a success.
    fn resolve(&self, id: &Value, frame: &Map<String, Value>) -> bool {
        let outcome = match frame.get("error") {
            Some(error) => Err(error
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| error.to_string())),
            None => Ok(frame.get("result").cloned().unwrap_or(Value::Null)),
        };
        let success = outcome.is_ok();
        if let Some(waiting) = id.as_u64().and_then(|id| self.pending_map().remove(&id)) {
            // A caller that already timed out has gone; nothing to do.
            let _ = waiting.send(outcome);
        }
        success
    }

    fn pending_map(&self) -> MutexGuard<'_, HashMap<u64, oneshot::Sender<Result<Value, String>>>> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// The client's task: read frames and drive the handshake side by side.
/// Ends when the pipe from Chromium closes; the writer stays alive with the
/// [`Client`].
async fn run<R, W>(
    app_id: String,
    reader: R,
    writer: Writer<W>,
    shared: Arc<Mutex<Shared>>,
    timing: Timing,
) where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send,
{
    let connection = Connection {
        writer,
        pending: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(1),
        timeout: timing.request_timeout,
    };
    let (targets_tx, targets_rx) = mpsc::unbounded_channel();
    tokio::select! {
        why = read_loop(&app_id, reader, &connection, &shared, targets_tx) => {
            tracing::info!(app = %app_id, reason = %why, "DevTools session closed");
            let mut shared = lock(&shared);
            shared.status = DevtoolsStatus::new(DevtoolsState::Closed, Some(why));
        }
        _ = control(&app_id, &connection, &shared, targets_rx, timing) => {}
    }
}

/// A `Target.*` event, forwarded from the reader to [`control`].
#[derive(Debug)]
struct TargetEvent {
    method: String,
    params: Value,
}

/// Read until the pipe closes; returns why it ended.
async fn read_loop<R, W>(
    app_id: &str,
    mut reader: R,
    connection: &Connection<W>,
    shared: &Mutex<Shared>,
    targets: mpsc::UnboundedSender<TargetEvent>,
) -> String
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut decoder = FrameDecoder::default();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let read = match reader.read(&mut chunk).await {
            Ok(0) => return "Chromium closed the DevTools pipe".to_string(),
            Ok(read) => read,
            Err(error) => return format!("reading the DevTools pipe failed: {error}"),
        };
        for frame in decoder.push(&chunk[..read]) {
            handle_frame(app_id, &frame, connection, shared, &targets);
        }
        let dropped = decoder.take_dropped();
        if dropped > 0 {
            tracing::warn!(
                app = %app_id,
                dropped,
                limit = MAX_FRAME_BYTES,
                "dropped oversized DevTools frames"
            );
        }
    }
}

fn handle_frame<W: AsyncWrite + Unpin>(
    app_id: &str,
    frame: &[u8],
    connection: &Connection<W>,
    shared: &Mutex<Shared>,
    targets: &mpsc::UnboundedSender<TargetEvent>,
) {
    let value: Value = match serde_json::from_slice(frame) {
        Ok(value) => value,
        Err(error) => {
            tracing::debug!(app = %app_id, %error, "skipping a DevTools frame that is not JSON");
            return;
        }
    };
    let Some(object) = value.as_object() else {
        tracing::debug!(app = %app_id, "skipping a DevTools frame that is not an object");
        return;
    };
    if let Some(id) = object.get("id") {
        if connection.resolve(id, object) {
            let mut shared = lock(shared);
            if shared.status.state == DevtoolsState::Connecting {
                shared.status.state = DevtoolsState::Connected;
            }
        }
        return;
    }
    let Some(method) = object.get("method").and_then(Value::as_str) else {
        return;
    };
    let params = object.get("params").cloned().unwrap_or(Value::Null);
    if method.starts_with("Media.") {
        let session = object.get("sessionId").and_then(Value::as_str);
        lock(shared)
            .media
            .apply(session, method, &params, crate::util::unix_now());
    } else if method.starts_with("Target.") {
        let _ = targets.send(TargetEvent {
            method: method.to_string(),
            params,
        });
    }
}

/// One playhead read in flight: its session and the raw answer.
type PlayheadRead<'a> = std::pin::Pin<
    Box<dyn std::future::Future<Output = (String, Result<Value, String>)> + Send + 'a>,
>;

/// The handshake, then attaching to new pages and the periodic refresh.
/// Never returns on its own; ends when [`run`] drops it.
async fn control<W: AsyncWrite + Unpin + Send>(
    app_id: &str,
    connection: &Connection<W>,
    shared: &Mutex<Shared>,
    mut targets: mpsc::UnboundedReceiver<TargetEvent>,
    timing: Timing,
) {
    let refresh = timing.refresh_interval;
    let mut failures: Vec<String> = Vec::new();
    let mut answered = 0;

    match connection.call("Browser.getVersion", json!({}), None).await {
        Ok(result) => {
            answered += 1;
            lock(shared).version = Some(parse_version(&result));
        }
        Err(error) => failures.push(format!("Browser.getVersion: {error}")),
    }
    match connection.call("SystemInfo.getInfo", json!({}), None).await {
        Ok(result) => {
            answered += 1;
            lock(shared).gpu = Some(parse_gpu_info(&result, crate::util::unix_now()));
        }
        Err(error) => failures.push(format!("SystemInfo.getInfo: {error}")),
    }
    match connection
        .call(
            "Target.setDiscoverTargets",
            json!({ "discover": true }),
            None,
        )
        .await
    {
        Ok(_) => answered += 1,
        Err(error) => failures.push(format!("Target.setDiscoverTargets: {error}")),
    }
    let mut pages = Vec::new();
    match connection.call("Target.getTargets", json!({}), None).await {
        Ok(result) => {
            answered += 1;
            pages = result
                .get("targetInfos")
                .and_then(Value::as_array)
                .map(|infos| infos.iter().filter_map(page_target_id).collect())
                .unwrap_or_default();
        }
        Err(error) => failures.push(format!("Target.getTargets: {error}")),
    }

    {
        let detail = (!failures.is_empty()).then(|| failures.join("; "));
        let mut shared = lock(shared);
        shared.status = if answered == 0 {
            DevtoolsStatus::new(DevtoolsState::Failed, detail)
        } else {
            DevtoolsStatus::new(DevtoolsState::Connected, detail)
        };
    }
    if answered == 0 {
        tracing::warn!(app = %app_id, failures = %failures.join("; "), "DevTools handshake failed");
    } else {
        tracing::info!(app = %app_id, "DevTools session connected");
    }

    // targetId → sessionId for every page with Media enabled.
    let mut sessions: HashMap<String, String> = HashMap::new();
    for target in pages {
        attach(app_id, connection, &mut sessions, target).await;
    }

    let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + refresh, refresh);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let poll_every = timing.playhead_interval;
    let mut playhead_tick =
        tokio::time::interval_at(tokio::time::Instant::now() + poll_every, poll_every);
    playhead_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Sessions with a playhead evaluate outstanding, and the evaluates.
    let mut in_flight: HashSet<String> = HashSet::new();
    let mut polls: FuturesUnordered<PlayheadRead<'_>> = FuturesUnordered::new();
    loop {
        tokio::select! {
            Some((session, outcome)) = polls.next(), if !polls.is_empty() => {
                in_flight.remove(&session);
                if !sessions.values().any(|known| *known == session) {
                    continue;
                }
                match outcome.and_then(|result| parse_playheads(&result, crate::util::unix_now())) {
                    Ok(playheads) => lock(shared).media.set_playheads(&session, playheads),
                    Err(error) => {
                        tracing::debug!(app = %app_id, %error, "playhead read failed");
                    }
                }
            }
            _ = playhead_tick.tick() => {
                for session in sessions.values() {
                    if in_flight.insert(session.clone()) {
                        let session = session.clone();
                        polls.push(Box::pin(async move {
                            let outcome = connection
                                .call("Runtime.evaluate", playhead_params(), Some(&session))
                                .await;
                            (session, outcome)
                        }));
                    }
                }
            }
            event = targets.recv() => {
                let Some(event) = event else {
                    // The reader has ended, and `run` is about to drop this.
                    std::future::pending::<()>().await;
                    return;
                };
                on_target_event(app_id, connection, shared, &mut sessions, event).await;
            }
            _ = tick.tick() => {
                match connection.call("SystemInfo.getInfo", json!({}), None).await {
                    Ok(result) => {
                        lock(shared).gpu = Some(parse_gpu_info(&result, crate::util::unix_now()));
                    }
                    Err(error) => {
                        tracing::debug!(app = %app_id, %error, "SystemInfo.getInfo refresh failed");
                    }
                }
            }
        }
    }
}

/// The `targetId` of a `page` target, from a `TargetInfo`.
fn page_target_id(info: &Value) -> Option<String> {
    if info.get("type").and_then(Value::as_str) != Some("page") {
        return None;
    }
    info.get("targetId")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Attach to a page (flattened, so its events arrive on this pipe with a
/// `sessionId`) and enable the Media domain in it.
async fn attach<W: AsyncWrite + Unpin>(
    app_id: &str,
    connection: &Connection<W>,
    sessions: &mut HashMap<String, String>,
    target: String,
) {
    if sessions.contains_key(&target) {
        return;
    }
    let session = match connection
        .call(
            "Target.attachToTarget",
            json!({ "targetId": target, "flatten": true }),
            None,
        )
        .await
    {
        Ok(result) => match result.get("sessionId").and_then(Value::as_str) {
            Some(session) => session.to_string(),
            None => {
                tracing::debug!(app = %app_id, %target, "attachToTarget answered without a sessionId");
                return;
            }
        },
        Err(error) => {
            // Usually a page that closed between discovery and attach.
            tracing::debug!(app = %app_id, %target, %error, "cannot attach to page");
            return;
        }
    };
    if let Err(error) = connection
        .call("Media.enable", json!({}), Some(&session))
        .await
    {
        tracing::debug!(app = %app_id, %target, %error, "Media.enable failed");
    }
    sessions.insert(target, session);
}

async fn on_target_event<W: AsyncWrite + Unpin>(
    app_id: &str,
    connection: &Connection<W>,
    shared: &Mutex<Shared>,
    sessions: &mut HashMap<String, String>,
    event: TargetEvent,
) {
    match event.method.as_str() {
        "Target.targetCreated" => {
            if let Some(target) = event.params.get("targetInfo").and_then(page_target_id) {
                attach(app_id, connection, sessions, target).await;
            }
        }
        "Target.targetDestroyed" => {
            if let Some(target) = event.params.get("targetId").and_then(Value::as_str) {
                if let Some(session) = sessions.remove(target) {
                    lock(shared)
                        .media
                        .forget_session(&session, crate::util::unix_now());
                }
            }
        }
        "Target.detachedFromTarget" => {
            if let Some(session) = event.params.get("sessionId").and_then(Value::as_str) {
                sessions.retain(|_, known| known != session);
                lock(shared)
                    .media
                    .forget_session(session, crate::util::unix_now());
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Parsing answers

/// `Browser.getVersion`'s result.
pub(crate) fn parse_version(result: &Value) -> BrowserVersion {
    BrowserVersion {
        product: string_at(result, "product").unwrap_or_default(),
        protocol_version: string_at(result, "protocolVersion").unwrap_or_default(),
        js_version: string_at(result, "jsVersion"),
        user_agent: string_at(result, "userAgent"),
    }
}

/// `SystemInfo.getInfo`'s result. Missing pieces become empty, never an
/// error: the shape has changed between Chromium versions before.
pub(crate) fn parse_gpu_info(result: &Value, now: u64) -> BrowserGpuInfo {
    let gpu = result.get("gpu").cloned().unwrap_or(Value::Null);
    let devices = array_at(&gpu, "devices")
        .iter()
        .map(|device| BrowserGpuDevice {
            vendor: string_at(device, "vendorString").unwrap_or_default(),
            device: string_at(device, "deviceString").unwrap_or_default(),
            driver_vendor: string_at(device, "driverVendor").filter(|s| !s.is_empty()),
            driver_version: string_at(device, "driverVersion").filter(|s| !s.is_empty()),
        })
        .collect();
    let feature_status = gpu
        .get("featureStatus")
        .and_then(Value::as_object)
        .map(|features| {
            features
                .iter()
                .map(|(key, value)| (key.clone(), value_text(value)))
                .collect()
        })
        .unwrap_or_default();
    BrowserGpuInfo {
        devices,
        feature_status,
        video_decoding: array_at(&gpu, "videoDecoding")
            .iter()
            .map(parse_profile)
            .collect(),
        video_encoding: array_at(&gpu, "videoEncoding")
            .iter()
            .map(parse_profile)
            .collect(),
        driver_bug_workarounds: array_at(&gpu, "driverBugWorkarounds")
            .iter()
            .map(value_text)
            .collect(),
        command_line: string_at(result, "commandLine"),
        fetched_at: now,
    }
}

fn parse_profile(entry: &Value) -> VideoProfile {
    let dimension = |object: &str, field: &str| {
        entry
            .get(object)
            .and_then(|size| size.get(field))
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(0)
    };
    VideoProfile {
        profile: string_at(entry, "profile").unwrap_or_default(),
        max_width: dimension("maxResolution", "width"),
        max_height: dimension("maxResolution", "height"),
        min_width: dimension("minResolution", "width"),
        min_height: dimension("minResolution", "height"),
    }
}

fn string_at(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

fn array_at<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// A string as itself, anything else as its JSON.
fn value_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// The playhead

/// How long the page may spend on [`PLAYHEAD_EXPRESSION`] before Chromium
/// abandons it (`Runtime.evaluate`'s experimental `timeout`).
pub const EVALUATE_TIMEOUT_MS: u64 = 2000;

/// Walks every `video` and `audio` element, including those in open shadow
/// roots, and returns plain JSON shaped like [`Playhead`] (without
/// `readAt`, which Suede stamps on arrival).
pub const PLAYHEAD_EXPRESSION: &str = r#"(() => {
  const out = [];
  const walk = (root) => {
    for (const el of root.querySelectorAll("video, audio")) {
      const isVideo = el.tagName === "VIDEO";
      let quality = null;
      if (isVideo && typeof el.getVideoPlaybackQuality === "function") {
        quality = el.getVideoPlaybackQuality();
      }
      out.push({
        element: isVideo ? "video" : "audio",
        currentSrc: el.currentSrc || "",
        currentTimeSeconds: Number.isFinite(el.currentTime) ? el.currentTime : 0,
        durationSeconds: Number.isFinite(el.duration) ? el.duration : null,
        paused: !!el.paused,
        ended: !!el.ended,
        readyState: el.readyState,
        networkState: el.networkState,
        playbackRate: el.playbackRate,
        muted: !!el.muted,
        volume: el.volume,
        videoWidth: el.videoWidth || 0,
        videoHeight: el.videoHeight || 0,
        totalFrames: quality ? quality.totalVideoFrames : null,
        droppedFrames: quality ? quality.droppedVideoFrames : null,
      });
    }
    for (const host of root.querySelectorAll("*")) {
      if (host.shadowRoot) walk(host.shadowRoot);
    }
  };
  walk(document);
  return out;
})()"#;

/// The `Runtime.evaluate` parameters for one playhead read.
fn playhead_params() -> Value {
    json!({
        "expression": PLAYHEAD_EXPRESSION,
        "returnByValue": true,
        "awaitPromise": false,
        "timeout": EVALUATE_TIMEOUT_MS,
    })
}

/// `Runtime.evaluate`'s result for [`PLAYHEAD_EXPRESSION`], stamped `now`.
pub(crate) fn parse_playheads(result: &Value, now: u64) -> Result<Vec<Playhead>, String> {
    if let Some(details) = result.get("exceptionDetails") {
        let text = details
            .get("exception")
            .and_then(|exception| exception.get("description"))
            .and_then(Value::as_str)
            .or_else(|| details.get("text").and_then(Value::as_str))
            .unwrap_or("the script threw");
        return Err(format!("the playhead script failed: {text}"));
    }
    let Some(Value::Array(elements)) = result.get("result").and_then(|r| r.get("value")) else {
        return Err("the playhead script returned no array".to_string());
    };
    Ok(elements
        .iter()
        .filter(|entry| entry.is_object())
        .map(|entry| {
            let number = |key: &str| entry.get(key).and_then(Value::as_f64);
            let flag = |key: &str| entry.get(key).and_then(Value::as_bool).unwrap_or(false);
            let count = |key: &str| {
                entry
                    .get(key)
                    .and_then(Value::as_f64)
                    .filter(|n| n.is_finite() && *n >= 0.0)
                    .map(|n| n as u64)
            };
            Playhead {
                element: string_at(entry, "element").unwrap_or_else(|| "video".to_string()),
                current_src: string_at(entry, "currentSrc").unwrap_or_default(),
                current_time_seconds: number("currentTimeSeconds")
                    .filter(|n| n.is_finite())
                    .unwrap_or(0.0),
                duration_seconds: number("durationSeconds").filter(|n| n.is_finite()),
                paused: flag("paused"),
                ended: flag("ended"),
                ready_state: count("readyState").unwrap_or(0).min(255) as u8,
                network_state: count("networkState").unwrap_or(0).min(255) as u8,
                playback_rate: number("playbackRate")
                    .filter(|n| n.is_finite())
                    .unwrap_or(1.0),
                muted: flag("muted"),
                volume: number("volume").filter(|n| n.is_finite()).unwrap_or(1.0),
                video_width: count("videoWidth").unwrap_or(0).min(u32::MAX as u64) as u32,
                video_height: count("videoHeight").unwrap_or(0).min(u32::MAX as u64) as u32,
                total_frames: count("totalFrames"),
                dropped_frames: count("droppedFrames"),
                read_at: now,
            }
        })
        .collect())
}

/// `url` without its query and fragment.
fn without_query(url: &str) -> &str {
    url.split(['?', '#']).next().unwrap_or(url)
}

/// Pair a page's media elements (document order) with its players
/// (`created_at` order): by source URL, then by the URL's path alone, then
/// among several candidates by resolution, then by order. Each element
/// matches at most one player. Returns the elements nobody matched.
pub(crate) fn match_playheads(
    players: &mut [&mut MediaPlayer],
    elements: Vec<Playhead>,
) -> Vec<Playhead> {
    let mut slots: Vec<Option<Playhead>> = elements.into_iter().map(Some).collect();
    let mut order: Vec<usize> = (0..players.len()).collect();
    order.sort_by_key(|&index| players[index].created_at);
    for index in order {
        let player = &mut *players[index];
        let Some(url) = player.source_url.clone() else {
            continue;
        };
        let candidates = |same: &dyn Fn(&str) -> bool| -> Vec<usize> {
            slots
                .iter()
                .enumerate()
                .filter(|(_, slot)| slot.as_ref().is_some_and(|head| same(&head.current_src)))
                .map(|(position, _)| position)
                .collect()
        };
        let mut found = candidates(&|src| src == url);
        if found.is_empty() {
            let path = without_query(&url);
            found = candidates(&|src| without_query(src) == path);
        }
        if found.len() > 1 {
            if let Some(resolution) = player.resolution.as_deref() {
                let narrowed: Vec<usize> = found
                    .iter()
                    .copied()
                    .filter(|&position| {
                        slots[position].as_ref().is_some_and(|head| {
                            format!("{}x{}", head.video_width, head.video_height) == resolution
                        })
                    })
                    .collect();
                if !narrowed.is_empty() {
                    found = narrowed;
                }
            }
        }
        if let Some(&position) = found.first() {
            player.playhead = slots[position].take();
        }
    }
    slots.into_iter().flatten().collect()
}

/// A `kVideoTracks`/`kAudioTracks` value (a JSON array of objects encoded
/// as a string) as one [`MediaTrack`] per entry. `None` when it is not one.
fn parse_tracks(value: &str) -> Option<Vec<MediaTrack>> {
    let entries = match serde_json::from_str::<Value>(value).ok()? {
        Value::Array(entries) => entries,
        object @ Value::Object(_) => vec![object],
        _ => return None,
    };
    Some(
        entries
            .iter()
            .filter_map(Value::as_object)
            .map(|object| MediaTrack {
                fields: object
                    .iter()
                    .map(|(key, value)| {
                        let text = match value {
                            Value::String(text) => text.clone(),
                            other => sorted(other).to_string(),
                        };
                        (key.clone(), text)
                    })
                    .collect(),
            })
            .collect(),
    )
}

/// `value` with every object's keys in sorted order, at any depth.
fn sorted(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let ordered: BTreeMap<&String, Value> =
                object.iter().map(|(key, v)| (key, sorted(v))).collect();
            Value::Object(ordered.into_iter().map(|(k, v)| (k.clone(), v)).collect())
        }
        Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
        other => other.clone(),
    }
}

// ---------------------------------------------------------------------------
// Media players

/// Every media player one browser has reported, keyed by the DevTools
/// session (page) it came from and its `playerId`.
#[derive(Debug, Default)]
pub(crate) struct MediaTracker {
    /// In order of first sight.
    players: Vec<Tracked>,
    /// The last successful playhead read per session.
    playheads: HashMap<String, Vec<Playhead>>,
}

#[derive(Debug)]
struct Tracked {
    session: String,
    player: MediaPlayer,
    /// Unix seconds of the destroy, for [`DESTROYED_RETENTION_SECONDS`].
    destroyed_at: Option<u64>,
}

impl MediaTracker {
    /// Fold one `Media.*` event in. Unknown methods and shapes are ignored.
    pub(crate) fn apply(&mut self, session: Option<&str>, method: &str, params: &Value, now: u64) {
        let session = session.unwrap_or("");
        let player_id = params.get("playerId").and_then(Value::as_str);
        match method {
            "Media.playerCreated" => {
                if let Some(id) = params
                    .get("player")
                    .and_then(|player| player.get("playerId"))
                    .and_then(Value::as_str)
                {
                    self.touch(session, id, now);
                }
            }
            // The older, plural spelling, kept for older browsers.
            "Media.playersCreated" => {
                for entry in array_at(params, "players") {
                    let id = entry
                        .as_str()
                        .or_else(|| entry.get("playerId").and_then(Value::as_str));
                    if let Some(id) = id {
                        self.touch(session, id, now);
                    }
                }
            }
            "Media.playerPropertiesChanged" => {
                let Some(id) = player_id else { return };
                let tracked = self.touch(session, id, now);
                for property in array_at(params, "properties") {
                    let Some(name) = property.get("name").and_then(Value::as_str) else {
                        continue;
                    };
                    let value = property.get("value").map(value_text).unwrap_or_default();
                    set_property(&mut tracked.player, name, value);
                }
            }
            "Media.playerEventsAdded" => {
                let Some(id) = player_id else { return };
                let tracked = self.touch(session, id, now);
                for event in array_at(params, "events") {
                    // `value` is a JSON object encoded as a string.
                    let parsed = match event.get("value") {
                        Some(Value::String(text)) => serde_json::from_str::<Value>(text).ok(),
                        Some(other) => Some(other.clone()),
                        None => None,
                    };
                    if let Some(parsed) = parsed {
                        apply_event(tracked, &parsed, now);
                    }
                }
            }
            "Media.playerMessagesLogged" => {
                let Some(id) = player_id else { return };
                let tracked = self.touch(session, id, now);
                for message in array_at(params, "messages") {
                    let level = string_at(message, "level").unwrap_or_default();
                    if level != "warning" && level != "error" {
                        continue;
                    }
                    let text = string_at(message, "message").unwrap_or_default();
                    push_bounded(
                        &mut tracked.player.messages,
                        MediaMessage {
                            level,
                            message: text,
                        },
                        MAX_MESSAGES,
                    );
                }
            }
            "Media.playerErrorsRaised" => {
                let Some(id) = player_id else { return };
                let tracked = self.touch(session, id, now);
                for error in array_at(params, "errors") {
                    push_bounded(&mut tracked.player.errors, error.to_string(), MAX_ERRORS);
                }
            }
            _ => return,
        }
        self.prune(now);
    }

    /// Mark every player of a page that has gone as destroyed, so it ages
    /// out like one whose destroy event arrived.
    pub(crate) fn forget_session(&mut self, session: &str, now: u64) {
        self.playheads.remove(session);
        for tracked in self.players.iter_mut().filter(|t| t.session == session) {
            if !tracked.player.destroyed {
                tracked.player.destroyed = true;
                tracked.destroyed_at = Some(now);
            }
        }
    }

    /// The players as of `now`, oldest first, after dropping destroyed ones
    /// past their retention.
    #[cfg(test)]
    pub(crate) fn players(&mut self, now: u64) -> Vec<MediaPlayer> {
        self.snapshot(now).0
    }

    /// Replace a session's playheads with a fresh read.
    pub(crate) fn set_playheads(&mut self, session: &str, playheads: Vec<Playhead>) {
        self.playheads.insert(session.to_string(), playheads);
    }

    /// The players as in [`Self::players`], each with its matched playhead
    /// (those older than [`crate::diagnostics::stale_after`] are dropped),
    /// and the elements no player matched.
    pub(crate) fn snapshot(&mut self, now: u64) -> (Vec<MediaPlayer>, Vec<Playhead>) {
        self.prune(now);
        let stale = crate::diagnostics::stale_after().as_secs_f64();
        let mut players: Vec<MediaPlayer> = self.players.iter().map(|t| t.player.clone()).collect();
        let mut sessions: Vec<&String> = self.playheads.keys().collect();
        sessions.sort();
        let mut unmatched = Vec::new();
        for session in sessions {
            let fresh: Vec<Playhead> = self.playheads[session]
                .iter()
                .filter(|head| now.saturating_sub(head.read_at) as f64 <= stale)
                .cloned()
                .collect();
            let mut mine: Vec<&mut MediaPlayer> = players
                .iter_mut()
                .zip(&self.players)
                .filter(|(player, tracked)| tracked.session == *session && !player.destroyed)
                .map(|(player, _)| player)
                .collect();
            unmatched.extend(match_playheads(&mut mine, fresh));
        }
        (players, unmatched)
    }

    /// The player, created if new, with its activity time bumped.
    fn touch(&mut self, session: &str, id: &str, now: u64) -> &mut Tracked {
        let index = match self
            .players
            .iter()
            .position(|t| t.session == session && t.player.id == id)
        {
            Some(index) => index,
            None => {
                while self.players.len() >= MAX_PLAYERS {
                    self.evict_one();
                }
                self.players.push(Tracked {
                    session: session.to_string(),
                    player: MediaPlayer::new(id, now),
                    destroyed_at: None,
                });
                self.players.len() - 1
            }
        };
        let tracked = &mut self.players[index];
        tracked.player.last_activity_at = now;
        tracked
    }

    /// Drop the oldest destroyed player, or failing that the oldest player.
    fn evict_one(&mut self) {
        let oldest = |destroyed_only: bool| {
            self.players
                .iter()
                .enumerate()
                .filter(|(_, t)| !destroyed_only || t.player.destroyed)
                .min_by_key(|(index, t)| (t.player.created_at, *index))
                .map(|(index, _)| index)
        };
        if let Some(index) = oldest(true).or_else(|| oldest(false)) {
            self.players.remove(index);
        }
    }

    fn prune(&mut self, now: u64) {
        self.players.retain(|t| {
            t.destroyed_at
                .is_none_or(|at| now.saturating_sub(at) < DESTROYED_RETENTION_SECONDS)
        });
    }
}

fn push_bounded<T>(list: &mut Vec<T>, item: T, limit: usize) {
    list.push(item);
    if list.len() > limit {
        let excess = list.len() - limit;
        list.drain(..excess);
    }
}

fn non_empty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_string())
}

fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn parse_seconds(value: &Value) -> Option<f64> {
    let seconds = match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    }?;
    seconds.is_finite().then_some(seconds)
}

/// The first track of a `kVideoTracks`/`kAudioTracks` value, which is a JSON
/// array of objects encoded as a string.
fn first_track(value: &str) -> Option<Value> {
    match serde_json::from_str::<Value>(value).ok()? {
        Value::Array(tracks) => tracks.into_iter().next(),
        object @ Value::Object(_) => Some(object),
        _ => None,
    }
}

/// "h264 high" from `{"codec":"h264","profile":"h264 high"}`; the codec
/// alone when there is no profile, and "codec profile" when the profile
/// does not already name the codec.
fn video_codec(track: &Value) -> Option<String> {
    let codec = string_at(track, "codec").filter(|c| !c.is_empty());
    let profile = string_at(track, "profile").filter(|p| !p.is_empty());
    match (codec, profile) {
        (Some(codec), Some(profile)) if profile.starts_with(&codec) => Some(profile),
        (Some(codec), Some(profile)) => Some(format!("{codec} {profile}")),
        (Some(codec), None) => Some(codec),
        (None, profile) => profile,
    }
}

fn set_property(player: &mut MediaPlayer, name: &str, value: String) {
    match name {
        "kFrameUrl" => player.frame_url = non_empty(&value),
        "kVideoDecoderName" => player.video_decoder = non_empty(&value),
        "kIsPlatformVideoDecoder" => player.video_decoder_platform = parse_bool(&value),
        "kAudioDecoderName" => player.audio_decoder = non_empty(&value),
        "kIsPlatformAudioDecoder" => player.audio_decoder_platform = parse_bool(&value),
        "kVideoTracks" => {
            if let Some(tracks) = parse_tracks(&value) {
                player.video_tracks = tracks;
            }
            if let Some(codec) = first_track(&value).as_ref().and_then(video_codec) {
                player.video_codec = Some(codec);
            }
        }
        "kAudioTracks" => {
            if let Some(tracks) = parse_tracks(&value) {
                player.audio_tracks = tracks;
            }
            if let Some(codec) = first_track(&value)
                .and_then(|track| string_at(&track, "codec"))
                .filter(|codec| !codec.is_empty())
            {
                player.audio_codec = Some(codec);
            }
        }
        "kResolution" => {
            if let Some(resolution) = non_empty(&value) {
                player.resolution = Some(resolution);
            }
        }
        "kMaxDuration" => {
            if let Some(seconds) = parse_seconds(&Value::String(value.clone())) {
                player.duration_seconds = Some(seconds);
            }
        }
        "kBitrate" => {
            let trimmed = value.trim();
            player.bitrate = trimmed.parse::<u64>().ok().or_else(|| {
                trimmed
                    .parse::<f64>()
                    .ok()
                    .filter(|bits| bits.is_finite() && *bits >= 0.0)
                    .map(|bits| bits.round() as u64)
            });
        }
        "kIsStreaming" => player.streaming = parse_bool(&value),
        "kRendererName" => player.renderer = non_empty(&value),
        _ => {}
    }
    player.properties.insert(name.to_string(), value);
}

/// Whether a property was reported with a non-empty value.
fn has_property(player: &MediaPlayer, name: &str) -> bool {
    player.properties.get(name).is_some_and(|v| !v.is_empty())
}

fn apply_event(tracked: &mut Tracked, event: &Value, now: u64) {
    let player = &mut tracked.player;
    match event.get("event").and_then(Value::as_str).unwrap_or("") {
        "kLoad" => {
            if let Some(url) = string_at(event, "url") {
                player.source_url = non_empty(&url);
            }
        }
        "kPlay" => {
            player.playing = Some(true);
            player.ended = false;
        }
        "kPause" => player.playing = Some(false),
        "kEnded" => {
            player.playing = Some(false);
            player.ended = true;
        }
        // A `loop` video, or a page replaying one, logs `kEnded`, then
        // `kSeek` to 0 and `kPipelineStateChange: kPlaying` with no second
        // `kPlay` (measured on Chrome 151). The seek is what un-ends it; a
        // seek while paused was preceded by `kPause`, so only a seek out of
        // the ended state resumes `playing`.
        "kSeek" => {
            if player.ended {
                player.ended = false;
                player.playing = Some(true);
            }
        }
        "kPipelineStateChange" => {
            if let Some(state) = string_at(event, "pipeline_state") {
                let state = state.strip_prefix('k').unwrap_or(&state).to_string();
                player.pipeline_state = non_empty(&state);
            }
        }
        "kVideoSizeChanged" => {
            if !has_property(player, "kResolution") {
                if let Some(dimensions) = string_at(event, "dimensions") {
                    player.resolution = non_empty(&dimensions);
                }
            }
        }
        "kDurationChanged" => {
            if !has_property(player, "kMaxDuration") {
                if let Some(seconds) = event.get("duration").and_then(parse_seconds) {
                    player.duration_seconds = Some(seconds);
                }
            }
        }
        "kBufferingStateChanged" => {
            if let Some(state) = event
                .get("pipeline_buffering_state")
                .and_then(|buffering| buffering.get("state"))
                .and_then(Value::as_str)
            {
                player.buffering_state = non_empty(state);
            }
        }
        "kWebMediaPlayerDestroyed" if !player.destroyed => {
            player.destroyed = true;
            tracked.destroyed_at = Some(now);
        }
        // Anything else only counts as activity, which `touch` recorded.
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use tokio::io::DuplexStream;

    // --- framing ---

    #[test]
    fn frames_split_on_nul() {
        let mut decoder = FrameDecoder::default();
        let frames = decoder.push(b"{\"id\":1}\0{\"id\":2}\0");
        assert_eq!(frames, vec![b"{\"id\":1}".to_vec(), b"{\"id\":2}".to_vec()]);
    }

    #[test]
    fn a_partial_frame_waits_for_the_rest() {
        let mut decoder = FrameDecoder::default();
        assert!(decoder.push(b"{\"id\":").is_empty());
        assert!(decoder.push(b"1,\"result\"").is_empty());
        let frames = decoder.push(b":{}}\0{\"id\"");
        assert_eq!(frames, vec![b"{\"id\":1,\"result\":{}}".to_vec()]);
        let frames = decoder.push(b":2}\0");
        assert_eq!(frames, vec![b"{\"id\":2}".to_vec()]);
    }

    #[test]
    fn empty_frames_are_skipped() {
        let mut decoder = FrameDecoder::default();
        assert_eq!(decoder.push(b"\0\0{}\0"), vec![b"{}".to_vec()]);
    }

    #[test]
    fn an_oversized_frame_is_dropped_and_the_next_one_survives() {
        let mut decoder = FrameDecoder::default();
        let big = vec![b'x'; MAX_FRAME_BYTES / 2 + 1];
        assert!(decoder.push(&big).is_empty());
        // Crosses the limit without a NUL: dropped, then skipped to the NUL.
        assert!(decoder.push(&big).is_empty());
        assert_eq!(decoder.take_dropped(), 1);
        assert!(decoder.push(&big).is_empty());
        let frames = decoder.push(b"tail\0{\"id\":3}\0");
        assert_eq!(frames, vec![b"{\"id\":3}".to_vec()]);
        assert_eq!(decoder.take_dropped(), 0);

        // And one that arrives whole in a single read.
        let mut whole = vec![b'y'; MAX_FRAME_BYTES + 1];
        whole.push(0);
        whole.extend_from_slice(b"{}\0");
        assert_eq!(decoder.push(&whole), vec![b"{}".to_vec()]);
        assert_eq!(decoder.take_dropped(), 1);
    }

    #[test]
    fn requests_are_nul_terminated_json() {
        let frame = encode_request(7, "Media.enable", &json!({}), Some("S1"));
        assert_eq!(frame.last(), Some(&0));
        let value: Value = serde_json::from_slice(&frame[..frame.len() - 1]).unwrap();
        assert_eq!(
            value,
            json!({"id": 7, "method": "Media.enable", "params": {}, "sessionId": "S1"})
        );
        let frame = encode_request(1, "Browser.getVersion", &json!({}), None);
        let value: Value = serde_json::from_slice(&frame[..frame.len() - 1]).unwrap();
        assert!(value.get("sessionId").is_none());
    }

    // --- parsing ---

    fn sample_version() -> Value {
        json!({
            "protocolVersion": "1.3",
            "product": "Chrome/151.0.7922.71",
            "revision": "@abc",
            "userAgent": "Mozilla/5.0 Chrome/151",
            "jsVersion": "15.1.1"
        })
    }

    fn sample_info() -> Value {
        json!({
            "gpu": {
                "devices": [{
                    "vendorId": 4318, "deviceId": 7728,
                    "vendorString": "NVIDIA Corporation",
                    "deviceString": "Quadro RTX 8000",
                    "driverVendor": "NVIDIA", "driverVersion": "615.71.09"
                }],
                "auxAttributes": {"glRenderer": "x"},
                "featureStatus": {
                    "video_decode": "enabled",
                    "gpu_compositing": "enabled",
                    "canvas_oop_rasterization": "disabled_software"
                },
                "driverBugWorkarounds": ["disable_a", "enable_b"],
                "videoDecoding": [{
                    "profile": "h264 high",
                    "maxResolution": {"width": 4096, "height": 4096},
                    "minResolution": {"width": 16, "height": 16}
                }],
                "videoEncoding": [{
                    "profile": "h264 main",
                    "maxResolution": {"width": 1920, "height": 1080},
                    "maxFramerateNumerator": 30, "maxFramerateDenominator": 1
                }]
            },
            "modelName": "", "modelVersion": "",
            "commandLine": "/opt/google/chrome/chrome --kiosk"
        })
    }

    #[test]
    fn version_is_parsed() {
        assert_eq!(
            parse_version(&sample_version()),
            BrowserVersion {
                product: "Chrome/151.0.7922.71".into(),
                protocol_version: "1.3".into(),
                js_version: Some("15.1.1".into()),
                user_agent: Some("Mozilla/5.0 Chrome/151".into()),
            }
        );
    }

    #[test]
    fn system_info_is_parsed() {
        let info = parse_gpu_info(&sample_info(), 1234);
        assert_eq!(
            info.devices,
            vec![BrowserGpuDevice {
                vendor: "NVIDIA Corporation".into(),
                device: "Quadro RTX 8000".into(),
                driver_vendor: Some("NVIDIA".into()),
                driver_version: Some("615.71.09".into()),
            }]
        );
        assert_eq!(info.feature_status["video_decode"], "enabled");
        assert_eq!(
            info.feature_status.keys().collect::<Vec<_>>(),
            vec![
                "canvas_oop_rasterization",
                "gpu_compositing",
                "video_decode"
            ],
            "sorted by key"
        );
        assert_eq!(
            info.video_decoding,
            vec![VideoProfile {
                profile: "h264 high".into(),
                max_width: 4096,
                max_height: 4096,
                min_width: 16,
                min_height: 16,
            }]
        );
        assert_eq!(info.video_encoding[0].max_width, 1920);
        assert_eq!(info.video_encoding[0].min_width, 0, "encode has no minimum");
        assert_eq!(info.driver_bug_workarounds, vec!["disable_a", "enable_b"]);
        assert_eq!(
            info.command_line.as_deref(),
            Some("/opt/google/chrome/chrome --kiosk")
        );
        assert_eq!(info.fetched_at, 1234);
    }

    #[test]
    fn a_surprising_system_info_shape_is_not_an_error() {
        let info = parse_gpu_info(&json!({"gpu": {"devices": "nope", "featureStatus": 3}}), 1);
        assert!(info.devices.is_empty());
        assert!(info.feature_status.is_empty());
        let info = parse_gpu_info(&json!(null), 1);
        assert!(info.video_decoding.is_empty());
        assert_eq!(info.command_line, None);
    }

    #[test]
    fn contract_types_serialize_in_camel_case() {
        let status = DevtoolsStatus::new(DevtoolsState::Connected, None);
        assert_eq!(
            serde_json::to_value(&status).unwrap(),
            json!({"state": "connected", "detail": null})
        );
        let player = serde_json::to_value(MediaPlayer::new("p", 5)).unwrap();
        assert!(player.get("videoDecoderPlatform").is_some());
        assert!(player.get("lastActivityAt").is_some());
        assert_eq!(player["sourceUrl"], Value::Null);
    }

    // --- media players ---

    fn props(id: &str, properties: &[(&str, &str)]) -> Value {
        json!({
            "playerId": id,
            "properties": properties
                .iter()
                .map(|(name, value)| json!({"name": name, "value": value}))
                .collect::<Vec<_>>()
        })
    }

    fn events(id: &str, values: &[&str]) -> Value {
        json!({
            "playerId": id,
            "events": values
                .iter()
                .map(|value| json!({"timestamp": 1.0, "value": value}))
                .collect::<Vec<_>>()
        })
    }

    #[test]
    fn a_player_is_assembled_from_its_events() {
        let mut media = MediaTracker::default();
        media.apply(
            Some("S1"),
            "Media.playerCreated",
            &json!({"player": {"playerId": "P1", "domNodeId": 9}}),
            100,
        );
        // An early event may carry no properties at all.
        media.apply(
            Some("S1"),
            "Media.playerPropertiesChanged",
            &props("P1", &[]),
            100,
        );
        media.apply(
            Some("S1"),
            "Media.playerEventsAdded",
            &events(
                "P1",
                &[
                    r#"{"created":"2026-10-12","event":"kMediaLogCreated"}"#,
                    r#"{"event":"kWebMediaPlayerCreated","origin_url":"file:///tmp/"}"#,
                    r#"{"event":"kLoad","url":"file:///tmp/t.mp4"}"#,
                ],
            ),
            101,
        );
        media.apply(
            Some("S1"),
            "Media.playerPropertiesChanged",
            &props(
                "P1",
                &[
                    ("kFrameUrl", "file:///tmp/video.html"),
                    ("kVideoDecoderName", "VaapiVideoDecoder"),
                    ("kIsPlatformVideoDecoder", "true"),
                    ("kAudioDecoderName", "FFmpegAudioDecoder"),
                    ("kIsPlatformAudioDecoder", "false"),
                    (
                        "kVideoTracks",
                        r#"[{"codec":"h264","profile":"h264 high","coded size":"320x240"}]"#,
                    ),
                    ("kAudioTracks", r#"[{"codec":"aac","channels":2}]"#),
                    ("kBitrate", "421000"),
                    ("kIsStreaming", "false"),
                    ("kRendererName", "VideoRendererImpl"),
                ],
            ),
            102,
        );
        media.apply(
            Some("S1"),
            "Media.playerEventsAdded",
            &events(
                "P1",
                &[
                    r#"{"event":"kVideoSizeChanged","dimensions":"320x240"}"#,
                    r#"{"event":"kDurationChanged","duration":20.0}"#,
                    r#"{"event":"kPlay"}"#,
                    r#"{"event":"kPipelineStateChange","pipeline_state":"kPlaying"}"#,
                    r#"{"event":"kBufferingStateChanged","pipeline_buffering_state":{"state":"BUFFERING_HAVE_ENOUGH","reason":"x"}}"#,
                ],
            ),
            103,
        );
        media.apply(
            Some("S1"),
            "Media.playerMessagesLogged",
            &json!({"playerId": "P1", "messages": [
                {"level": "info", "message": "Selected VaapiVideoDecoder for video decoding"},
                {"level": "warning", "message": "slow"},
            ]}),
            104,
        );

        let players = media.players(104);
        assert_eq!(players.len(), 1);
        let p = &players[0];
        assert_eq!(p.id, "P1");
        assert_eq!(p.created_at, 100);
        assert_eq!(p.last_activity_at, 104);
        assert_eq!(p.frame_url.as_deref(), Some("file:///tmp/video.html"));
        assert_eq!(p.source_url.as_deref(), Some("file:///tmp/t.mp4"));
        assert_eq!(p.video_decoder.as_deref(), Some("VaapiVideoDecoder"));
        assert_eq!(p.video_decoder_platform, Some(true));
        assert_eq!(p.audio_decoder.as_deref(), Some("FFmpegAudioDecoder"));
        assert_eq!(p.audio_decoder_platform, Some(false));
        assert_eq!(p.video_codec.as_deref(), Some("h264 high"));
        assert_eq!(p.audio_codec.as_deref(), Some("aac"));
        assert_eq!(
            p.resolution.as_deref(),
            Some("320x240"),
            "from kVideoSizeChanged"
        );
        assert_eq!(p.duration_seconds, Some(20.0), "from kDurationChanged");
        assert_eq!(p.bitrate, Some(421000));
        assert_eq!(p.streaming, Some(false));
        assert_eq!(p.renderer.as_deref(), Some("VideoRendererImpl"));
        assert_eq!(p.pipeline_state.as_deref(), Some("Playing"));
        assert_eq!(p.buffering_state.as_deref(), Some("BUFFERING_HAVE_ENOUGH"));
        assert_eq!(p.playing, Some(true));
        assert!(!p.ended && !p.destroyed);
        assert_eq!(
            p.messages,
            vec![MediaMessage {
                level: "warning".into(),
                message: "slow".into()
            }],
            "info is not kept"
        );
        assert_eq!(p.properties["kVideoDecoderName"], "VaapiVideoDecoder");
        assert_eq!(p.properties.len(), 10);

        // Properties win over events for resolution and duration.
        media.apply(
            Some("S1"),
            "Media.playerPropertiesChanged",
            &props(
                "P1",
                &[("kResolution", "1920x1080"), ("kMaxDuration", "21.5")],
            ),
            105,
        );
        media.apply(
            Some("S1"),
            "Media.playerEventsAdded",
            &events(
                "P1",
                &[
                    r#"{"event":"kVideoSizeChanged","dimensions":"640x480"}"#,
                    r#"{"event":"kDurationChanged","duration":99.0}"#,
                    r#"{"event":"kPause"}"#,
                ],
            ),
            106,
        );
        let p = &media.players(106)[0];
        assert_eq!(p.resolution.as_deref(), Some("1920x1080"));
        assert_eq!(p.duration_seconds, Some(21.5));
        assert_eq!(p.playing, Some(false));

        media.apply(
            Some("S1"),
            "Media.playerEventsAdded",
            &events("P1", &[r#"{"event":"kEnded"}"#]),
            107,
        );
        let p = &media.players(107)[0];
        assert_eq!(p.playing, Some(false));
        assert!(p.ended);

        // The loop boundary: kEnded, kSeek 0, kPlaying, and no second kPlay.
        media.apply(
            Some("S1"),
            "Media.playerEventsAdded",
            &events(
                "P1",
                &[
                    r#"{"event":"kSeek","seek_target":0.0}"#,
                    r#"{"event":"kPipelineStateChange","pipeline_state":"kSeeking"}"#,
                    r#"{"event":"kPipelineStateChange","pipeline_state":"kPlaying"}"#,
                ],
            ),
            108,
        );
        let p = &media.players(108)[0];
        assert_eq!(p.playing, Some(true));
        assert!(!p.ended);
        assert_eq!(p.pipeline_state.as_deref(), Some("Playing"));

        // Errors are kept as their JSON, the last five.
        for n in 0..7 {
            media.apply(
                Some("S1"),
                "Media.playerErrorsRaised",
                &json!({"playerId": "P1", "errors": [{"errorType": "pipeline_error", "code": n}]}),
                108,
            );
        }
        let p = &media.players(108)[0];
        assert_eq!(p.errors.len(), 5);
        assert!(p.errors[0].contains("\"code\":2"), "{:?}", p.errors);
        assert!(p.errors[4].contains("\"code\":6"));
    }

    #[test]
    fn messages_keep_the_last_ten() {
        let mut media = MediaTracker::default();
        for n in 0..15 {
            media.apply(
                None,
                "Media.playerMessagesLogged",
                &json!({"playerId": "P", "messages": [{"level": "error", "message": format!("m{n}")}]}),
                1,
            );
        }
        let p = &media.players(1)[0];
        assert_eq!(p.messages.len(), 10);
        assert_eq!(p.messages[0].message, "m5");
        assert_eq!(p.messages[9].message, "m14");
    }

    #[test]
    fn a_destroyed_player_is_kept_for_a_minute() {
        let mut media = MediaTracker::default();
        media.apply(
            Some("S"),
            "Media.playerEventsAdded",
            &events("P", &[r#"{"event":"kWebMediaPlayerDestroyed"}"#]),
            1000,
        );
        assert!(media.players(1000)[0].destroyed);
        assert_eq!(
            media.players(1000 + DESTROYED_RETENTION_SECONDS - 1).len(),
            1
        );
        assert!(media.players(1000 + DESTROYED_RETENTION_SECONDS).is_empty());
    }

    #[test]
    fn players_are_told_apart_by_page() {
        let mut media = MediaTracker::default();
        let created = json!({"player": {"playerId": "P"}});
        media.apply(Some("S1"), "Media.playerCreated", &created, 1);
        media.apply(Some("S2"), "Media.playerCreated", &created, 2);
        assert_eq!(media.players(2).len(), 2);
        // A page that went away takes its players with it, after retention.
        media.forget_session("S1", 10);
        let players = media.players(10);
        assert!(players[0].destroyed && !players[1].destroyed);
        assert_eq!(media.players(10 + DESTROYED_RETENTION_SECONDS).len(), 1);
    }

    #[test]
    fn eviction_takes_destroyed_players_first_then_the_oldest() {
        let mut media = MediaTracker::default();
        for n in 0..MAX_PLAYERS as u64 {
            media.apply(
                None,
                "Media.playerCreated",
                &json!({"player": {"playerId": format!("P{n}")}}),
                n,
            );
        }
        // P5 is destroyed; P0 is older but still alive.
        media.apply(
            None,
            "Media.playerEventsAdded",
            &events("P5", &[r#"{"event":"kWebMediaPlayerDestroyed"}"#]),
            40,
        );
        media.apply(
            None,
            "Media.playerCreated",
            &json!({"player": {"playerId": "new1"}}),
            41,
        );
        let ids: Vec<String> = media.players(41).into_iter().map(|p| p.id).collect();
        assert_eq!(ids.len(), MAX_PLAYERS);
        assert!(!ids.contains(&"P5".to_string()), "destroyed evicted first");
        assert!(ids.contains(&"P0".to_string()));
        assert_eq!(ids.last().unwrap(), "new1");

        // With none destroyed, the oldest by creation goes.
        media.apply(
            None,
            "Media.playerCreated",
            &json!({"player": {"playerId": "new2"}}),
            42,
        );
        let ids: Vec<String> = media.players(42).into_iter().map(|p| p.id).collect();
        assert_eq!(ids.len(), MAX_PLAYERS);
        assert!(!ids.contains(&"P0".to_string()));
        assert!(ids.contains(&"P1".to_string()));
    }

    #[test]
    fn unknown_events_and_shapes_only_count_as_activity() {
        let mut media = MediaTracker::default();
        media.apply(
            None,
            "Media.playerCreated",
            &json!({"player": {"playerId": "P"}}),
            1,
        );
        media.apply(
            None,
            "Media.playerEventsAdded",
            &json!({"playerId": "P", "events": [
                {"timestamp": 1, "value": "not json"},
                {"timestamp": 1, "value": r#"{"event":"kSomethingNew"}"#},
                {"timestamp": 1},
            ]}),
            5,
        );
        media.apply(
            None,
            "Media.playerPropertiesChanged",
            &json!({"playerId": "P", "properties": "x"}),
            6,
        );
        media.apply(
            None,
            "Media.playerPropertiesChanged",
            &json!({"properties": []}),
            7,
        );
        media.apply(None, "Media.somethingElse", &json!({"playerId": "Q"}), 8);
        let players = media.players(8);
        assert_eq!(players.len(), 1);
        assert_eq!(players[0].last_activity_at, 6);
        assert_eq!(players[0].pipeline_state, None);
    }

    #[test]
    fn video_codec_takes_the_profile_when_it_names_the_codec() {
        assert_eq!(
            video_codec(&json!({"codec": "h264", "profile": "h264 high"})).as_deref(),
            Some("h264 high")
        );
        assert_eq!(
            video_codec(&json!({"codec": "vp9"})).as_deref(),
            Some("vp9")
        );
        assert_eq!(
            video_codec(&json!({"codec": "hevc", "profile": "main"})).as_deref(),
            Some("hevc main")
        );
        assert_eq!(video_codec(&json!({})), None);
    }

    // --- the session, against a fake Chromium ---

    /// Plays Chromium's side of the two pipes.
    struct FakeChromium {
        /// What the client wrote (Chromium's fd 3).
        requests: DuplexStream,
        /// What the client reads (Chromium's fd 4).
        events: Option<DuplexStream>,
        decoder: FrameDecoder,
        queued: VecDeque<Value>,
    }

    impl FakeChromium {
        async fn next_request(&mut self) -> Value {
            loop {
                if let Some(request) = self.queued.pop_front() {
                    return request;
                }
                let mut chunk = [0u8; 4096];
                let read =
                    tokio::time::timeout(Duration::from_secs(5), self.requests.read(&mut chunk))
                        .await
                        .expect("a request within 5 s")
                        .unwrap();
                assert!(read > 0, "the client closed its writer");
                for frame in self.decoder.push(&chunk[..read]) {
                    self.queued
                        .push_back(serde_json::from_slice(&frame).unwrap());
                }
            }
        }

        async fn send(&mut self, value: Value) {
            let mut bytes = serde_json::to_vec(&value).unwrap();
            bytes.push(0);
            self.events
                .as_mut()
                .unwrap()
                .write_all(&bytes)
                .await
                .unwrap();
        }

        async fn answer(&mut self, request: &Value, result: Value) {
            let mut response = json!({"id": request["id"], "result": result});
            if let Some(session) = request.get("sessionId") {
                response["sessionId"] = session.clone();
            }
            self.send(response).await;
        }

        async fn expect(&mut self, method: &str) -> Value {
            let request = self.next_request().await;
            assert_eq!(request["method"], method, "{request}");
            request
        }
    }

    fn connect(timing: Timing) -> (Client, FakeChromium) {
        let (client_writer, fake_requests) = tokio::io::duplex(1 << 16);
        let (client_reader, fake_events) = tokio::io::duplex(1 << 16);
        let client = Client::with_io("test".into(), client_reader, client_writer, timing);
        let fake = FakeChromium {
            requests: fake_requests,
            events: Some(fake_events),
            decoder: FrameDecoder::default(),
            queued: VecDeque::new(),
        };
        (client, fake)
    }

    async fn eventually(
        client: &Client,
        what: &str,
        check: impl Fn(&Internals) -> bool,
    ) -> Internals {
        for _ in 0..500 {
            let internals = client.internals();
            if check(&internals) {
                return internals;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("never saw {what}: {:?}", client.internals());
    }

    #[tokio::test]
    async fn the_handshake_attaches_to_every_page_and_follows_media() {
        let (client, mut fake) = connect(Timing::default());
        assert_eq!(client.internals().status.state, DevtoolsState::Connecting);

        let request = fake.expect("Browser.getVersion").await;
        assert!(request.get("sessionId").is_none());
        fake.answer(&request, sample_version()).await;
        let request = fake.expect("SystemInfo.getInfo").await;
        fake.answer(&request, sample_info()).await;
        let request = fake.expect("Target.setDiscoverTargets").await;
        assert_eq!(request["params"], json!({"discover": true}));
        fake.answer(&request, json!({})).await;
        // Discovery announces the existing page before getTargets answers;
        // it must not be attached twice.
        fake.send(json!({"method": "Target.targetCreated", "params": {"targetInfo":
            {"targetId": "T1", "type": "page", "title": "", "url": "about:blank", "attached": false}}}))
            .await;
        let request = fake.expect("Target.getTargets").await;
        fake.answer(
            &request,
            json!({"targetInfos": [
                {"targetId": "T1", "type": "page", "title": "", "url": "about:blank", "attached": false},
                {"targetId": "W1", "type": "service_worker", "title": "", "url": "", "attached": false},
            ]}),
        )
        .await;
        let request = fake.expect("Target.attachToTarget").await;
        assert_eq!(
            request["params"],
            json!({"targetId": "T1", "flatten": true})
        );
        fake.answer(&request, json!({"sessionId": "S1"})).await;
        let request = fake.expect("Media.enable").await;
        assert_eq!(request["sessionId"], "S1");
        fake.answer(&request, json!({})).await;

        let internals = eventually(&client, "connected", |i| {
            i.status.state == DevtoolsState::Connected
        })
        .await;
        assert_eq!(internals.status.detail, None);
        assert_eq!(internals.version.unwrap().product, "Chrome/151.0.7922.71");
        assert_eq!(internals.gpu.unwrap().devices[0].device, "Quadro RTX 8000");

        // A page that appears later is attached too; the known one is not
        // attached again, and a service worker never is.
        fake.send(
            json!({"method": "Target.targetCreated", "params": {"targetInfo":
            {"targetId": "W2", "type": "service_worker"}}}),
        )
        .await;
        fake.send(
            json!({"method": "Target.targetCreated", "params": {"targetInfo":
            {"targetId": "T2", "type": "page"}}}),
        )
        .await;
        let request = fake.expect("Target.attachToTarget").await;
        assert_eq!(request["params"]["targetId"], "T2");
        fake.answer(&request, json!({"sessionId": "S2"})).await;
        let request = fake.expect("Media.enable").await;
        assert_eq!(request["sessionId"], "S2");
        fake.answer(&request, json!({})).await;

        // Session-scoped media events land in the player list.
        fake.send(json!({"method": "Media.playerCreated", "sessionId": "S1",
            "params": {"player": {"playerId": "P1", "domNodeId": 3}}}))
            .await;
        fake.send(
            json!({"method": "Media.playerPropertiesChanged", "sessionId": "S1",
            "params": {"playerId": "P1", "properties": [
                {"name": "kVideoDecoderName", "value": "VaapiVideoDecoder"},
                {"name": "kIsPlatformVideoDecoder", "value": "true"}]}}),
        )
        .await;
        // Garbage in between is skipped, not fatal.
        fake.events
            .as_mut()
            .unwrap()
            .write_all(b"not json\0[1,2]\0")
            .await
            .unwrap();
        fake.send(
            json!({"method": "Media.playerEventsAdded", "sessionId": "S1",
            "params": {"playerId": "P1", "events": [
                {"timestamp": 1.0, "value": "{\"event\":\"kPlay\"}"}]}}),
        )
        .await;
        let internals = eventually(&client, "a playing player", |i| {
            i.players.first().is_some_and(|p| p.playing == Some(true))
        })
        .await;
        assert_eq!(internals.players[0].video_decoder_platform, Some(true));

        // The page closing marks its players destroyed.
        fake.send(json!({"method": "Target.targetDestroyed", "params": {"targetId": "T1"}}))
            .await;
        eventually(&client, "the player destroyed", |i| i.players[0].destroyed).await;

        // Chromium exiting closes its fd 4: the session is closed, and the
        // client still holds the write end until it is dropped.
        fake.events = None;
        let internals = eventually(&client, "closed", |i| {
            i.status.state == DevtoolsState::Closed
        })
        .await;
        assert!(internals.status.detail.unwrap().contains("closed"));
        let mut byte = [0u8; 1];
        let still_open =
            tokio::time::timeout(Duration::from_millis(100), fake.requests.read(&mut byte)).await;
        assert!(still_open.is_err(), "the writer must outlive the task");

        drop(client);
        let read = tokio::time::timeout(Duration::from_secs(5), fake.requests.read(&mut byte))
            .await
            .expect("dropping the client closes the writer")
            .unwrap();
        assert_eq!(read, 0);
    }

    #[tokio::test]
    async fn a_handshake_where_everything_fails_is_failed() {
        let (client, mut fake) = connect(Timing::default());
        for method in [
            "Browser.getVersion",
            "SystemInfo.getInfo",
            "Target.setDiscoverTargets",
            "Target.getTargets",
        ] {
            let request = fake.expect(method).await;
            fake.send(json!({"id": request["id"], "error": {"code": -32601, "message": format!("'{method}' wasn't found")}}))
                .await;
        }
        let internals = eventually(&client, "failed", |i| {
            i.status.state == DevtoolsState::Failed
        })
        .await;
        let detail = internals.status.detail.unwrap();
        assert!(
            detail.contains("Browser.getVersion: 'Browser.getVersion' wasn't found"),
            "{detail}"
        );
        assert!(detail.contains("Target.getTargets"), "{detail}");
    }

    #[tokio::test]
    async fn a_partial_handshake_is_connected_with_the_failures_noted() {
        let (client, mut fake) = connect(Timing::default());
        let request = fake.expect("Browser.getVersion").await;
        fake.answer(&request, sample_version()).await;
        let request = fake.expect("SystemInfo.getInfo").await;
        fake.send(json!({"id": request["id"], "error": {"code": -1, "message": "nope"}}))
            .await;
        let request = fake.expect("Target.setDiscoverTargets").await;
        fake.answer(&request, json!({})).await;
        let request = fake.expect("Target.getTargets").await;
        fake.answer(&request, json!({"targetInfos": []})).await;
        let internals = eventually(&client, "connected with detail", |i| {
            i.status.state == DevtoolsState::Connected && i.status.detail.is_some()
        })
        .await;
        assert_eq!(
            internals.status.detail.as_deref(),
            Some("SystemInfo.getInfo: nope")
        );
        assert!(internals.gpu.is_none());
    }

    #[tokio::test]
    async fn a_silent_browser_times_out_into_failed() {
        let timing = Timing {
            request_timeout: Duration::from_millis(50),
            refresh_interval: Duration::from_secs(3600),
            ..Timing::default()
        };
        let (client, _fake) = connect(timing);
        let internals = eventually(&client, "failed", |i| {
            i.status.state == DevtoolsState::Failed
        })
        .await;
        assert!(internals.status.detail.unwrap().contains("timed out"));
    }

    #[tokio::test]
    async fn system_info_is_refreshed() {
        let timing = Timing {
            request_timeout: Duration::from_secs(5),
            refresh_interval: Duration::from_millis(50),
            ..Timing::default()
        };
        let (client, mut fake) = connect(timing);
        for method in [
            "Browser.getVersion",
            "SystemInfo.getInfo",
            "Target.setDiscoverTargets",
        ] {
            let request = fake.expect(method).await;
            fake.answer(
                &request,
                if method == "SystemInfo.getInfo" {
                    sample_info()
                } else {
                    json!({})
                },
            )
            .await;
        }
        let request = fake.expect("Target.getTargets").await;
        fake.answer(&request, json!({"targetInfos": []})).await;
        let request = fake.expect("SystemInfo.getInfo").await;
        let mut changed = sample_info();
        changed["gpu"]["featureStatus"]["video_decode"] = json!("disabled_software");
        fake.answer(&request, changed).await;
        eventually(&client, "the refreshed feature status", |i| {
            i.gpu
                .as_ref()
                .is_some_and(|gpu| gpu.feature_status["video_decode"] == "disabled_software")
        })
        .await;
    }

    #[test]
    fn a_failed_client_reports_why() {
        let client = Client::failed("cannot create the DevTools pipes: EMFILE");
        let internals = client.internals();
        assert_eq!(internals.status.state, DevtoolsState::Failed);
        assert_eq!(
            internals.status.detail.as_deref(),
            Some("cannot create the DevTools pipes: EMFILE")
        );
    }

    // --- the playhead ---

    fn head(src: &str, width: u32, height: u32) -> Playhead {
        Playhead {
            element: "video".into(),
            current_src: src.into(),
            current_time_seconds: 1.5,
            duration_seconds: Some(20.0),
            paused: false,
            ended: false,
            ready_state: 4,
            network_state: 1,
            playback_rate: 1.0,
            muted: false,
            volume: 1.0,
            video_width: width,
            video_height: height,
            total_frames: Some(10),
            dropped_frames: Some(0),
            read_at: 100,
        }
    }

    fn player(id: &str, created_at: u64, url: &str, resolution: Option<&str>) -> MediaPlayer {
        let mut player = MediaPlayer::new(id, created_at);
        player.source_url = Some(url.to_string());
        player.resolution = resolution.map(str::to_string);
        player
    }

    fn matched(
        players: &mut [MediaPlayer],
        elements: Vec<Playhead>,
    ) -> (Vec<Option<String>>, Vec<Playhead>) {
        let mut refs: Vec<&mut MediaPlayer> = players.iter_mut().collect();
        let unmatched = match_playheads(&mut refs, elements);
        let srcs = players
            .iter()
            .map(|p| p.playhead.as_ref().map(|h| h.current_src.clone()))
            .collect();
        (srcs, unmatched)
    }

    #[test]
    fn playheads_parse_with_null_duration_audio_and_shadow_elements() {
        let result = json!({"result": {"type": "object", "value": [
            {"element": "video", "currentSrc": "file:///a.mp4", "currentTimeSeconds": 12.25,
             "durationSeconds": 20.0, "paused": false, "ended": false, "readyState": 4,
             "networkState": 2, "playbackRate": 1.0, "muted": true, "volume": 0.5,
             "videoWidth": 1920, "videoHeight": 1080, "totalFrames": 300, "droppedFrames": 3},
            {"element": "video", "currentSrc": "http://x/live.m3u8", "currentTimeSeconds": 5.0,
             "durationSeconds": null, "paused": true, "ended": false, "readyState": 3,
             "networkState": 2, "playbackRate": 1.0, "muted": false, "volume": 1.0,
             "videoWidth": 640, "videoHeight": 360, "totalFrames": 10, "droppedFrames": 0},
            {"element": "audio", "currentSrc": "file:///b.mp3", "currentTimeSeconds": 0.0,
             "durationSeconds": 3.5, "paused": true, "ended": true, "readyState": 4,
             "networkState": 1, "playbackRate": 1.0, "muted": false, "volume": 1.0,
             "videoWidth": 0, "videoHeight": 0, "totalFrames": null, "droppedFrames": null},
        ]}});
        let heads = parse_playheads(&result, 777).unwrap();
        assert_eq!(heads.len(), 3);
        assert_eq!(heads[0].current_time_seconds, 12.25);
        assert_eq!(heads[0].duration_seconds, Some(20.0));
        assert!(heads[0].muted);
        assert_eq!(heads[0].dropped_frames, Some(3));
        assert_eq!((heads[0].video_width, heads[0].video_height), (1920, 1080));
        assert_eq!(heads[0].read_at, 777);
        assert_eq!(heads[1].duration_seconds, None, "a live stream");
        assert!(heads[1].paused);
        assert_eq!(heads[2].element, "audio");
        assert!(heads[2].ended);
        assert_eq!(heads[2].total_frames, None);
        assert_eq!(heads[2].dropped_frames, None);

        let thrown =
            json!({"result": {"type": "object"}, "exceptionDetails": {"text": "Uncaught"}});
        assert!(parse_playheads(&thrown, 1)
            .unwrap_err()
            .contains("Uncaught"));
        assert!(parse_playheads(&json!({"result": {"value": 3}}), 1).is_err());
    }

    #[test]
    fn the_expression_reaches_shadow_roots_and_both_element_kinds() {
        assert!(PLAYHEAD_EXPRESSION.contains(r#"querySelectorAll("video, audio")"#));
        assert!(PLAYHEAD_EXPRESSION.contains("shadowRoot"));
        assert!(PLAYHEAD_EXPRESSION.contains("Number.isFinite(el.duration)"));
    }

    #[test]
    fn playheads_match_by_url_then_path_then_resolution_then_order() {
        // By URL: order of the elements does not matter.
        let mut players = vec![
            player("A", 1, "file:///a.mp4", None),
            player("B", 2, "file:///b.mp4", None),
        ];
        let (srcs, unmatched) = matched(
            &mut players,
            vec![
                head("file:///b.mp4", 0, 0),
                head("file:///a.mp4", 0, 0),
                head("file:///c.mp4", 0, 0),
            ],
        );
        assert_eq!(
            srcs,
            vec![Some("file:///a.mp4".into()), Some("file:///b.mp4".into())]
        );
        assert_eq!(unmatched.len(), 1);
        assert_eq!(unmatched[0].current_src, "file:///c.mp4");

        // By path when the query differs.
        let mut players = vec![player("A", 1, "http://h/a.mp4?token=1", None)];
        let (srcs, unmatched) = matched(&mut players, vec![head("http://h/a.mp4?token=2", 0, 0)]);
        assert_eq!(srcs, vec![Some("http://h/a.mp4?token=2".into())]);
        assert!(unmatched.is_empty());

        // By resolution among identical URLs.
        let mut players = vec![player("A", 1, "file:///a.mp4", Some("1920x1080"))];
        let (_, unmatched) = matched(
            &mut players,
            vec![
                head("file:///a.mp4", 640, 360),
                head("file:///a.mp4", 1920, 1080),
            ],
        );
        assert_eq!(players[0].playhead.as_ref().unwrap().video_width, 1920);
        assert_eq!(unmatched.len(), 1);
        assert_eq!(unmatched[0].video_width, 640);

        // By order when nothing else separates them: players in
        // created_at order take elements in document order.
        let mut first = head("file:///a.mp4", 100, 100);
        first.current_time_seconds = 1.0;
        let mut second = head("file:///a.mp4", 100, 100);
        second.current_time_seconds = 2.0;
        let mut players = vec![
            player("late", 9, "file:///a.mp4", None),
            player("early", 3, "file:///a.mp4", None),
        ];
        let (_, unmatched) = matched(&mut players, vec![first, second]);
        assert!(unmatched.is_empty());
        assert_eq!(
            players[1].playhead.as_ref().unwrap().current_time_seconds,
            1.0
        );
        assert_eq!(
            players[0].playhead.as_ref().unwrap().current_time_seconds,
            2.0
        );

        // An element is never shared between players.
        let mut players = vec![
            player("A", 1, "file:///a.mp4", None),
            player("B", 2, "file:///a.mp4", None),
        ];
        let (srcs, unmatched) = matched(&mut players, vec![head("file:///a.mp4", 0, 0)]);
        assert_eq!(srcs, vec![Some("file:///a.mp4".into()), None]);
        assert!(unmatched.is_empty());
    }

    #[test]
    fn stale_playheads_are_dropped_and_destroyed_players_get_none() {
        let mut media = MediaTracker::default();
        media.apply(
            Some("S1"),
            "Media.playerEventsAdded",
            &events("P1", &[r#"{"event":"kLoad","url":"file:///a.mp4"}"#]),
            100,
        );
        let mut fresh = head("file:///a.mp4", 0, 0);
        fresh.read_at = 100;
        media.set_playheads("S1", vec![fresh]);
        let (players, unmatched) = media.snapshot(101);
        assert!(players[0].playhead.is_some());
        assert!(unmatched.is_empty());

        let stale_secs = crate::diagnostics::stale_after().as_secs() + 1;
        let (players, unmatched) = media.snapshot(100 + stale_secs);
        assert!(players[0].playhead.is_none(), "too old");
        assert!(unmatched.is_empty(), "dropped, not unmatched");

        media.forget_session("S1", 102);
        let (players, unmatched) = media.snapshot(102);
        assert!(players[0].destroyed);
        assert!(players[0].playhead.is_none());
        assert!(unmatched.is_empty(), "the page's reads went with the page");
    }

    #[test]
    fn tracks_parse_with_nested_objects_serialized_and_sorted() {
        let mut media = MediaTracker::default();
        media.apply(
            Some("S1"),
            "Media.playerPropertiesChanged",
            &props(
                "P1",
                &[
                    (
                        "kVideoTracks",
                        r#"[{"codec":"h264","profile":"h264 high","coded size":"1920x1080","color space":{"transfer":"BT709","primaries":"BT709","range":"LIMITED"},"hdr metadata":{}},{"codec":"vp9","bitrate":1000}]"#,
                    ),
                    ("kAudioTracks", r#"[{"codec":"aac","channels":2}]"#),
                ],
            ),
            100,
        );
        let p = &media.players(100)[0];
        assert_eq!(p.video_tracks.len(), 2);
        let first = &p.video_tracks[0].fields;
        assert_eq!(first["codec"], "h264");
        assert_eq!(
            first["color space"],
            r#"{"primaries":"BT709","range":"LIMITED","transfer":"BT709"}"#
        );
        assert_eq!(first["hdr metadata"], "{}");
        let keys: Vec<&String> = first.keys().collect();
        let mut sorted_keys = keys.clone();
        sorted_keys.sort();
        assert_eq!(keys, sorted_keys);
        assert_eq!(p.video_tracks[1].fields["bitrate"], "1000");
        assert_eq!(p.audio_tracks.len(), 1);
        assert_eq!(p.audio_tracks[0].fields["channels"], "2");
        assert_eq!(
            p.video_codec.as_deref(),
            Some("h264 high"),
            "first track still wins"
        );

        // Garbage leaves the last good list in place.
        media.apply(
            Some("S1"),
            "Media.playerPropertiesChanged",
            &props("P1", &[("kAudioTracks", "not json")]),
            101,
        );
        assert_eq!(media.players(101)[0].audio_tracks.len(), 1);
        // An empty list empties it.
        media.apply(
            Some("S1"),
            "Media.playerPropertiesChanged",
            &props("P1", &[("kAudioTracks", "[]")]),
            102,
        );
        assert!(media.players(102)[0].audio_tracks.is_empty());
    }

    #[tokio::test]
    async fn the_playhead_is_read_once_at_a_time_per_page() {
        let timing = Timing {
            request_timeout: Duration::from_secs(5),
            refresh_interval: Duration::from_secs(3600),
            playhead_interval: Duration::from_millis(40),
        };
        let (client, mut fake) = connect(timing);
        for method in [
            "Browser.getVersion",
            "SystemInfo.getInfo",
            "Target.setDiscoverTargets",
        ] {
            let request = fake.expect(method).await;
            fake.answer(&request, json!({})).await;
        }
        let request = fake.expect("Target.getTargets").await;
        fake.answer(
            &request,
            json!({"targetInfos": [{"targetId": "T1", "type": "page"}]}),
        )
        .await;
        let request = fake.expect("Target.attachToTarget").await;
        fake.answer(&request, json!({"sessionId": "S1"})).await;
        let request = fake.expect("Media.enable").await;
        fake.answer(&request, json!({})).await;

        let first = fake.expect("Runtime.evaluate").await;
        assert_eq!(first["sessionId"], "S1");
        assert_eq!(first["params"]["expression"], PLAYHEAD_EXPRESSION);
        assert_eq!(first["params"]["returnByValue"], true);
        assert_eq!(first["params"]["awaitPromise"], false);
        assert_eq!(first["params"]["timeout"], 2000);

        // Unanswered: several ticks pass and nothing else is sent.
        let another = tokio::time::timeout(Duration::from_millis(250), fake.next_request()).await;
        assert!(
            another.is_err(),
            "a second evaluate while one is outstanding"
        );

        fake.answer(
            &first,
            json!({"result": {"type": "object", "value": [
                {"element": "video", "currentSrc": "file:///a.mp4", "currentTimeSeconds": 2.0,
                 "durationSeconds": null, "paused": false, "ended": false, "readyState": 4,
                 "networkState": 2, "playbackRate": 1.0, "muted": false, "volume": 1.0,
                 "videoWidth": 10, "videoHeight": 10, "totalFrames": 5, "droppedFrames": 0}]}}),
        )
        .await;
        let internals = eventually(&client, "the unmatched element", |i| {
            !i.unmatched_elements.is_empty()
        })
        .await;
        assert_eq!(internals.unmatched_elements[0].current_src, "file:///a.mp4");
        assert_eq!(internals.unmatched_elements[0].duration_seconds, None);

        // With the answer in, the next tick asks again.
        fake.expect("Runtime.evaluate").await;
    }

    // --- fd wiring ---

    #[cfg(unix)]
    #[tokio::test]
    async fn a_child_sees_the_pipes_on_fds_3_and_4() {
        let pipes = DevtoolsPipes::new().unwrap();
        let mut command = tokio::process::Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("echo hi >&4; read x <&3; echo got $x >&4")
            .kill_on_drop(true);
        let (mut child, mut sender, mut receiver) = pipes.spawn(&mut command).unwrap();

        let mut seen = Vec::new();
        let mut chunk = [0u8; 64];
        while !seen.ends_with(b"hi\n") {
            let read = tokio::time::timeout(Duration::from_secs(5), receiver.read(&mut chunk))
                .await
                .expect("the child writes to fd 4")
                .unwrap();
            assert!(
                read > 0,
                "fd 4 closed early: {:?}",
                String::from_utf8_lossy(&seen)
            );
            seen.extend_from_slice(&chunk[..read]);
        }
        sender.write_all(b"there\n").await.unwrap();
        seen.clear();
        loop {
            let read = tokio::time::timeout(Duration::from_secs(5), receiver.read(&mut chunk))
                .await
                .expect("the child answers on fd 4")
                .unwrap();
            if read == 0 {
                break;
            }
            seen.extend_from_slice(&chunk[..read]);
        }
        assert_eq!(String::from_utf8_lossy(&seen), "got there\n");
        assert!(child.wait().await.unwrap().success());
    }
}
