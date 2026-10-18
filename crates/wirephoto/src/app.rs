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

    pub fn changed(&self) {
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
