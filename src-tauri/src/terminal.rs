//! Terminal session — bridges a `PtyHandle` to a tokio broadcast +
//! ring buffer so the frontend can stream live bytes over a Tauri
//! channel and reattach without losing scrollback.
//!
//! Per DESIGN-desktop-v2.md § "terminal.rs". The Tauri channel wiring
//! itself lives in Phase 3 (`commands.rs`); this module is transport-
//! agnostic.
#![allow(dead_code)]

use anyhow::Result;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

use crate::spawn::PtyHandle;

/// Default reattach scrollback budget — 256 KiB. The ring is full bytes
/// (escape sequences included); the frontend re-feeds them to xterm.js
/// on connect.
pub const DEFAULT_RING_BYTES: usize = 256 * 1024;

const BROADCAST_CAPACITY: usize = 64;
const READ_CHUNK: usize = 4096;

/// Fixed-capacity byte ring buffer. New bytes evict from the front
/// once `cap` is exceeded.
pub struct RingBuffer {
    data: VecDeque<u8>,
    cap: usize,
}

impl RingBuffer {
    pub fn new(cap: usize) -> Self {
        Self {
            data: VecDeque::with_capacity(cap.min(64 * 1024)),
            cap,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        if self.cap == 0 {
            return;
        }
        if bytes.len() >= self.cap {
            self.data.clear();
            self.data.extend(&bytes[bytes.len() - self.cap..]);
            return;
        }
        let overflow = (self.data.len() + bytes.len()).saturating_sub(self.cap);
        if overflow > 0 {
            self.data.drain(..overflow);
        }
        self.data.extend(bytes);
    }

    pub fn snapshot(&self) -> Vec<u8> {
        self.data.iter().copied().collect()
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.cap
    }
}

/// Live PTY session. The reader runs on a dedicated OS thread (PTY I/O
/// is blocking); each chunk it reads is appended to the ring AND sent
/// on the broadcast channel for any active subscribers.
pub struct TerminalSession {
    id: String,
    ring: Arc<Mutex<RingBuffer>>,
    tx: broadcast::Sender<Vec<u8>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    pty: Mutex<PtyHandle>,
    /// Last `(cols, rows)` this session was resized to. Each session
    /// tracks its own size so switching tabs in the UI never clobbers
    /// another session's PTY dimensions, and a future reattach can use
    /// the real size instead of the spawn-time default.
    last_size: Mutex<Option<(u16, u16)>>,
    /// Abort handle for the most recent `pty_attach` stream loop. Each
    /// attach replaces (and aborts) the previous one so there is at most
    /// one live stream loop per session, even across webview reloads.
    attach_task: Mutex<Option<tokio::task::AbortHandle>>,
}

impl TerminalSession {
    pub fn start(id: impl Into<String>, pty: PtyHandle) -> Result<Arc<Self>> {
        Self::start_with_cap(id, pty, DEFAULT_RING_BYTES)
    }

