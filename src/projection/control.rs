//! Versioned, bounded control messages for a running slicer.
//!
//! The initial `--spec` remains a [`SlicerSpec`].  Subsequent edits use a
//! complete snapshot so a slow reader can discard superseded work without
//! needing to reconstruct a delta chain.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};

use serde::{de::DeserializeOwned, Deserialize, Serialize};

use super::blend::SlicerSpec;
use crate::model::{ProjectionMode, Renderer, SamplingMode};

/// The only protocol version understood by this release.
pub const CONTROL_VERSION: u32 = 1;
/// A JSON payload is deliberately capped before deserializing it. The
/// terminating newline is not part of this limit.
pub const MAX_CONTROL_LINE_BYTES: usize = 1024 * 1024;

/// One complete desired revision sent to an already-running slicer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlUpdate {
    pub version: u32,
    pub session: String,
    pub generation: u64,
    pub spec: SlicerSpec,
}

impl ControlUpdate {
    pub fn new(session: String, generation: u64, spec: SlicerSpec) -> Self {
        Self {
            version: CONTROL_VERSION,
            session,
            generation,
            spec,
        }
    }
}

/// A lifecycle report emitted by a slicer on stdout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlEvent {
    pub version: u32,
    pub session: String,
    pub generation: u64,
    #[serde(flatten)]
    pub kind: ControlEventKind,
}

/// The stage reached by a control generation.
///
/// `outputs` is carried with each successful stage because an ordinary edit
/// need only rebuild a subset of a complete desired snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ControlEventKind {
    /// Independent of config progress: animation never sends preview writes.
    BlackLift {
        status: Option<crate::model::observed::ProjectionBlackLiftStatus>,
    },
    /// The renderer selected after the slicer has probed its actual capture
    /// and presentation path.  This is deliberately a child report: a
    /// configured GPU label alone does not prove that a capture image can be
    /// sampled with the filtering a warp needs.
    Capability {
        requested_renderer: Renderer,
        effective_renderer: Renderer,
        warp_available: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        requested_mode: ProjectionMode,
        effective_mode: ProjectionMode,
    },
    Accepted,
    Built {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        outputs: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        build_ms: Option<f64>,
    },
    Applied {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        outputs: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        build_ms: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        upload_ms: Option<f64>,
        /// Effective sampler selected for each affected output.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        sampling_modes: BTreeMap<String, SamplingMode>,
    },
    Submitted {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        outputs: Vec<String>,
    },
    Presented {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        outputs: Vec<String>,
    },
    Rejected {
        reason: String,
    },
    Closed {
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

impl ControlEvent {
    pub fn new(session: String, generation: u64, kind: ControlEventKind) -> Self {
        Self {
            version: CONTROL_VERSION,
            session,
            generation,
            kind,
        }
    }
}

/// Read one newline-delimited payload without ever retaining more than
/// `limit` bytes.  An overlong line is drained before returning an error, so
/// a caller may report it and continue at the next message boundary.
pub fn read_bounded_line<R: Read>(reader: &mut R, limit: usize) -> io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        match reader.read(&mut byte) {
            Ok(0) if line.is_empty() => return Ok(None),
            Ok(0) => return Ok(Some(line)),
            Ok(_) if byte[0] == b'\n' => {
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(Some(line));
            }
            Ok(_) if line.len() == limit => {
                while reader.read(&mut byte)? != 0 && byte[0] != b'\n' {}
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("control line exceeds {limit} bytes"),
                ));
            }
            Ok(_) => line.push(byte[0]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

/// Decode one capped JSON line.  Use this in both child stdin and parent
/// stdout readers so neither side grows a string from an untrusted pipe.
pub fn read_bounded_json<R: Read, T: DeserializeOwned>(
    reader: &mut R,
    limit: usize,
) -> io::Result<Option<T>> {
    let Some(line) = read_bounded_line(reader, limit)? else {
        return Ok(None);
    };
    serde_json::from_slice(&line)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Serialize a control message as one capped JSON line, ready for a pipe.
pub fn encode_control_update(update: &ControlUpdate) -> io::Result<Vec<u8>> {
    let mut line = serde_json::to_vec(update)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if line.len() > MAX_CONTROL_LINE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("control line exceeds {MAX_CONTROL_LINE_BYTES} bytes"),
        ));
    }
    line.push(b'\n');
    Ok(line)
}

/// A one-item mailbox that always retains the newest value.
///
/// Writers do not wait for a reader blocked on an operating-system pipe.
/// Replacing the pending value bounds both queue length and memory.
pub struct NewestMailbox<T> {
    inner: Arc<MailboxInner<T>>,
}

impl<T> Clone for NewestMailbox<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

struct MailboxInner<T> {
    state: Mutex<MailboxState<T>>,
    ready: Condvar,
}

struct MailboxState<T> {
    value: Option<T>,
    closed: bool,
}

impl<T> NewestMailbox<T> {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(MailboxInner {
                state: Mutex::new(MailboxState {
                    value: None,
                    closed: false,
                }),
                ready: Condvar::new(),
            }),
        }
    }

    /// Store `value`, returning the pending value it superseded, if any.
    pub fn push(&self, value: T) -> Result<Option<T>, T> {
        let mut state = self.inner.state.lock().unwrap();
        if state.closed {
            return Err(value);
        }
        let old = state.value.replace(value);
        self.inner.ready.notify_one();
        Ok(old)
    }

    /// Wait for the next available value, or `None` once closed and drained.
    pub fn take(&self) -> Option<T> {
        let mut state = self.inner.state.lock().unwrap();
        loop {
            if let Some(value) = state.value.take() {
                return Some(value);
            }
            if state.closed {
                return None;
            }
            state = self.inner.ready.wait(state).unwrap();
        }
    }

    /// Take a pending value without waiting.  This lets a reconciler drain
    /// child reports without turning an idle pass into a blocking read.
    pub fn try_take(&self) -> Option<T> {
        self.inner.state.lock().unwrap().value.take()
    }

    pub fn close(&self) {
        let mut state = self.inner.state.lock().unwrap();
        state.closed = true;
        self.inner.ready.notify_all();
    }
}

