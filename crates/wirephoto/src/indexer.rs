//! Keeps the index equal to the disk (design.md, indexer).
//!
//! - **Scan**: every folder is listed and compared with the index; new entries get an ID, changed files get
//!   their facts and hash cleared, vanished entries are marked missing and deleted at the end of the pass unless
//!   they reappear elsewhere (same device and inode: a move, which keeps the ID).
//! - **Watch**: filesystem events name folders to re-list; polling compares folder modification times where
//!   events are unavailable (network mounts, too few inotify watches).
//! - **Background passes**: media facts, then the cached preview, then hashes, in inode order so hard disks
//!   read sequentially.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use rusqlite::params;
use sha2::{Digest, Sha256};
use wirephoto_api::{ChangeType, IndexingState, Kind};

use crate::app::{App, Library};
use crate::db::{self, EntryRow, NewEntry};
use crate::fsx::{self, Stat};
use crate::{kind, media, preview};

/// A folder still to list: its row and path.
type Pending = (i64, String);
/// A folder to list, and whether to list the folders below it too.
type Queued = (Pending, bool);

/// The root folder's row, created on first start.
fn ensure_root(app: &App, lib: &Library) -> anyhow::Result<i64> {
    let st = lib
        .fs
        .stat("")?
        .ok_or_else(|| anyhow::anyhow!("library root is not a folder"))?;
    let (rowid, id) = app.write(|t| {
        if let Some(r) = db::entry_by_path(t, &lib.api_id, "")? {
            return Ok((r.rowid, r.id));
        }
        let (rowid, id) = db::insert_entry(
            t,
            &NewEntry {
                library: &lib.api_id,
                parent: None,
                name: &lib.cfg.name,
                path: "",
                kind: Kind::Folder,
                mime: None,
                size: 0,
                mtime_ns: st.mtime_ns,
                dev: st.dev,
                ino: st.ino,
            },
        )?;
        db::record_change(t, ChangeType::Created, &id, &lib.api_id, None)?;
        Ok((rowid, id))
    })?;
    *lib.root_id.write() = id;
    Ok(rowid)
}

