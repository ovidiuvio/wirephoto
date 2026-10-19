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
    tracing::info!(library = %lib.api_id, path = %lib.fs.root.display(), "scanning");
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
    *lib.last_scan.write() = Some(crate::app::ScanInfo {
        at: crate::util::now_secs() - started.elapsed().as_secs() as i64,
        secs: started.elapsed().as_secs_f64(),
        entries: total as u64,
    });
    tracing::info!(
        "{}: scanned in {}, {} entries",
        lib.api_id,
        duration_words(started.elapsed().as_secs_f64()),
        thousands(total as u64)
    );
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
    relist(app, lib, start)
}

/// Whether an entry is on disk as the index has it: a folder, or a file of the same size, time and inode (one
/// that changed may not have the bytes its hash says).
pub fn on_disk(lib: &Library, row: &EntryRow) -> bool {
    match lib.fs.stat(&row.path) {
        Ok(Some(st)) if row.kind == Kind::Folder => st.is_dir,
        Ok(Some(st)) => {
            st.is_file && (st.size, st.mtime_ns, st.ino) == (row.size, row.mtime_ns, row.ino)
        }
        _ => false,
    }
}

/// An entry was found not to be on disk as indexed: lists its nearest folder still on disk again, in the
/// background, so the index catches up without waiting for the watcher or the next poll. Never blocks (the
/// caller may hold the library's lock).
pub fn forget_later(app: &Arc<App>, lib: &Arc<Library>, rel: &str) {
    let mut p = fsx::parent_of(rel).to_string();
    while !p.is_empty() && !lib.fs.stat(&p).ok().flatten().is_some_and(|s| s.is_dir) {
        p = fsx::parent_of(&p).to_string();
    }
    tracing::debug!(library = %lib.api_id, path = rel, "not on disk as indexed: listing {p:?} again");
    let (app, lib) = (app.clone(), lib.clone());
    if let Ok(h) = tokio::runtime::Handle::try_current() {
        h.spawn(async move {
            let _ = tokio::task::spawn_blocking(move || sync_paths(&app, &lib, HashSet::from([p])))
                .await;
        });
    }
}

/// Lists folders again for changes made on disk, and notes when one changed the index (`wirephoto status`).
fn relist(app: &App, lib: &Library, folders: Vec<Pending>) -> anyhow::Result<()> {
    crate::metrics::Metrics::add(&app.metrics.folders_relisted, folders.len() as u64);
    let before = *app.seq.borrow();
    walk(app, lib, folders, false)?;
    if *app.seq.borrow() != before {
        app.metrics.disk_changed();
    }
    Ok(())
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
    relist(app, lib, changed)
}

/// Whether a filesystem event may mean a change. Opening something, or closing it after only reading, does not:
/// inotify reports both (for a folder too, when it is listed), so taking them for changes made every listing
/// set off another, forever, and every read of a photo (a download, Synology's indexer, a backup) started one.
fn is_change(ev: &notify::Result<notify::Event>) -> bool {
    use notify::event::{AccessKind, AccessMode, EventKind};
    match ev {
        Ok(ev) => !matches!(
            ev.kind,
            EventKind::Access(
                AccessKind::Open(_) | AccessKind::Read | AccessKind::Close(AccessMode::Read)
            )
        ),
        Err(_) => true,
    }
}

/// A library's filesystem watcher, registered but not yet applied: events queue until [`watch`] runs.
pub struct Watching {
    watcher: Option<notify::RecommendedWatcher>,
    rx: tokio::sync::mpsc::UnboundedReceiver<notify::Result<notify::Event>>,
    root: std::path::PathBuf,
    /// Folders watched, relative ("" is the root): one watch each, on Linux.
    watched: HashSet<String>,
}

/// On Linux every indexed folder gets its own watch (inotify), so what the index ignores is never watched:
/// Synology keeps a hidden `@eaDir` folder per PHOTO, which alone can exceed the system's watch limit
/// (`fs.inotify.max_user_watches`, often 8192). Elsewhere (FSEvents on macOS: no such limit) one recursive watch
/// on the root.
/// (`WIREPHOTO_WATCH_PER_FOLDER=1` forces per-folder watches anywhere, for tests.)
fn per_folder() -> bool {
    cfg!(target_os = "linux") || std::env::var_os("WIREPHOTO_WATCH_PER_FOLDER").is_some()
}

