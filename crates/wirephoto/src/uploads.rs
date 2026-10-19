//! Upload sessions (api.md section 6): a large file sent in chunks, several at once and in any order, resumable
//! after a dropped connection or a reload, and stored like a `PUT` into its folder once complete.
//!
//! The bytes go straight into one file per session in the library's `.wp-uploads` folder (the indexer skips
//! `.wp-` names), each chunk written at its offset; the index keeps the session and the ranges received, so a
//! restart loses nothing. Completing publishes that file under its name without copying it: the file appears
//! whole or not at all. Sessions belong to the key that opened them and expire after [`TTL`].

use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use rusqlite::{OptionalExtension, params};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use wirephoto_api::{CreateUpload, Kind, Received, Upload};

use crate::app::{App, Library};
use crate::auth::KeyRow;
use crate::db::EntryRow;
use crate::fsx;
use crate::http::auth::Caller;
use crate::http::entries::{load, to_api};
use crate::http::error::{ApiError, ApiResult};
use crate::util;
use crate::write::{content_digest, same_content, store_new, writable};

/// Bytes per chunk (`GET /v1` `limits.uploadChunk`): every chunk but the last is at most this long.
pub const CHUNK: u64 = 8 << 20;
/// Chunks worth sending at once (`limits.uploadParallel`).
pub const PARALLEL: u32 = 4;
/// How long a session lives after it is opened.
pub const TTL: i64 = 48 * 3600;
/// Where a library keeps the bytes of its sessions.
pub const DIR: &str = ".wp-uploads";

fn staging(id: &str) -> String {
    format!("{DIR}/{id}")
}

/// A session as the index keeps it.
#[derive(Debug, Clone)]
struct Session {
    id: String,
    key: String,
    library: String,
    parent: String,
    name: String,
    size: u64,
    sha256: Option<Vec<u8>>,
    received: Vec<[u64; 2]>,
    state: i64,
    expires: i64,
}

const OPEN: i64 = 0;
const COMPLETING: i64 = 1;

fn session(c: &rusqlite::Connection, id: &str) -> rusqlite::Result<Option<Session>> {
    c.query_row(
        "SELECT id, key, library, parent, name, size, sha256, received, state, expires FROM uploads WHERE id = ?",
        [id],
        |r| {
            Ok(Session {
                id: r.get(0)?,
                key: r.get(1)?,
                library: r.get(2)?,
                parent: r.get(3)?,
                name: r.get(4)?,
                size: r.get::<_, i64>(5)? as u64,
                sha256: r.get(6)?,
                received: serde_json::from_str(&r.get::<_, String>(7)?).unwrap_or_default(),
                state: r.get(8)?,
                expires: r.get(9)?,
            })
        },
    )
    .optional()
}

/// Adds `[start, end)` to sorted, merged ranges.
pub fn merge(ranges: &[[u64; 2]], start: u64, end: u64) -> Vec<[u64; 2]> {
    let mut all: Vec<[u64; 2]> = ranges.to_vec();
    all.push([start, end]);
    all.sort();
    let mut out: Vec<[u64; 2]> = Vec::with_capacity(all.len());
    for [s, e] in all {
        match out.last_mut() {
            Some(last) if s <= last[1] => last[1] = last[1].max(e),
            _ => out.push([s, e]),
        }
    }
    out
}

/// Whether the ranges cover `[0, size)`.
pub fn covers(ranges: &[[u64; 2]], size: u64) -> bool {
    matches!(ranges, [[0, e]] if *e >= size)
}

/// `Content-Range: bytes a-b/total` -> (a, b + 1, total).
pub fn parse_content_range(v: &str) -> Option<(u64, u64, u64)> {
    let (range, total) = v.trim().strip_prefix("bytes ")?.split_once('/')?;
    let (a, b) = range.split_once('-')?;
    let (a, b, total): (u64, u64, u64) = (a.parse().ok()?, b.parse().ok()?, total.parse().ok()?);
    (a <= b && b < total).then_some((a, b + 1, total))
}

fn expires_at(secs: i64) -> String {
    util::rfc3339_ns(secs.saturating_mul(1_000_000_000))
}

fn upload_json(s: &Session) -> Upload {
    Upload {
        id: s.id.clone(),
        size: s.size,
        chunk: CHUNK,
        received: s.received.clone(),
        expires: expires_at(s.expires),
    }
}

