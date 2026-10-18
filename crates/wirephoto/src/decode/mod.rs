//! Decode workers (design.md, Architecture and Security): every decoder that sees a file's content runs in a
//! `wirephoto decode` child process, sandboxed on Linux. The server keeps a small pool of them ([`Pool`]) and
//! talks to each over a Unix socket on its stdin:
//!
//! - request: `u32` length (little-endian) + a JSON [`Request`], with the source file's read-only descriptor
//!   attached (`SCM_RIGHTS`) or, for bytes still in memory, followed by those bytes;
//! - reply: `u8` status (0 ok, 1 failed) + `u32` length + the JPEG, the probe JSON or the error message.
//!
//! A worker that dies or hangs costs the request it was working on; the pool replaces it.

mod pool;
mod sandbox;
mod video;

use std::io::{self, Read, Write};
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use anyhow::{Context, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::preview::render::{self, Decoded, Encoder};

pub use pool::{DecodeFailed, Input, Pool, is_decode_failure};

/// Requests and replies larger than this are refused (the largest uploads kept in memory are far smaller).
const MAX_MESSAGE: usize = 1 << 30;

/// How a source is decoded.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Format {
    /// A camera RAW's embedded JPEG.
    Raw,
    Jpeg,
    /// HEIC, HEIF, AVIF.
    Heif,
    /// PNG, GIF, WebP, BMP, TIFF through the `image` crate, with the EXIF orientation still to apply.
    Image {
        mime: String,
        orientation: u8,
    },
    /// A poster frame through ffmpeg, about 10% in.
    Video {
        duration: Option<f64>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Op {
    /// A preview JPEG at `size` on the long edge; `cache` picks the encoder for the cached size.
    Preview {
        format: Format,
        size: u32,
        cache: bool,
    },
    /// A video's duration and displayed size, as JSON [`Probe`].
    Probe,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub op: Op,
    /// `None`: the source is the attached descriptor; `Some(n)`: n bytes follow the request.
    pub bytes: Option<u64>,
    /// ffmpeg and ffprobe, when the server found them.
    pub tools: Option<Tools>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tools {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

impl Tools {
    /// ffmpeg and ffprobe: `ffmpeg` given (ffprobe next to it), else from `PATH`. `None` when either is missing.
    pub fn find(ffmpeg: Option<&std::path::Path>) -> Option<Tools> {
        let ffmpeg = match ffmpeg {
            Some(p) => p.to_path_buf(),
            None => which("ffmpeg")?,
        };
        let ffprobe = match ffmpeg.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir.join("ffprobe"),
            _ => which("ffprobe")?,
        };
        (is_executable(&ffmpeg) && is_executable(&ffprobe)).then_some(Tools { ffmpeg, ffprobe })
    }
}

fn which(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| is_executable(p))
}

fn is_executable(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// What [`Op::Probe`] finds in a video.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Probe {
    pub duration: Option<f64>,
    /// Displayed size: rotation and pixel aspect ratio applied.
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// The source a worker reads.
pub(crate) enum Source {
    File(std::fs::File),
    Bytes(Vec<u8>),
}

impl Source {
    fn into_bytes(self) -> io::Result<Vec<u8>> {
        match self {
            Source::File(mut f) => {
                let mut b = Vec::new();
                f.read_to_end(&mut b)?;
                Ok(b)
            }
            Source::Bytes(b) => Ok(b),
        }
    }
}

/// `wirephoto decode`: serves requests on stdin until the server closes it.
pub fn worker_main() -> anyhow::Result<()> {
    // A background worker steps aside for everything else: raising one's own niceness needs no privilege.
    #[cfg(unix)]
    if let Some(n) = std::env::var("WIREPHOTO_WORKER_NICE")
        .ok()
        .and_then(|v| v.parse::<i32>().ok())
    {
        let _ = rustix::process::nice(n);
    }
    sandbox::enter().context("entering the sandbox")?;
    // SAFETY: the server gives the worker one end of a socket pair as its stdin, and nothing else uses fd 0.
    let mut sock = unsafe { UnixStream::from_raw_fd(0) };
    loop {
        let Some((req, src)) = read_request(&mut sock)? else {
            return Ok(());
        };
        let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle(req, src)))
            .unwrap_or_else(|_| Err(anyhow!("decoder panicked")));
        let (status, payload) = match out {
            Ok(b) => (0u8, b),
            Err(e) => (1u8, format!("{e:#}").into_bytes()),
        };
        let mut head = [0u8; 5];
        head[0] = status;
        head[1..].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        sock.write_all(&head)?;
        sock.write_all(&payload)?;
    }
}

fn handle(req: Request, src: Source) -> anyhow::Result<Vec<u8>> {
    match req.op {
        Op::Preview {
            format,
            size,
            cache,
        } => {
            let decoded = decode(&format, src, size, req.tools.as_ref())?;
            let encoder = if cache {
                Encoder::Cache
            } else {
                Encoder::Turbo
            };
            render::finish(decoded, size, encoder)
        }
        Op::Probe => {
            let tools = req.tools.as_ref().ok_or_else(|| anyhow!("no ffprobe"))?;
            Ok(serde_json::to_vec(&video::probe(tools, src)?)?)
        }
    }
}

fn decode(
    format: &Format,
    src: Source,
    size: u32,
    tools: Option<&Tools>,
) -> anyhow::Result<Decoded> {
    Ok(match format {
        Format::Raw => match src {
            Source::File(f) => render::decode_raw(&f, size)?,
            Source::Bytes(b) => render::decode_raw(&b[..], size)?,
        },
        Format::Jpeg => render::decode_jpeg(&src.into_bytes()?, size)?,
        Format::Heif => render::decode_heif(&src.into_bytes()?)?,
        Format::Image { mime, orientation } => {
            render::decode_image(&src.into_bytes()?, mime, *orientation)?
        }
        Format::Video { duration } => {
            let tools = tools.ok_or_else(|| anyhow!("no ffmpeg"))?;
            video::poster(tools, src, *duration, size)?
        }
    })
}

/// Reads one request and its source; `None` when the server has closed the socket.
fn read_request(sock: &mut UnixStream) -> anyhow::Result<Option<(Request, Source)>> {
    let mut len = [0u8; 4];
    let (got, fd) = recv_with_fd(sock, &mut len)?;
    if got == 0 {
        return Ok(None);
    }
    sock.read_exact(&mut len[got..])?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_MESSAGE {
        bail!("request too large");
    }
    let mut json = vec![0u8; len];
    sock.read_exact(&mut json)?;
    let req: Request = serde_json::from_slice(&json)?;
    let src = match req.bytes {
        Some(n) => {
            if n as usize > MAX_MESSAGE {
                bail!("request too large");
            }
            let mut b = vec![0u8; n as usize];
            sock.read_exact(&mut b)?;
            Source::Bytes(b)
        }
        None => Source::File(fd.ok_or_else(|| anyhow!("no file descriptor"))?.into()),
    };
    Ok(Some((req, src)))
}

/// Receives into `buf` (at least one byte unless at end of stream), with the descriptor sent along, if any.
fn recv_with_fd(sock: &UnixStream, buf: &mut [u8]) -> io::Result<(usize, Option<OwnedFd>)> {
    use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, recvmsg};
    use std::mem::MaybeUninit;
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut cmsg = RecvAncillaryBuffer::new(&mut space);
    let mut iov = [io::IoSliceMut::new(buf)];
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let flags = RecvFlags::CMSG_CLOEXEC;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let flags = RecvFlags::empty();
    let r = loop {
        match recvmsg(sock.as_fd(), &mut iov, &mut cmsg, flags) {
            Err(rustix::io::Errno::INTR) => continue,
            r => break r?,
        }
    };
    let mut fd = None;
    for msg in cmsg.drain() {
        if let RecvAncillaryMessage::ScmRights(fds) = msg {
            for f in fds {
                fd.get_or_insert(f); // extra descriptors are closed as they drop
            }
        }
    }
    Ok((r.bytes, fd))
}

/// Sends `buf` with `fd` attached to its first byte.
fn send_with_fd(sock: &UnixStream, buf: &[u8], fd: Option<&std::fs::File>) -> io::Result<()> {
    use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage, SendFlags, sendmsg};
    use std::mem::MaybeUninit;
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut cmsg = SendAncillaryBuffer::new(&mut space);
    let fds;
    if let Some(f) = fd {
        fds = [f.as_fd()];
        if !cmsg.push(SendAncillaryMessage::ScmRights(&fds)) {
            return Err(io::Error::other("no room for the descriptor"));
        }
    }
    let sent = loop {
        match sendmsg(
            sock.as_fd(),
            &[io::IoSlice::new(buf)],
            &mut cmsg,
            SendFlags::empty(),
        ) {
            Err(rustix::io::Errno::INTR) => continue,
            r => break r?,
        }
    };
    let mut s = sock;
    s.write_all(&buf[sent..])
}