/// Every indexed folder of a library, relative ("" is the root).
fn indexed_folders(app: &App, lib: &Library) -> anyhow::Result<Vec<String>> {
    app.db.read(|c| {
        let mut st = c.prepare(
            "SELECT path FROM entries WHERE library = ? AND kind = 0 AND trashed_at IS NULL AND missing = 0",
        )?;
        let rows = st.query_map([&lib.api_id], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
}

/// Watches each folder not watched yet, one level each. An error only when the system's watch limit is reached
/// (a folder gone meanwhile is skipped).
fn add_watches(
    w: &mut notify::RecommendedWatcher,
    root: &Path,
    watched: &mut HashSet<String>,
    rels: impl IntoIterator<Item = String>,
) -> notify::Result<Vec<String>> {
    use notify::Watcher;
    let mut added = Vec::new();
    for rel in rels {
        if watched.contains(&rel) {
            continue;
        }
        let p = if rel.is_empty() {
            root.to_path_buf()
        } else {
            root.join(&rel)
        };
        match w.watch(&p, notify::RecursiveMode::NonRecursive) {
            Ok(()) => {
                watched.insert(rel.clone());
                added.push(rel);
            }
            Err(e) if matches!(e.kind, notify::ErrorKind::MaxFilesWatch) => return Err(e),
            Err(_) => {}
        }
    }
    Ok(added)
}

/// Registers the watcher BEFORE the library's scan, so nothing that changes during the scan is missed: its
/// events wait in the queue and are applied once the scan is done ([`watch`]). Applying them during the scan
/// instead would run a pass's end (deleting what is missing) while the scan has not yet seen where a moved
/// entry went. On Linux the folders known from the last run are watched now and the ones the scan finds after it.
pub fn begin_watch(app: &App, lib: &Library) -> Watching {
    use notify::Watcher;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<notify::Result<notify::Event>>();
    let root = std::fs::canonicalize(&lib.fs.root).unwrap_or_else(|_| lib.fs.root.clone());
    let mut watched = HashSet::new();
    let unavailable = |e: &dyn std::fmt::Display| {
        *lib.watch_note.write() =
            format!("watching is unavailable ({e}): raise fs.inotify.max_user_watches to watch");
        tracing::warn!(
            library = %lib.api_id,
            "watching unavailable, polling every {} s instead: {e} (raise fs.inotify.max_user_watches to watch)",
            lib.cfg.poll_seconds.max(5)
        )
    };
    let watcher = if lib.cfg.watch == crate::config::WatchMode::Poll {
        *lib.watch_note.write() = "watch = \"poll\" in the config".into();
        None
    } else {
        match notify::recommended_watcher(move |ev| {
            if is_change(&ev) {
                let _ = tx.send(ev);
            }
        }) {
            Err(e) => {
                unavailable(&e);
                None
            }
            Ok(mut w) if per_folder() => {
                let mut rels = indexed_folders(app, lib).unwrap_or_default();
                rels.push(String::new());
                match add_watches(&mut w, &root, &mut watched, rels) {
                    Ok(_) => {
                        *lib.watch_note.write() = "inotify, a watch per folder".into();
                        lib.watched.store(watched.len() as u64, Ordering::Relaxed);
                        Some(w)
                    }
                    Err(e) => {
                        unavailable(&e);
                        None
                    }
                }
            }
            Ok(mut w) => match w.watch(&root, notify::RecursiveMode::Recursive) {
                Ok(()) => {
                    *lib.watch_note.write() = "one watch on the whole tree".into();
                    Some(w)
                }
                Err(e) => {
                    unavailable(&e);
                    None
                }
            },
        }
    };
    Watching {
        watcher,
        rx,
        root,
        watched,
    }
}

/// Applies a library's changes from then on: the queued and new filesystem events when watching, polling
/// otherwise (or once the watch limit is reached).
pub fn watch(app: Arc<App>, lib: Arc<Library>, watching: Watching) {
    let Watching {
        watcher,
        mut rx,
        root,
        mut watched,
    } = watching;
    lib.live.store(watcher.is_some(), Ordering::Relaxed);
    let Some(mut watcher) = watcher else {
        tokio::spawn(poll_forever(app, lib));
        return;
    };
    tokio::spawn(async move {
        // Folders the scan found that the last run did not know.
        if per_folder()
            && let Err(e) = watch_new_folders(&app, &lib, &mut watcher, &root, &mut watched).await
        {
            tracing::warn!(library = %lib.api_id, "watch limit reached, polling instead: {e} (raise fs.inotify.max_user_watches to watch)");
            *lib.watch_note.write() = format!(
                "the inotify watch limit was reached at {} folders ({e}): raise fs.inotify.max_user_watches",
                watched.len()
            );
            lib.live.store(false, Ordering::Relaxed);
            drop(watcher);
            return poll_forever(app, lib).await;
        }
        loop {
            let Some(first) = rx.recv().await else { break };
            let mut dirs = HashSet::new();
            let mut overflow = false;
            let mut take = |ev: notify::Result<notify::Event>, dirs: &mut HashSet<String>| match ev
            {
                Ok(ev) => {
                    crate::metrics::Metrics::add(&app.metrics.fs_events, 1);
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
            let named: Vec<String> = dirs.iter().cloned().collect();
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
            // A folder deleted on disk took its watch with it: forgotten, so one made again under its name is
            // watched again (and so is everything that was below it).
            for rel in named {
                if !rel.is_empty() && !lib.fs.stat(&rel).ok().flatten().is_some_and(|s| s.is_dir) {
                    let below = format!("{rel}/");
                    watched.retain(|w| *w != rel && !w.starts_with(&below));
                }
            }
            if per_folder()
                && let Err(e) =
                    watch_new_folders(&app, &lib, &mut watcher, &root, &mut watched).await
            {
                tracing::warn!(library = %lib.api_id, "watch limit reached, polling instead: {e} (raise fs.inotify.max_user_watches to watch)");
                *lib.watch_note.write() = format!(
                    "the inotify watch limit was reached at {} folders ({e}): raise fs.inotify.max_user_watches",
                    watched.len()
                );
                lib.live.store(false, Ordering::Relaxed);
                drop(watcher);
                return poll_forever(app, lib).await;
            }
            lib.watched.store(watched.len() as u64, Ordering::Relaxed);
        }
    });
}

/// Watches the indexed folders not watched yet, then lists the new ones again: files that arrived between a
/// folder's listing and its watch are not missed. Listing may find folders that are new in turn (made in that
/// gap), so it repeats until there are none.
async fn watch_new_folders(
    app: &Arc<App>,
    lib: &Arc<Library>,
    watcher: &mut notify::RecommendedWatcher,
    root: &Path,
    watched: &mut HashSet<String>,
) -> notify::Result<()> {
    for _ in 0..32 {
        let (a, l) = (app.clone(), lib.clone());
        let rels = tokio::task::spawn_blocking(move || indexed_folders(&a, &l))
            .await
            .ok()
            .and_then(|r| r.ok())
            .unwrap_or_default();
        let added = add_watches(watcher, root, watched, rels)?;
        if added.is_empty() {
            break;
        }
        let (a, l) = (app.clone(), lib.clone());
        let _ =
            tokio::task::spawn_blocking(move || sync_paths(&a, &l, added.into_iter().collect()))
                .await;
    }
    Ok(())
}

/// Polls the library every `poll_seconds`, for good.
async fn poll_forever(app: Arc<App>, lib: Arc<Library>) {
    let every = Duration::from_secs(lib.cfg.poll_seconds.max(5));
    loop {
        tokio::time::sleep(every).await;
        let (a, l) = (app.clone(), lib.clone());
        if let Ok(Err(e)) = tokio::task::spawn_blocking(move || poll(&a, &l)).await {
            tracing::warn!(library = %lib.api_id, "poll failed: {e:#}");
        }
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
            media::add_video_facts(&mut facts, app.previews.bg_pool(), f);
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
/// `preview_done`: not made yet / made (or not previewable) / failed for a reason that may pass (a worker that
/// crashed, a file that could not be opened): left alone until the next start, which retries it.
pub const PREVIEW_PENDING: i64 = 0;
pub const PREVIEW_DONE: i64 = 1;
pub const PREVIEW_DEFERRED: i64 = 2;

/// Failures logged one by one before only every hundredth is (a broken setup fails every photo).
const FAILURES_LOGGED: u64 = 20;
static PREVIEW_FAILURES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn log_preview_failure(lib: &str, path: &str, e: &anyhow::Error, permanent: bool) {
    let n = PREVIEW_FAILURES.fetch_add(1, Ordering::Relaxed) + 1;
    let what = if permanent {
        "cannot be previewed"
    } else {
        "preview failed, retried after a restart"
    };
    if n <= FAILURES_LOGGED {
        tracing::warn!(library = %lib, path = %path, "{what}: {e:#}");
        if n == FAILURES_LOGGED {
            tracing::warn!(
                "further preview failures are logged one in a hundred (debug level logs each)"
            );
        }
    } else if n.is_multiple_of(100) {
        tracing::warn!(library = %lib, path = %path, failures = n, "{what}: {e:#}");
    } else {
        tracing::debug!(library = %lib, path = %path, "{what}: {e:#}");
    }
}

static HASH_FAILURES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn log_hash_failure(lib: &str, path: &str, why: &str) {
    let n = HASH_FAILURES.fetch_add(1, Ordering::Relaxed) + 1;
    if n <= FAILURES_LOGGED || n.is_multiple_of(100) {
        tracing::warn!(library = %lib, path = %path, failures = n, "cannot hash, retried after a restart: {why}");
    } else {
        tracing::debug!(library = %lib, path = %path, "cannot hash: {why}");
    }
}

async fn preview_pass(app: &Arc<App>) -> anyhow::Result<usize> {
    if !app.config.cache.pregenerate {
        return Ok(0);
    }
    // While the API is in use, one at a time (its workers already yield the CPU to requests'); a full batch
    // otherwise.
    let batch = if app.api_busy() { 1 } else { 16 };
    let rows: Vec<EntryRow> = app.db.read(|c| {
        let mut st = c.prepare(&format!(
            "SELECT {} FROM {} WHERE e.previewable = 1 AND e.preview_done = 0 AND e.trashed_at IS NULL AND e.missing = 0
             ORDER BY e.dev, e.ino LIMIT {batch}",
            db::ENTRY_COLS,
            db::ENTRY_FROM
        ))?;
        let rows = st.query_map([], EntryRow::from_row)?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })?;
    if rows.is_empty() {
        // Nothing to do, so no write: every write wakes the background loop, which would then never sleep.
        return Ok(0);
    }
    let n = rows.len();
    let jobs = rows.into_iter().filter_map(|row| {
        let lib = app.library(&row.library)?.clone();
        let app = app.clone();
        Some(async move {
            let src = preview::Source::File(lib.fs.clone(), row.path.clone());
            let outcome = match app.previews.ensure_cached(&row, src).await {
                Ok(_) => PREVIEW_DONE,
                Err(e) => {
                    let permanent = crate::decode::is_decode_failure(&e);
                    log_preview_failure(&row.library, &row.path, &e, permanent);
                    if permanent { -1 } else { PREVIEW_DEFERRED }
                }
            };
            (row, outcome)
        })
    });
    let results = futures::future::join_all(jobs).await;
    // The previews must be on disk before the index says so: otherwise a stop in between loses them for good.
    let disk = app.previews.disk().clone();
    tokio::task::spawn_blocking(move || disk.flush()).await??;
    app.write(|t| {
        for (row, outcome) in &results {
            match *outcome {
                -1 => {
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
                state => {
                    t.execute(
                        "UPDATE entries SET preview_done = ? WHERE rowid = ?",
                        params![state, row.rowid],
                    )?;
                }
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
            "SELECT {} FROM {} WHERE e.sha256 IS NULL AND e.hash_deferred = 0 AND e.kind != 0 AND e.trashed_at IS NULL
             AND e.missing = 0 ORDER BY e.dev, e.ino LIMIT 8",
            db::ENTRY_COLS,
            db::ENTRY_FROM
        ))?;
        let rows = st.query_map([], EntryRow::from_row)?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })?;
    let mut n = 0;
    for row in &rows {
        if app.api_busy() {
            break; // the rest after the API goes idle
        }
        // A file that cannot be hashed is set aside until the next start: picked again at once, it would be
        // the same few files every pass, forever, and nothing else would be hashed.
        let failed = match hash_entry(app, row) {
            Ok(Some(_)) => None,
            Ok(None) => Some("it changed while being read".to_string()),
            Err(e) => Some(format!("{e:#}")),
        };
        if let Some(why) = failed {
            log_hash_failure(&row.library, &row.path, &why);
            app.write(|t| {
                t.execute(
                    "UPDATE entries SET hash_deferred = 1 WHERE rowid = ?",
                    [row.rowid],
                )?;
                Ok(())
            })?;
        }
        n += 1;
    }
    Ok(n)
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
    if let Err(e) = retry_failed_previews(&app) {
        tracing::warn!("resetting failed previews: {e:#}");
    }
    tokio::spawn(redo_rotated_heic(app.clone()));
    tokio::spawn(facts_loop(app.clone()));
    tokio::spawn(hash_loop(app.clone()));
    let mut changes = app.seq.subscribe();
    // The index write the progress was last counted after: counting reads the whole library, so it waits for a
    // write that may have changed it.
    let mut counted = None;
    loop {
        if paused(&app) {
            tokio::time::sleep(PAUSE_CHECK).await;
            continue;
        }
        let writes = app.writes();
        if counted != Some(writes) && count_progress(&app) {
            counted = Some(writes);
        }
        // Previews here; facts and hashes in loops of their own beside this one (facts_loop, hash_loop): previews
        // are CPU work, hashing is disk work and reading facts is light, so none waits for another to finish.
        let previews = match preview_pass(&app).await {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!("preview pass: {e:#}");
                0
            }
        };
        if previews == 0 {
            // New facts make new previews to do: the change sequence moves with every one stored.
            tokio::select! {
                _ = app.work.notified() => {}
                _ = changes.changed() => {}
                _ = tokio::time::sleep(Duration::from_secs(30)) => {}
            }
        }
    }
}

/// Counts each library's progress for `GET /v1/libraries` (a scan reports its own). Returns whether every
/// library was counted.
fn count_progress(app: &App) -> bool {
    let mut all = true;
    for lib in &app.libraries {
        if lib
            .status
            .get()
            .is_some_and(|s| s.state == IndexingState::Scanning)
        {
            all = false; // counted again once the scan is done
            continue;
        }
        if let Ok((facts, hashes)) = pending_counts(app, &lib.api_id) {
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
        } else {
            all = false;
        }
    }
    all
}

/// Once per index: HEIC previews made through ffmpeg 7, which does not apply a HEIF's rotation and mirroring,
/// came out as stored, sideways or flipped. Every HEIC that has either gets its previews made again, under new
/// URLs (browsers keep a preview URL's picture for good), and a change saying so.
pub async fn redo_rotated_heic(app: Arc<App>) {
    const DONE: &str = "heic_rotation_v1";
    if app.db.meta_get(DONE).ok().flatten().is_some() {
        return;
    }
    let a = app.clone();
    let found = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<EntryRow>> {
        let rows: Vec<EntryRow> = a.db.read(|c| {
            let mut st = c.prepare(&format!(
                "SELECT {} FROM {} WHERE e.mime IN ('image/heic', 'image/heif') AND e.trashed_at IS NULL AND e.missing = 0",
                db::ENTRY_COLS,
                db::ENTRY_FROM
            ))?;
            let rows = st.query_map([], EntryRow::from_row)?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        // The boxes that say so are at the start of the file.
        Ok(rows
            .into_iter()
            .filter(|r| {
                a.library(&r.library)
                    .and_then(|l| l.fs.read_head(&r.path, 256 << 10).ok())
                    .is_some_and(|head| crate::preview::render::heif_orientation(&head) != 1)
            })
            .collect())
    })
    .await;
    let rows = match found {
        Ok(Ok(rows)) => rows,
        Ok(Err(e)) => return tracing::warn!("looking for rotated HEICs: {e:#}"),
        Err(e) => return tracing::warn!("looking for rotated HEICs: {e}"),
    };
    for row in &rows {
        if let Err(e) = app.previews.forget(row).await {
            tracing::debug!(entry = %row.id, "forgetting previews: {e:#}");
        }
    }
    let done = app.write(|t| {
        for row in &rows {
            t.execute(
                "UPDATE entries SET preview_done = 0, preview_gen = preview_gen + 1 WHERE rowid = ?",
                [row.rowid],
            )?;
            db::record_change(t, ChangeType::Updated, &row.id, &row.library, row.parent_id.as_deref())?;
        }
        Ok(())
    });
    match done.and_then(|()| app.db.meta_set(DONE, b"1")) {
        Ok(()) if !rows.is_empty() => tracing::info!(
            photos = rows.len(),
            "making the previews of rotated HEICs again (they came out unrotated)"
        ),
        Ok(()) => {}
        Err(e) => tracing::warn!("redoing rotated HEIC previews: {e:#}"),
    }
}

/// How often paused background work looks whether it may resume.
const PAUSE_CHECK: Duration = Duration::from_secs(5);

/// Whether background work (reading files, previews, hashing) is paused: `wirephoto pause` / `resume`, which
/// another process runs, so it is a flag in the index. Requests, scans and changes on disk carry on.
pub fn paused(app: &App) -> bool {
    paused_in(&app.db)
}

pub fn paused_in(db: &crate::db::Db) -> bool {
    db.meta_get("paused")
        .ok()
        .flatten()
        .is_some_and(|v| v == b"1")
}

/// Pauses or resumes background work (`wirephoto pause` / `resume`).
pub fn set_paused(db: &crate::db::Db, paused: bool) -> anyhow::Result<()> {
    db.meta_set("paused", if paused { b"1" } else { b"0" })
}

/// Reads media facts as fast as the disk allows, beside the preview loop.
async fn facts_loop(app: Arc<App>) {
    let mut changes = app.seq.subscribe();
    loop {
        if paused(&app) {
            tokio::time::sleep(PAUSE_CHECK).await;
            continue;
        }
        let a = app.clone();
        let n = match tokio::task::spawn_blocking(move || media_pass(&a)).await {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => {
                tracing::warn!("media pass: {e:#}");
                0
            }
            Err(_) => 0,
        };
        if n == 0 {
            tokio::select! {
                _ = changes.changed() => {}
                _ = tokio::time::sleep(Duration::from_secs(30)) => {}
            }
        }
    }
}

/// Hashes files beside the other loops: whole-file reads, so it waits while the API is in use (they compete
/// with downloads for the disk) and otherwise runs alongside the previews, which need the CPU instead.
async fn hash_loop(app: Arc<App>) {
    let mut changes = app.seq.subscribe();
    loop {
        if paused(&app) {
            tokio::time::sleep(PAUSE_CHECK).await;
            continue;
        }
        app.yield_to_requests().await;
        let a = app.clone();
        let n = match tokio::task::spawn_blocking(move || hash_pass(&a)).await {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => {
                tracing::warn!("hash pass: {e:#}");
                0
            }
            Err(_) => 0,
        };
        if n == 0 && !app.api_busy() {
            tokio::select! {
                _ = changes.changed() => {}
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
        let a = app.clone();
        let _ = tokio::task::spawn_blocking(move || {
            let _ = disk.flush();
            if evict
                && let Ok(n) = disk.evict()
                && n > 0
            {
                crate::metrics::Metrics::add(&a.metrics.evicted_bytes, n);
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
                if let Err(e) = crate::audit::prune(&a, a.config.log.audit_days) {
                    tracing::warn!("pruning the audit trail: {e:#}");
                }
                match crate::tus::purge(&a) {
                    Ok(n) if n > 0 => tracing::info!(uploads = n, "expired uploads removed"),
                    Ok(_) => {}
                    Err(e) => tracing::warn!("removing expired uploads: {e:#}"),
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

/// At start: previews that failed for a reason that may have passed are tried again. Once per index, also the
/// photos an older server marked unpreviewable after ANY failure (a crashed worker, a file it could not open
/// for want of permission): their facts are read again and their previews retried.
pub fn retry_failed_previews(app: &App) -> anyhow::Result<()> {
    let first = app.db.meta_get("previews_retry_v1")?.is_none();
    let (deferred, rechecked) = app.write(|t| {
        let deferred = t.execute(
            "UPDATE entries SET preview_done = ? WHERE preview_done = ?",
            params![PREVIEW_PENDING, PREVIEW_DEFERRED],
        )? + t.execute("UPDATE entries SET hash_deferred = 0 WHERE hash_deferred = 1", [])?;
        let rechecked = if first {
            t.execute(
                "UPDATE entries SET media_state = 0, preview_done = 0 WHERE kind IN (1, 2, 3) AND previewable = 0",
                [],
            )?
        } else {
            0
        };
        Ok((deferred, rechecked))
    })?;
    if first {
        app.db.meta_set("previews_retry_v1", b"1")?;
    }
    if deferred + rechecked > 0 {
        tracing::info!(deferred, rechecked, "retrying previews that failed before");
    }
    Ok(())
}

/// Logs each library's progress every 30 seconds while there is work, and once when it is done: what the
/// server is doing, how fast, and how long it has left.
pub async fn report(app: Arc<App>) {
    #[derive(Default, Clone, Copy)]
    struct Counts {
        files: u64,
        facts_pending: u64,
        previews_pending: u64,
        previews_done: u64,
        deferred: u64,
        not_previewable: u64,
        hashes_pending: u64,
        hash_failed: u64,
    }
    const EVERY: Duration = Duration::from_secs(30);
    let mut last: HashMap<String, (Counts, bool, u64)> = HashMap::new();
    let pregenerate = app.config.cache.pregenerate;
    let started = crate::util::now_secs();
    let mut was_paused: HashSet<String> = HashSet::new();
    // Counting reads the whole library: done again only after an index write (App::writes).
    let mut counted_after: Option<u64> = None;
    // The status last saved (without its time): saved again only when it changes, so an idle server leaves
    // the disks alone.
    let mut saved: Option<Vec<u8>> = None;
    // What each library holds, and the cache by size: recounted, like the progress, only after a change.
    let mut contents: HashMap<String, Contents> = HashMap::new();
    let mut cache_sizes: (Option<u64>, Vec<CacheSize>) = (None, Vec::new());
    loop {
        tokio::time::sleep(EVERY).await;
        let writes = app.writes();
        let recount = counted_after != Some(writes);
        counted_after = Some(writes);
        if recount {
            for lib in &app.libraries {
                match library_contents(&app, &lib.api_id) {
                    Ok(c) => {
                        contents.insert(lib.api_id.clone(), c);
                    }
                    Err(e) => tracing::debug!("counting {}: {e:#}", lib.api_id),
                }
            }
        }
        let disk = app.previews.disk().clone();
        if cache_sizes.0 != Some(disk.changes()) {
            let changes = disk.changes();
            if let Ok(Ok(v)) = tokio::task::spawn_blocking(move || disk.by_size()).await {
                cache_sizes = (
                    Some(changes),
                    v.into_iter()
                        .map(|(size, count, bytes)| CacheSize { size, count, bytes })
                        .collect(),
                );
            }
        }
        let mut lines: Vec<(String, String)> = Vec::new();
        let mut stages: HashMap<String, Stages> = HashMap::new();
        for lib in &app.libraries {
            let id = lib.api_id.clone();
            let scanning = lib
                .status
                .get()
                .filter(|s| s.state == IndexingState::Scanning);
            let (prev, was_busy, prev_listed) = last.get(&id).copied().unwrap_or_default();
            if let Some(s) = scanning {
                let rate = s.done.saturating_sub(prev_listed) as f64 / EVERY.as_secs_f64();
                let line = format!(
                    "scanning folders, {} entries listed ({}/s)",
                    thousands(s.done),
                    rate.round()
                );
                tracing::info!("{id}: {line}");
                lines.push((id.clone(), line));
                stages.insert(
                    id.clone(),
                    Stages {
                        state: "scanning".into(),
                        listed: s.done,
                        list_rate: rate,
                        ..Stages::default()
                    },
                );
                last.insert(id, (prev, true, s.done));
                continue;
            }
            let lib_id = id.clone();
            let counts = match last.get(&id) {
                Some((prev, ..)) if !recount => Ok(*prev),
                _ => app.db.read(move |c| {
                Ok(c.query_row(
                    "SELECT COALESCE(SUM(kind != 0), 0),
                            COALESCE(SUM(kind != 0 AND media_state = 0), 0),
                            COALESCE(SUM(previewable = 1 AND preview_done = 0), 0),
                            COALESCE(SUM(previewable = 1 AND preview_done = 1), 0),
                            COALESCE(SUM(preview_done = 2), 0),
                            COALESCE(SUM(kind IN (1, 2, 3) AND media_state != 0 AND previewable = 0), 0),
                            COALESCE(SUM(kind != 0 AND sha256 IS NULL AND hash_deferred = 0), 0),
                            COALESCE(SUM(kind != 0 AND sha256 IS NULL AND hash_deferred = 1), 0)
                     FROM entries WHERE library = ? AND trashed_at IS NULL AND missing = 0",
                    [&lib_id],
                    |r| {
                        Ok(Counts {
                            files: r.get::<_, i64>(0)? as u64,
                            facts_pending: r.get::<_, i64>(1)? as u64,
                            previews_pending: r.get::<_, i64>(2)? as u64,
                            previews_done: r.get::<_, i64>(3)? as u64,
                            deferred: r.get::<_, i64>(4)? as u64,
                            not_previewable: r.get::<_, i64>(5)? as u64,
                            hashes_pending: r.get::<_, i64>(6)? as u64,
                            hash_failed: r.get::<_, i64>(7)? as u64,
                        })
                    },
                )?)
            }),
            };
            let Ok(now) = counts else { continue };
            let previews_pending = if pregenerate { now.previews_pending } else { 0 };
            let busy = now.facts_pending + previews_pending + now.hashes_pending > 0;
            let per_sec = |done: u64, before: u64| {
                if was_busy {
                    done.saturating_sub(before) as f64 / EVERY.as_secs_f64()
                } else {
                    0.0
                }
            };
            stages.insert(
                id.clone(),
                Stages {
                    state: if busy && paused(&app) {
                        "paused"
                    } else if busy {
                        "working"
                    } else {
                        "up to date"
                    }
                    .into(),
                    files: now.files,
                    facts_pending: now.facts_pending,
                    facts_rate: per_sec(
                        now.files - now.facts_pending,
                        prev.files - prev.facts_pending,
                    ),
                    previews_done: now.previews_done,
                    previews_pending,
                    previews_failed: now.deferred,
                    not_previewable: now.not_previewable,
                    previews_rate: per_sec(now.previews_done, prev.previews_done),
                    hashes_pending: now.hashes_pending,
                    hash_failed: now.hash_failed,
                    hashes_rate: per_sec(
                        now.files - now.hashes_pending,
                        prev.files - prev.hashes_pending,
                    ),
                    pregenerate,
                    ..Stages::default()
                },
            );
            // Speed from what got DONE since the last line, not from what is left: new files keep arriving
            // in a stage's queue while it works (reading files feeds previews), so "left" can stand still.
            let stage =
                |name: &str, pending: u64, total: u64, done_before: u64| -> Option<String> {
                    if pending == 0 {
                        return None;
                    }
                    let done = total - pending;
                    let rate = done.saturating_sub(done_before) as f64 / EVERY.as_secs_f64();
                    let left = if rate > 0.0 && was_busy {
                        format!(", {} left", duration_words(pending as f64 / rate))
                    } else {
                        String::new()
                    };
                    let speed = match (was_busy, rate > 0.0) {
                        (true, true) => format!(" ({rate:.1}/s{left})"),
                        // Hashing waits for facts and previews; anything not moving is queued behind them.
                        (true, false) => " (waiting)".to_string(),
                        _ => String::new(),
                    };
                    Some(format!(
                        "{name} {} / {}{speed}",
                        thousands(total - pending),
                        thousands(total)
                    ))
                };
            if busy && paused(&app) {
                let line = format!(
                    "paused (wirephoto resume continues): {} files to read, {} previews to make, {} to hash",
                    thousands(now.facts_pending),
                    thousands(previews_pending),
                    thousands(now.hashes_pending)
                );
                if !was_paused.contains(&id) {
                    tracing::info!("{id}: {line}");
                    was_paused.insert(id.clone());
                }
                lines.push((id.clone(), line));
                last.insert(id, (now, false, 0));
                continue;
            }
            was_paused.remove(&id);
            if busy {
                let parts: Vec<String> = [
                    stage(
                        "reading files",
                        now.facts_pending,
                        now.files,
                        prev.files - prev.facts_pending,
                    ),
                    stage(
                        "previews",
                        previews_pending,
                        now.previews_pending + now.previews_done,
                        prev.previews_done,
                    ),
                    stage(
                        "hashing",
                        now.hashes_pending,
                        now.files,
                        prev.files - prev.hashes_pending,
                    ),
                ]
                .into_iter()
                .flatten()
                .collect();
                let mut line = parts.join(" · ");
                if app.api_busy() {
                    line.push_str(" · slowed while the API is in use");
                }
                tracing::info!("{id}: {line}");
                lines.push((id.clone(), line));
            } else {
                let mut deferred = String::new();
                if now.deferred > 0 {
                    deferred.push_str(&format!(
                        ", {} previews failed (retried after a restart)",
                        thousands(now.deferred)
                    ));
                }
                if now.hash_failed > 0 {
                    deferred.push_str(&format!(
                        ", {} files could not be hashed (retried after a restart)",
                        thousands(now.hash_failed)
                    ));
                }
                let line = format!(
                    "up to date: {} files, {} previews, {} not previewable{deferred}",
                    thousands(now.files),
                    thousands(now.previews_done),
                    thousands(now.not_previewable)
                );
                if was_busy || !last.contains_key(&id) {
                    tracing::info!("{id}: {line}");
                }
                lines.push((id.clone(), line));
            }
            last.insert(id, (now, busy, 0));
        }
        let mut snapshot =
            status_snapshot(&app, started, lines, &stages, &contents, &cache_sizes.1);
        let time = std::mem::take(&mut snapshot.time);
        let Ok(unchanged) = serde_json::to_vec(&snapshot) else {
            continue;
        };
        if saved.as_ref() != Some(&unchanged) {
            snapshot.time = time;
            if let Ok(json) = serde_json::to_vec(&snapshot)
                && app.db.meta_set("status", &json).is_ok()
            {
                saved = Some(unchanged);
            }
        }
    }
}

/// What the server is doing, saved every 30 s for `wirephoto status` (another process: it reads the index).
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct Snapshot {
    pub version: String,
    pub started: i64,
    pub time: i64,
    pub libraries: Vec<LibraryStatus>,
    pub times: Vec<crate::preview::stats::TypeTimes>,
    pub served_memory: u64,
    pub served_disk: u64,
    pub rendered: u64,
    pub cache_bytes: u64,
    pub cache_budget: u64,
    pub workers: usize,
    pub background: usize,
    pub workers_started: u64,
    pub workers_lost: u64,
    /// The GPU: what it decodes, or why it is not used.
    #[serde(default)]
    pub gpu: String,
    /// Images decoded on the GPU, and ones it failed on (decoded on the CPU instead).
    #[serde(default)]
    pub gpu_decoded: u64,
    #[serde(default)]
    pub gpu_fell_back: u64,
    /// The server's process, for `wirephoto status` to read its CPU and memory itself.
    #[serde(default)]
    pub pid: u32,
    #[serde(default)]
    pub requests: Vec<crate::metrics::RequestKind>,
    #[serde(default)]
    pub previews_by_size: Vec<crate::preview::stats::SizeServed>,
    #[serde(default)]
    pub cache_sizes: Vec<CacheSize>,
    /// Previews in memory, their bytes, and the memory allowed.
    #[serde(default)]
    pub memory_cache: (u64, u64, u64),
    #[serde(default)]
    pub uploads: UploadCounts,
    #[serde(default)]
    pub disk_watch: DiskWatch,
    /// Previews made from originals, by purpose, type, decoder and size.
    #[serde(default)]
    pub renders: Vec<crate::preview::stats::RenderTimes>,
    /// The last warnings and errors.
    #[serde(default)]
    pub problems: Vec<crate::problems::Problem>,
    #[serde(default)]
    pub ffmpeg: String,
    #[serde(default)]
    pub heic_route: String,
    /// Live change feeds open (apps following along).
    #[serde(default)]
    pub change_feeds: i64,
    #[serde(default)]
    pub evicted_bytes: u64,
    /// Background work is waiting for the API to go idle.
    #[serde(default)]
    pub yielding: bool,
    #[serde(default)]
    pub yield_seconds: u64,
    /// Whether 640 px previews are made ahead of time (`[cache] pregenerate`).
    #[serde(default)]
    pub pregenerate: bool,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct LibraryStatus {
    pub id: String,
    pub name: String,
    pub path: String,
    pub watching: bool,
    pub poll_seconds: u64,
    pub progress: String,
    #[serde(default)]
    pub contents: Option<Contents>,
    /// Each background stage's numbers.
    #[serde(default)]
    pub stages: Option<Stages>,
    /// How changes on disk are noticed, and why; folders with a watch of their own.
    #[serde(default)]
    pub watch_note: String,
    #[serde(default)]
    pub watched_folders: u64,
    #[serde(default)]
    pub last_scan: Option<crate::app::ScanInfo>,
    #[serde(default)]
    pub read_only: bool,
}

/// Where a library's background work stands (`wirephoto status`). Rates are per second over the last 30 s.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Stages {
    /// scanning, working, paused, up to date
    pub state: String,
    /// While scanning: entries listed so far.
    pub listed: u64,
    pub list_rate: f64,
    /// Files (not folders, not in the trash).
    pub files: u64,
    /// Facts (size, orientation, capture time) still to read.
    pub facts_pending: u64,
    pub facts_rate: f64,
    /// The cached 640 px preview: made, still to make, failed for a reason that may pass (retried after a
    /// restart), and files that cannot have one (the decoder rejected them).
    pub previews_done: u64,
    pub previews_pending: u64,
    pub previews_failed: u64,
    pub not_previewable: u64,
    pub previews_rate: f64,
    /// Whether 640 px previews are made ahead of time (`[cache] pregenerate`).
    pub pregenerate: bool,
    pub hashes_pending: u64,
    /// Could not be read (retried after a restart).
    pub hash_failed: u64,
    pub hashes_rate: f64,
}

/// What a library holds (`wirephoto status`).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Contents {
    pub folders: u64,
    pub images: u64,
    pub raws: u64,
    pub videos: u64,
    pub others: u64,
    /// Of every file not in the trash.
    pub bytes: u64,
    pub trash_files: u64,
    pub trash_bytes: u64,
    /// Sets of files with the same bytes, the copies beyond the first, and the space those take.
    pub duplicate_sets: u64,
    pub duplicate_copies: u64,
    pub duplicate_bytes: u64,
}

/// Previews in the disk cache of one size.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct CacheSize {
    pub size: u32,
    pub count: u64,
    pub bytes: u64,
}

/// Upload sessions (`wirephoto status`).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct UploadCounts {
    pub open: u64,
    pub opened: u64,
    pub completed: u64,
    pub abandoned: u64,
    pub expired: u64,
    /// Bytes received for files, single PUTs and chunks, since start.
    pub bytes: u64,
}

/// Changes on disk seen while running (`wirephoto status`).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct DiskWatch {
    pub events: u64,
    pub folders_relisted: u64,
    /// Unix seconds; 0: none yet.
    pub last_change: i64,
}

fn library_contents(app: &App, lib: &str) -> anyhow::Result<Contents> {
    app.db.read(|c| {
        let mut out = c.query_row(
            "SELECT COALESCE(SUM(kind = 0), 0), COALESCE(SUM(kind = 1), 0), COALESCE(SUM(kind = 2), 0),
                    COALESCE(SUM(kind = 3), 0), COALESCE(SUM(kind = 4), 0),
                    COALESCE(SUM(CASE WHEN kind != 0 THEN size ELSE 0 END), 0)
             FROM entries WHERE library = ? AND trashed_at IS NULL AND missing = 0",
            [lib],
            |r| {
                Ok(Contents {
                    folders: r.get::<_, i64>(0)? as u64,
                    images: r.get::<_, i64>(1)? as u64,
                    raws: r.get::<_, i64>(2)? as u64,
                    videos: r.get::<_, i64>(3)? as u64,
                    others: r.get::<_, i64>(4)? as u64,
                    bytes: r.get::<_, i64>(5)? as u64,
                    ..Contents::default()
                })
            },
        )?;
        (out.trash_files, out.trash_bytes) = c.query_row(
            "SELECT COUNT(*), COALESCE(SUM(size), 0) FROM entries
             WHERE library = ? AND trashed_at IS NOT NULL AND kind != 0",
            [lib],
            |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64)),
        )?;
        (out.duplicate_sets, out.duplicate_copies, out.duplicate_bytes) = c.query_row(
            "SELECT COUNT(*), COALESCE(SUM(n - 1), 0), COALESCE(SUM((n - 1) * size), 0) FROM (
                 SELECT COUNT(*) AS n, MAX(size) AS size FROM entries
                 WHERE library = ? AND sha256 IS NOT NULL AND kind != 0 AND trashed_at IS NULL AND missing = 0
                 GROUP BY sha256 HAVING COUNT(*) > 1)",
            [lib],
            |r| {
                Ok((
                    r.get::<_, i64>(0)? as u64,
                    r.get::<_, i64>(1)? as u64,
                    r.get::<_, i64>(2)? as u64,
                ))
            },
        )?;
        Ok(out)
    })
}

fn status_snapshot(
    app: &App,
    started: i64,
    lines: Vec<(String, String)>,
    stages: &HashMap<String, Stages>,
    contents: &HashMap<String, Contents>,
    cache_sizes: &[CacheSize],
) -> Snapshot {
    let p = &app.previews;
    let (served_memory, served_disk) = p.stats.served();
    let (a, b) = (p.pool().stats(), p.bg_pool().stats());
    let (workers_started, workers_lost) = (a.0 + b.0, a.1 + b.1);
    Snapshot {
        version: crate::version(),
        started,
        time: crate::util::now_secs(),
        libraries: app
            .libraries
            .iter()
            .map(|l| LibraryStatus {
                id: l.api_id.clone(),
                name: l.cfg.name.clone(),
                path: l.fs.root.display().to_string(),
                watching: l.live.load(Ordering::Relaxed),
                poll_seconds: l.cfg.poll_seconds,
                progress: lines
                    .iter()
                    .find(|(id, _)| *id == l.api_id)
                    .map(|(_, line)| line.clone())
                    .unwrap_or_default(),
                contents: contents.get(&l.api_id).cloned(),
                stages: stages.get(&l.api_id).cloned(),
                watch_note: l.watch_note.read().clone(),
                watched_folders: l.watched.load(Ordering::Relaxed),
                last_scan: *l.last_scan.read(),
                read_only: l.cfg.read_only,
            })
            .collect(),
        times: p.stats.times(),
        served_memory,
        served_disk,
        rendered: p.renders(),
        cache_bytes: p.disk().total_bytes(),
        cache_budget: app.config.cache.max_size,
        workers: p.slots.0,
        background: p.slots.1,
        workers_started,
        workers_lost,
        gpu: p.pool().gpu().status(),
        gpu_decoded: p.pool().gpu().decoded.load(Ordering::Relaxed),
        gpu_fell_back: p.pool().gpu().fell_back.load(Ordering::Relaxed),
        pid: std::process::id(),
        requests: app.metrics.requests(),
        previews_by_size: p.stats.by_size(),
        cache_sizes: cache_sizes.to_vec(),
        memory_cache: {
            let (n, b) = p.memory();
            (n, b, app.config.cache.memory)
        },
        uploads: {
            use crate::metrics::Metrics;
            let m = &app.metrics;
            UploadCounts {
                open: crate::tus::open_count(app),
                opened: Metrics::get(&m.uploads_opened),
                completed: Metrics::get(&m.uploads_completed),
                abandoned: Metrics::get(&m.uploads_abandoned),
                expired: Metrics::get(&m.uploads_expired),
                bytes: Metrics::get(&m.upload_bytes),
            }
        },
        renders: p.stats.renders(),
        problems: crate::problems::recent(),
        ffmpeg: app.ffmpeg.clone(),
        heic_route: app.heic_route.clone(),
        change_feeds: app.metrics.change_feeds.load(Ordering::Relaxed),
        evicted_bytes: crate::metrics::Metrics::get(&app.metrics.evicted_bytes),
        yielding: app.yielding.load(Ordering::Relaxed),
        yield_seconds: app.config.background.yield_seconds,
        pregenerate: app.config.cache.pregenerate,
        disk_watch: DiskWatch {
            events: crate::metrics::Metrics::get(&app.metrics.fs_events),
            folders_relisted: crate::metrics::Metrics::get(&app.metrics.folders_relisted),
            last_change: app.metrics.last_disk_change.load(Ordering::Relaxed),
        },
    }
}

/// 1234567 -> "1,234,567".
fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Seconds as "about 2 h 05 min", "about 4 min", "under a minute".
fn duration_words(secs: f64) -> String {
    let m = (secs / 60.0).round() as u64;
    match m {
        0 => "under a minute".into(),
        1..=59 => format!("about {m} min"),
        _ => format!("about {} h {:02} min", m / 60, m % 60),
    }
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

#[cfg(test)]
mod tests {
    #[test]
    fn reading_is_not_a_change() {
        use notify::event::{
            AccessKind, AccessMode, CreateKind, DataChange, EventKind, ModifyKind,
        };
        let ev = |kind| Ok(notify::Event::new(kind));
        for kind in [
            EventKind::Access(AccessKind::Open(AccessMode::Any)),
            EventKind::Access(AccessKind::Read),
            EventKind::Access(AccessKind::Close(AccessMode::Read)),
        ] {
            assert!(!super::is_change(&ev(kind)), "{kind:?}");
        }
        for kind in [
            EventKind::Access(AccessKind::Close(AccessMode::Write)),
            EventKind::Create(CreateKind::File),
            EventKind::Modify(ModifyKind::Data(DataChange::Any)),
            EventKind::Remove(notify::event::RemoveKind::Any),
        ] {
            assert!(super::is_change(&ev(kind)), "{kind:?}");
        }
    }

    #[test]
    fn numbers_and_durations() {
        assert_eq!(super::thousands(0), "0");
        assert_eq!(super::thousands(999), "999");
        assert_eq!(super::thousands(1_234_567), "1,234,567");
        assert_eq!(super::duration_words(20.0), "under a minute");
        assert_eq!(super::duration_words(240.0), "about 4 min");
        assert_eq!(super::duration_words(7500.0), "about 2 h 05 min");
    }
}