/// The caller's session, open and not expired; anyone else's is not found.
fn mine(app: &App, key: &KeyRow, id: &str) -> ApiResult<(Session, Arc<Library>)> {
    let s = app
        .db
        .read(|c| Ok(session(c, id)?))?
        .filter(|s| s.key == key.id && s.expires > util::now_secs())
        .ok_or_else(ApiError::not_found)?;
    let lib = app
        .library(&s.library)
        .cloned()
        .ok_or_else(ApiError::not_found)?;
    Ok((s, lib))
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> ApiResult<T> + Send + 'static,
) -> ApiResult<T> {
    tokio::task::spawn_blocking(f).await?
}

/// `POST /v1/uploads`: opens a session, or answers with the file the folder already has with those bytes.
pub async fn create(
    State(app): State<Arc<App>>,
    Caller(key): Caller,
    Json(req): Json<CreateUpload>,
) -> ApiResult<Response> {
    if !fsx::valid_name(&req.name) {
        return Err(ApiError::invalid("not a valid file name"));
    }
    if req.size == 0 {
        return Err(ApiError::invalid(
            "an empty file is a PUT to /v1/entries/{folder}/children/{name}",
        ));
    }
    let sha = match &req.sha256 {
        Some(h) => Some(
            hex::decode(h)
                .ok()
                .filter(|b| b.len() == 32)
                .ok_or_else(|| ApiError::invalid("sha256 is 64 hex characters"))?,
        ),
        None => None,
    };
    blocking(move || {
        let parent = app.db.read(|c| Ok(load(c, &key, &req.parent)))??;
        if parent.kind != Kind::Folder || parent.is_trashed() {
            return Err(ApiError::invalid("parent is not a folder"));
        }
        let lib = writable(&app, &key, &parent)?;
        crate::write::folder_on_disk(&app, &lib, &parent)?;
        if let Some(sha) = &sha {
            let sha: [u8; 32] = sha[..].try_into().unwrap();
            if let Some(existing) = same_content(&app, &parent, req.size, &sha)? {
                let entry = app.db.read(|c| Ok(to_api(&app, c, &existing, &key)?))?;
                return Ok(Json(serde_json::json!({ "entry": entry })).into_response());
            }
        }
        let id = util::random_id("upl_");
        lib.fs.create_dir_all(DIR)?;
        lib.fs.create_new(&staging(&id))?;
        let now = util::now_secs();
        let s = Session {
            id: id.clone(),
            key: key.id.clone(),
            library: lib.api_id.clone(),
            parent: parent.id.clone(),
            name: req.name.clone(),
            size: req.size,
            sha256: sha,
            received: Vec::new(),
            state: OPEN,
            expires: now + TTL,
        };
        app.db.write(|t| {
            t.execute(
                "INSERT INTO uploads (id, key, library, parent, name, size, sha256, created, expires)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    s.id,
                    s.key,
                    s.library,
                    s.parent,
                    s.name,
                    s.size as i64,
                    s.sha256,
                    now,
                    s.expires
                ],
            )?;
            Ok(())
        })?;
        crate::metrics::Metrics::add(&app.metrics.uploads_opened, 1);
        let mut r = (StatusCode::CREATED, Json(upload_json(&s))).into_response();
        r.headers_mut().insert(
            header::LOCATION,
            format!("/v1/uploads/{id}").parse().unwrap(),
        );
        Ok(r)
    })
    .await
}

/// `GET /v1/uploads/{id}`: what has arrived, to resume.
pub async fn status(
    State(app): State<Arc<App>>,
    Caller(key): Caller,
    Path(id): Path<String>,
) -> ApiResult<Json<Upload>> {
    blocking(move || {
        let (s, _) = mine(&app, &key, &id)?;
        Ok(Json(upload_json(&s)))
    })
    .await
}

