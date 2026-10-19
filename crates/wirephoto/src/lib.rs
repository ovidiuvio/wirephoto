//! wirephoto: serves the photos on a machine's own disks to apps over HTTP (docs/design.md).

pub mod app;
pub mod audit;
pub mod auth;
pub mod config;
pub mod db;
pub mod decode;
pub mod fsx;
pub mod http;
pub mod indexer;
pub mod kind;
pub mod media;
pub mod preview;
pub mod util;
pub mod write;

/// The version and the commit it was built from (CI passes `WIREPHOTO_COMMIT`): which image is running.
pub fn version() -> String {
    match option_env!("WIREPHOTO_COMMIT") {
        Some(c) if !c.is_empty() => {
            format!("{} ({})", env!("CARGO_PKG_VERSION"), &c[..c.len().min(7)])
        }
        _ => format!("{} (local build)", env!("CARGO_PKG_VERSION")),
    }
}

use std::sync::Arc;

use anyhow::Context;
use parking_lot::RwLock;

use app::{App, IndexStatus, Library};
use config::Config;

/// Opens the index, the preview cache and the libraries. Nothing runs until [`start`].
pub fn build(config: Config) -> anyhow::Result<(Arc<App>, axum::Router)> {
    std::fs::create_dir_all(&config.data)
        .with_context(|| format!("creating {}", config.data.display()))?;
    let db = Arc::new(db::Db::open(&config.data.join("index.db"))?);
    let keys = auth::Keys::load(&db)?;
    let disk = preview::cache::DiskCache::open(
        &config.data.join("previews.redb"),
        config.cache.max_size,
        preview::CACHED_SIZE,
    )?;
    let workers = config.decode.workers;
    let memory = config.cache.memory;
    let pool = {
        let n = match workers {
            0 => std::thread::available_parallelism().map_or(4, |n| n.get()),
            n => n,
        };
        let program = match &config.decode.program {
            Some(p) => p.clone(),
            None => std::env::current_exe().context("locating the wirephoto binary")?,
        };
        let tools = decode::Tools::find(config.decode.ffmpeg.as_deref());
        match &tools {
            Some(t) => tracing::info!(ffmpeg = %t.ffmpeg.display(), "video posters on"),
            None => tracing::warn!("ffmpeg/ffprobe not found: videos get no posters or durations"),
        }
        let (candidate, why_not) = gpu_candidate(&config.decode, tools.is_some());
        let heic_ffmpeg = config.decode.heic == config::HeicMode::Auto && tools.is_some();
        let gpu = Arc::new(decode::GpuState::new(
            heic_ffmpeg,
            candidate.is_some(),
            why_not,
        ));
        // Requests' renderings and a probe; background ones get their own workers (half as many, and a probe)
        // at low CPU priority, so they use what requests leave over and never hold up a request's turn.
        let bg = decode::Pool::with_nice(
            program.clone(),
            (n / 2).max(1) + 1,
            tools.clone(),
            10,
            gpu.clone(),
        );
        // Once, here: what the GPU decodes decides the route of every image from now on.
        if let Some((api, device)) = candidate {
            let found = bg.probe_gpu(api, device);
            match found.gpu {
                Some(g) => {
                    tracing::info!("decoding on the GPU: {}", g.describe());
                    if !found.detail.is_empty() {
                        tracing::info!("the GPU does not decode: {}", found.detail);
                    }
                    gpu.found(Some(g.clone()), g.describe());
                }
                None => {
                    tracing::warn!("not decoding on the GPU: {}", found.detail);
                    gpu.found(None, format!("not used: {}", found.detail));
                }
            }
        }
        (
            Arc::new(decode::Pool::new(program, n + 1, tools, gpu)),
            Arc::new(bg),
        )
    };
    let (pool, bg_pool) = pool;
    if pool.has_video() {
        // Videos read while ffmpeg was missing: read them again.
        db.write(|t| {
            t.execute(
                "UPDATE entries SET media_state = ? WHERE kind = ? AND media_state = ? AND previewable = 0 AND duration IS NULL",
                rusqlite::params![db::MEDIA_PENDING, kind::to_i64(wirephoto_api::Kind::Video), db::MEDIA_DONE],
            )?;
            Ok(())
        })?;
    }
    let mut libraries = Vec::new();
    for cfg in &config.libraries {
        let fs = fsx::LibFs::open(&cfg.path)
            .with_context(|| format!("library {}: opening {}", cfg.id, cfg.path.display()))?;
        libraries.push(Arc::new(Library {
            api_id: cfg.api_id(),
            cfg: cfg.clone(),
            fs: Arc::new(fs),
            root_id: RwLock::new(String::new()),
            lock: tokio::sync::Mutex::new(()),
            status: IndexStatus::default(),
            live: false.into(),
        }));
    }
    let seq = db.read(|c| Ok(db::current_seq(c)?))?;
    let long_ago = std::time::Instant::now()
        .checked_sub(std::time::Duration::from_secs(3600))
        .unwrap_or_else(std::time::Instant::now);
    let app = Arc::new(App {
        config,
        db,
        libraries,
        previews: Arc::new(preview::Previews::new(disk, pool, bg_pool, workers, memory)),
        keys,
        seq: tokio::sync::watch::channel(seq).0,
        work: tokio::sync::Notify::new(),
        router: std::sync::OnceLock::new(),
        origins: RwLock::new((long_ago, Vec::new())),
        api_activity: std::sync::atomic::AtomicI64::new(0),
        yielding: std::sync::atomic::AtomicBool::new(false),
        writes: std::sync::atomic::AtomicU64::new(0),
    });
    // The root IDs of libraries indexed before, so they are known before the scan finishes.
    for lib in &app.libraries {
        if let Some(r) = app
            .db
            .read(|c| Ok(db::entry_by_path(c, &lib.api_id, "")?))?
        {
            *lib.root_id.write() = r.id;
        }
    }
    let router = http::router(app.clone());
    Ok((app, router))
}

