//! Previews (design.md, Previews): memory cache, then the disk cache, then rendering in a decode worker
//! ([`crate::decode`]). Every rendering of a preview — for requests and for background pre-generation alike —
//! goes through the memory cache's `try_get_with`, so whoever asks second waits for the rendering in progress
//! instead of starting another. A rendering nobody waits for any more is dropped with its request before it
//! reaches a worker (one already in a worker finishes and is cached).
//!
//! Sizes below the cached 640 px are made from it, not from the original, and kept in memory only: shrinking our
//! own 640 px JPEG takes 0.4-1.2 ms (measured, one M2 Max core) where decoding a 40 MP original takes tens to
//! hundreds, and keeping a 320 on disk would add ~56% to the cache for that.

pub mod cache;
pub mod inspect;
pub mod render;
pub mod stats;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::sync::Semaphore;

use stats::Served;
use wirephoto_api::Kind;

use crate::db::EntryRow;
use crate::decode::{Format, Input, Op, Pool};
use crate::fsx::LibFs;

/// The preview sizes, long edge in pixels (api.md, `GET /v1`).
pub const SIZES: [u32; 6] = [160, 320, 480, 640, 1280, 2560];
/// The size pre-generated for every photo and kept longest.
pub const CACHED_SIZE: u32 = 640;

/// Where a preview's source bytes come from.
#[derive(Clone)]
pub enum Source {
    File(Arc<LibFs>, String),
    /// An upload still in memory.
    Bytes(Arc<Vec<u8>>),
}

/// How a preview was obtained, for `Server-Timing`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Timing {
    pub hit: bool,
    pub render_ms: f64,
}

pub struct Previews {
    disk: Arc<cache::DiskCache>,
    mem: moka::future::Cache<String, Bytes>,
    /// Renderings for requests.
    interactive: Arc<Semaphore>,
    /// Background pre-generation, kept to a share of the CPUs.
    background: Arc<Semaphore>,
    pool: Arc<Pool>,
    /// Background renderings' own workers, at low CPU priority.
    bg_pool: Arc<Pool>,
    /// Renderings from originals so far (for tests and debugging).
    renders: Arc<AtomicU64>,
    /// Timings by source type and what was served from where (`wirephoto status`).
    pub stats: Arc<stats::Stats>,
    /// Renderings at once in all, and of them for pre-generation.
    pub slots: (usize, usize),
}

/// An error shared by everyone who waited on one rendering, as an error of one's own. A decoder's verdict on
/// the file ([`crate::decode::DecodeFailed`]) stays one, or every failure would look like it may pass and be
/// retried forever.
fn unshare(e: Arc<anyhow::Error>) -> anyhow::Error {
    if crate::decode::is_decode_failure(&e) {
        crate::decode::DecodeFailed(format!("{e:#}")).into()
    } else {
        anyhow::anyhow!("{e:#}")
    }
}

/// A preview's cache key. Once a file's hash is known its previews are keyed by CONTENT (`sha256-<hex>:<size>`),
/// so copies of a photo share one set; before that by entry and version (`<entry id>:<etag>:<size>`). The
/// indexer moves them over when it hashes a file ([`Previews::adopt_hash`]); uploads and API copies know the
/// hash from the start.
pub fn key(entry: &EntryRow, size: u32) -> String {
    match entry.sha256.as_deref() {
        Some(h) if h.len() == 32 => content_key(h, size),
        _ => entry_key(entry, size),
    }
}

fn content_key(hash: &[u8], size: u32) -> String {
    format!("sha256-{}:{size}", hex::encode(hash))
}

fn entry_key(entry: &EntryRow, size: u32) -> String {
    format!("{}:{}:{}", entry.id, entry.etag(), size)
}

/// What a cache key names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyOf {
    /// Content hash, size.
    Content([u8; 32], u32),
    /// Entry id, etag, size.
    Entry(String, String, u32),
}

pub fn parse_key(key: &str) -> Option<KeyOf> {
    let (rest, size) = key.rsplit_once(':')?;
    let size = size.parse().ok()?;
    if let Some(h) = rest.strip_prefix("sha256-") {
        let mut hash = [0u8; 32];
        hex::decode_to_slice(h, &mut hash).ok()?;
        return Some(KeyOf::Content(hash, size));
    }
    let (id, etag) = rest.split_once(':')?;
    Some(KeyOf::Entry(id.to_string(), etag.to_string(), size))
}

pub fn valid_size(size: u32) -> bool {
    SIZES.contains(&size)
}