    pub fn start_with_cap(
        id: impl Into<String>,
        pty: PtyHandle,
        ring_cap: usize,
    ) -> Result<Arc<Self>> {
        let id = id.into();
        let reader = pty.try_clone_reader()?;
        let writer = Arc::new(Mutex::new(pty.take_writer()?));
        let ring = Arc::new(Mutex::new(RingBuffer::new(ring_cap)));
        let (tx, _) = broadcast::channel::<Vec<u8>>(BROADCAST_CAPACITY);

        let session = Arc::new(Self {
            id: id.clone(),
            ring: ring.clone(),
            tx: tx.clone(),
            writer: writer.clone(),
            pty: Mutex::new(pty),
            last_size: Mutex::new(None),
            attach_task: Mutex::new(None),
        });

        // Move reader + ring + tx into a dedicated thread. PTY reads
        // are blocking; we don't want them on the tokio runtime. The
        // writer goes along too so the loop can answer the ConPTY DSR
        // query (see run_reader_loop).
        let writer_for_reader = writer.clone();
        std::thread::Builder::new()
            .name(format!("pty-reader-{id}"))
            .spawn(move || {
                run_reader_loop(reader, ring, tx, writer_for_reader);
            })?;

        Ok(session)
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn write(&self, data: &[u8]) -> Result<()> {
        let mut w = self.writer.lock().unwrap();
        w.write_all(data)?;
        w.flush()?;
        Ok(())
    }

    /// Subscribe to live bytes. The receiver gets every chunk emitted
    /// AFTER subscription; pair with `snapshot()` for full scrollback.
    pub fn subscribe(&self) -> broadcast::Receiver<Vec<u8>> {
        self.tx.subscribe()
    }

    /// Atomically capture scrollback and subscribe to future bytes.
    ///
    /// The reader publishes while holding the same ring lock, so there is
    /// no gap where a chunk can be absent from the snapshot and also sent
    /// before this receiver exists.
    pub fn subscribe_with_snapshot(&self) -> (Vec<u8>, broadcast::Receiver<Vec<u8>>) {
        let ring = self.ring.lock().unwrap();
        let rx = self.tx.subscribe();
        (ring.snapshot(), rx)
    }

    /// Copy of the current ring buffer (entire scrollback within cap).
    pub fn snapshot(&self) -> Vec<u8> {
        self.ring.lock().unwrap().snapshot()
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        self.pty.lock().unwrap().resize(cols, rows)?;
        *self.last_size.lock().unwrap() = Some((cols, rows));
        Ok(())
    }

    /// Last `(cols, rows)` passed to `resize()`, or `None` if the session
    /// was never resized away from its spawn-time default.
    pub fn last_size(&self) -> Option<(u16, u16)> {
        *self.last_size.lock().unwrap()
    }

    /// Record the abort handle for a freshly-spawned `pty_attach` stream
    /// loop, aborting the previous loop (if any) so a session never has
    /// two live stream loops at once.
    pub fn set_attach_task(&self, handle: tokio::task::AbortHandle) {
        if let Some(prev) = self.attach_task.lock().unwrap().replace(handle) {
            prev.abort();
        }
    }

    pub fn kill(&self) -> Result<()> {
        self.pty.lock().unwrap().kill()
    }
}

fn run_reader_loop(
    mut reader: Box<dyn Read + Send>,
    ring: Arc<Mutex<RingBuffer>>,
    tx: broadcast::Sender<Vec<u8>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
) {
    // Windows ConPTY withholds the child's output until the terminal
    // answers a Device Status Report query at startup; the writer goes
    // along so we can reply (see spawn::answer_dsr_cpr). A no-op on Unix.
    let mut dsr_state: u8 = 0;
    let mut buf = [0u8; READ_CHUNK];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => {
                tracing::debug!("pty reader: EOF");
                break;
            }
            Ok(n) => {
                {
                    let mut r = ring.lock().unwrap();
                    r.push(&buf[..n]);
                    // Keep this send under the ring lock so
                    // subscribe_with_snapshot() cannot interleave between
                    // "bytes entered scrollback" and "bytes went live".
                    let _ = tx.send(buf[..n].to_vec());
                }
                crate::spawn::answer_dsr_cpr(&mut dsr_state, &buf[..n], &writer);
            }
            Err(e) => {
                tracing::warn!(error = %e, "pty reader: read error, stopping");
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spawn::SpawnConfig;
    use std::time::{Duration, Instant};

    #[test]
    fn ring_under_cap_accumulates() {
        let mut r = RingBuffer::new(100);
        r.push(b"hello ");
        r.push(b"world");
        assert_eq!(r.snapshot(), b"hello world");
        assert_eq!(r.len(), 11);
    }

    #[test]
    fn ring_truncates_oldest_when_overflowing() {
        let mut r = RingBuffer::new(10);
        r.push(b"0123456789"); // exactly cap
        assert_eq!(r.snapshot(), b"0123456789");
        r.push(b"ABCDE"); // pushes "01234" out
        assert_eq!(r.snapshot(), b"56789ABCDE");
    }

    #[test]
    fn ring_handles_single_chunk_larger_than_cap() {
        let mut r = RingBuffer::new(4);
        r.push(b"ABCDEFGHIJ");
        assert_eq!(r.snapshot(), b"GHIJ");
    }

    #[test]
    fn ring_zero_cap_is_a_noop() {
        let mut r = RingBuffer::new(0);
        r.push(b"anything");
        assert!(r.is_empty());
    }

    fn echo_hi() -> SpawnConfig {
        if cfg!(windows) {
            SpawnConfig::new("cmd").arg("/C").arg("echo hi")
        } else {
            SpawnConfig::new("/bin/sh").arg("-c").arg("echo hi")
        }
    }

    fn echo(text: &str) -> SpawnConfig {
        if cfg!(windows) {
            SpawnConfig::new("cmd")
                .arg("/C")
                .arg(format!("echo {text}"))
        } else {
            SpawnConfig::new("/bin/sh")
                .arg("-c")
                .arg(format!("echo {text}"))
        }
    }

    /// Poll a session's ring for `needle`, answering the ConPTY DSR query
    /// so the child's output flushes on Windows. Returns whether it
    /// appeared before the deadline.
    fn wait_for(session: &TerminalSession, needle: &str, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        let mut answered_dsr = false;
        loop {
            let snap = session.snapshot();
            if String::from_utf8_lossy(&snap).contains(needle) {
                return true;
            }
            if !answered_dsr && snap.windows(4).any(|w| w == b"\x1b[6n") {
                let _ = session.write(b"\x1b[1;1R");
                answered_dsr = true;
            }
            if Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn session_captures_output_in_ring() {
        let pty = PtyHandle::spawn(echo_hi()).expect("spawn");
        let session = TerminalSession::start("test", pty).expect("start session");

        // The reader thread runs the PTY. Poll the ring with a deadline.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut answered_dsr = false;
        loop {
            let snap = session.snapshot();
            if let Ok(s) = std::str::from_utf8(&snap) {
                if s.contains("hi") {
                    return;
                }
            }
            // Windows ConPTY emits a Device Status Report query (ESC[6n)
            // on startup and withholds program output until the terminal
            // answers with a cursor position report. xterm.js answers
            // automatically in the webview; the test must do the same via
            // the session writer, or `echo hi` never flushes. No-op on
            // Unix PTYs, which don't send the query.
            if !answered_dsr && snap.windows(4).any(|w| w == b"\x1b[6n") {
                let _ = session.write(b"\x1b[1;1R");
                answered_dsr = true;
            }
            if Instant::now() > deadline {
                let snap = session.snapshot();
                panic!(
                    "expected 'hi' in ring, got {} bytes: {:?}",
                    snap.len(),
                    String::from_utf8_lossy(&snap)
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn independent_sessions_do_not_share_output() {
        // Two concurrent sessions must each capture ONLY their own PTY's
        // bytes — the multi-terminal bug was one shared sink receiving
        // output from every session. Each session owns a private ring +
        // broadcast, so AAA must never leak into B's ring nor BBB into A.
        let a = TerminalSession::start("A", PtyHandle::spawn(echo("AAA")).expect("spawn A"))
            .expect("start A");
        let b = TerminalSession::start("B", PtyHandle::spawn(echo("BBB")).expect("spawn B"))
            .expect("start B");

        assert!(
            wait_for(&a, "AAA", Duration::from_secs(5)),
            "session A never captured its own output"
        );
        assert!(
            wait_for(&b, "BBB", Duration::from_secs(5)),
            "session B never captured its own output"
        );

        let a_snap = String::from_utf8_lossy(&a.snapshot()).into_owned();
        let b_snap = String::from_utf8_lossy(&b.snapshot()).into_owned();
        assert!(
            !a_snap.contains("BBB"),
            "B's output leaked into A: {a_snap:?}"
        );
        assert!(
            !b_snap.contains("AAA"),
            "A's output leaked into B: {b_snap:?}"
        );
    }

    #[test]
    fn resize_persists_last_size() {
        let session = TerminalSession::start("size", PtyHandle::spawn(echo_hi()).expect("spawn"))
            .expect("start");
        assert_eq!(session.last_size(), None);
        session.resize(100, 40).expect("resize");
        assert_eq!(session.last_size(), Some((100, 40)));
        session.resize(80, 24).expect("resize");
        assert_eq!(session.last_size(), Some((80, 24)));
    }
}
