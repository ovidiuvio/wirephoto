//! Writing (api.md section 6): new files, replacing bytes, folders and copies, rename/move/restore, trash and
//! delete. Every change to a library holds its lock, so the indexer never sees half of one.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use rusqlite::params;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use wirephoto_api::{ChangeType, CreateEntry, Kind};

use crate::app::{App, Library};
use crate::db::{self, EntryRow, NewEntry};
use crate::fsx::{self, TRASH_DIR};
use crate::http::auth::Caller;
use crate::http::entries::{entry_response, header_tag, load, to_api};
use crate::http::error::{ApiError, ApiResult};
use crate::{indexer, media, preview, util};

/// Uploads up to this size are kept in memory too, so facts and the cached preview come from the bytes in
/// flight instead of a read back from disk.
const KEEP_IN_MEMORY: usize = 256 << 20;

fn writable(app: &App, key: &crate::auth::KeyRow, row: &EntryRow) -> ApiResult<Arc<Library>> {
    if key.scope != wirephoto_api::Scope::Write {
        return Err(ApiError::forbidden("this key is read-only"));
    }
    let lib = app
        .library(&row.library)
        .ok_or_else(ApiError::not_found)?
        .clone();
    if lib.cfg.read_only {
        return Err(ApiError::forbidden("the library is read-only"));
    }
    Ok(lib)
}

fn check_if_match(headers: &HeaderMap, row: &EntryRow) -> ApiResult<()> {
    if let Some(tag) = header_tag(headers, header::IF_MATCH) {
        let current = row.etag();
        if tag != "*" && tag != current {
            return Err(ApiError::version_mismatch(format!(
                "If-Match {tag}, current {current}"
            )));
        }
    }
    Ok(())
}

/// `Content-Digest: sha-256=:<base64>:` -> the 32 bytes.
fn content_digest(headers: &HeaderMap) -> ApiResult<Option<Vec<u8>>> {
    let Some(v) = headers.get("content-digest") else {
        return Ok(None);
    };
    let v = v
        .to_str()
        .map_err(|_| ApiError::invalid("bad Content-Digest"))?;
    for part in v.split(',') {
        if let Some((alg, val)) = part.trim().split_once('=')
            && alg.trim().eq_ignore_ascii_case("sha-256")
        {
            let b64 = val.trim().trim_matches(':');
            return util::b64_decode(b64)
                .filter(|b| b.len() == 32)
                .map(Some)
                .ok_or_else(|| ApiError::invalid("bad sha-256 Content-Digest"));
        }
    }
    Ok(None)
}

/// Streams a request body into a new temporary file next to `final_rel`: (temp path, hash, bytes if small).
async fn receive(
    lib: &Library,
    final_rel: &str,
    body: Body,
) -> ApiResult<(String, [u8; 32], Option<Vec<u8>>)> {
    let fs = lib.fs.clone();
    let fr = final_rel.to_string();
    let (tmp, std_file) = tokio::task::spawn_blocking(move || fs.create_temp(&fr)).await??;
    let mut file = tokio::fs::File::from_std(std_file);
    let mut hasher = Sha256::new();
    let mut kept: Option<Vec<u8>> = Some(Vec::new());
    let mut stream = body.into_data_stream();
    let result: ApiResult<()> = async {
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| ApiError::invalid(format!("upload interrupted: {e}")))?;
            hasher.update(&chunk);
            if let Some(k) = &mut kept {
                if k.len() + chunk.len() <= KEEP_IN_MEMORY {
                    k.extend_from_slice(&chunk);
                } else {
                    kept = None;
                }
            }
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        file.sync_all().await?;
        Ok(())
    }
    .await;
    if let Err(e) = result {
        let _ = lib.fs.remove_file(&tmp);
        return Err(e);
    }
    Ok((tmp, hasher.finalize().into(), kept))
}

/// Facts for a file just written: from the bytes in memory when we have them (a video is probed from the file).
fn facts_for(
    app: &App,
    lib: &Library,
    rel: &str,
    name: &str,
    kept: Option<&Vec<u8>>,
) -> Option<media::Facts> {
    let mut facts = match kept {
        Some(b) => media::read_facts(name, &b[..]).ok(),
        None => lib
            .fs
            .open_file(rel)
            .ok()
            .and_then(|f| media::read_facts(name, &f).ok()),
    }?;
    if facts.kind == Some(Kind::Video)
        && let Ok(f) = lib.fs.open_file(rel)
    {
        media::add_video_facts(&mut facts, app.previews.pool(), f);
    }
    Some(facts)
}

