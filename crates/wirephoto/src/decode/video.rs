//! Videos through ffmpeg and ffprobe, run by the worker (they inherit its sandbox). The source reaches them as
//! their stdin: the file's descriptor itself (seekable, so any MP4 or MOV layout works), or the bytes in memory
//! through a pipe.

use std::io::{Seek, Write};
use std::process::{Command, Stdio};
use std::sync::Arc;

use anyhow::{Context, anyhow, bail};

use super::{Probe, Source, Tools};
use crate::preview::render::{Decoded, Rgb};

/// The largest poster ffmpeg may hand back (8K RGB).
const MAX_PIXELS: usize = 7680 * 4320;

pub(super) enum Input {
    File(std::fs::File),
    Bytes(Arc<Vec<u8>>),
}

impl Input {
    /// The source again, for another run (the caller keeps it).
    pub(super) fn of(src: &Source) -> std::io::Result<Input> {
        Ok(match src {
            Source::File(f) => Input::File(f.try_clone()?),
            Source::Bytes(b) => Input::Bytes(Arc::new(b.clone())),
        })
    }
}

impl From<Source> for Input {
    fn from(s: Source) -> Input {
        match s {
            Source::File(f) => Input::File(f),
            Source::Bytes(b) => Input::Bytes(Arc::new(b)),
        }
    }
}

/// Runs `cmd` with the source as stdin and returns its stdout.
pub(super) fn run(mut cmd: Command, input: &Input) -> anyhow::Result<Vec<u8>> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let feed = match input {
        Input::File(f) => {
            let mut f = f.try_clone()?;
            f.rewind()?;
            cmd.stdin(Stdio::from(f));
            None
        }
        Input::Bytes(b) => {
            cmd.stdin(Stdio::piped());
            Some(b.clone())
        }
    };
    let mut child = cmd.spawn().context("starting ffmpeg")?;
    let writer = feed.and_then(|b| {
        let mut stdin = child.stdin.take()?;
        // ffmpeg stops reading once it has its frame: a broken pipe here is expected.
        Some(std::thread::spawn(move || {
            let _ = stdin.write_all(&b);
        }))
    });
    let out = child.wait_with_output()?;
    if let Some(w) = writer {
        let _ = w.join();
    }
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr);
        bail!("{:?}: {}", out.status, msg.trim());
    }
    Ok(out.stdout)
}

/// A frame about 10% in (the first frame when the duration is unknown or that fails), fitted within `size`.
/// Decoded on `gpu` when given (ffmpeg uses the CPU by itself for what the GPU does not decode), on the CPU when
/// that fails.
pub(super) fn poster(
    tools: &Tools,
    gpu: Option<&super::accel::Gpu>,
    src: Source,
    duration: Option<f64>,
    size: u32,
) -> anyhow::Result<Decoded> {
    if let Some(g) = gpu {
        let again = Input::of(&src)?;
        match poster_with(tools, Some(g), Input::from(src), duration, size) {
            Ok(d) => return Ok(d),
            Err(e) => {
                eprintln!("wirephoto decode: poster on the GPU failed, using the CPU: {e:#}");
                return poster_with(tools, None, again, duration, size);
            }
        }
    }
    poster_with(tools, None, Input::from(src), duration, size)
}

fn poster_with(
    tools: &Tools,
    gpu: Option<&super::accel::Gpu>,
    input: Input,
    duration: Option<f64>,
    size: u32,
) -> anyhow::Result<Decoded> {
    let at = duration
        .filter(|d| d.is_finite() && *d > 0.0)
        .map(|d| d * 0.1);
    let grab = |at: Option<f64>| -> anyhow::Result<Vec<u8>> {
        let mut cmd = Command::new(&tools.ffmpeg);
        cmd.args(["-hide_banner", "-loglevel", "error", "-threads", "1"]);
        if let Some(g) = gpu {
            cmd.args(g.input_args(false));
        }
        if let Some(t) = at {
            cmd.args(["-ss", &format!("{t:.3}")]);
        }
        // Square pixels, then fit within the size, never enlarging. ffmpeg applies the rotation itself.
        let vf = format!(
            "scale='iw*sar':'ih',setsar=1,scale=w='min({size},iw)':h='min({size},ih)':force_original_aspect_ratio=decrease"
        );
        cmd.args([
            "-i",
            "fd:",
            "-map",
            "0:v:0",
            "-frames:v",
            "1",
            "-an",
            "-sn",
            "-dn",
        ])
        .args([
            "-vf",
            &vf,
            "-f",
            "image2pipe",
            "-c:v",
            "ppm",
            "-pix_fmt",
            "rgb24",
            "-",
        ]);
        run(cmd, &input)
    };
    let ppm = match grab(at) {
        Ok(b) if !b.is_empty() => b,
        first => {
            if at.is_none() {
                first?;
                bail!("no frame");
            }
            let b = grab(None)?;
            if b.is_empty() {
                bail!("no frame");
            }
            b
        }
    };
    Ok(Decoded {
        img: parse_ppm(&ppm)?,
        icc: None,
        orientation: 1,
    })
}

