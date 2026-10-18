//! The preview cache on disk: one redb file (millions of small previews without millions of small files),
//! least recently used out first, the pre-generated size last (design.md, previews).
//!
//! Inserts commit without fsync and a periodic commit makes them durable: losing the last seconds of a cache
//! in a crash costs a regeneration, never data.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use redb::{Database, Durability, ReadableDatabase, ReadableTable, TableDefinition};

const PREVIEWS: TableDefinition<&str, &[u8]> = TableDefinition::new("previews");
/// key -> (bytes, last used, unix seconds)
const META: TableDefinition<&str, (u64, u64)> = TableDefinition::new("meta");

/// Hits only refresh the last-used time when it is older than this, so reads stay reads.
const TOUCH_AFTER: u64 = 24 * 3600;

pub struct DiskCache {
    db: Database,
    max_bytes: u64,
    total: AtomicU64,
    dirty: AtomicBool,
    /// The size kept longest (evicted after every other size).
    keep_size: u32,
}

fn size_of_key(key: &str) -> u32 {
    key.rsplit(':')
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

impl DiskCache {
    pub fn open(path: &Path, max_bytes: u64, keep_size: u32) -> anyhow::Result<DiskCache> {
        let db = Database::builder().set_cache_size(64 << 20).create(path)?;
        {
            let w = db.begin_write()?;
            w.open_table(PREVIEWS)?;
            w.open_table(META)?;
            w.commit()?;
        }
        let total = {
            let r = db.begin_read()?;
            let meta = r.open_table(META)?;
            let mut t = 0u64;
            for item in meta.iter()? {
                t += item?.1.value().0;
            }
            t
        };
        Ok(DiskCache {
            db,
            max_bytes,
            total: AtomicU64::new(total),
            dirty: AtomicBool::new(false),
            keep_size,
        })
    }

    pub fn get(&self, key: &str) -> anyhow::Result<Option<Vec<u8>>> {
        let r = self.db.begin_read()?;
        let previews = r.open_table(PREVIEWS)?;
        let Some(v) = previews.get(key)? else {
            return Ok(None);
        };
        let bytes = v.value().to_vec();
        let stale = r
            .open_table(META)?
            .get(key)?
            .map(|m| m.value())
            .is_some_and(|(_, used)| crate::util::now_secs() as u64 > used + TOUCH_AFTER);
        drop(previews);
        drop(r);
        if stale {
            self.touch(key, bytes.len() as u64)?;
        }
        Ok(Some(bytes))
    }

    pub fn contains(&self, key: &str) -> anyhow::Result<bool> {
        let r = self.db.begin_read()?;
        Ok(r.open_table(META)?.get(key)?.is_some())
    }

    fn touch(&self, key: &str, len: u64) -> anyhow::Result<()> {
        let mut w = self.db.begin_write()?;
        w.set_durability(Durability::None)?;
        w.open_table(META)?
            .insert(key, (len, crate::util::now_secs() as u64))?;
        w.commit()?;
        self.dirty.store(true, Ordering::Relaxed);
        Ok(())
    }

    pub fn put(&self, key: &str, bytes: &[u8]) -> anyhow::Result<()> {
        let mut w = self.db.begin_write()?;
        w.set_durability(Durability::None)?;
        let old = {
            let mut meta = w.open_table(META)?;
            let old = meta
                .insert(key, (bytes.len() as u64, crate::util::now_secs() as u64))?
                .map(|g| g.value().0);
            w.open_table(PREVIEWS)?.insert(key, bytes)?;
            old
        };
        w.commit()?;
        self.total.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        if let Some(old) = old {
            self.total.fetch_sub(old, Ordering::Relaxed);
        }
        self.dirty.store(true, Ordering::Relaxed);
        Ok(())
    }

    pub fn total_bytes(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }

    /// Makes recent inserts durable; called periodically.
    pub fn flush(&self) -> anyhow::Result<()> {
        if self.dirty.swap(false, Ordering::Relaxed) {
            let mut w = self.db.begin_write()?;
            w.set_durability(Durability::Immediate)?;
            w.commit()?;
        }
        Ok(())
    }

    /// Evicts down to 90% of the budget when over it: other sizes before the kept size, least recently used
    /// first.
    pub fn evict(&self) -> anyhow::Result<u64> {
        if self.total_bytes() <= self.max_bytes {
            return Ok(0);
        }
        let target = self.max_bytes / 10 * 9;
        let mut all: Vec<(bool, u64, String, u64)> = {
            let r = self.db.begin_read()?;
            let meta = r.open_table(META)?;
            let mut v = Vec::new();
            for item in meta.iter()? {
                let (k, m) = item?;
                let (len, used) = m.value();
                let key = k.value().to_string();
                v.push((size_of_key(&key) == self.keep_size, used, key, len));
            }
            v
        };
        all.sort_by_key(|a| (a.0, a.1));
        let mut freed = 0u64;
        let mut w = self.db.begin_write()?;
        {
            let mut meta = w.open_table(META)?;
            let mut previews = w.open_table(PREVIEWS)?;
            for (_, _, key, len) in all {
                if self.total_bytes() - freed <= target {
                    break;
                }
                meta.remove(key.as_str())?;
                previews.remove(key.as_str())?;
                freed += len;
            }
        }
        w.set_durability(Durability::Immediate)?;
        w.commit()?;
        self.total.fetch_sub(freed, Ordering::Relaxed);
        Ok(freed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_get_evict() {
        let t = tempfile::tempdir().unwrap();
        let c = DiskCache::open(&t.path().join("p.redb"), 1000, 640).unwrap();
        c.put("a:e:640", &[1u8; 400]).unwrap();
        c.put("b:e:320", &[2u8; 400]).unwrap();
        assert_eq!(c.get("a:e:640").unwrap().unwrap().len(), 400);
        c.put("c:e:1280", &[3u8; 400]).unwrap();
        assert_eq!(c.total_bytes(), 1200);
        c.evict().unwrap();
        assert!(c.total_bytes() <= 900);
        assert!(c.contains("a:e:640").unwrap(), "the kept size goes last");
        c.flush().unwrap();
        drop(c);
        let c = DiskCache::open(&t.path().join("p.redb"), 1000, 640).unwrap();
        assert!(c.total_bytes() <= 900, "totals survive a reopen");
    }
}