/// Renders the cached preview in the background, from memory when possible.
fn pregenerate(app: &Arc<App>, entry_id: String, src: preview::Source) {
    let app = app.clone();
    tokio::spawn(async move {
        let row = app
            .db
            .read(|c| Ok(db::entry_by_id(c, &entry_id)?))
            .ok()
            .flatten();
        if let Some(row) = row.filter(|r| r.previewable) {
            let ok = app.previews.ensure_cached(&row, src).await.is_ok();
            let _ = app.db.write(|t| {
                t.execute("UPDATE entries SET preview_done = 1, previewable = previewable AND ? WHERE rowid = ?", params![ok, row.rowid])?;
                Ok(())
            });
        }
    });
}

/// `PUT /v1/entries/{folder}/children/{name}`
pub async fn put_child(
    State(app): State<Arc<App>>,
    Caller(key): Caller,
    Path((folder, name)): Path<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Response> {
    if header_tag(&headers, header::IF_NONE_MATCH).as_deref() != Some("*") {
        return Err(ApiError::version_required().detail("creating a file needs If-None-Match: *"));
    }
    if !fsx::valid_name(&name) {
        return Err(ApiError::invalid("not a valid file name"));
    }
    let digest = content_digest(&headers)?;
    let a = app.clone();
    let k = key.clone();
    let parent =
        tokio::task::spawn_blocking(move || a.db.read(|c| Ok(load(c, &k, &folder)))).await???;
    if parent.kind != Kind::Folder || parent.is_trashed() {
        return Err(ApiError::invalid("parent is not a folder"));
    }
    let lib = writable(&app, &key, &parent)?;
    let rel = fsx::join(&parent.path, &name);
    let taken = |lib: &Library| lib.fs.exists(&rel);
    if taken(&lib) {
        return Err(ApiError::version_mismatch("the name is taken"));
    }

    let (tmp, sha, kept) = receive(&lib, &rel, body).await?;
    if digest.is_some_and(|d| d != sha) {
        let _ = lib.fs.remove_file(&tmp);
        return Err(ApiError::digest_mismatch());
    }
    let a = app.clone();
    let lib2 = lib.clone();
    let kept = kept.map(Arc::new);
    let kept2 = kept.clone();
    let (entry, id) = tokio::task::spawn_blocking(move || -> ApiResult<_> {
        let _guard = lib2.lock.blocking_lock();
        match lib2.fs.publish_new(&tmp, &rel) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(ApiError::version_mismatch("the name is taken"));
            }
            Err(e) => return Err(e.into()),
        }
        lib2.fs.fsync_dir(&parent.path);
        let st = lib2.fs.stat(&rel)?.ok_or_else(ApiError::internal)?;
        let facts = facts_for(&a, &lib2, &rel, &name, kept2.as_deref());
        let row = a.write(|t| {
            // The indexer may have seen the file already; take its row over.
            if let Some(existing) = db::entry_by_path(t, &lib2.api_id, &rel)? {
                db::delete_tree(t, &existing)?;
            }
            let (k, mime) = crate::kind::guess(&name);
            let (rowid, id) = db::insert_entry(
                t,
                &NewEntry {
                    library: &lib2.api_id,
                    parent: Some(parent.rowid),
                    name: &name,
                    path: &rel,
                    kind: k,
                    mime,
                    size: st.size,
                    mtime_ns: st.mtime_ns,
                    dev: st.dev,
                    ino: st.ino,
                },
            )?;
            t.execute(
                "UPDATE entries SET sha256 = ? WHERE rowid = ?",
                params![&sha[..], rowid],
            )?;
            let row = db::entry_by_rowid(t, rowid)?.ok_or_else(|| anyhow::anyhow!("vanished"))?;
            indexer::store_facts(t, &row, facts.as_ref(), false)?;
            db::record_change(t, ChangeType::Created, &id, &lib2.api_id, Some(&parent.id))?;
            db::entry_by_rowid(t, rowid)?.ok_or_else(|| anyhow::anyhow!("vanished"))
        })?;
        let entry = a.db.read(|c| Ok(to_api(&a, c, &row, &key)?))?;
        Ok((entry, row.id))
    })
    .await??;
    // Bytes in memory reach a decoder through a pipe, which ffmpeg cannot seek: videos are read from the file.
    let src = match kept {
        Some(b) if entry.kind != Kind::Video => preview::Source::Bytes(b),
        _ => preview::Source::File(
            lib.fs.clone(),
            entry.path.trim_start_matches('/').to_string(),
        ),
    };
    pregenerate(&app, id, src);
    let location = format!("/v1/entries/{}", entry.id);
    let mut r = entry_response(entry);
    *r.status_mut() = StatusCode::CREATED;
    r.headers_mut()
        .insert(header::LOCATION, location.parse().unwrap());
    Ok(r)
}

