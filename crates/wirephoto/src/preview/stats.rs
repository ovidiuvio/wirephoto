//! How long previews take, by source type, and how each size was served, since the server started: what
//! `wirephoto status` shows. The timings also serve for requests ([`crate::metrics`]).

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// Recent timings kept per type, for the median and the 95th percentile.
const RECENT: usize = 1000;

#[derive(Default)]
struct PerType {
    count: u64,
    total_ms: u64,
    max_ms: u64,
    recent: VecDeque<u32>,
}

/// Timings and counters, cheap to update from any thread.
#[derive(Default)]
pub struct Stats {
    by_type: Mutex<HashMap<String, PerType>>,
    pub served_memory: AtomicU64,
    pub served_disk: AtomicU64,
    by_size: Mutex<BTreeMap<u32, [u64; 5]>>,
    renders: Mutex<HashMap<RenderKey, RenderAcc>>,
}

/// What a preview rendered from its original was for and how it was made.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RenderKey {
    /// `request` (someone was waiting for it) or `ahead` (pre-generation, an upload's preview).
    pub purpose: String,
    /// The source's type: jpeg, heic, raw, video, png, ...
    pub kind: String,
    /// What decoded it: GPU, ffmpeg (on the CPU), libheif, libjpeg-turbo, ...
    pub decoder: String,
    pub size: u32,
}

#[derive(Default)]
struct RenderAcc {
    count: u64,
    failed: u64,
    /// Decoded another way after the GPU failed on them.
    fell_back: u64,
    total_ms: u64,
    max_ms: u64,
    wait_ms: u64,
    decode_ms: u64,
    finish_ms: u64,
    recent: VecDeque<u32>,
}

/// One kind of rendering's counts and times (ms), as `wirephoto status` prints them. `total` is from asking
/// for a worker to the preview stored: `wait` for a free one, then in the worker `decode` (reading and decoding
/// the source) and `finish` (resize, colour, orientation, encode), and `other` (starting the job, sending the
/// file, storing the preview).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RenderTimes {
    pub purpose: String,
    pub kind: String,
    pub decoder: String,
    pub size: u32,
    pub count: u64,
    pub failed: u64,
    pub fell_back: u64,
    pub p50_ms: u64,
    pub p95_ms: u64,
    pub max_ms: u64,
    pub mean_ms: u64,
    pub wait_ms: u64,
    pub decode_ms: u64,
    pub finish_ms: u64,
    pub other_ms: u64,
}

/// One rendering's times (ms).
#[derive(Debug, Clone, Copy, Default)]
pub struct Took {
    pub total: u64,
    pub wait: u64,
    pub decode: u64,
    pub finish: u64,
}

/// Where a preview a request asked for came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Served {
    Memory = 0,
    Disk = 1,
    /// Made from the cached 640.
    FromCached = 2,
    /// Decoded from the original.
    Rendered = 3,
    /// `cached=only` and there was none.
    NotCached = 4,
}

/// One size's counts, as `wirephoto status` prints them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SizeServed {
    pub size: u32,
    pub memory: u64,
    pub disk: u64,
    pub from_cached: u64,
    pub rendered: u64,
    pub not_cached: u64,
}

/// One type's timings, as `wirephoto status` prints them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeTimes {
    pub kind: String,
    pub count: u64,
    pub mean_ms: u64,
    pub p50_ms: u64,
    pub p95_ms: u64,
    pub max_ms: u64,
}

impl Stats {
    pub fn record(&self, kind: &str, ms: u64) {
        let mut all = self.by_type.lock();
        let t = all.entry(kind.to_string()).or_default();
        t.count += 1;
        t.total_ms += ms;
        t.max_ms = t.max_ms.max(ms);
        if t.recent.len() == RECENT {
            t.recent.pop_front();
        }
        t.recent.push_back(ms.min(u32::MAX as u64) as u32);
    }

