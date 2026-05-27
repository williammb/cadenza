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
    writer: Mutex<Box<dyn Write + Send>>,
    pty: Mutex<PtyHandle>,
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
        let writer = pty.take_writer()?;
        let ring = Arc::new(Mutex::new(RingBuffer::new(ring_cap)));
        let (tx, _) = broadcast::channel::<Vec<u8>>(BROADCAST_CAPACITY);

        let session = Arc::new(Self {
            id: id.clone(),
            ring: ring.clone(),
            tx: tx.clone(),
            writer: Mutex::new(writer),
            pty: Mutex::new(pty),
        });

        // Move reader + ring + tx into a dedicated thread. PTY reads
        // are blocking; we don't want them on the tokio runtime.
        std::thread::Builder::new()
            .name(format!("pty-reader-{id}"))
            .spawn(move || {
                run_reader_loop(reader, ring, tx);
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

    /// Copy of the current ring buffer (entire scrollback within cap).
    pub fn snapshot(&self) -> Vec<u8> {
        self.ring.lock().unwrap().snapshot()
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        self.pty.lock().unwrap().resize(cols, rows)
    }

    pub fn kill(&self) -> Result<()> {
        self.pty.lock().unwrap().kill()
    }
}

fn run_reader_loop(
    mut reader: Box<dyn Read + Send>,
    ring: Arc<Mutex<RingBuffer>>,
    tx: broadcast::Sender<Vec<u8>>,
) {
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
                }
                // send fails only when there are no receivers, which
                // is expected before the frontend subscribes.
                let _ = tx.send(buf[..n].to_vec());
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

    #[test]
    fn session_captures_output_in_ring() {
        let pty = PtyHandle::spawn(echo_hi()).expect("spawn");
        let session = TerminalSession::start("test", pty).expect("start session");

        // The reader thread runs the PTY. Poll the ring with a deadline.
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let snap = session.snapshot();
            if let Ok(s) = std::str::from_utf8(&snap) {
                if s.contains("hi") {
                    return;
                }
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
}
