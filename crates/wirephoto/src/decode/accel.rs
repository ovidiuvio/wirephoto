//! Decoding through ffmpeg: on the GPU (VA-API on Linux, VideoToolbox on macOS) when the server found one that
//! works ([`probe`], once, at start), and HEIC on the CPU too (ffmpeg's HEVC decoder is several times faster
//! than libde265's). Every path here is a fast path: when it fails the caller decodes the usual way.
//!
//! Strict on purpose: frames are asked for in GPU memory and downloaded (`hwdownload`), so a decoder that
//! quietly falls back to the CPU fails here instead, and the GPU is never taken for working when it is not.

use std::process::Command;

use anyhow::{Context, anyhow, bail};
use serde::{Deserialize, Serialize};

use super::video::{Input, parse_ppm, run};
use super::{Source, Tools};
use crate::preview::render::{Decoded, Rgb};

/// How a GPU is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Api {
    Vaapi,
    VideoToolbox,
}

/// A GPU and what [`probe`] found it decodes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gpu {
    pub api: Api,
    /// The DRM render node (VA-API).
    pub device: Option<String>,
    pub hevc: bool,
    pub hevc10: bool,
    pub jpeg: bool,
}

impl Gpu {
    /// "VA-API /dev/dri/renderD128: HEVC, HEVC 10-bit, JPEG".
    pub fn describe(&self) -> String {
        let what: Vec<&str> = [
            (self.hevc, "HEVC"),
            (self.hevc10, "HEVC 10-bit"),
            (self.jpeg, "JPEG"),
        ]
        .into_iter()
        .filter_map(|(on, name)| on.then_some(name))
        .collect();
        let api = match self.api {
            Api::Vaapi => "VA-API",
            Api::VideoToolbox => "VideoToolbox",
        };
        let at = self
            .device
            .as_deref()
            .map(|d| format!(" {d}"))
            .unwrap_or_default();
        format!("{api}{at}: {}", what.join(", "))
    }

    pub fn any(&self) -> bool {
        self.hevc || self.hevc10 || self.jpeg
    }

    /// Input options that decode on this GPU, frames kept in its memory (`strict`) or handed over in system
    /// memory, ffmpeg falling back to the CPU by itself (video posters).
    pub fn input_args(&self, strict: bool) -> Vec<String> {
        let mut a: Vec<String> = Vec::new();
        match self.api {
            Api::Vaapi => {
                let dev = self.device.as_deref().unwrap_or(DEFAULT_DEVICE);
                a.extend(["-init_hw_device".into(), format!("vaapi=gpu:{dev}")]);
                a.extend(["-hwaccel", "vaapi", "-hwaccel_device", "gpu"].map(String::from));
                if strict {
                    a.extend(["-hwaccel_output_format", "vaapi"].map(String::from));
                }
            }
            Api::VideoToolbox => {
                a.extend(["-hwaccel", "videotoolbox"].map(String::from));
                if strict {
                    a.extend(["-hwaccel_output_format", "videotoolbox_vld"].map(String::from));
                }
            }
        }
        a
    }
}

pub const DEFAULT_DEVICE: &str = "/dev/dri/renderD128";

/// What the startup probe found (JSON from the worker).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProbeResult {
    pub gpu: Option<Gpu>,
    /// Why there is none, or what failed.
    pub detail: String,
}

const SAMPLE_HEVC: &[u8] = include_bytes!("samples/hevc8.hevc");
const SAMPLE_HEVC10: &[u8] = include_bytes!("samples/hevc10.hevc");
const SAMPLE_JPEG: &[u8] = include_bytes!("samples/jpeg.jpg");

/// Decodes a tiny HEVC, 10-bit HEVC and JPEG on the GPU, strictly: what decodes is what the GPU can do. Run once,
/// at start, in a worker (where the decodes will run).
pub fn probe(tools: &Tools, api: Api, device: Option<String>) -> ProbeResult {
    let candidate = Gpu {
        api,
        device,
        hevc: true,
        hevc10: true,
        jpeg: true,
    };
    let try_one = |sample: &'static [u8], demux: Option<&str>, fmt: &str| -> anyhow::Result<()> {
        let mut cmd = Command::new(&tools.ffmpeg);
        cmd.args(["-hide_banner", "-loglevel", "error"]);
        cmd.args(candidate.input_args(true));
        if let Some(f) = demux {
            cmd.args(["-f", f]);
        }
        cmd.args([
            "-i",
            "fd:",
            "-vf",
            &format!("hwdownload,format={fmt}"),
            "-f",
            "null",
            "-",
        ]);
        run(cmd, &Input::Bytes(std::sync::Arc::new(sample.to_vec()))).map(|_| ())
    };
    let mut errors = Vec::new();
    let mut check = |name: &str, r: anyhow::Result<()>| match r {
        Ok(()) => true,
        Err(e) => {
            errors.push(format!("{name}: {}", first_line(&format!("{e:#}"))));
            false
        }
    };
    let gpu = Gpu {
        hevc: check("HEVC", try_one(SAMPLE_HEVC, Some("hevc"), "nv12")),
        hevc10: check(
            "HEVC 10-bit",
            try_one(SAMPLE_HEVC10, Some("hevc"), "p010le"),
        ),
        jpeg: check("JPEG", try_one(SAMPLE_JPEG, None, "nv12")),
        ..candidate
    };
    ProbeResult {
        detail: errors.join("; "),
        gpu: gpu.any().then_some(gpu),
    }
}

