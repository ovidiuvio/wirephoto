//! Reading the preview cache from outside the server, for debugging (`wirephoto cache …`).
//!
//! redb locks its file while a process has it open for writing, and a running server always does. So the cache
//! is opened read-only when the server is stopped, and otherwise a copy of the file is taken and opened instead
//! (redb repairs it on open, as after a crash). A copy holds what the server has made durable: it inserts
//! without a flush and flushes every 30 seconds (indexer.rs upkeep), so the newest previews may be missing.

use std::path::Path;

use redb::{
    Database, DatabaseError, ReadOnlyDatabase, ReadTransaction, ReadableDatabase, ReadableTable,
};

use super::cache::{META, PREVIEWS};

/// One cached preview: its key is `<entry id>:<etag>:<size>`.
#[derive(Debug, Clone)]
pub struct Cached {
    pub entry: String,
    pub etag: String,
    pub size: u32,
    pub bytes: u64,
    /// Unix seconds.
    pub last_used: u64,
}

impl Cached {
    pub fn key(&self) -> String {
        format!("{}:{}:{}", self.entry, self.etag, self.size)
    }

    fn parse(key: &str, bytes: u64, last_used: u64) -> Option<Cached> {
        let mut it = key.rsplitn(3, ':');
        let size = it.next()?.parse().ok()?;
        let etag = it.next()?.to_string();
        let entry = it.next()?.to_string();
        Some(Cached {
            entry,
            etag,
            size,
            bytes,
            last_used,
        })
    }
}

enum Handle {
    Direct(ReadOnlyDatabase),
    Snapshot(Database, #[allow(dead_code)] tempfile::TempDir),
}

pub struct CacheReader {
    handle: Handle,
}

impl CacheReader {
    /// Opens the cache file; `snapshot()` says whether a copy is being read (the server has the file open).
    pub fn open(path: &Path) -> anyhow::Result<CacheReader> {
        anyhow::ensure!(path.exists(), "no preview cache at {}", path.display());
        let handle = match ReadOnlyDatabase::open(path) {
            Ok(db) => Handle::Direct(db),
            // Open for writing elsewhere, or not closed cleanly (which a read-only open will not repair).
            Err(DatabaseError::DatabaseAlreadyOpen | DatabaseError::RepairAborted) => {
                let dir = tempfile::tempdir()?;
                let copy = dir.path().join("previews.redb");
                std::fs::copy(path, &copy)?;
                Handle::Snapshot(Database::create(&copy)?, dir)
            }
            Err(e) => return Err(e.into()),
        };
        Ok(CacheReader { handle })
    }

    pub fn snapshot(&self) -> bool {
        matches!(self.handle, Handle::Snapshot(..))
    }

    fn read(&self) -> anyhow::Result<ReadTransaction> {
        Ok(match &self.handle {
            Handle::Direct(db) => db.begin_read()?,
            Handle::Snapshot(db, _) => db.begin_read()?,
        })
    }

    /// Every cached preview, from the bookkeeping table (no image bytes read).
    pub fn list(&self) -> anyhow::Result<Vec<Cached>> {
        let r = self.read()?;
        let meta = r.open_table(META)?;
        let mut out = Vec::new();
        for item in meta.iter()? {
            let (k, m) = item?;
            let (bytes, used) = m.value();
            if let Some(c) = Cached::parse(k.value(), bytes, used) {
                out.push(c);
            }
        }
        Ok(out)
    }

    /// A preview's JPEG.
    pub fn get(&self, key: &str) -> anyhow::Result<Option<Vec<u8>>> {
        let r = self.read()?;
        let previews = r.open_table(PREVIEWS)?;
        Ok(previews.get(key)?.map(|v| v.value().to_vec()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preview::cache::DiskCache;

    #[test]
    fn reads_beside_a_running_writer_and_after_it() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("previews.redb");
        let c = DiskCache::open(&path, 1 << 20, 640).unwrap();
        c.put("ent_a:3a9f:640", b"jpeg-a").unwrap();
        c.put("ent_b:77c0:320", b"jpeg-b").unwrap();
        c.flush().unwrap();

        // The writer still has it open: a snapshot is read, as of its last flush.
        let r = CacheReader::open(&path).unwrap();
        assert!(r.snapshot());
        let mut all = r.list().unwrap();
        all.sort_by(|a, b| a.entry.cmp(&b.entry));
        assert_eq!(all.len(), 2);
        assert_eq!(
            (all[0].entry.as_str(), all[0].etag.as_str(), all[0].size),
            ("ent_a", "3a9f", 640)
        );
        assert_eq!(
            r.get(&all[1].key()).unwrap().as_deref(),
            Some(&b"jpeg-b"[..])
        );
        drop(r);

        // Closed: read in place.
        drop(c);
        let r = CacheReader::open(&path).unwrap();
        assert!(!r.snapshot());
        assert_eq!(r.list().unwrap().len(), 2);
    }
}