/// Lists one folder and brings its children in the index up to date. Returns the subfolders to list next:
/// all of them when `recursive`, otherwise only new or moved ones.
fn sync_dir(
    app: &App,
    lib: &Library,
    dir: &Pending,
    recursive: bool,
) -> anyhow::Result<Vec<Queued>> {
    let (dir_rowid, dir_rel) = dir;
    let listing = match lib.fs.list(dir_rel) {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let listing: Vec<(String, Stat)> = listing
        .into_iter()
        .filter(|(n, _)| !ignored(lib, n))
        .collect();
    let dir_stat = lib.fs.stat(dir_rel).ok().flatten();
    let _guard = lib.lock.blocking_lock();
    let mut next = Vec::new();
    app.write(|t| {
        let mut known: HashMap<String, EntryRow> =
            db::children(t, *dir_rowid)?.into_iter().filter(|r| !r.is_trashed()).map(|r| (r.name.clone(), r)).collect();
        for (name, st) in &listing {
            let rel = fsx::join(dir_rel, name);
            if let Some(row) = known.remove(name) {
                if (row.kind == Kind::Folder) == st.is_dir {
                    if st.is_dir {
                        if (row.dev, row.ino) != (st.dev, st.ino) {
                            t.execute("UPDATE entries SET dev = ?, ino = ? WHERE rowid = ?", params![st.dev as i64, st.ino as i64, row.rowid])?;
                        }
                        // The folder's own modification time is updated when it is listed (polling compares it).
                        t.execute("UPDATE entries SET missing = 0 WHERE rowid = ? AND missing != 0", [row.rowid])?;
                        if recursive {
                            next.push(((row.rowid, rel), true));
                        }
                    } else {
                        if (row.size, row.mtime_ns, row.ino) != (st.size, st.mtime_ns, st.ino) {
                            db::update_file_stat(t, row.rowid, st.size, st.mtime_ns, st.dev, st.ino)?;
                            let (k, mime) = kind::guess(name);
                            if row.media_state == db::MEDIA_DONE && row.kind != k {
                                t.execute("UPDATE entries SET kind = ?, mime = ? WHERE rowid = ?", params![kind::to_i64(k), mime, row.rowid])?;
                            }
                            db::record_change(t, ChangeType::Updated, &row.id, &row.library, row.parent_id.as_deref())?;
                        }
                        t.execute("UPDATE entries SET missing = 0 WHERE rowid = ? AND missing != 0", [row.rowid])?;
                    }
                    lib.status.bump();
                    continue;
                }
                // A file became a folder or the reverse: a different entry.
                db::delete_tree(t, &row)?;
            }
            // New here: a move from elsewhere (same device and inode, gone from its old path), or a new entry.
            let moved: Option<EntryRow> = if st.ino != 0 {
                let mut q = t.prepare_cached(&format!(
                    "SELECT {} FROM {} WHERE e.library = ? AND e.dev = ? AND e.ino = ? AND e.trashed_at IS NULL AND (e.kind = 0) = ?",
                    db::ENTRY_COLS,
                    db::ENTRY_FROM
                ))?;
                let rows = q
                    .query_map(params![lib.api_id, st.dev as i64, st.ino as i64, st.is_dir], EntryRow::from_row)?
                    .collect::<Result<Vec<_>, _>>()?;
                rows.into_iter().find(|r| r.path != rel && (st.is_dir || r.size == st.size) && !lib.fs.exists(&r.path))
            } else {
                None
            };
            if let Some(row) = moved {
                db::move_entry(t, &row, Some(*dir_rowid), name, &rel)?;
                t.execute("UPDATE entries SET missing = 0 WHERE rowid = ?", [row.rowid])?;
                if !st.is_dir && (row.size, row.mtime_ns) != (st.size, st.mtime_ns) {
                    db::update_file_stat(t, row.rowid, st.size, st.mtime_ns, st.dev, st.ino)?;
                }
                let parent_id = t.query_row("SELECT id FROM entries WHERE rowid = ?", [dir_rowid], |r| r.get::<_, String>(0))?;
                db::record_change(t, ChangeType::Updated, &row.id, &row.library, Some(&parent_id))?;
                // A moved folder is listed again with everything below it: folders under it that a scan in
                // progress had queued under their old paths would otherwise never be listed.
                if st.is_dir {
                    next.push(((row.rowid, rel), true));
                }
            } else {
                let (k, mime) = if st.is_dir { (Kind::Folder, None) } else { kind::guess(name) };
                let (rowid, id) = db::insert_entry(t, &NewEntry {
                    library: &lib.api_id,
                    parent: Some(*dir_rowid),
                    name,
                    path: &rel,
                    kind: k,
                    mime,
                    size: st.size,
                    mtime_ns: st.mtime_ns,
                    dev: st.dev,
                    ino: st.ino,
                })?;
                let parent_id = t.query_row("SELECT id FROM entries WHERE rowid = ?", [dir_rowid], |r| r.get::<_, String>(0))?;
                db::record_change(t, ChangeType::Created, &id, &lib.api_id, Some(&parent_id))?;
                if st.is_dir {
                    next.push(((rowid, rel), true));
                }
            }
            lib.status.bump();
        }
        for row in known.values() {
            t.execute("UPDATE entries SET missing = 1 WHERE rowid = ?", [row.rowid])?;
        }
        if let Some(ds) = dir_stat {
            t.execute("UPDATE entries SET mtime_ns = ? WHERE rowid = ?", params![ds.mtime_ns, dir_rowid])?;
        }
        Ok(())
    })?;
    Ok(next)
}

/// Deletes entries still marked missing at the end of a pass (unless they are back where they were).
fn finalize(app: &App, lib: &Library) -> anyhow::Result<()> {
    let missing: Vec<EntryRow> = app.db.read(|c| {
        let mut st = c.prepare(&format!(
            "SELECT {} FROM {} WHERE e.library = ? AND e.missing = 1",
            db::ENTRY_COLS,
            db::ENTRY_FROM
        ))?;
        let rows = st
            .query_map([&lib.api_id], EntryRow::from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })?;
    if missing.is_empty() {
        return Ok(());
    }
    let _guard = lib.lock.blocking_lock();
    app.write(|t| {
        for row in missing {
            let Some(row) = db::entry_by_rowid(t, row.rowid)? else {
                continue;
            }; // went with an ancestor
            if lib.fs.exists(&row.path) {
                t.execute(
                    "UPDATE entries SET missing = 0 WHERE rowid = ?",
                    [row.rowid],
                )?;
            } else {
                db::delete_tree(t, &row)?;
            }
        }
        Ok(())
    })
}

/// Lists folders breadth-first from `start`.
fn walk(app: &App, lib: &Library, start: Vec<Pending>, recursive: bool) -> anyhow::Result<()> {
    let mut queue: VecDeque<Queued> = start.into_iter().map(|p| (p, recursive)).collect();
    // By row and path: a folder renamed during the walk is queued again under its new path, and listing its old
    // one (not found) must not stop that.
    let mut seen = HashSet::new();
    while let Some((dir, deep)) = queue.pop_front() {
        if !seen.insert(dir.clone()) {
            continue;
        }
        match sync_dir(app, lib, &dir, deep) {
            Ok(next) => queue.extend(next),
            Err(e) => tracing::warn!(library = %lib.api_id, path = %dir.1, "listing failed: {e:#}"),
        }
    }
    finalize(app, lib)
}

/// Names never indexed: wirephoto's own trash and temporary files, and the library's ignore patterns.
fn ignored(lib: &Library, name: &str) -> bool {
    name == fsx::TRASH_DIR || name.starts_with(".wp-") || lib.cfg.ignores(name)
}

/// Indexes a folder tree that was just created through the API (a copied folder).
pub fn index_tree(app: &App, lib: &Library, rowid: i64, rel: &str) -> anyhow::Result<()> {
    walk(app, lib, vec![(rowid, rel.to_string())], true)
}

/// The full scan of a library (blocking).
pub fn scan(app: &App, lib: &Library) -> anyhow::Result<()> {
    let started = std::time::Instant::now();
    lib.status.set(Some(IndexingState::Scanning), 0, 0);
    let root = ensure_root(app, lib)?;
    walk(app, lib, vec![(root, String::new())], true)?;
    let total: i64 = app.db.read(|c| {
        Ok(c.query_row(
            "SELECT COUNT(*) FROM entries WHERE library = ?",
            [&lib.api_id],
            |r| r.get(0),
        )?)
    })?;
    lib.status.set(None, 0, 0);
    tracing::info!(library = %lib.api_id, entries = total, secs = started.elapsed().as_secs_f32(), "scanned");
    Ok(())
}

/// Re-lists the folders that changed: by path (from filesystem events) or, when polling, every folder whose
/// modification time differs from the index.
pub fn sync_paths(app: &App, lib: &Library, dirs: HashSet<String>) -> anyhow::Result<()> {
    let mut start = Vec::new();
    for rel in dirs {
        // The nearest folder the index knows: a new folder is found by listing its parent.
        let mut p = rel.as_str();
        loop {
            let row = app.db.read(|c| Ok(db::entry_by_path(c, &lib.api_id, p)?))?;
            if let Some(r) = row.filter(|r| r.kind == Kind::Folder && !r.is_trashed()) {
                start.push((r.rowid, r.path));
                break;
            }
            if p.is_empty() {
                break;
            }
            p = fsx::parent_of(p);
        }
    }
    walk(app, lib, start, false)
}

fn poll(app: &App, lib: &Library) -> anyhow::Result<()> {
    let folders: Vec<(i64, String, i64)> = app.db.read(|c| {
        let mut st = c.prepare("SELECT rowid, path, mtime_ns FROM entries WHERE library = ? AND kind = 0 AND trashed_at IS NULL")?;
        let rows = st.query_map([&lib.api_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })?;
    let changed: Vec<Pending> = folders
        .into_iter()
        .filter(|(_, path, mtime)| {
            lib.fs
                .stat(path)
                .ok()
                .flatten()
                .is_none_or(|s| s.mtime_ns != *mtime)
        })
        .map(|(rowid, path, _)| (rowid, path))
        .collect();
    if changed.is_empty() {
        return Ok(());
    }
    walk(app, lib, changed, false)
}

/// A library's filesystem watcher, registered but not yet applied: events queue until [`watch`] runs.
pub struct Watching {
    watcher: Option<notify::RecommendedWatcher>,
    rx: tokio::sync::mpsc::UnboundedReceiver<notify::Result<notify::Event>>,
    root: std::path::PathBuf,
}

/// Registers the watcher BEFORE the library's scan, so nothing that changes during the scan is missed: its
/// events wait in the queue and are applied once the scan is done ([`watch`]). Applying them during the scan
/// instead would run a pass's end (deleting what is missing) while the scan has not yet seen where a moved
/// entry went.
pub fn begin_watch(lib: &Library) -> Watching {
    use notify::Watcher;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<notify::Result<notify::Event>>();
    let root = std::fs::canonicalize(&lib.fs.root).unwrap_or_else(|_| lib.fs.root.clone());
    let watch_root = root.clone();
    let watcher =
        if lib.cfg.watch == crate::config::WatchMode::Poll {
            None
        } else {
            notify::recommended_watcher(move |ev| {
            let _ = tx.send(ev);
        })
        .and_then(|mut w| w.watch(&watch_root, notify::RecursiveMode::Recursive).map(|_| w))
        .map_err(
            |e| tracing::warn!(library = %lib.api_id, "watching unavailable, polling instead: {e}"),
        )
        .ok()
        };
    Watching { watcher, rx, root }
}

/// Applies a library's changes from then on: the queued and new filesystem events when watching, polling
/// otherwise.
pub fn watch(app: Arc<App>, lib: Arc<Library>, watching: Watching) {
    let Watching {
        watcher,
        mut rx,
        root,
    } = watching;
    lib.live.store(watcher.is_some(), Ordering::Relaxed);
    if let Some(watcher) = watcher {
        let (app, lib) = (app.clone(), lib.clone());
        tokio::spawn(async move {
            let _watcher = watcher; // lives as long as the task
            loop {
                let Some(first) = rx.recv().await else { break };
                let mut dirs = HashSet::new();
                let mut overflow = false;
                let mut take =
                    |ev: notify::Result<notify::Event>, dirs: &mut HashSet<String>| match ev {
                        Ok(ev) => {
                            if ev.need_rescan() {
                                overflow = true;
                            }
                            for p in ev.paths {
                                if let Some(rel) = relative(&root, &p, &lib) {
                                    dirs.insert(fsx::parent_of(&rel).to_string());
                                    dirs.insert(rel); // a folder's own contents, when the event names a folder
                                }
                            }
                        }
                        Err(e) => tracing::debug!("watch error: {e}"),
                    };
                take(first, &mut dirs);
                // Settle: collect what arrives in the next moment as one batch.
                tokio::time::sleep(Duration::from_millis(500)).await;
                while let Ok(ev) = rx.try_recv() {
                    take(ev, &mut dirs);
                }
                let (app2, lib2) = (app.clone(), lib.clone());
                let res = tokio::task::spawn_blocking(move || {
                    if overflow {
                        poll(&app2, &lib2)
                    } else {
                        sync_paths(&app2, &lib2, dirs)
                    }
                })
                .await;
                if let Ok(Err(e)) = res {
                    tracing::warn!(library = %lib.api_id, "sync failed: {e:#}");
                }
            }
        });
    } else {
        let every = Duration::from_secs(lib.cfg.poll_seconds.max(5));
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(every).await;
                let (a, l) = (app.clone(), lib.clone());
                if let Ok(Err(e)) = tokio::task::spawn_blocking(move || poll(&a, &l)).await {
                    tracing::warn!(library = %lib.api_id, "poll failed: {e:#}");
                }
            }
        });
    }
}

/// An event path as a library-relative path, unless it is ignored (dotfiles, the trash, temporary files).
fn relative(root: &Path, p: &Path, lib: &Library) -> Option<String> {
    let rel = p.strip_prefix(root).ok()?;
    let mut parts = Vec::new();
    for c in rel.components() {
        let s = c.as_os_str().to_str()?;
        if ignored(lib, s) {
            return None;
        }
        parts.push(s);
    }
    Some(parts.join("/"))
}

// ---- background passes ----

/// Reads media facts for entries that have none yet. Returns how many were read.
fn media_pass(app: &App) -> anyhow::Result<usize> {
    let rows: Vec<EntryRow> = app.db.read(|c| {
        let mut st = c.prepare(&format!(
            "SELECT {} FROM {} WHERE e.media_state = 0 AND e.trashed_at IS NULL AND e.missing = 0 ORDER BY e.dev, e.ino LIMIT 64",
            db::ENTRY_COLS,
            db::ENTRY_FROM
        ))?;
        let rows = st.query_map([], EntryRow::from_row)?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })?;
    if rows.is_empty() {
        return Ok(0);
    }
    let mut results = Vec::with_capacity(rows.len());
    for row in &rows {
        let Some(lib) = app.library(&row.library) else {
            continue;
        };
        let facts = lib.fs.open_file(&row.path).and_then(|f| {
            let mut facts = media::read_facts(&row.name, &f)?;
            media::add_video_facts(&mut facts, app.previews.pool(), f);
            Ok(facts)
        });
        results.push((row, facts));
    }
    app.write(|t| {
        for (row, facts) in &results {
            store_facts(t, row, facts.as_ref().ok(), true)?;
        }
        Ok(())
    })?;
    Ok(rows.len())
}

/// Writes facts read from a file (`None`: unreadable), recording an update when `record`.
pub fn store_facts(
    t: &rusqlite::Connection,
    row: &EntryRow,
    facts: Option<&media::Facts>,
    record: bool,
) -> anyhow::Result<()> {
    match facts {
        Some(f) => {
            let k = f.kind.unwrap_or(row.kind);
            let media_state = if matches!(k, Kind::Image | Kind::Raw | Kind::Video) {
                db::MEDIA_DONE
            } else {
                db::MEDIA_NONE
            };
            let previewable = f.previewable
                && f.mime.as_deref().is_some_and(|m| {
                    row.kind == Kind::Raw
                        || k == Kind::Raw
                        || k == Kind::Video
                        || media::decodable(m)
                });
            t.execute(
                "UPDATE entries SET kind = ?, mime = ?, width = ?, height = ?, orientation = ?, taken = ?,
                     sort_taken = COALESCE(?, mtime_ns / 1000000000), duration = ?, media_state = ?, previewable = ?, preview_done = 0
                 WHERE rowid = ?",
                params![
                    kind::to_i64(k),
                    f.mime,
                    f.width,
                    f.height,
                    f.orientation,
                    f.taken,
                    f.taken_ts,
                    f.duration,
                    media_state,
                    previewable,
                    row.rowid
                ],
            )?;
        }
        None => {
            t.execute(
                "UPDATE entries SET media_state = ?, previewable = 0 WHERE rowid = ?",
                params![db::MEDIA_NONE, row.rowid],
            )?;
        }
    }
    if record {
        db::record_change(
            t,
            ChangeType::Updated,
            &row.id,
            &row.library,
            row.parent_id.as_deref(),
        )?;
    }
    Ok(())
}

/// Pre-generates the cached preview size. Returns how many entries were handled.
async fn preview_pass(app: &Arc<App>) -> anyhow::Result<usize> {
    if !app.config.cache.pregenerate {
        return Ok(0);
    }
    let rows: Vec<EntryRow> = app.db.read(|c| {
        let mut st = c.prepare(&format!(
            "SELECT {} FROM {} WHERE e.previewable = 1 AND e.preview_done = 0 AND e.trashed_at IS NULL AND e.missing = 0
             ORDER BY e.dev, e.ino LIMIT 16",
            db::ENTRY_COLS,
            db::ENTRY_FROM
        ))?;
        let rows = st.query_map([], EntryRow::from_row)?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })?;
    let n = rows.len();
    let jobs = rows.into_iter().filter_map(|row| {
        let lib = app.library(&row.library)?.clone();
        let app = app.clone();
        Some(async move {
            let src = preview::Source::File(lib.fs.clone(), row.path.clone());
            let ok = match app.previews.ensure_cached(&row, src).await {
                Ok(_) => true,
                Err(e) => {
                    tracing::debug!(entry = %row.id, path = %row.path, "no preview: {e:#}");
                    false
                }
            };
            (row, ok)
        })
    });
    let results = futures::future::join_all(jobs).await;
    app.write(|t| {
        for (row, ok) in &results {
            if *ok {
                t.execute(
                    "UPDATE entries SET preview_done = 1 WHERE rowid = ?",
                    [row.rowid],
                )?;
            } else {
                t.execute(
                    "UPDATE entries SET preview_done = 1, previewable = 0 WHERE rowid = ?",
                    [row.rowid],
                )?;
                db::record_change(
                    t,
                    ChangeType::Updated,
                    &row.id,
                    &row.library,
                    row.parent_id.as_deref(),
                )?;
            }
        }
        Ok(())
    })?;
    Ok(n)
}

/// SHA-256 of a whole file.
pub fn hash_file(f: &mut std::fs::File) -> std::io::Result<[u8; 32]> {
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().into())
}