fn first_line(s: &str) -> &str {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or(s)
}

/// Whether this ffprobe lists HEIF tile grids (`-show_stream_groups`, ffmpeg 7.0 and later), which HEIC through
/// ffmpeg needs. Asked once, at start.
pub fn reads_tile_grids(tools: &Tools) -> bool {
    Command::new(&tools.ffprobe)
        .args(["-hide_banner", "-help", "full"])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("show_stream_groups"))
}

/// The size ffmpeg scales to before the usual resize: twice the target (or the image itself if smaller), so the
/// last step is still the usual Lanczos one.
fn intermediate(w: u32, h: u32, target: u32) -> (u32, u32) {
    let long = w.max(h);
    let want = target.saturating_mul(2).max(1);
    if long <= want {
        return (w, h);
    }
    let s = want as f64 / long as f64;
    let even = |v: f64| ((v.round() as u32).max(2) + 1) & !1;
    (even(w as f64 * s), even(h as f64 * s))
}

/// An image item as ffprobe sees a HEIF file: a grid of tiles, or one picture.
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    /// (stream index, x, y) of each tile; one entry at (0, 0) for a single picture.
    pub tiles: Vec<(u32, u32, u32)>,
    pub width: u32,
    pub height: u32,
    /// The tiles' pixel format ("yuvj420p", "yuv420p10le", ...).
    pub pix_fmt: String,
}

impl Layout {
    pub fn ten_bit(&self) -> bool {
        self.pix_fmt.contains("10")
    }

    /// 4:2:0, 8 or 10 bits: what a GPU's HEVC decoder takes.
    pub fn gpu_friendly(&self) -> bool {
        matches!(
            self.pix_fmt.as_str(),
            "yuv420p" | "yuvj420p" | "yuv420p10le" | "yuv420p10be"
        )
    }
}

/// The primary image's layout from `ffprobe -show_stream_groups -show_streams -of json`.
pub fn parse_layout(v: &serde_json::Value) -> anyhow::Result<Layout> {
    let streams = v["streams"].as_array().cloned().unwrap_or_default();
    let stream = |i: u64| streams.iter().find(|s| s["index"].as_u64() == Some(i));
    let grid = v["stream_groups"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|g| g["type"] == "Tile Grid" && g["disposition"]["default"] == 1)
        .or_else(|| {
            v["stream_groups"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|g| g["type"] == "Tile Grid")
        });
    if let Some(g) = grid {
        let c = &g["components"][0];
        let tiles: Vec<(u32, u32, u32)> = c["subcomponents"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|s| {
                Some((
                    s["stream_index"].as_u64()? as u32,
                    s["tile_horizontal_offset"].as_u64()? as u32,
                    s["tile_vertical_offset"].as_u64()? as u32,
                ))
            })
            .collect();
        let first = tiles
            .first()
            .ok_or_else(|| anyhow!("a grid without tiles"))?;
        let s = stream(first.0 as u64).ok_or_else(|| anyhow!("tile stream missing"))?;
        return Ok(Layout {
            width: c["width"].as_u64().context("grid width")? as u32,
            height: c["height"].as_u64().context("grid height")? as u32,
            pix_fmt: s["pix_fmt"].as_str().unwrap_or("").to_string(),
            tiles,
        });
    }
    let s = streams
        .iter()
        .filter(|s| s["codec_type"] == "video")
        .find(|s| s["disposition"]["default"] == 1)
        .or_else(|| streams.iter().find(|s| s["codec_type"] == "video"))
        .ok_or_else(|| anyhow!("no picture"))?;
    Ok(Layout {
        tiles: vec![(s["index"].as_u64().context("stream index")? as u32, 0, 0)],
        width: s["width"].as_u64().context("width")? as u32,
        height: s["height"].as_u64().context("height")? as u32,
        pix_fmt: s["pix_fmt"].as_str().unwrap_or("").to_string(),
    })
}