impl Previews {
    /// `workers`: how many renderings run at once (0: one per CPU); `memory`: bytes of previews kept in memory.
    pub fn new(
        disk: cache::DiskCache,
        pool: Arc<Pool>,
        bg_pool: Arc<Pool>,
        workers: usize,
        memory: u64,
    ) -> Previews {
        let cpus = match workers {
            0 => std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4),
            n => n,
        };
        Previews {
            pool,
            bg_pool,
            disk: Arc::new(disk),
            mem: moka::future::Cache::builder()
                .weigher(|_k: &String, v: &Bytes| v.len().min(u32::MAX as usize) as u32)
                .max_capacity(memory)
                .time_to_idle(Duration::from_secs(600))
                .build(),
            interactive: Arc::new(Semaphore::new(cpus)),
            // Half the renderings for pre-generation (a NAS is mostly idle otherwise); requests get the rest.
            background: Arc::new(Semaphore::new((cpus / 2).max(1))),
            renders: Arc::new(AtomicU64::new(0)),
            stats: Arc::new(stats::Stats::default()),
            slots: (cpus, (cpus / 2).max(1)),
        }
    }

    /// Previews in memory, and their bytes.
    pub fn memory(&self) -> (u64, u64) {
        (self.mem.entry_count(), self.mem.weighted_size())
    }

    pub fn disk(&self) -> &Arc<cache::DiskCache> {
        &self.disk
    }

    pub fn pool(&self) -> &Arc<Pool> {
        &self.pool
    }

    /// The background workers (pre-generation, facts read in the background).
    pub fn bg_pool(&self) -> &Arc<Pool> {
        &self.bg_pool
    }

    /// How many previews have been rendered from originals since start.
    pub fn renders(&self) -> u64 {
        self.renders.load(Ordering::Relaxed)
    }

    /// A cached preview, without rendering, counted as served (`wirephoto status`).
    pub async fn cached(&self, key: &str) -> Option<Bytes> {
        let size = key
            .rsplit(':')
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let (b, from) = self.lookup(key).await?;
        self.stats.served_at(size, from);
        Some(b)
    }

    /// A cached preview from memory or the disk, not counted (what other sizes are made from).
    async fn lookup(&self, key: &str) -> Option<(Bytes, Served)> {
        if let Some(b) = self.mem.get(key).await {
            return Some((b, Served::Memory));
        }
        let disk = self.disk.clone();
        let k = key.to_string();
        let found = tokio::task::spawn_blocking(move || disk.get(&k))
            .await
            .ok()?
            .ok()??;
        let b = Bytes::from(found);
        self.mem.insert(key.to_string(), b.clone()).await;
        Some((b, Served::Disk))
    }

    /// The preview of `entry` at `size`, rendering it if needed.
    pub async fn get(
        &self,
        entry: &EntryRow,
        src: Source,
        size: u32,
    ) -> anyhow::Result<(Bytes, Timing)> {
        let k = key(entry, size);
        // Sizes made from the 640 live in memory only: the disk never has them.
        let hit = if size < CACHED_SIZE {
            let b = self.mem.get(&k).await;
            if b.is_some() {
                self.stats.served_at(size, Served::Memory);
            }
            b
        } else {
            self.cached(&k).await
        };
        if let Some(b) = hit {
            return Ok((
                b,
                Timing {
                    hit: true,
                    render_ms: 0.0,
                },
            ));
        }
        let started = Instant::now();
        let bytes = if size < CACHED_SIZE {
            let base = self.render_cached(entry, src, CACHED_SIZE).await?;
            let stats = self.stats.clone();
            self.mem
                .try_get_with(k, async move {
                    let out = tokio::task::spawn_blocking(move || {
                        let t = Instant::now();
                        let out = shrink(&base, size);
                        stats.record("from the 640", t.elapsed().as_millis() as u64);
                        out
                    })
                    .await??;
                    anyhow::Ok(Bytes::from(out))
                })
                .await
                .map_err(unshare)?
        } else {
            self.render_cached(entry, src, size).await?
        };
        self.stats.served_at(
            size,
            if size < CACHED_SIZE {
                Served::FromCached
            } else {
                Served::Rendered
            },
        );
        Ok((
            bytes,
            Timing {
                hit: false,
                render_ms: started.elapsed().as_secs_f64() * 1000.0,
            },
        ))
    }

    /// A preview from the caches, or rendered from the original in a worker (coalesced).
    async fn render_cached(
        &self,
        entry: &EntryRow,
        src: Source,
        size: u32,
    ) -> anyhow::Result<Bytes> {
        let k = key(entry, size);
        if let Some((b, _)) = self.lookup(&k).await {
            return Ok(b);
        }
        let job = self.job(entry, src, size, &k, self.pool.clone());
        let sem = self.interactive.clone();
        self.mem
            .try_get_with(k, async move {
                let _permit = sem.acquire_owned().await?;
                job.run().await
            })
            .await
            .map_err(unshare)
    }

    /// Makes sure the cached size exists (background pre-generation). Returns whether it rendered.
    ///
    /// The background slot is taken BEFORE joining the memory cache's coalescing: a request arriving while this
    /// waits for a slot renders at once on its own (interactive) slots rather than queueing behind pre-generation,
    /// and this then finds its result. A rendering of ours that a request joins hands it the bytes; once done,
    /// the preview is dropped from memory again, so pre-generating a library does not push out what people are
    /// looking at.
    pub async fn ensure_cached(&self, entry: &EntryRow, src: Source) -> anyhow::Result<bool> {
        let k = key(entry, CACHED_SIZE);
        if self.on_disk(&k).await? {
            return Ok(false);
        }
        let _permit = self.background.clone().acquire_owned().await?;
        let job = self.job(entry, src, CACHED_SIZE, &k, self.bg_pool.clone());
        let before = self.renders();
        let res = self
            .mem
            .entry(k.clone())
            .or_try_insert_with(async move { job.run().await })
            .await
            .map_err(unshare)?;
        // `is_fresh`: this call's own initializer ran (no request was rendering it, nothing was in memory).
        if res.is_fresh() {
            self.mem.invalidate(&k).await;
        }
        Ok(self.renders() > before && res.is_fresh())
    }

    /// A preview only if it can be had without decoding the original: cached, or made from the cached 640
    /// (about a millisecond).
    pub async fn get_cached(&self, entry: &EntryRow, size: u32) -> Option<Bytes> {
        if size >= CACHED_SIZE {
            let b = self.cached(&key(entry, size)).await;
            if b.is_none() {
                self.stats.served_at(size, Served::NotCached);
            }
            return b;
        }
        let k = key(entry, size);
        if let Some(b) = self.mem.get(&k).await {
            self.stats.served_at(size, Served::Memory);
            return Some(b);
        }
        let Some((base, _)) = self.lookup(&key(entry, CACHED_SIZE)).await else {
            self.stats.served_at(size, Served::NotCached);
            return None;
        };
        let out = tokio::task::spawn_blocking(move || shrink(&base, size))
            .await
            .ok()?
            .ok()?;
        let b = Bytes::from(out);
        self.stats.served_at(size, Served::FromCached);
        self.mem.insert(k, b.clone()).await;
        Some(b)
    }

    /// Forgets every preview of `entry`, in memory and on disk, to make them again.
    pub async fn forget(&self, entry: &EntryRow) -> anyhow::Result<()> {
        let mut keys: Vec<String> = SIZES.iter().map(|&s| entry_key(entry, s)).collect();
        if let Some(h) = entry.sha256.as_deref().filter(|h| h.len() == 32) {
            keys.extend(SIZES.iter().map(|&s| content_key(h, s)));
        }
        for k in &keys {
            self.mem.invalidate(k).await;
        }
        let disk = self.disk.clone();
        tokio::task::spawn_blocking(move || disk.remove(&keys)).await?
    }

    /// A file was just hashed: its entry-keyed previews move to the content key, or are dropped where a copy's
    /// are there already (blocking).
    pub fn adopt_hash(&self, before: &EntryRow, hash: &[u8; 32]) -> anyhow::Result<()> {
        let moves: Vec<(String, String)> = SIZES
            .iter()
            .map(|&s| (entry_key(before, s), content_key(hash, s)))
            .collect();
        self.disk.rekey(&moves)
    }

    async fn on_disk(&self, k: &str) -> anyhow::Result<bool> {
        let (disk, k) = (self.disk.clone(), k.to_string());
        tokio::task::spawn_blocking(move || disk.contains(&k)).await?
    }

    fn job(&self, entry: &EntryRow, src: Source, size: u32, k: &str, pool: Arc<Pool>) -> RenderJob {
        RenderJob {
            entry: entry.clone(),
            src,
            size,
            key: k.to_string(),
            disk: self.disk.clone(),
            pool,
            renders: self.renders.clone(),
            stats: self.stats.clone(),
        }
    }
}

