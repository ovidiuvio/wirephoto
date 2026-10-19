//! What the server has done since it started, beyond previews (those are in [`crate::preview::stats`]):
//! requests by kind, uploads, changes seen on disk. Counters only, cheap to update; `wirephoto status` shows
//! them.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::preview::stats::{Stats, TypeTimes};

#[derive(Default)]
pub struct Metrics {
    requests: Mutex<BTreeMap<&'static str, Counts>>,
    /// Time to the response's start, by kind.
    times: Stats,
    pub uploads_opened: AtomicU64,
    pub uploads_completed: AtomicU64,
    pub uploads_abandoned: AtomicU64,
    pub uploads_expired: AtomicU64,
    /// Bytes received for files: single PUTs and upload chunks.
    pub upload_bytes: AtomicU64,
    /// Filesystem events taken for changes, and folders listed again for them (watching or polling).
    pub fs_events: AtomicU64,
    pub folders_relisted: AtomicU64,
    /// When the index last changed for something done on disk, not through the API (unix seconds).
    pub last_disk_change: AtomicI64,
}

#[derive(Debug, Clone, Copy, Default)]
struct Counts {
    count: u64,
    client_errors: u64,
    server_errors: u64,
    bytes: u64,
}

/// One kind of request, as `wirephoto status` prints it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RequestKind {
    pub kind: String,
    pub count: u64,
    pub client_errors: u64,
    pub server_errors: u64,
    pub bytes: u64,
    pub p50_ms: u64,
    pub p95_ms: u64,
    pub max_ms: u64,
}

/// The kind of a request, by method and path.
pub fn kind_of(method: &str, path: &str) -> &'static str {
    let path = path.split('?').next().unwrap_or(path);
    let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match (method, parts.as_slice()) {
        (_, ["healthz"]) => "health",
        (_, ["v1"]) | (_, ["v1", "libraries", ..]) => "server",
        ("GET", ["v1", "entries"]) => "listings",
        ("GET", ["v1", "entries", _]) => "entries",
        ("GET", ["v1", "entries", _, "preview", _]) => "previews",
        ("POST", ["v1", "previews"]) => "batch previews",
        ("GET" | "HEAD", ["v1", "entries", _, "content"]) => "originals",
        ("PUT", ["v1", "entries", ..]) | (_, ["v1", "tus", ..]) => "uploads",
        (_, ["v1", "entries", ..]) => "changes made",
        (_, ["v1", "batch"]) => "batch",
        (_, ["v1", "changes"]) => "change feed",
        ("OPTIONS", _) => "preflight",
        _ => "other",
    }
}

impl Metrics {
    /// A request answered: its kind, status, size (when known) and time to the response.
    pub fn request(&self, kind: &'static str, status: u16, bytes: Option<u64>, ms: u64) {
        {
            let mut all = self.requests.lock();
            let c = all.entry(kind).or_default();
            c.count += 1;
            match status {
                400..=499 => c.client_errors += 1,
                500..=599 => c.server_errors += 1,
                _ => {}
            }
            c.bytes += bytes.unwrap_or(0);
        }
        self.times.record(kind, ms);
    }

    pub fn requests(&self) -> Vec<RequestKind> {
        let times: Vec<TypeTimes> = self.times.times();
        let mut out: Vec<RequestKind> = self
            .requests
            .lock()
            .iter()
            .map(|(&kind, c)| {
                let t = times.iter().find(|t| t.kind == kind);
                RequestKind {
                    kind: kind.to_string(),
                    count: c.count,
                    client_errors: c.client_errors,
                    server_errors: c.server_errors,
                    bytes: c.bytes,
                    p50_ms: t.map_or(0, |t| t.p50_ms),
                    p95_ms: t.map_or(0, |t| t.p95_ms),
                    max_ms: t.map_or(0, |t| t.max_ms),
                }
            })
            .collect();
        out.sort_by_key(|r| std::cmp::Reverse(r.count));
        out
    }

    pub fn add(counter: &AtomicU64, n: u64) {
        counter.fetch_add(n, Ordering::Relaxed);
    }

    pub fn disk_changed(&self) {
        self.last_disk_change
            .store(crate::util::now_secs(), Ordering::Relaxed);
    }

    pub fn get(counter: &AtomicU64) -> u64 {
        counter.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds() {
        assert_eq!(kind_of("GET", "/v1/entries?parent=ent_1"), "listings");
        assert_eq!(kind_of("GET", "/v1/entries/ent_1"), "entries");
        assert_eq!(
            kind_of("GET", "/v1/entries/ent_1/preview/640.jpg?v=1&sig=…"),
            "previews"
        );
        assert_eq!(
            kind_of("GET", "/v1/entries/ent_1/content?sig=…"),
            "originals"
        );
        assert_eq!(
            kind_of("PUT", "/v1/entries/ent_1/children/a.jpg"),
            "uploads"
        );
        assert_eq!(kind_of("PATCH", "/v1/tus/tus_1"), "uploads");
        assert_eq!(kind_of("PATCH", "/v1/entries/ent_1"), "changes made");
        assert_eq!(kind_of("GET", "/v1/changes"), "change feed");
        let m = Metrics::default();
        m.request("previews", 200, Some(30_000), 2);
        m.request("previews", 404, Some(100), 1);
        let r = m.requests();
        assert_eq!((r[0].count, r[0].client_errors, r[0].bytes), (2, 1, 30_100));
    }
}