/// A binary PPM (P6, 8-bit) -> pixels.
pub(super) fn parse_ppm(b: &[u8]) -> anyhow::Result<Rgb> {
    let mut fields = Vec::with_capacity(4);
    let mut i = 0;
    while fields.len() < 4 {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if b.get(i) == Some(&b'#') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        let start = i;
        while i < b.len() && !b[i].is_ascii_whitespace() {
            i += 1;
        }
        if start == i {
            bail!("truncated PPM header");
        }
        fields.push(std::str::from_utf8(&b[start..i])?);
    }
    i += 1; // the single whitespace before the pixels
    if fields[0] != "P6" || fields[3] != "255" {
        bail!("not an 8-bit PPM");
    }
    let (w, h): (usize, usize) = (fields[1].parse()?, fields[2].parse()?);
    if w == 0 || h == 0 || w * h > MAX_PIXELS {
        bail!("poster size {w}x{h}");
    }
    let px = b
        .get(i..i + w * h * 3)
        .ok_or_else(|| anyhow!("truncated PPM"))?
        .to_vec();
    Ok(Rgb {
        w: w as u32,
        h: h as u32,
        px,
    })
}

/// Duration and displayed size of the first video stream.
pub(super) fn probe(tools: &Tools, src: Source) -> anyhow::Result<Probe> {
    let input = Input::from(src);
    let mut cmd = Command::new(&tools.ffprobe);
    cmd.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-print_format",
        "json",
    ])
    .args([
        "-show_format",
        "-show_streams",
        "-select_streams",
        "v:0",
        "fd:",
    ]);
    let out = run(cmd, &input)?;
    let v: serde_json::Value = serde_json::from_slice(&out)?;
    Ok(parse_probe(&v))
}

fn parse_probe(v: &serde_json::Value) -> Probe {
    let num = |x: &serde_json::Value| -> Option<f64> {
        x.as_f64()
            .or_else(|| x.as_str().and_then(|s| s.parse().ok()))
    };
    let duration = num(&v["format"]["duration"])
        .or_else(|| num(&v["streams"][0]["duration"]))
        .filter(|d| d.is_finite() && *d >= 0.0);
    let s = &v["streams"][0];
    let (w, h) = match (s["width"].as_u64(), s["height"].as_u64()) {
        (Some(w), Some(h)) if w > 0 && h > 0 => (w as f64, h as f64),
        _ => {
            return Probe {
                duration,
                ..Probe::default()
            };
        }
    };
    // Non-square pixels widen or narrow the picture.
    let sar = s["sample_aspect_ratio"]
        .as_str()
        .and_then(|r| r.split_once(':'))
        .and_then(|(a, b)| Some((a.parse::<f64>().ok()?, b.parse::<f64>().ok()?)))
        .filter(|(a, b)| *a > 0.0 && *b > 0.0)
        .map(|(a, b)| a / b)
        .unwrap_or(1.0);
    let w = w * sar;
    let rotation = s["side_data_list"]
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|d| num(&d["rotation"]))
        .or_else(|| num(&s["tags"]["rotate"]))
        .unwrap_or(0.0);
    let quarter = (rotation / 90.0).round() as i64;
    let (w, h) = if quarter.rem_euclid(2) == 1 {
        (h, w)
    } else {
        (w, h)
    };
    Probe {
        duration,
        width: Some(w.round() as u32),
        height: Some(h.round() as u32),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ppm() {
        let mut b = b"P6\n# by ffmpeg\n2 1\n255\n".to_vec();
        b.extend([1, 2, 3, 4, 5, 6]);
        let img = parse_ppm(&b).unwrap();
        assert_eq!((img.w, img.h, img.px), (2, 1, vec![1, 2, 3, 4, 5, 6]));
        assert!(parse_ppm(b"P6\n2 1\n255\n\x01").is_err());
    }

    #[test]
    fn probes() {
        let v = serde_json::json!({
            "streams": [{"width": 1920, "height": 1080, "sample_aspect_ratio": "1:1",
                         "side_data_list": [{"side_data_type": "Display Matrix", "rotation": -90}]}],
            "format": {"duration": "12.345000"}
        });
        assert_eq!(
            parse_probe(&v),
            Probe {
                duration: Some(12.345),
                width: Some(1080),
                height: Some(1920)
            }
        );
        let v = serde_json::json!({"streams": [], "format": {"duration": "3.0"}});
        assert_eq!(parse_probe(&v).width, None);
    }
}