/// The GPU to probe at start, or why there is none.
fn gpu_candidate(
    cfg: &config::DecodeConfig,
    ffmpeg: bool,
) -> (Option<(decode::accel::Api, Option<String>)>, String) {
    if cfg.gpu == config::GpuMode::Off {
        return (None, "off ([decode] gpu = \"off\")".into());
    }
    if !ffmpeg {
        return (None, "none: no ffmpeg".into());
    }
    if cfg!(target_os = "macos") {
        if cfg.gpu != config::GpuMode::On {
            return (
                None,
                "not used: VideoToolbox is slower than the CPU path here ([decode] gpu = \"on\" uses it)".into(),
            );
        }
        return (
            Some((decode::accel::Api::VideoToolbox, None)),
            String::new(),
        );
    }
    if cfg!(target_os = "linux") {
        let dev = cfg
            .gpu_device
            .clone()
            .unwrap_or_else(|| decode::accel::DEFAULT_DEVICE.to_string());
        if !std::path::Path::new(&dev).exists() {
            return (
                None,
                format!("none: no {dev} (map /dev/dri into the container)"),
            );
        }
        return (Some((decode::accel::Api::Vaapi, Some(dev))), String::new());
    }
    (None, "none on this system".into())
}

/// Starts indexing, watching and the background passes.
pub fn start(app: &Arc<App>) {
    for lib in &app.libraries {
        let (a, l) = (app.clone(), lib.clone());
        // Watching starts before the scan so nothing changed during it is missed (indexer::begin_watch).
        let watching = indexer::begin_watch(app, lib);
        tokio::spawn(async move {
            let (a2, l2) = (a.clone(), l.clone());
            match tokio::task::spawn_blocking(move || indexer::scan(&a2, &l2)).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::error!(library = %l.api_id, "scan failed: {e:#}"),
                Err(e) => tracing::error!(library = %l.api_id, "scan panicked: {e}"),
            }
            a.work.notify_one();
            indexer::watch(a, l, watching);
        });
    }
    tokio::spawn(indexer::background(app.clone()));
    tokio::spawn(indexer::report(app.clone()));
    tokio::spawn(indexer::upkeep(app.clone()));
}
