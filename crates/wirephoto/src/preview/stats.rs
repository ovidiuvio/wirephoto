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