    pub fn times(&self) -> Vec<TypeTimes> {
        let all = self.by_type.lock();
        let mut out: Vec<TypeTimes> = all
            .iter()
            .map(|(kind, t)| {
                let mut v: Vec<u32> = t.recent.iter().copied().collect();
                v.sort_unstable();
                let at = |q: f64| {
                    v.get(((v.len() as f64 - 1.0) * q).round() as usize)
                        .copied()
                        .unwrap_or(0) as u64
                };
                TypeTimes {
                    kind: kind.clone(),
                    count: t.count,
                    mean_ms: t.total_ms / t.count.max(1),
                    p50_ms: at(0.5),
                    p95_ms: at(0.95),
                    max_ms: t.max_ms,
                }
            })
            .collect();
        out.sort_by_key(|t| std::cmp::Reverse(t.count));
        out
    }

    /// A preview rendered from its original.
    pub fn rendered(&self, key: RenderKey, took: Took, fell_back: bool) {
        let mut all = self.renders.lock();
        let a = all.entry(key).or_default();
        a.count += 1;
        a.fell_back += fell_back as u64;
        a.total_ms += took.total;
        a.max_ms = a.max_ms.max(took.total);
        a.wait_ms += took.wait;
        a.decode_ms += took.decode;
        a.finish_ms += took.finish;
        if a.recent.len() == RECENT {
            a.recent.pop_front();
        }
        a.recent.push_back(took.total.min(u32::MAX as u64) as u32);
    }

    /// A rendering from the original that failed.
    pub fn render_failed(&self, key: RenderKey) {
        self.renders.lock().entry(key).or_default().failed += 1;
    }

    pub fn renders(&self) -> Vec<RenderTimes> {
        let all = self.renders.lock();
        let mut out: Vec<RenderTimes> = all
            .iter()
            .map(|(k, a)| {
                let mut v: Vec<u32> = a.recent.iter().copied().collect();
                v.sort_unstable();
                let at = |q: f64| {
                    v.get(((v.len() as f64 - 1.0) * q).round() as usize)
                        .copied()
                        .unwrap_or(0) as u64
                };
                let n = a.count.max(1);
                let (mean, wait, decode, finish) = (
                    a.total_ms / n,
                    a.wait_ms / n,
                    a.decode_ms / n,
                    a.finish_ms / n,
                );
                RenderTimes {
                    purpose: k.purpose.clone(),
                    kind: k.kind.clone(),
                    decoder: k.decoder.clone(),
                    size: k.size,
                    count: a.count,
                    failed: a.failed,
                    fell_back: a.fell_back,
                    p50_ms: at(0.5),
                    p95_ms: at(0.95),
                    max_ms: a.max_ms,
                    mean_ms: mean,
                    wait_ms: wait,
                    decode_ms: decode,
                    finish_ms: finish,
                    other_ms: mean.saturating_sub(wait + decode + finish),
                }
            })
            .collect();
        out.sort_by(|a, b| {
            (&a.purpose, std::cmp::Reverse(a.count), &a.kind, a.size).cmp(&(
                &b.purpose,
                std::cmp::Reverse(b.count),
                &b.kind,
                b.size,
            ))
        });
        out
    }

    /// A preview of `size` served (or not) from `how`.
    pub fn served_at(&self, size: u32, how: Served) {
        match how {
            Served::Memory => self.served_memory.fetch_add(1, Ordering::Relaxed),
            Served::Disk => self.served_disk.fetch_add(1, Ordering::Relaxed),
            _ => 0,
        };
        self.by_size.lock().entry(size).or_default()[how as usize] += 1;
    }

    pub fn by_size(&self) -> Vec<SizeServed> {
        self.by_size
            .lock()
            .iter()
            .map(|(&size, c)| SizeServed {
                size,
                memory: c[0],
                disk: c[1],
                from_cached: c[2],
                rendered: c[3],
                not_cached: c[4],
            })
            .collect()
    }

    pub fn served(&self) -> (u64, u64) {
        (
            self.served_memory.load(Ordering::Relaxed),
            self.served_disk.load(Ordering::Relaxed),
        )
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn percentiles() {
        let s = super::Stats::default();
        for ms in 1..=100 {
            s.record("heic", ms);
        }
        s.record("jpeg", 40);
        let t = s.times();
        assert_eq!(t[0].kind, "heic");
        assert_eq!(
            (
                t[0].count,
                t[0].mean_ms,
                t[0].p50_ms,
                t[0].p95_ms,
                t[0].max_ms
            ),
            (100, 50, 51, 95, 100)
        );
        assert_eq!(t[1].count, 1);
    }
}
