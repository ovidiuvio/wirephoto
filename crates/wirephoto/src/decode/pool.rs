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

use super::accel::{self, Gpu, ProbeResult};
use super::{Accel, MAX_MESSAGE, Op, Probe, Request, Tools, how, send_with_fd};

/// What a worker reads: an open file (sent as a read-only descriptor) or bytes still in memory.
pub enum Input {
    File(std::fs::File),
    Bytes(Arc<Vec<u8>>),
}

/// A reply, and how it was made ([`how`]).
pub struct Done {
    pub bytes: Vec<u8>,
    pub made: u8,
}

/// GPU failures in a row after which the GPU is left alone until the next start.
const GPU_GIVE_UP: u64 = 5;

/// The faster ways to decode, found once at start ([`Pool::probe_gpu`]) and shared by the pools. A GPU that
/// keeps failing is turned off for the rest of the run.
pub struct GpuState {
    accel: parking_lot::RwLock<Accel>,
    /// What `wirephoto status` says about the GPU.
    status: parking_lot::RwLock<String>,
    in_a_row: AtomicU64,
    /// Workers are started able to open the GPU (their sandbox lets them open its render node read-write).
    pub allow: bool,
}

impl GpuState {
    pub fn new(heic_ffmpeg: bool, allow: bool, status: String) -> GpuState {
        GpuState {
            accel: parking_lot::RwLock::new(Accel {
                gpu: None,
                heic_ffmpeg,
            }),
            status: parking_lot::RwLock::new(status),
            in_a_row: AtomicU64::new(0),
            allow,
        }
    }

    pub fn accel(&self) -> Accel {
        self.accel.read().clone()
    }

    pub fn gpu(&self) -> Option<Gpu> {
        self.accel.read().gpu.clone()
    }

    pub fn status(&self) -> String {
        self.status.read().clone()
    }

    /// The probe's outcome.
    pub fn found(&self, gpu: Option<Gpu>, status: String) {
        self.accel.write().gpu = gpu;
        *self.status.write() = status;
    }

    fn record(&self, made: u8) {
        if made & how::GPU_FAILED != 0 {
            let n = self.in_a_row.fetch_add(1, Ordering::Relaxed) + 1;
            if n >= GPU_GIVE_UP && self.accel.read().gpu.is_some() {
                self.accel.write().gpu = None;
                let why =
                    format!("turned off after {n} failures in a row (back on at the next start)");
                tracing::warn!("GPU decoding {why}; decoding on the CPU");
                *self.status.write() = why;
            }
        } else if made & !how::GPU_FAILED == how::GPU {
            self.in_a_row.store(0, Ordering::Relaxed);
        }
    }
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
    /// CPU priority the workers lower themselves to (`nice`): 0, or more for background work.
    nice: i32,
    gpu: Arc<GpuState>,
}

/// How long a worker may take over one image, and over one video.
const IMAGE_TIMEOUT: Duration = Duration::from_secs(60);
const VIDEO_TIMEOUT: Duration = Duration::from_secs(120);

impl Pool {
    /// A pool of up to `max` workers running `program decode`; `tools` are ffmpeg and ffprobe, if found.
    pub fn new(program: PathBuf, max: usize, tools: Option<Tools>, gpu: Arc<GpuState>) -> Pool {
        Self::with_nice(program, max, tools, 0, gpu)
    }

    /// The same, with workers that run at a lower CPU priority (`nice`): the kernel gives the CPU to other work
    /// first, so background renderings use what requests leave over.
    pub fn with_nice(
        program: PathBuf,
        max: usize,
        tools: Option<Tools>,
        nice: i32,
        gpu: Arc<GpuState>,
    ) -> Pool {
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
            nice,
            gpu,
        }
    }

    /// Asks a worker what the GPU decodes (once, at start).
    pub fn probe_gpu(&self, api: accel::Api, device: Option<String>) -> ProbeResult {
        if self.tools.is_none() {
            return ProbeResult {
                gpu: None,
                detail: "no ffmpeg".into(),
            };
        }
        match self.run(
            Op::GpuProbe { api, device },
            Input::Bytes(Arc::new(Vec::new())),
        ) {
            Ok(b) => serde_json::from_slice(&b).unwrap_or_else(|e| ProbeResult {
                gpu: None,
                detail: format!("unreadable probe: {e}"),
            }),
            Err(e) => ProbeResult {
                gpu: None,
                detail: format!("{e:#}"),
            },
        }
    }

    /// The GPU state the pools share.
    pub fn gpu(&self) -> &Arc<GpuState> {
        &self.gpu
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
        self.run_full(op, input).map(|d| d.bytes)
    }

    /// [`Pool::run`], saying how the reply was made.
    pub fn run_full(&self, op: Op, input: Input) -> anyhow::Result<Done> {
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
            accel: self.gpu.accel(),
        };
        let mut w = self.checkout()?;
        match exchange(&mut w, &req, &input, timeout) {
            Ok(reply) => {
                self.checkin(w);
                self.in_a_row.store(0, Ordering::Relaxed);
                if let Ok(d) = &reply {
                    self.gpu.record(d.made);
                }
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
                    let wait = Duration::from_millis(100u64 << (n - 3).min(9))
                        .min(Duration::from_secs(30));
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
        let mut cmd = Command::new(&self.program);
        if self.nice > 0 {
            cmd.env("WIREPHOTO_WORKER_NICE", self.nice.to_string());
        }
        if self.gpu.allow {
            cmd.env("WIREPHOTO_WORKER_GPU", "1");
        }
        let child = cmd
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
) -> anyhow::Result<anyhow::Result<Done>> {
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
    let mut status = [0u8; 6];
    w.sock
        .read_exact(&mut status)
        .map_err(|e| anyhow!("no reply: {e}"))?;
    let made = status[1];
    let len = u32::from_le_bytes(status[2..].try_into().unwrap()) as usize;
    if len > MAX_MESSAGE {
        bail!("reply too large");
    }
    let mut payload = vec![0u8; len];
    w.sock.read_exact(&mut payload)?;
    Ok(match status[0] {
        0 => Ok(Done {
            bytes: payload,
            made,
        }),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn gpu() -> Gpu {
        Gpu {
            api: accel::Api::Vaapi,
            device: None,
            hevc: true,
            hevc10: false,
            jpeg: true,
        }
    }

    #[test]
    fn a_failing_gpu_is_turned_off() {
        let s = GpuState::new(true, true, String::new());
        s.found(Some(gpu()), gpu().describe());
        // Failures with successes between them keep it on.
        for _ in 0..10 {
            s.record(how::NATIVE | how::GPU_FAILED);
            s.record(how::GPU);
        }
        assert!(s.gpu().is_some());
        for _ in 0..GPU_GIVE_UP {
            s.record(how::FFMPEG | how::GPU_FAILED);
        }
        assert!(s.gpu().is_none(), "off after {GPU_GIVE_UP} in a row");
        assert!(s.accel().heic_ffmpeg, "the CPU paths stay");
        assert!(s.status().contains("turned off"));
    }
}