/// `PUT /v1/uploads/{id}` with `Content-Range: bytes a-b/total`: one chunk, written at its offset.
pub async fn put_chunk(
    State(app): State<Arc<App>>,
    Caller(key): Caller,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Json<Received>> {
    let (start, end, total) = headers
        .get(header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_content_range)
        .ok_or_else(|| ApiError::invalid("Content-Range: bytes <first>-<last>/<size> is needed"))?;
    let digest = content_digest(&headers)?;
    let (a, k, i) = (app.clone(), key.clone(), id.clone());
    let (s, lib) = blocking(move || mine(&a, &k, &i)).await?;
    if s.state != OPEN {
        return Err(ApiError::conflict("the upload is being completed"));
    }
    if total != s.size {
        return Err(ApiError::invalid(format!(
            "the upload is {} bytes, not {total}",
            s.size
        )));
    }
    let len = end - start;
    if len > CHUNK {
        return Err(ApiError::too_large(format!(
            "chunks are at most {CHUNK} bytes"
        )));
    }
    let (fs, rel) = (lib.fs.clone(), staging(&s.id));
    let file = blocking(move || Ok(fs.open_write(&rel)?)).await?;
    let mut file = tokio::fs::File::from_std(file);
    file.seek(SeekFrom::Start(start)).await?;
    let mut hasher = Sha256::new();
    let mut got = 0u64;
    let mut stream = body.into_data_stream();
    while let Some(piece) = stream.next().await {
        let piece = piece.map_err(|e| ApiError::invalid(format!("upload interrupted: {e}")))?;
        got += piece.len() as u64;
        if got > len {
            return Err(ApiError::invalid(format!(
                "more than the {len} bytes Content-Range announces"
            )));
        }
        hasher.update(&piece);
        file.write_all(&piece).await?;
    }
    if got != len {
        return Err(ApiError::invalid(format!(
            "{got} bytes, Content-Range announces {len}"
        )));
    }
    if digest.is_some_and(|d| d[..] != hasher.finalize()[..]) {
        return Err(ApiError::digest_mismatch());
    }
    // On disk before it counts as received: a restart resumes from what is really there.
    file.sync_data().await?;
    crate::metrics::Metrics::add(&app.metrics.upload_bytes, got);
    blocking(move || {
        let received = app.db.write(|t| {
            let Some(cur) = session(t, &id)?.filter(|s| s.state == OPEN) else {
                return Ok(None);
            };
            let merged = merge(&cur.received, start, end);
            t.execute(
                "UPDATE uploads SET received = ? WHERE id = ?",
                params![serde_json::to_string(&merged)?, id],
            )?;
            Ok(Some(merged))
        })?;
        let received =
            received.ok_or_else(|| ApiError::conflict("the upload was completed or abandoned"))?;
        Ok(Json(Received { received }))
    })
    .await
}

/// The SHA-256 of a whole file.
fn hash_file(lib: &Library, rel: &str) -> std::io::Result<[u8; 32]> {
    let mut f = lib.fs.open_file(rel)?;
    f.rewind()?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            return Ok(h.finalize().into());
        }
        h.update(&buf[..n]);
    }
}

/// Removes a session and its bytes.
fn drop_session(app: &App, lib: &Library, id: &str) -> ApiResult<()> {
    let _ = lib.fs.remove_file(&staging(id));
    app.db.write(|t| {
        t.execute("DELETE FROM uploads WHERE id = ?", [id])?;
        Ok(())
    })?;
    Ok(())
}