/// `PUT /v1/entries/{id}/content`
pub async fn put_content(
    State(app): State<Arc<App>>,
    Caller(key): Caller,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Response> {
    let Some(tag) = header_tag(&headers, header::IF_MATCH) else {
        return Err(ApiError::version_required().detail("replacing a file needs If-Match"));
    };
    let digest = content_digest(&headers)?;
    let a = app.clone();
    let k = key.clone();
    let row = tokio::task::spawn_blocking(move || a.db.read(|c| Ok(load(c, &k, &id)))).await???;
    if row.kind == Kind::Folder || row.is_trashed() {
        return Err(ApiError::invalid("not a file"));
    }
    let lib = writable(&app, &key, &row)?;
    if tag != row.etag() {
        return Err(ApiError::version_mismatch(format!(
            "If-Match {tag}, current {}",
            row.etag()
        )));
    }
    let (tmp, sha, kept) = receive(&lib, &row.path, body).await?;
    if digest.is_some_and(|d| d != sha) {
        let _ = lib.fs.remove_file(&tmp);
        return Err(ApiError::digest_mismatch());
    }
    let a = app.clone();
    let lib2 = lib.clone();
    let kept = kept.map(Arc::new);
    let kept2 = kept.clone();
    let entry = tokio::task::spawn_blocking(move || -> ApiResult<_> {
        let _guard = lib2.lock.blocking_lock();
        // Check again under the lock: someone may have replaced it meanwhile.
        let current =
            a.db.read(|c| Ok(db::entry_by_rowid(c, row.rowid)?))?
                .ok_or_else(ApiError::not_found)?;
        if current.etag() != tag {
            let _ = lib2.fs.remove_file(&tmp);
            return Err(ApiError::version_mismatch(format!(
                "If-Match {tag}, current {}",
                current.etag()
            )));
        }
        lib2.fs.publish_replace(&tmp, &current.path)?;
        let st = lib2
            .fs
            .stat(&current.path)?
            .ok_or_else(ApiError::internal)?;
        let facts = facts_for(&a, &lib2, &current.path, &current.name, kept2.as_deref());
        let row = a.write(|t| {
            db::update_file_stat(t, current.rowid, st.size, st.mtime_ns, st.dev, st.ino)?;
            t.execute(
                "UPDATE entries SET sha256 = ? WHERE rowid = ?",
                params![&sha[..], current.rowid],
            )?;
            let row =
                db::entry_by_rowid(t, current.rowid)?.ok_or_else(|| anyhow::anyhow!("vanished"))?;
            indexer::store_facts(t, &row, facts.as_ref(), true)?;
            db::entry_by_rowid(t, current.rowid)?.ok_or_else(|| anyhow::anyhow!("vanished"))
        })?;
        Ok(a.db.read(|c| Ok(to_api(&a, c, &row, &key)?))?)
    })
    .await??;
    // Bytes in memory reach a decoder through a pipe, which ffmpeg cannot seek: videos are read from the file.
    let src = match kept {
        Some(b) if entry.kind != Kind::Video => preview::Source::Bytes(b),
        _ => preview::Source::File(
            lib.fs.clone(),
            entry.path.trim_start_matches('/').to_string(),
        ),
    };
    pregenerate(&app, entry.id.clone(), src);
    Ok(entry_response(entry))
}

/// `POST /v1/entries`: a folder or a copy.
pub async fn post_entry(
    State(app): State<Arc<App>>,
    Caller(key): Caller,
    body: axum::body::Bytes,
) -> ApiResult<Response> {
    let req: CreateEntry =
        serde_json::from_slice(&body).map_err(|e| ApiError::invalid(e.to_string()))?;
    let a = app.clone();
    let entry = tokio::task::spawn_blocking(move || create(&a, &key, req)).await??;
    let location = format!("/v1/entries/{}", entry.id);
    let mut r = entry_response(entry);
    *r.status_mut() = StatusCode::CREATED;
    r.headers_mut()
        .insert(header::LOCATION, location.parse().unwrap());
    Ok(r)
}

fn create(
    app: &Arc<App>,
    key: &crate::auth::KeyRow,
    req: CreateEntry,
) -> ApiResult<wirephoto_api::Entry> {
    let parent = app.db.read(|c| Ok(load(c, key, &req.parent)))??;
    if parent.kind != Kind::Folder || parent.is_trashed() {
        return Err(ApiError::invalid("parent is not a folder"));
    }
    let lib = writable(app, key, &parent)?;
    let parent_id = parent.id.clone();
    let row = match (req.kind, &req.copy_of) {
        (Some(Kind::Folder), None) => {
            let name = req
                .name
                .clone()
                .ok_or_else(|| ApiError::invalid("a folder needs a name"))?;
            if !fsx::valid_name(&name) {
                return Err(ApiError::invalid("not a valid folder name"));
            }
            let rel = fsx::join(&parent.path, &name);
            let _guard = lib.lock.blocking_lock();
            match lib.fs.create_dir(&rel) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    return Err(ApiError::conflict("the name is taken"));
                }
                Err(e) => return Err(e.into()),
            }
            let st = lib.fs.stat(&rel)?.ok_or_else(ApiError::internal)?;
            app.write(|t| {
                if let Some(existing) = db::entry_by_path(t, &lib.api_id, &rel)? {
                    db::delete_tree(t, &existing)?;
                }
                let (rowid, id) = db::insert_entry(
                    t,
                    &NewEntry {
                        library: &lib.api_id,
                        parent: Some(parent.rowid),
                        name: &name,
                        path: &rel,
                        kind: Kind::Folder,
                        mime: None,
                        size: 0,
                        mtime_ns: st.mtime_ns,
                        dev: st.dev,
                        ino: st.ino,
                    },
                )?;
                db::record_change(t, ChangeType::Created, &id, &lib.api_id, Some(&parent_id))?;
                db::entry_by_rowid(t, rowid)?.ok_or_else(|| anyhow::anyhow!("vanished"))
            })?
        }
        (None, Some(src_id)) => {
            let src = app.db.read(|c| Ok(load(c, key, src_id)))??;
            if src.is_trashed() || src.path.is_empty() {
                return Err(ApiError::invalid("that entry cannot be copied"));
            }
            let src_lib = app
                .library(&src.library)
                .ok_or_else(ApiError::not_found)?
                .clone();
            let name = req.name.clone().unwrap_or_else(|| src.name.clone());
            if !fsx::valid_name(&name) {
                return Err(ApiError::invalid("not a valid name"));
            }
            let rel = fsx::join(&parent.path, &name);
            if src.kind == Kind::Folder
                && src.library == lib.api_id
                && (parent.path == src.path || parent.path.starts_with(&format!("{}/", src.path)))
            {
                return Err(ApiError::conflict("a folder cannot be copied into itself"));
            }
            let row = {
                let _guard = lib.lock.blocking_lock();
                src_lib
                    .fs
                    .copy_to(&src.path, &lib.fs, &rel)
                    .map_err(|e| match e.kind() {
                        std::io::ErrorKind::AlreadyExists => {
                            ApiError::conflict("the name is taken")
                        }
                        _ => ApiError::from(e),
                    })?;
                let st = lib.fs.stat(&rel)?.ok_or_else(ApiError::internal)?;
                app.write(|t| {
                    if let Some(existing) = db::entry_by_path(t, &lib.api_id, &rel)? {
                        db::delete_tree(t, &existing)?;
                    }
                    let (rowid, id) = db::insert_entry(t, &NewEntry {
                        library: &lib.api_id,
                        parent: Some(parent.rowid),
                        name: &name,
                        path: &rel,
                        kind: src.kind,
                        mime: src.mime.as_deref(),
                        size: st.size,
                        mtime_ns: st.mtime_ns,
                        dev: st.dev,
                        ino: st.ino,
                    })?;
                    if src.kind != Kind::Folder {
                        // Same bytes: same facts and hash.
                        t.execute(
                            "UPDATE entries SET sha256 = ?, width = ?, height = ?, orientation = ?, taken = ?, sort_taken = ?,
                                 duration = ?, media_state = ?, previewable = ? WHERE rowid = ?",
                            params![src.sha256, src.width, src.height, src.orientation, src.taken, src.sort_taken, src.duration, src.media_state, src.previewable, rowid],
                        )?;
                    }
                    db::record_change(t, ChangeType::Created, &id, &lib.api_id, Some(&parent_id))?;
                    db::entry_by_rowid(t, rowid)?.ok_or_else(|| anyhow::anyhow!("vanished"))
                })?
            };
            if row.kind == Kind::Folder {
                // Index the copied tree (outside the lock: the indexer takes it per folder).
                crate::indexer::index_tree(app, &lib, row.rowid, &row.path)?;
            }
            row
        }
        _ => {
            return Err(ApiError::invalid(
                "give kind: \"folder\" with a name, or copyOf",
            ));
        }
    };
    app.work.notify_one();
    Ok(app.db.read(|c| Ok(to_api(app, c, &row, key)?))?)
}

