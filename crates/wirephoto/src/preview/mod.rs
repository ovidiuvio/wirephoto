//! Previews (design.md, Previews): memory cache, then the disk cache, then rendering in a decode worker
//! ([`crate::decode`]). Identical requests coalesce into one rendering; a rendering nobody waits for any more is
//! dropped with its request before it reaches a worker (one already in a worker finishes and is cached).
//!
//! Sizes below the cached 640 px are made from it, not from the original: shrinking our own 640 px JPEG takes a
//! couple of milliseconds where decoding a 40 MP original takes tens to hundreds.

pub mod cache;
pub mod render;

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::sync::Semaphore;
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
}

pub fn key(entry: &EntryRow, size: u32) -> String {
    format!("{}:{}:{}", entry.id, entry.etag(), size)
}

pub fn valid_size(size: u32) -> bool {
    SIZES.contains(&size)
}

impl Previews {
    /// `workers`: how many renderings run at once (0: one per CPU); `memory`: bytes of previews kept in memory.
    pub fn new(disk: cache::DiskCache, pool: Arc<Pool>, workers: usize, memory: u64) -> Previews {
        let cpus = match workers {
            0 => std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4),
            n => n,
        };
        Previews {
            pool,
            disk: Arc::new(disk),
            mem: moka::future::Cache::builder()
                .weigher(|_k: &String, v: &Bytes| v.len().min(u32::MAX as usize) as u32)
                .max_capacity(memory)
                .time_to_idle(Duration::from_secs(600))
                .build(),
            interactive: Arc::new(Semaphore::new(cpus)),
            background: Arc::new(Semaphore::new((cpus / 4).max(1))),
        }
    }

    pub fn disk(&self) -> &Arc<cache::DiskCache> {
        &self.disk
    }

    pub fn pool(&self) -> &Arc<Pool> {
        &self.pool
    }

    /// A cached preview, without rendering.
    pub async fn cached(&self, key: &str) -> Option<Bytes> {
        if let Some(b) = self.mem.get(key).await {
            return Some(b);
        }
        let disk = self.disk.clone();
        let k = key.to_string();
        let found = tokio::task::spawn_blocking(move || disk.get(&k))
            .await
            .ok()?
            .ok()??;
        let b = Bytes::from(found);
        self.mem.insert(key.to_string(), b.clone()).await;
        Some(b)
    }

    /// The preview of `entry` at `size`, rendering it if needed.
    pub async fn get(
        &self,
        entry: &EntryRow,
        src: Source,
        size: u32,
    ) -> anyhow::Result<(Bytes, Timing)> {
        let k = key(entry, size);
        if let Some(b) = self.cached(&k).await {
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
            let disk = self.disk.clone();
            let kk = k.clone();
            self.mem
                .try_get_with(k, async move {
                    let out = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<u8>> {
                        let jpeg = shrink(&base, size)?;
                        disk.put(&kk, &jpeg)?;
                        Ok(jpeg)
                    })
                    .await??;
                    anyhow::Ok(Bytes::from(out))
                })
                .await
                .map_err(|e: Arc<anyhow::Error>| anyhow::anyhow!("{e:#}"))?
        } else {
            self.render_cached(entry, src, size).await?
        };
        Ok((
            bytes,
            Timing {
                hit: false,
                render_ms: started.elapsed().as_secs_f64() * 1000.0,
            },
        ))
    }

    /// A preview from the caches, or rendered from the original in a worker (coalesced).
    async fn render_cached(&self, entry: &EntryRow, src: Source, size: u32) -> anyhow::Result<Bytes> {
        let k = key(entry, size);
        if let Some(b) = self.cached(&k).await {
            return Ok(b);
        }
        let (entry, disk, sem) = (entry.clone(), self.disk.clone(), self.interactive.clone());
        let (kk, pool) = (k.clone(), self.pool.clone());
        self.mem
            .try_get_with(k, async move {
                render_and_store(entry, src, size, kk, disk, sem, pool).await
            })
            .await
            .map_err(|e: Arc<anyhow::Error>| anyhow::anyhow!("{e:#}"))
    }

    /// Makes sure the cached size exists (background pre-generation). Returns whether it rendered.
    pub async fn ensure_cached(&self, entry: &EntryRow, src: Source) -> anyhow::Result<bool> {
        let k = key(entry, CACHED_SIZE);
        let disk = self.disk.clone();
        let kk = k.clone();
        if tokio::task::spawn_blocking(move || disk.contains(&kk)).await?? {
            return Ok(false);
        }
        render_and_store(
            entry.clone(),
            src,
            CACHED_SIZE,
            k,
            self.disk.clone(),
            self.background.clone(),
            self.pool.clone(),
        )
        .await?;
        Ok(true)
    }
}

async fn render_and_store(
    entry: EntryRow,
    src: Source,
    size: u32,
    key: String,
    disk: Arc<cache::DiskCache>,
    sem: Arc<Semaphore>,
    pool: Arc<Pool>,
) -> anyhow::Result<Bytes> {
    let _permit = sem.acquire_owned().await?;
    let out = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<u8>> {
        let jpeg = render(&pool, &entry, &src, size)?;
        disk.put(&key, &jpeg)?;
        Ok(jpeg)
    })
    .await??;
    Ok(Bytes::from(out))
}

/// A smaller preview from a cached one (our own baseline sRGB JPEG, so it is decoded here, not in a worker):
/// DCT scaling does most of it (640 -> 320 -> 160 exactly), Lanczos the rest.
pub fn shrink(jpeg: &[u8], size: u32) -> anyhow::Result<Vec<u8>> {
    render::finish(render::decode_jpeg(jpeg, size)?, size, render::Encoder::Turbo)
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

/// Makes the JPEG in a decode worker (blocking).
pub fn render(pool: &Pool, entry: &EntryRow, src: &Source, size: u32) -> anyhow::Result<Vec<u8>> {
    let input = match src {
        Source::File(fs, rel) => Input::File(fs.open_file(rel)?),
        Source::Bytes(b) => Input::Bytes(b.clone()),
    };
    let op = Op::Preview {
        format: format(entry),
        size,
        cache: size == CACHED_SIZE,
    };
    pool.run(op, input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smaller_sizes_from_the_cached_one() {
        let px: Vec<u8> = (0..640 * 427).flat_map(|i| [(i % 251) as u8, (i % 241) as u8, 90]).collect();
        let base = render::encode(&render::Rgb { w: 640, h: 427, px }, render::Encoder::Turbo).unwrap();
        for (size, want) in [(160, (160, 107)), (320, (320, 214)), (480, (480, 320))] {
            let out = shrink(&base, size).unwrap();
            let d = render::decode_jpeg(&out, 10_000).unwrap();
            // DCT scaling rounds up (427 * 6/8 = 321), a pixel off plain rounding.
            assert_eq!(d.img.w, want.0, "size {size}");
            assert!(d.img.h.abs_diff(want.1) <= 1, "size {size}: height {}", d.img.h);
        }
    }
}