/// `POST /v1/uploads/{id}/complete`: checks, then stores the file like a `PUT` into its folder.
pub async fn complete(
    State(app): State<Arc<App>>,
    Caller(key): Caller,
    info: Option<axum::Extension<crate::audit::RequestInfo>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let started = std::time::Instant::now();
    let digest = content_digest(&headers)?;
    let (a, k, i) = (app.clone(), key.clone(), id.clone());
    let (s, lib, parent, sha) = blocking(move || {
        let (s, lib) = mine(&a, &k, &i)?;
        if !covers(&s.received, s.size) {
            let have: u64 = s.received.iter().map(|[s, e]| e - s).sum();
            return Err(ApiError::incomplete(format!(
                "{have} of {} bytes received",
                s.size
            )));
        }
        // One completion at a time; chunks are refused meanwhile.
        let claimed = a.db.write(|t| {
            Ok(t.execute(
                "UPDATE uploads SET state = ? WHERE id = ? AND state = ?",
                params![COMPLETING, i, OPEN],
            )?)
        })?;
        if claimed == 0 {
            return Err(ApiError::conflict("the upload is being completed"));
        }
        let rel = staging(&s.id);
        let ready = (|| -> ApiResult<(EntryRow, [u8; 32])> {
            let len = lib.fs.stat(&rel)?.map_or(0, |st| st.size);
            if len != s.size {
                return Err(ApiError::invalid(format!(
                    "{len} bytes stored, the upload is {}",
                    s.size
                )));
            }
            let sha = hash_file(&lib, &rel)?;
            let parent = a.db.read(|c| Ok(load(c, &k, &s.parent)))??;
            if parent.kind != Kind::Folder || parent.is_trashed() {
                return Err(ApiError::conflict("the folder is gone or in the trash"));
            }
            crate::write::folder_on_disk(&a, &lib, &parent)?;
            Ok((parent, sha))
        })();
        match ready {
            Ok((parent, sha)) => {
                let expected = digest.as_deref().or(s.sha256.as_deref());
                if expected.is_some_and(|d| d != sha) {
                    // The bytes are not what was meant: nothing is stored, the session is gone.
                    drop_session(&a, &lib, &s.id)?;
                    return Err(ApiError::digest_mismatch());
                }
                Ok((s, lib, parent, sha))
            }
            Err(e) => {
                let _ = a.db.write(|t| {
                    t.execute(
                        "UPDATE uploads SET state = ? WHERE id = ?",
                        params![OPEN, i],
                    )?;
                    Ok(())
                });
                Err(e)
            }
        }
    })
    .await?;
    let res = store_new(
        app.clone(),
        key,
        info.map(|i| i.0),
        parent,
        s.name.clone(),
        lib.clone(),
        staging(&s.id),
        sha,
        None,
        started,
    )
    .await;
    let id = s.id.clone();
    let stored = res.is_ok();
    if stored {
        crate::metrics::Metrics::add(&app.metrics.uploads_completed, 1);
    }
    blocking(move || {
        if stored {
            app.db.write(|t| {
                t.execute("DELETE FROM uploads WHERE id = ?", [&id])?;
                Ok(())
            })?;
        } else {
            // Left as it was, to complete again.
            app.db.write(|t| {
                t.execute(
                    "UPDATE uploads SET state = ? WHERE id = ?",
                    params![OPEN, id],
                )?;
                Ok(())
            })?;
        }
        Ok(())
    })
    .await?;
    res
}

/// `DELETE /v1/uploads/{id}`: abandons the session and its bytes.
pub async fn abandon(
    State(app): State<Arc<App>>,
    Caller(key): Caller,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    blocking(move || {
        let (s, lib) = mine(&app, &key, &id)?;
        if s.state != OPEN {
            return Err(ApiError::conflict("the upload is being completed"));
        }
        drop_session(&app, &lib, &s.id)?;
        crate::metrics::Metrics::add(&app.metrics.uploads_abandoned, 1);
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

/// Removes expired sessions with their bytes, and bytes no session owns any more (upkeep). Returns how many
/// sessions expired.
pub fn purge(app: &App) -> anyhow::Result<usize> {
    let now = util::now_secs();
    let expired: Vec<(String, String)> = app.db.read(|c| {
        let mut st = c.prepare("SELECT id, library FROM uploads WHERE expires <= ?")?;
        let rows = st
            .query_map([now], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })?;
    for (id, library) in &expired {
        if let Some(lib) = app.library(library) {
            let _ = lib.fs.remove_file(&staging(id));
        }
        app.db.write(|t| {
            t.execute("DELETE FROM uploads WHERE id = ?", [id])?;
            Ok(())
        })?;
    }
    for lib in &app.libraries {
        let Ok(files) = lib.fs.list(DIR) else {
            continue;
        };
        for (name, st) in files {
            let known = app.db.read(|c| Ok(session(c, &name)?.is_some()))?;
            let old = st.mtime_ns / 1_000_000_000 < now - TTL;
            if !known && old {
                let _ = lib.fs.remove_file(&staging(&name));
            }
        }
    }
    crate::metrics::Metrics::add(&app.metrics.uploads_expired, expired.len() as u64);
    Ok(expired.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_merge_and_cover() {
        let r = merge(&[], 10, 20);
        let r = merge(&r, 0, 5);
        assert_eq!(r, vec![[0, 5], [10, 20]]);
        assert!(!covers(&r, 20));
        let r = merge(&r, 5, 10);
        assert_eq!(r, vec![[0, 20]]);
        assert!(covers(&r, 20));
        assert_eq!(merge(&r, 3, 8), vec![[0, 20]], "a chunk sent twice");
    }

    #[test]
    fn content_ranges() {
        assert_eq!(parse_content_range("bytes 0-99/1000"), Some((0, 100, 1000)));
        assert_eq!(
            parse_content_range("bytes 900-999/1000"),
            Some((900, 1000, 1000))
        );
        assert_eq!(parse_content_range("bytes 900-1000/1000"), None);
        assert_eq!(parse_content_range("bytes */1000"), None);
    }
}
