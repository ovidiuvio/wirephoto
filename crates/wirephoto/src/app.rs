//! The running server's shared state.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use parking_lot::RwLock;
use tokio::sync::{Mutex, Notify, watch};
use wirephoto_api::{Indexing, IndexingState};

use crate::auth::Keys;
use crate::config::{Config, LibraryConfig};
use crate::db::Db;
use crate::fsx::LibFs;
use crate::preview::Previews;

pub struct Library {
    pub cfg: LibraryConfig,
    pub api_id: String,
    pub fs: Arc<LibFs>,
    /// The root folder's entry ID, once indexed.
    pub root_id: RwLock<String>,
    /// Serialises changes to the library's folders: API writes and the indexer's per-folder updates.
    pub lock: Mutex<()>,
    pub status: IndexStatus,
    pub live: AtomicBool,
    /// How changes on disk are noticed, and why (`wirephoto status`).
    pub watch_note: RwLock<String>,
    /// Folders with a watch of their own (Linux).
    pub watched: AtomicU64,
    /// The last full scan.
    pub last_scan: RwLock<Option<ScanInfo>>,
}

/// A full scan of a library: when it started (unix seconds), how long it took, how many entries it listed.
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
pub struct ScanInfo {
    pub at: i64,
    pub secs: f64,
    pub entries: u64,
}

/// Indexing progress, for `GET /v1/libraries`.
#[derive(Default)]
pub struct IndexStatus {
    /// 0 idle, 1 scanning, 2 reading, 3 hashing
    state: AtomicU8,
    done: AtomicU64,
    total: AtomicU64,
}

impl IndexStatus {
    pub fn set(&self, state: Option<IndexingState>, done: u64, total: u64) {
        let s = match state {
            None => 0,
            Some(IndexingState::Scanning) => 1,
            Some(IndexingState::Reading) => 2,
            Some(IndexingState::Hashing) => 3,
        };
        self.state.store(s, Ordering::Relaxed);
        self.done.store(done, Ordering::Relaxed);
        self.total.store(total, Ordering::Relaxed);
    }

    pub fn bump(&self) {
        self.done.fetch_add(1, Ordering::Relaxed);
    }

    pub fn get(&self) -> Option<Indexing> {
        let state = match self.state.load(Ordering::Relaxed) {
            1 => IndexingState::Scanning,
            2 => IndexingState::Reading,
            3 => IndexingState::Hashing,
            _ => return None,
        };
        Some(Indexing {
            state,
            done: self.done.load(Ordering::Relaxed),
            total: self.total.load(Ordering::Relaxed),
        })
    }
}

pub struct App {
    pub config: Config,
    pub db: Arc<Db>,
    pub libraries: Vec<Arc<Library>>,
    pub previews: Arc<Previews>,
    pub keys: Keys,
    /// The latest change sequence number; live change streams wait on it.
    pub seq: watch::Sender<i64>,
    /// Wakes the background passes (media facts, hashes, pre-generation) early.
    pub work: Notify,
    /// The API router without its outer layers, for `/v1/batch` to run requests through.
    pub router: std::sync::OnceLock<axum::Router>,
    /// The CORS allow-list (origins holding a key) and when it was read.
    pub origins: RwLock<(std::time::Instant, Vec<String>)>,
    /// When the API last had a request (unix ms): background work waits until it has been idle a while.
    pub api_activity: std::sync::atomic::AtomicI64,
    /// Background work is waiting for the API to go idle (for the progress log and `wirephoto status`).
    pub yielding: std::sync::atomic::AtomicBool,
    /// Index writes so far ([`App::write`]): progress is counted again only after one.
    pub writes: AtomicU64,
    /// Requests, uploads and changes on disk since start (`wirephoto status`).
    pub metrics: crate::metrics::Metrics,
    /// `ffmpeg -version`'s first line, or "not found"; how HEIC is decoded (`wirephoto status`).
    pub ffmpeg: String,
    pub heic_route: String,
}

impl App {
    /// A request came in, or a response went out.
    pub fn api_active(&self) {
        self.api_activity.store(
            crate::util::now_millis(),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// Whether the API had a request within the quiet period (`[background] yield_seconds`).
    pub fn api_busy(&self) -> bool {
        let quiet = self.config.background.yield_seconds as i64 * 1000;
        quiet > 0
            && crate::util::now_millis()
                - self.api_activity.load(std::sync::atomic::Ordering::Relaxed)
                < quiet
    }

    /// Waits until the API has been idle for the quiet period: requests come first.
    pub async fn yield_to_requests(&self) {
        use std::sync::atomic::Ordering;
        if !self.api_busy() {
            return;
        }
        self.yielding.store(true, Ordering::Relaxed);
        while self.api_busy() {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        self.yielding.store(false, Ordering::Relaxed);
    }
}

impl App {
    pub fn library(&self, api_id: &str) -> Option<&Arc<Library>> {
        self.libraries.iter().find(|l| l.api_id == api_id)
    }

    /// Runs a write transaction, then tells change listeners and background work about it.
    pub fn write<T>(
        &self,
        f: impl FnOnce(&rusqlite::Transaction) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let out = self.db.write(f)?;
        self.changed();
        Ok(out)
    }

    /// How many index writes there have been ([`App::writes`]).
    pub fn writes(&self) -> u64 {
        self.writes.load(Ordering::Relaxed)
    }

    pub fn changed(&self) {
        self.writes.fetch_add(1, Ordering::Relaxed);
        if let Ok(seq) = self.db.read(|c| Ok(crate::db::current_seq(c)?)) {
            self.seq.send_if_modified(|s| {
                let moved = *s != seq;
                *s = seq;
                moved
            });
        }
        self.work.notify_one();
    }
}