impl<T> Default for NewestMailbox<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// Owns the render thread's stdout so it never blocks on — or panics from —
/// writing it directly. `println!` calls `.unwrap()` internally, so a reader
/// that stops draining the pipe (the daemon restarting, a broken pipe) would
/// otherwise either stall the render thread on a full OS pipe buffer or crash
/// it outright; both used to happen from inside the per-frame/per-generation
/// path itself (`LiftRuntime::report`, the stats publisher,
/// `warp_update::Controller::event`).
///
/// Two channels feed one pair of writer threads, matched to how each message
/// kind tolerates loss:
/// - `lifecycle` is a bounded, ordered queue for messages that must not be
///   dropped — one send per control-generation transition (`Accepted`,
///   `Built`, `Applied`, ...), never per frame, so the 4096-deep bound is
///   enormous headroom and an occasional blocking send here is not a
///   per-frame cost.
/// - `telemetry` is [`NewestMailbox`]: periodic status (adaptive black-lift,
///   frame stats) where only the newest value matters and a slow reader
///   must never make memory grow.
///
/// Cheap to clone: every field is itself a cheap-to-clone handle to shared
/// state, so each owner (a `Controller`, a `LiftRuntime`, `State` itself)
/// keeps its own handle to the same two writer threads.
#[derive(Clone)]
pub struct StdoutWriter {
    lifecycle: mpsc::SyncSender<String>,
    telemetry: NewestMailbox<String>,
    closed: Arc<AtomicBool>,
}

/// Write one line to stdout and flush it, returning `false` on any I/O
/// error — most commonly `BrokenPipe`, when the reader has stopped — instead
/// of panicking the way `println!`'s internal `.unwrap()` does.
fn write_line(line: &str) -> bool {
    let mut stdout = io::stdout().lock();
    stdout
        .write_all(line.as_bytes())
        .and_then(|()| stdout.write_all(b"\n"))
        .and_then(|()| stdout.flush())
        .is_ok()
}

impl StdoutWriter {
    pub fn spawn() -> Self {
        let (lifecycle_tx, lifecycle_rx) = mpsc::sync_channel::<String>(4096);
        let telemetry = NewestMailbox::<String>::new();
        let closed = Arc::new(AtomicBool::new(false));

        let lifecycle_closed = closed.clone();
        // Ordered, one line per `send`: nothing here coalesces, so a burst
        // of lifecycle events is never collapsed into just the last one.
        let _ = std::thread::Builder::new()
            .name("slicer-stdout-lifecycle".into())
            .spawn(move || {
                while let Ok(line) = lifecycle_rx.recv() {
                    if !write_line(&line) {
                        lifecycle_closed.store(true, Ordering::Relaxed);
                        break;
                    }
                }
            });

        let telemetry_reader = telemetry.clone();
        let telemetry_closed = closed.clone();
        // Blocks on the mailbox's own condvar between values, so an idle
        // period costs nothing and a burst only ever sends the newest one.
        let _ = std::thread::Builder::new()
            .name("slicer-stdout-telemetry".into())
            .spawn(move || {
                while let Some(line) = telemetry_reader.take() {
                    if !write_line(&line) {
                        telemetry_closed.store(true, Ordering::Relaxed);
                        break;
                    }
                }
            });

        Self {
            lifecycle: lifecycle_tx,
            telemetry,
            closed,
        }
    }

    /// Enqueue a line that must reach the reader, in order. Only ever called
    /// once per control-generation transition, not per frame, so blocking on
    /// a full queue (which the 4096 bound makes practically unreachable) is
    /// not the per-frame stall this type exists to avoid.
    pub fn event(&self, line: String) {
        let _ = self.lifecycle.send(line);
    }

    /// Publish a periodic status line. Calls between writer-thread wakeups
    /// collapse into one: only the newest line is ever written.
    pub fn telemetry(&self, line: String) {
        let _ = self.telemetry.push(line);
    }

    /// Whether a writer thread hit a broken pipe (or another write failure)
    /// and gave up. The render loop polls this to end the child cleanly —
    /// the way stdin EOF already does — instead of a write panicking on the
    /// render thread.
    pub fn closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn bounded_reader_drains_an_oversize_line() {
        let mut input = Cursor::new(b"12345\n{}\n".to_vec());
        let error = read_bounded_line(&mut input, 4).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            read_bounded_line(&mut input, 4).unwrap(),
            Some(b"{}".to_vec())
        );
    }

    #[test]
    fn bounded_reader_keeps_a_final_unterminated_line() {
        let mut input = Cursor::new(b"{}".to_vec());
        assert_eq!(
            read_bounded_line(&mut input, 2).unwrap(),
            Some(b"{}".to_vec())
        );
        assert_eq!(read_bounded_line(&mut input, 2).unwrap(), None);
    }

    #[test]
    fn mailbox_replaces_pending_work() {
        let mailbox = NewestMailbox::new();
        assert!(mailbox.push(1).unwrap().is_none());
        assert_eq!(mailbox.push(2).unwrap(), Some(1));
        assert_eq!(mailbox.take(), Some(2));
        mailbox.close();
        assert_eq!(mailbox.take(), None);
    }
}
