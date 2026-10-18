//! How long previews take, by source type, since the server started: what `wirephoto status` shows.

use std::collections::{HashMap, VecDeque};
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