/// Hashes one file and stores the hash if the file did not change meanwhile.
pub fn hash_entry(app: &App, row: &EntryRow) -> anyhow::Result<Option<[u8; 32]>> {
    let Some(lib) = app.library(&row.library) else {
        return Ok(None);
    };
    let mut f = lib.fs.open_file(&row.path)?;
    let sum = hash_file(&mut f)?;
    let after = lib.fs.stat(&row.path)?;
    if after.is_none_or(|s| (s.size, s.mtime_ns, s.ino) != (row.size, row.mtime_ns, row.ino)) {
        return Ok(None); // changed while hashing; the next pass will try again
    }
    let stored = app.write(|t| {
        let n = t.execute(
            "UPDATE entries SET sha256 = ? WHERE rowid = ? AND mtime_ns = ? AND size = ?",
            params![&sum[..], row.rowid, row.mtime_ns, row.size as i64],
        )?;
        db::record_change(
            t,
            ChangeType::Updated,
            &row.id,
            &row.library,
            row.parent_id.as_deref(),
        )?;
        Ok(n > 0)
    })?;
    if stored {
        // Its previews are now keyed by content: moved there, or dropped if a copy's are there already.
        if let Err(e) = app.previews.adopt_hash(row, &sum) {
            tracing::debug!(entry = %row.id, "moving previews to the content key: {e:#}");
        }
    }
    Ok(Some(sum))
}