/// The filter graph: tiles downloaded from the GPU (when on it), placed, cropped to the image, scaled to
/// `out`, as RGB.
pub fn graph(l: &Layout, on_gpu: bool, out: (u32, u32)) -> String {
    let fmt = if l.ten_bit() { "p010le" } else { "nv12" };
    let pre = if on_gpu {
        format!("hwdownload,format={fmt}")
    } else {
        "null".to_string()
    };
    let tail = format!("scale={}:{}:flags=area,format=rgb24", out.0, out.1);
    if l.tiles.len() == 1 {
        let (i, ..) = l.tiles[0];
        return format!("[0:{i}]{pre},crop={}:{}:0:0,{tail}[out]", l.width, l.height);
    }
    let mut g = String::new();
    let mut inputs = String::new();
    let mut layout = Vec::with_capacity(l.tiles.len());
    for (n, (i, x, y)) in l.tiles.iter().enumerate() {
        g.push_str(&format!("[0:{i}]{pre}[t{n}];"));
        inputs.push_str(&format!("[t{n}]"));
        layout.push(format!("{x}_{y}"));
    }
    g.push_str(&format!(
        "{inputs}xstack=inputs={}:layout={},crop={}:{}:0:0,{tail}[out]",
        l.tiles.len(),
        layout.join("|"),
        l.width,
        l.height
    ));
    g
}

/// The layout of a HEIF file (ffprobe).
fn layout(tools: &Tools, input: &Input) -> anyhow::Result<Layout> {
    let mut cmd = Command::new(&tools.ffprobe);
    cmd.args(["-hide_banner", "-loglevel", "error", "-of", "json"])
        .args(["-show_stream_groups", "-show_streams", "fd:"]);
    let out = run(cmd, input)?;
    parse_layout(&serde_json::from_slice(&out)?)
}

/// Runs the graph and returns the scaled picture.
fn render(
    tools: &Tools,
    gpu: Option<&Gpu>,
    input: &Input,
    demux_args: &[&str],
    graph: &str,
) -> anyhow::Result<Rgb> {
    let mut cmd = Command::new(&tools.ffmpeg);
    cmd.args(["-hide_banner", "-loglevel", "error", "-noautorotate"]);
    if let Some(g) = gpu {
        cmd.args(g.input_args(true));
    }
    cmd.args(demux_args)
        .args(["-i", "fd:", "-filter_complex", graph, "-map", "[out]"])
        .args(["-frames:v", "1", "-f", "image2pipe", "-c:v", "ppm", "-"]);
    let ppm = run(cmd, input)?;
    if ppm.is_empty() {
        bail!("no picture");
    }
    parse_ppm(&ppm)
}

/// A HEIC's primary image via ffmpeg, on `gpu` when given, scaled near `target`; `None` when the GPU does not
/// decode its format. `icc` comes from libheif, which the caller has checked the file with (no transparency).
pub(crate) fn heic(
    tools: &Tools,
    gpu: Option<&Gpu>,
    src: &Source,
    target: u32,
    icc: Option<Vec<u8>>,
    orientation: u8,
) -> anyhow::Result<Option<Decoded>> {
    let input = Input::of(src)?;
    let l = layout(tools, &input)?;
    // A grid's tiles are separate streams to ffmpeg, each with its own decoder and, on the GPU, its own
    // session: setting those up costs far more than the decoding (measured on a Celeron NAS: ~8 s for a
    // grid, against ~2 s on the CPU). Grids are decoded on the CPU.
    if let Some(g) = gpu
        && (l.tiles.len() > 1
            || !(l.gpu_friendly() && if l.ten_bit() { g.hevc10 } else { g.hevc }))
    {
        return Ok(None);
    }
    let out = intermediate(l.width, l.height, target);
    let img = render(tools, gpu, &input, &[], &graph(&l, gpu.is_some(), out))?;
    Ok(Some(Decoded {
        img,
        icc,
        orientation,
    }))
}

/// A baseline JPEG decoded on the GPU, scaled near `target`; orientation and ICC from its header.
pub fn jpeg(tools: &Tools, gpu: &Gpu, bytes: &[u8], target: u32) -> anyhow::Result<Decoded> {
    let info = crate::media::jpeg::info(bytes).ok_or_else(|| anyhow!("not a JPEG"))?;
    let out = intermediate(info.width, info.height, target);
    // On VA-API the GPU scales too, so only the small picture is copied back.
    let graph = match gpu.api {
        Api::Vaapi => format!(
            "[0:0]scale_vaapi=w={}:h={}:format=nv12,hwdownload,format=nv12,format=rgb24[out]",
            out.0, out.1
        ),
        Api::VideoToolbox => format!(
            "[0:0]hwdownload,format=nv12,scale={}:{}:flags=area,format=rgb24[out]",
            out.0, out.1
        ),
    };
    let input = Input::Bytes(std::sync::Arc::new(bytes.to_vec()));
    let img = render(tools, Some(gpu), &input, &[], &graph)?;
    Ok(Decoded {
        img,
        icc: crate::media::jpeg::icc(bytes),
        orientation: if info.orientation == 0 {
            1
        } else {
            info.orientation
        },
    })
}