/// One preview rendered from its original and stored on disk. The caller holds a rendering slot.
struct RenderJob {
    entry: EntryRow,
    src: Source,
    size: u32,
    key: String,
    disk: Arc<cache::DiskCache>,
    pool: Arc<Pool>,
    renders: Arc<AtomicU64>,
    stats: Arc<stats::Stats>,
}

impl RenderJob {
    async fn run(self) -> anyhow::Result<Bytes> {
        let out = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<u8>> {
            // Made while this waited for its slot (by a request, or by a server since restarted)?
            if let Some(jpeg) = self.disk.get(&self.key)? {
                return Ok(jpeg);
            }
            self.renders.fetch_add(1, Ordering::Relaxed);
            let started = Instant::now();
            let done = render(&self.pool, &self.entry, &self.src, self.size)?;
            let jpeg = done.bytes;
            let label = kind_label(&self.entry);
            let label = match done.made & !crate::decode::how::GPU_FAILED {
                crate::decode::how::GPU => format!("{label} (GPU)"),
                crate::decode::how::FFMPEG => format!("{label} (ffmpeg)"),
                _ => label,
            };
            self.stats
                .record(&label, started.elapsed().as_millis() as u64);
            tracing::debug!(
                path = %self.entry.path,
                size = self.size,
                ms = started.elapsed().as_millis() as u64,
                bytes = jpeg.len(),
                "rendered preview"
            );
            self.disk.put(&self.key, &jpeg)?;
            Ok(jpeg)
        })
        .await??;
        Ok(Bytes::from(out))
    }
}