fn hash_pass(app: &App) -> anyhow::Result<usize> {
    let rows: Vec<EntryRow> = app.db.read(|c| {
        let mut st = c.prepare(&format!(
            "SELECT {} FROM {} WHERE e.sha256 IS NULL AND e.kind != 0 AND e.trashed_at IS NULL AND e.missing = 0
             ORDER BY e.dev, e.ino LIMIT 8",
            db::ENTRY_COLS,
            db::ENTRY_FROM
        ))?;
        let rows = st.query_map([], EntryRow::from_row)?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })?;
    for row in &rows {
        if let Err(e) = hash_entry(app, row) {
            tracing::debug!(entry = %row.id, "hash failed: {e:#}");
        }
    }
    Ok(rows.len())
}

fn pending_counts(app: &App, lib: &str) -> anyhow::Result<(u64, u64)> {
    app.db.read(|c| {
        Ok(c.query_row(
            "SELECT COALESCE(SUM(media_state = 0), 0), COALESCE(SUM(sha256 IS NULL AND kind != 0), 0)
             FROM entries WHERE library = ? AND trashed_at IS NULL AND missing = 0",
            [lib],
            |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64)),
        )?)
    })
}

/// Runs the background passes forever: facts first (listings need them), then the cached preview (grids need
/// it), then hashes (only deduplication needs them).
pub async fn background(app: Arc<App>) {
    loop {
        for lib in &app.libraries {
            if lib
                .status
                .get()
                .is_some_and(|s| s.state == IndexingState::Scanning)
            {
                continue;
            }
            if let Ok((facts, hashes)) = pending_counts(&app, &lib.api_id) {
                let total: i64 = app
                    .db
                    .read(|c| {
                        Ok(c.query_row(
                            "SELECT COUNT(*) FROM entries WHERE library = ? AND kind != 0",
                            [&lib.api_id],
                            |r| r.get(0),
                        )?)
                    })
                    .unwrap_or(0);
                let total = total as u64;
                if facts > 0 {
                    lib.status.set(
                        Some(IndexingState::Reading),
                        total.saturating_sub(facts),
                        total,
                    );
                } else if hashes > 0 {
                    lib.status.set(
                        Some(IndexingState::Hashing),
                        total.saturating_sub(hashes),
                        total,
                    );
                } else {
                    lib.status.set(None, 0, 0);
                }
            }
        }
        let a = app.clone();
        let did = match tokio::task::spawn_blocking(move || media_pass(&a)).await {
            Ok(Ok(n)) if n > 0 => true,
            Ok(Err(e)) => {
                tracing::warn!("media pass: {e:#}");
                false
            }
            _ => match preview_pass(&app).await {
                Ok(n) if n > 0 => true,
                Err(e) => {
                    tracing::warn!("preview pass: {e:#}");
                    false
                }
                _ => {
                    let a = app.clone();
                    match tokio::task::spawn_blocking(move || hash_pass(&a)).await {
                        Ok(Ok(n)) => n > 0,
                        Ok(Err(e)) => {
                            tracing::warn!("hash pass: {e:#}");
                            false
                        }
                        Err(_) => false,
                    }
                }
            },
        };
        if !did {
            tokio::select! {
                _ = app.work.notified() => {}
                _ = tokio::time::sleep(Duration::from_secs(30)) => {}
            }
        }
    }
}