/// Whether a JPEG is worth the GPU: a baseline one (what GPU decoders take) of at least this many pixels, below
/// which libjpeg-turbo's scaled decode beats starting ffmpeg.
pub const JPEG_MIN_PIXELS: u64 = 8_000_000;

pub fn jpeg_for_gpu(bytes: &[u8]) -> bool {
    crate::media::jpeg::baseline(bytes)
        && crate::media::jpeg::info(bytes)
            .is_some_and(|i| i.width as u64 * i.height as u64 >= JPEG_MIN_PIXELS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grids_and_single_pictures() {
        let v = serde_json::json!({
            "streams": [
                {"index": 0, "codec_type": "video", "width": 512, "height": 512, "pix_fmt": "yuvj420p", "disposition": {"default": 0}},
                {"index": 1, "codec_type": "video", "width": 512, "height": 512, "pix_fmt": "yuvj420p", "disposition": {"default": 0}},
                {"index": 2, "codec_type": "video", "width": 320, "height": 240, "pix_fmt": "yuvj420p", "disposition": {"default": 0}}
            ],
            "stream_groups": [{"type": "Tile Grid", "disposition": {"default": 1}, "components": [{
                "width": 1000, "height": 500,
                "subcomponents": [
                    {"stream_index": 0, "tile_horizontal_offset": 0, "tile_vertical_offset": 0},
                    {"stream_index": 1, "tile_horizontal_offset": 512, "tile_vertical_offset": 0}
                ]
            }]}]
        });
        let l = parse_layout(&v).unwrap();
        assert_eq!((l.width, l.height, l.tiles.len()), (1000, 500, 2));
        assert!(l.gpu_friendly() && !l.ten_bit());
        assert_eq!(
            graph(&l, true, (640, 320)),
            "[0:0]hwdownload,format=nv12[t0];[0:1]hwdownload,format=nv12[t1];[t0][t1]xstack=inputs=2:\
             layout=0_0|512_0,crop=1000:500:0:0,scale=640:320:flags=area,format=rgb24[out]"
        );
        let single = serde_json::json!({"streams": [
            {"index": 0, "codec_type": "video", "width": 2000, "height": 1334, "pix_fmt": "yuv422p10le", "disposition": {"default": 1}}
        ]});
        let l = parse_layout(&single).unwrap();
        assert_eq!(l.tiles, vec![(0, 0, 0)]);
        assert!(!l.gpu_friendly());
        assert_eq!(
            graph(&l, false, (1280, 854)),
            "[0:0]null,crop=2000:1334:0:0,scale=1280:854:flags=area,format=rgb24[out]"
        );
    }

    /// ffmpeg's decode, rotation and mirroring included, against libheif's, for every EXIF orientation (tiled
    /// files from Apple's encoder) and a 10-bit 4:2:2 picture.
    #[cfg(feature = "heic")]
    #[test]
    fn same_pictures_as_libheif() {
        use crate::preview::render::{self, Encoder};
        let Some(tools) = Tools::find(None).filter(reads_tile_grids) else {
            eprintln!("no ffmpeg 7 or later: skipped");
            return;
        };
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/heic");
        let mut names: Vec<String> = (1..=8).map(|o| format!("orientation-{o}.heic")).collect();
        names.push("single-10bit-422.heic".into());
        for name in names {
            let path = dir.join(&name);
            let bytes = std::fs::read(&path).unwrap();
            let meta = render::heif_meta(&bytes).unwrap();
            let pixels = |d: Decoded| {
                let jpeg = render::finish(d, 320, Encoder::Turbo).unwrap();
                render::decode_jpeg(&jpeg, 10_000).unwrap().img
            };
            let want = pixels(render::decode_heif(&bytes).unwrap());
            let src = Source::File(std::fs::File::open(&path).unwrap());
            let got = pixels(
                heic(&tools, None, &src, 320, meta.icc, meta.orientation)
                    .unwrap()
                    .unwrap(),
            );
            assert_eq!((got.w, got.h), (want.w, want.h), "{name}");
            let diff: u64 = got
                .px
                .iter()
                .zip(&want.px)
                .map(|(a, b)| a.abs_diff(*b) as u64)
                .sum();
            let mean = diff as f64 / got.px.len() as f64;
            assert!(mean < 4.0, "{name}: mean difference {mean:.2}");
        }
    }

    #[test]
    fn scaled_near_twice_the_target() {
        assert_eq!(intermediate(7728, 5152, 640), (1280, 854));
        assert_eq!(intermediate(1000, 800, 640), (1000, 800));
    }
}