/// A smaller preview from a cached one (our own baseline sRGB JPEG, so it is decoded here, not in a worker):
/// DCT scaling does most of it (640 -> 320 -> 160 exactly), Lanczos the rest. 480 keeps the usual quality: a
/// 0.75 resample shows the 640's blocks more (SSIMULACRA2 57.8 at q80, 55.8 at q70).
pub fn shrink(jpeg: &[u8], size: u32) -> anyhow::Result<Vec<u8>> {
    let encoder = if size <= 320 {
        render::Encoder::Small
    } else {
        render::Encoder::Turbo
    };
    render::finish(render::decode_jpeg(jpeg, size)?, size, encoder)
}

/// A source type as `wirephoto status` names it.
pub fn kind_label(entry: &EntryRow) -> String {
    match format(entry) {
        Format::Jpeg => "jpeg".into(),
        Format::Raw => "raw".into(),
        Format::Heif => match entry.mime.as_deref() {
            Some("image/avif") => "avif".into(),
            _ => "heic".into(),
        },
        Format::Video { .. } => "video".into(),
        Format::Image { mime, .. } => mime.rsplit('/').next().unwrap_or("other").to_string(),
    }
}

/// How a worker decodes this entry.
pub fn format(entry: &EntryRow) -> Format {
    match entry.kind {
        Kind::Raw => Format::Raw,
        Kind::Video => Format::Video {
            duration: entry.duration,
        },
        _ => match entry.mime.as_deref().unwrap_or("") {
            "image/jpeg" => Format::Jpeg,
            "image/heic" | "image/heif" | "image/avif" => Format::Heif,
            m => Format::Image {
                mime: m.to_string(),
                orientation: entry.orientation.unwrap_or(1),
            },
        },
    }
}

/// Makes the JPEG in a decode worker (blocking), saying how it was decoded.
pub fn render(
    pool: &Pool,
    entry: &EntryRow,
    src: &Source,
    size: u32,
) -> anyhow::Result<crate::decode::Done> {
    let input = match src {
        Source::File(fs, rel) => Input::File(fs.open_file(rel)?),
        Source::Bytes(b) => Input::Bytes(b.clone()),
    };
    let op = Op::Preview {
        format: format(entry),
        size,
        cache: size == CACHED_SIZE,
    };
    pool.run_full(op, input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip() {
        let h = [7u8; 32];
        assert_eq!(
            parse_key(&content_key(&h, 640)),
            Some(KeyOf::Content(h, 640))
        );
        assert_eq!(
            parse_key("ent_k82Pq:3a9f0c1e:320"),
            Some(KeyOf::Entry("ent_k82Pq".into(), "3a9f0c1e".into(), 320))
        );
        assert_eq!(parse_key("nonsense"), None);
    }

    #[test]
    fn smaller_sizes_from_the_cached_one() {
        let px: Vec<u8> = (0..640 * 427)
            .flat_map(|i| [(i % 251) as u8, (i % 241) as u8, 90])
            .collect();
        let base =
            render::encode(&render::Rgb { w: 640, h: 427, px }, render::Encoder::Turbo).unwrap();
        for (size, want) in [(160, (160, 107)), (320, (320, 214)), (480, (480, 320))] {
            let out = shrink(&base, size).unwrap();
            let d = render::decode_jpeg(&out, 10_000).unwrap();
            // DCT scaling rounds up (427 * 6/8 = 321), a pixel off plain rounding.
            assert_eq!(d.img.w, want.0, "size {size}");
            assert!(
                d.img.h.abs_diff(want.1) <= 1,
                "size {size}: height {}",
                d.img.h
            );
        }
    }
}
