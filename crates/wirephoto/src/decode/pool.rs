//! The server's side: a pool of long-lived decode workers, spawned when needed and replaced when they die.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use parking_lot::{Condvar, Mutex};

use super::{MAX_MESSAGE, Op, Probe, Request, Tools, send_with_fd};

/// What a worker reads: an open file (sent as a read-only descriptor) or bytes still in memory.
pub enum Input {
    File(std::fs::File),
    Bytes(Arc<Vec<u8>>),
}

struct Worker {
    child: Child,
    sock: UnixStream,
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct State {
    idle: Vec<Worker>,
    /// Workers alive, idle or busy.
    live: usize,
}

pub struct Pool {
    program: PathBuf,
    max: usize,
    tools: Option<Tools>,
    state: Mutex<State>,
    freed: Condvar,
    spawned: AtomicU64,
    lost: AtomicU64,
    /// Workers lost since the last good reply.
    in_a_row: AtomicU64,
}

/// How long a worker may take over one image, and over one video.
const IMAGE_TIMEOUT: Duration = Duration::from_secs(60);
const VIDEO_TIMEOUT: Duration = Duration::from_secs(120);

impl Pool {
    /// A pool of up to `max` workers running `program decode`; `tools` are ffmpeg and ffprobe, if found.
    pub fn new(program: PathBuf, max: usize, tools: Option<Tools>) -> Pool {
        Pool {
            program,
            max: max.max(1),
            tools,
            state: Mutex::new(State {
                idle: Vec::new(),
                live: 0,
            }),
            freed: Condvar::new(),
            spawned: AtomicU64::new(0),
            lost: AtomicU64::new(0),
            in_a_row: AtomicU64::new(0),
        }
    }

    /// Whether videos can be probed and get posters.
    pub fn has_video(&self) -> bool {
        self.tools.is_some()
    }

    /// Workers started so far, and workers lost (crashed, hung or unreachable).
    pub fn stats(&self) -> (u64, u64) {
        (
            self.spawned.load(Ordering::Relaxed),
            self.lost.load(Ordering::Relaxed),
        )
    }

    /// Runs one request in a worker (blocking). Errors are the decoder's ([`DecodeFailed`]: the file cannot be
    /// read), or anything else: the worker's death, a timeout, no worker to start.
    pub fn run(&self, op: Op, input: Input) -> anyhow::Result<Vec<u8>> {
        let timeout = match &op {
            Op::Preview {
                format: super::Format::Video { .. },
                ..
            }
            | Op::Probe => VIDEO_TIMEOUT,
            _ => IMAGE_TIMEOUT,
        };
        let req = Request {
            op,
            bytes: match &input {
                Input::Bytes(b) => Some(b.len() as u64),
                Input::File(_) => None,
            },
            tools: self.tools.clone(),
        };
        let mut w = self.checkout()?;
        match exchange(&mut w, &req, &input, timeout) {
            Ok(reply) => {
                self.checkin(w);
                self.in_a_row.store(0, Ordering::Relaxed);
                reply
            }
            Err(e) => {
                // Dead, hung or out of step: this request fails, the next one gets a fresh worker.
                self.lost.fetch_add(1, Ordering::Relaxed);
                let status = {
                    let _ = w.child.kill();
                    w.child.wait().ok()
                };
                drop(w);
                self.release();
                // Workers that keep dying (one that cannot start, a broken environment) must not be respawned
                // in a tight loop: after three in a row, back off, up to 30 s, and log only now and then.
                let n = self.in_a_row.fetch_add(1, Ordering::Relaxed) + 1;
                if n <= 3 || n.is_multiple_of(50) {
                    tracing::warn!(?status, in_a_row = n, "decode worker lost: {e:#}");
                }
                if n >= 3 {
                    let wait = Duration::from_millis(100u64 << (n - 3).min(9)).min(Duration::from_secs(30));
                    std::thread::sleep(wait);
                }
                Err(e.context("decode worker lost"))
            }
        }
    }

    /// A video's duration and displayed size (blocking); `None` without ffprobe or when it cannot be read.
    pub fn probe(&self, file: std::fs::File) -> Option<Probe> {
        if !self.has_video() {
            return None;
        }
        match self.run(Op::Probe, Input::File(file)) {
            Ok(b) => serde_json::from_slice(&b).ok(),
            Err(e) => {
                tracing::debug!("probe failed: {e:#}");
                None
            }
        }
    }

    fn checkout(&self) -> anyhow::Result<Worker> {
        let mut st = self.state.lock();
        loop {
            while let Some(mut w) = st.idle.pop() {
                if matches!(w.child.try_wait(), Ok(None)) {
                    return Ok(w);
                }
                // Died while idle.
                st.live -= 1;
                self.lost.fetch_add(1, Ordering::Relaxed);
                drop(w);
            }
            if st.live < self.max {
                st.live += 1;
                drop(st);
                return self.spawn().inspect_err(|_| self.release());
            }
            self.freed.wait(&mut st);
        }
    }

    fn checkin(&self, w: Worker) {
        self.state.lock().idle.push(w);
        self.freed.notify_one();
    }

    fn release(&self) {
        self.state.lock().live -= 1;
        self.freed.notify_one();
    }

    fn spawn(&self) -> anyhow::Result<Worker> {
        let (ours, theirs) = UnixStream::pair()?;
        let child = Command::new(&self.program)
            .arg("decode")
            .stdin(Stdio::from(std::os::fd::OwnedFd::from(theirs)))
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("starting {} decode", self.program.display()))?;
        self.spawned.fetch_add(1, Ordering::Relaxed);
        Ok(Worker { child, sock: ours })
    }
}

/// One request and its reply. The outer error means the worker is unusable; the inner one is the decoder's.
fn exchange(
    w: &mut Worker,
    req: &Request,
    input: &Input,
    timeout: Duration,
) -> anyhow::Result<anyhow::Result<Vec<u8>>> {
    w.sock.set_read_timeout(Some(timeout))?;
    w.sock.set_write_timeout(Some(timeout))?;
    let json = serde_json::to_vec(req)?;
    let mut head = Vec::with_capacity(4 + json.len());
    head.extend_from_slice(&(json.len() as u32).to_le_bytes());
    head.extend_from_slice(&json);
    match input {
        Input::File(f) => send_with_fd(&w.sock, &head, Some(f))?,
        Input::Bytes(b) => {
            send_with_fd(&w.sock, &head, None)?;
            w.sock.write_all(b)?;
        }
    }
    let mut status = [0u8; 5];
    w.sock
        .read_exact(&mut status)
        .map_err(|e| anyhow!("no reply: {e}"))?;
    let len = u32::from_le_bytes(status[1..].try_into().unwrap()) as usize;
    if len > MAX_MESSAGE {
        bail!("reply too large");
    }
    let mut payload = vec![0u8; len];
    w.sock.read_exact(&mut payload)?;
    Ok(match status[0] {
        0 => Ok(payload),
        _ => Err(DecodeFailed(String::from_utf8_lossy(&payload).into_owned()).into()),
    })
}

/// A worker ran and its decoder could not read the file: the file's own fault (format, damage), so retrying will
/// not help. Every other error from [`Pool::run`] is the environment's (a worker that crashed or hung, a file
/// that could not be opened) and may well pass.
#[derive(Debug)]
pub struct DecodeFailed(pub String);

impl std::fmt::Display for DecodeFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DecodeFailed {}

/// Whether an error says the file itself cannot be decoded (as opposed to a failure that may pass).
pub fn is_decode_failure(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.is::<DecodeFailed>())
}