/// Hourly upkeep: empties expired trash, prunes old changes; flushes and trims the preview cache.
pub async fn upkeep(app: Arc<App>) {
    let mut tick = 0u64;
    loop {
        tokio::time::sleep(Duration::from_secs(30)).await;
        tick += 1;
        let disk = app.previews.disk().clone();
        let evict = tick.is_multiple_of(10);
        let _ = tokio::task::spawn_blocking(move || {
            let _ = disk.flush();
            if evict
                && let Ok(n) = disk.evict()
                && n > 0
            {
                tracing::info!(bytes = n, "evicted previews");
            }
        })
        .await;
        if tick % 120 == 1 {
            let a = app.clone();
            let _ = tokio::task::spawn_blocking(move || match sweep_previews(&a) {
                Ok(n) if n > 0 => {
                    tracing::info!(previews = n, "removed previews of files gone or changed")
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("sweeping previews: {e:#}"),
            })
            .await;
        }
        if tick % 120 == 1 {
            let a = app.clone();
            let _ = tokio::task::spawn_blocking(move || {
                if let Err(e) = crate::write::purge_trash(&a) {
                    tracing::warn!("emptying trash: {e:#}");
                }
                let cutoff = crate::util::now_secs() - 30 * 24 * 3600;
                let _ = a.db.write(|t| {
                    let through: Option<i64> = t.query_row("SELECT MAX(seq) FROM changes WHERE time < ?", [cutoff], |r| r.get(0))?;
                    if let Some(through) = through {
                        t.execute("DELETE FROM changes WHERE seq <= ?", [through])?;
                        t.execute("INSERT INTO meta(k, v) VALUES ('pruned_through', ?) ON CONFLICT(k) DO UPDATE SET v = excluded.v",
                            [through.to_le_bytes().to_vec()])?;
                    }
                    Ok(())
                });
            })
            .await;
        }
    }
}

/// Tidies the preview cache: removes previews no file needs any more (of entries deleted, of old versions of
/// changed files) and moves entry-keyed ones whose file has since been hashed to its content key (a cache
/// written before previews were content-keyed, or a rendering that finished after its file was hashed). Trashed
/// entries keep theirs, for a restore. Returns how many were removed.
pub fn sweep_previews(app: &App) -> anyhow::Result<u64> {
    let mut moves = Vec::new();
    let removed = app.previews.disk().retain(|key| {
        let live = app.db.read(|c| {
            Ok(match preview::parse_key(key) {
                Some(preview::KeyOf::Content(hash, _)) => c
                    .query_row(
                        "SELECT 1 FROM entries WHERE sha256 = ? LIMIT 1",
                        [&hash[..]],
                        |_| Ok(()),
                    )
                    .is_ok(),
                Some(preview::KeyOf::Entry(id, etag, size)) => match db::entry_by_id(c, &id)? {
                    Some(r) if r.etag() == etag => {
                        if r.sha256.is_some() {
                            moves.push((key.to_string(), preview::key(&r, size)));
                        }
                        true
                    }
                    _ => false,
                },
                None => false,
            })
        });
        live.unwrap_or(true) // keep on a database error
    })?;
    if !moves.is_empty() {
        app.previews.disk().rekey(&moves)?;
    }
    Ok(removed)
}

/// Runs the background passes until there is nothing left to do (tests, and tools that want a settled index).
pub async fn settle(app: &Arc<App>) -> anyhow::Result<()> {
    loop {
        let a = app.clone();
        let facts = tokio::task::spawn_blocking(move || media_pass(&a)).await??;
        let previews = preview_pass(app).await?;
        let a = app.clone();
        let hashes = tokio::task::spawn_blocking(move || hash_pass(&a)).await??;
        if facts + previews + hashes == 0 {
            return Ok(());
        }
    }
}