/// `PATCH /v1/entries/{id}`: a JSON Merge Patch of `name`, `parent`, `trashed`.
pub async fn patch_entry(
    State(app): State<Arc<App>>,
    Caller(key): Caller,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> ApiResult<Response> {
    let patch: serde_json::Map<String, serde_json::Value> = serde_json::from_slice(&body)
        .map_err(|e| ApiError::invalid(format!("not a JSON object: {e}")))?;
    for k in patch.keys() {
        if !matches!(k.as_str(), "name" | "parent" | "trashed") {
            return Err(ApiError::invalid(format!("{k} is not writable")));
        }
    }
    let name = match patch.get("name") {
        None => None,
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        _ => return Err(ApiError::invalid("name is a string")),
    };
    let parent = match patch.get("parent") {
        None => None,
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        _ => return Err(ApiError::invalid("parent is an entry ID")),
    };
    let trashed = match patch.get("trashed") {
        None => None,
        Some(serde_json::Value::Bool(b)) => Some(*b),
        _ => return Err(ApiError::invalid("trashed is true or false")),
    };
    let a = app.clone();
    let entry = tokio::task::spawn_blocking(move || -> ApiResult<_> {
        let row = a.db.read(|c| Ok(load(c, &key, &id)))??;
        let lib = writable(&a, &key, &row)?;
        check_if_match(&headers, &row)?;
        let mut row = row;
        if trashed == Some(false) && row.is_trashed() {
            row = restore(&a, &lib, &row)?;
        }
        if name.is_some() || parent.is_some() {
            if row.is_trashed() {
                return Err(ApiError::conflict("restore it first"));
            }
            row = rename(&a, &key, &lib, &row, name.as_deref(), parent.as_deref())?;
        }
        if trashed == Some(true) && !row.is_trashed() {
            row = trash(&a, &lib, &row)?;
        }
        Ok(a.db.read(|c| Ok(to_api(&a, c, &row, &key)?))?)
    })
    .await??;
    Ok(entry_response(entry))
}

fn rename(
    app: &App,
    key: &crate::auth::KeyRow,
    lib: &Library,
    row: &EntryRow,
    name: Option<&str>,
    parent: Option<&str>,
) -> ApiResult<EntryRow> {
    if row.path.is_empty() {
        return Err(ApiError::conflict("a library's root cannot be moved"));
    }
    let new_name = name.unwrap_or(&row.name).to_string();
    if !fsx::valid_name(&new_name) {
        return Err(ApiError::invalid("not a valid name"));
    }
    let new_parent = match parent {
        Some(pid) => {
            let p = app.db.read(|c| Ok(load(c, key, pid)))??;
            if p.library != row.library {
                return Err(ApiError::conflict(
                    "entries move within their library; copy, then delete, to move across",
                ));
            }
            if p.kind != Kind::Folder || p.is_trashed() {
                return Err(ApiError::invalid("parent is not a folder"));
            }
            if row.kind == Kind::Folder
                && (p.path == row.path || p.path.starts_with(&format!("{}/", row.path)))
            {
                return Err(ApiError::conflict("a folder cannot move into itself"));
            }
            p
        }
        None => app
            .db
            .read(|c| Ok(db::entry_by_rowid(c, row.parent.unwrap_or(0))?))?
            .ok_or_else(ApiError::internal)?,
    };
    let new_rel = fsx::join(&new_parent.path, &new_name);
    if new_rel == row.path {
        return Ok(row.clone());
    }
    let _guard = lib.lock.blocking_lock();
    // A case-only rename on a case-insensitive filesystem "exists" already; let rename handle it.
    let case_only = new_rel.eq_ignore_ascii_case(&row.path);
    if !case_only && lib.fs.exists(&new_rel) {
        return Err(ApiError::conflict("the name is taken"));
    }
    if case_only {
        let tmp = fsx::join(
            fsx::parent_of(&row.path),
            &format!(".wp-{}.tmp", util::random_id("")),
        );
        lib.fs.rename_new(&row.path, &tmp)?;
        lib.fs.rename_new(&tmp, &new_rel)?;
    } else {
        lib.fs.rename_new(&row.path, &new_rel)?;
    }
    let row2 = app.write(|t| {
        db::move_entry(t, row, Some(new_parent.rowid), &new_name, &new_rel)?;
        db::record_change(
            t,
            ChangeType::Updated,
            &row.id,
            &row.library,
            Some(&new_parent.id),
        )?;
        if row.kind == Kind::Folder {
            for d in db::descendants(t, &row.library, &new_rel)? {
                db::record_change(
                    t,
                    ChangeType::Updated,
                    &d.id,
                    &d.library,
                    d.parent_id.as_deref(),
                )?;
            }
        }
        db::entry_by_rowid(t, row.rowid)?.ok_or_else(|| anyhow::anyhow!("vanished"))
    })?;
    Ok(row2)
}

/// Moves an entry into the library's trash: `.wirephoto-trash/<id>/<name>`.
fn trash(app: &App, lib: &Library, row: &EntryRow) -> ApiResult<EntryRow> {
    if row.path.is_empty() {
        return Err(ApiError::conflict("a library's root cannot be deleted"));
    }
    let _guard = lib.lock.blocking_lock();
    let holder = fsx::join(TRASH_DIR, &row.id);
    lib.fs.create_dir_all(&holder)?;
    let dest = fsx::join(&holder, &row.name);
    lib.fs.rename_new(&row.path, &dest)?;
    let now = util::now_secs();
    let row2 = app.write(|t| {
        db::move_entry(t, row, row.parent, &row.name, &dest)?;
        t.execute(
            "UPDATE entries SET trashed_at = ?, trash_root = rowid, orig_parent = parent, orig_name = name WHERE rowid = ?",
            params![now, row.rowid],
        )?;
        record_tree(t, row, &dest, Some(now))?;
        db::entry_by_rowid(t, row.rowid)?.ok_or_else(|| anyhow::anyhow!("vanished"))
    })?;
    Ok(row2)
}

/// Marks everything below `path` as trashed with `root` (or restored) and records an update for the entry and
/// each descendant.
fn record_tree(
    t: &rusqlite::Connection,
    root: &EntryRow,
    path: &str,
    trashed_at: Option<i64>,
) -> anyhow::Result<()> {
    let below = db::descendants(t, &root.library, path)?;
    for d in &below {
        match trashed_at {
            Some(at) => t.execute(
                "UPDATE entries SET trashed_at = ?, trash_root = ? WHERE rowid = ?",
                params![at, root.rowid, d.rowid],
            )?,
            None => t.execute(
                "UPDATE entries SET trashed_at = NULL, trash_root = NULL WHERE rowid = ?",
                [d.rowid],
            )?,
        };
        db::record_change(
            t,
            ChangeType::Updated,
            &d.id,
            &d.library,
            d.parent_id.as_deref(),
        )?;
    }
    db::record_change(
        t,
        ChangeType::Updated,
        &root.id,
        &root.library,
        root.parent_id.as_deref(),
    )?;
    Ok(())
}

fn restore(app: &App, lib: &Library, row: &EntryRow) -> ApiResult<EntryRow> {
    if row.trash_root != Some(row.rowid) {
        return Err(ApiError::conflict("restore the folder it was deleted with"));
    }
    let parent = app
        .db
        .read(|c| Ok(db::entry_by_rowid(c, row.orig_parent.unwrap_or(0))?))?
        .filter(|p| !p.is_trashed())
        .ok_or_else(|| ApiError::conflict("the folder it was in is gone"))?;
    let name = row.orig_name.clone().unwrap_or_else(|| row.name.clone());
    let dest = fsx::join(&parent.path, &name);
    let _guard = lib.lock.blocking_lock();
    if lib.fs.exists(&dest) {
        return Err(ApiError::conflict("the name is taken now"));
    }
    lib.fs.rename_new(&row.path, &dest)?;
    let _ = lib.fs.remove_all(fsx::parent_of(&row.path)); // the empty holder
    let row2 = app.write(|t| {
        db::move_entry(t, row, Some(parent.rowid), &name, &dest)?;
        t.execute(
            "UPDATE entries SET trashed_at = NULL, trash_root = NULL, orig_parent = NULL, orig_name = NULL WHERE rowid = ?",
            [row.rowid],
        )?;
        let moved = db::entry_by_rowid(t, row.rowid)?.ok_or_else(|| anyhow::anyhow!("vanished"))?;
        record_tree(t, &moved, &dest, None)?;
        Ok(moved)
    })?;
    Ok(row2)
}

/// Deletes an entry and its files for good.
fn delete_permanently(app: &App, lib: &Library, row: &EntryRow) -> ApiResult<()> {
    if row.path.is_empty() {
        return Err(ApiError::conflict("a library's root cannot be deleted"));
    }
    let _guard = lib.lock.blocking_lock();
    match lib.fs.remove_all(&row.path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    if row.trash_root == Some(row.rowid) {
        let _ = lib.fs.remove_all(fsx::parent_of(&row.path));
    }
    app.write(|t| {
        db::delete_tree(t, row)?;
        Ok(())
    })?;
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct DeleteQuery {
    permanent: Option<bool>,
}

/// `DELETE /v1/entries/{id}`
pub async fn delete_entry(
    State(app): State<Arc<App>>,
    Caller(key): Caller,
    Path(id): Path<String>,
    Query(q): Query<DeleteQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let a = app.clone();
    tokio::task::spawn_blocking(move || -> ApiResult<()> {
        let row = a.db.read(|c| Ok(load(c, &key, &id)))??;
        let lib = writable(&a, &key, &row)?;
        check_if_match(&headers, &row)?;
        if q.permanent.unwrap_or(false) || lib.cfg.trash_days == 0 || row.is_trashed() {
            if row.is_trashed() && row.trash_root != Some(row.rowid) {
                return Err(ApiError::conflict("delete the folder it was deleted with"));
            }
            delete_permanently(&a, &lib, &row)
        } else {
            trash(&a, &lib, &row).map(|_| ())
        }
    })
    .await??;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Permanently deletes trashed entries older than their library's `trash_days`.
pub fn purge_trash(app: &App) -> anyhow::Result<()> {
    for lib in &app.libraries {
        if lib.cfg.trash_days == 0 {
            continue;
        }
        let cutoff = util::now_secs() - lib.cfg.trash_days as i64 * 86400;
        let expired: Vec<EntryRow> = app.db.read(|c| {
            let mut st = c.prepare(&format!(
                "SELECT {} FROM {} WHERE e.library = ? AND e.trash_root = e.rowid AND e.trashed_at < ?",
                db::ENTRY_COLS,
                db::ENTRY_FROM
            ))?;
            let rows = st.query_map(params![lib.api_id, cutoff], EntryRow::from_row)?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        for row in expired {
            if let Err(e) = delete_permanently(app, lib, &row) {
                tracing::warn!(entry = %row.id, "emptying trash: {e:?}");
            }
        }
    }
    Ok(())
}
