//! Resumable uploads: the tus protocol, 1.0.0 (https://tus.io/protocols/resumable-upload), with the creation,
//! creation-with-upload, checksum, termination and expiration extensions (api.md section 6).
//!
//! A file goes up in as few requests as possible, the whole of it in one when nothing breaks; after a dropped
//! connection the client asks how far it got (`HEAD`) and sends the rest. The bytes go straight into one file
//! per upload in the library's `.wp-uploads` folder (the indexer skips `.wp-` names); the index keeps the
//! upload and its offset, so a restart loses nothing, and bytes that arrived before a connection broke count.
//! Once complete the file is stored exactly like a `PUT` into its folder, moved there without a copy. Uploads
//! belong to the key that created them and expire after [`TTL`].

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::sync::{Arc, OnceLock};

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use rusqlite::{OptionalExtension, params};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use wirephoto_api::Kind;

use crate::app::{App, Library};
use crate::auth::KeyRow;
use crate::fsx;
use crate::http::auth::Caller;
use crate::http::entries::{load, to_api};
use crate::http::error::{ApiError, ApiResult};
use crate::metrics::Metrics;
use crate::util;
use crate::write::{folder_on_disk, store, writable};

pub const VERSION: &str = "1.0.0";
pub const EXTENSIONS: &str = "creation,creation-with-upload,checksum,termination,expiration";
pub const CHECKSUMS: &str = "sha1,sha256";
/// How long an upload lives after it is created.
pub const TTL: i64 = 48 * 3600;
/// How long a finished upload's outcome stays readable (`GET /v1/tus/{id}`), for a client whose answer was lost.
pub const OUTCOME_TTL: i64 = 3600;
/// Where a library keeps the bytes of its uploads.
pub const DIR: &str = ".wp-uploads";
const OCTETS: &str = "application/offset+octet-stream";

fn staging(id: &str) -> String {
    format!("{DIR}/{id}")
}

/// An upload as the index keeps it.
#[derive(Debug, Clone)]
struct Upload {
    id: String,
    key: String,
    library: String,
    parent: String,
    name: String,
    length: u64,
    sha256: Option<Vec<u8>>,
    offset: u64,
    expires: i64,
    /// Once stored: the entry, and whether it was new (`false`: the folder had these bytes already).
    entry: Option<String>,
    created_entry: bool,
}

fn upload(c: &rusqlite::Connection, id: &str) -> rusqlite::Result<Option<Upload>> {
    c.query_row(
        "SELECT id, key, library, parent, name, length, sha256, received, expires, entry, created_entry
         FROM tus WHERE id = ?",
        [id],
        |r| {
            Ok(Upload {
                id: r.get(0)?,
                key: r.get(1)?,
                library: r.get(2)?,
                parent: r.get(3)?,
                name: r.get(4)?,
                length: r.get::<_, i64>(5)? as u64,
                sha256: r.get(6)?,
                offset: r.get::<_, i64>(7)? as u64,
                expires: r.get(8)?,
                entry: r.get(9)?,
                created_entry: r.get::<_, Option<i64>>(10)?.unwrap_or(0) != 0,
            })
        },
    )
    .optional()
}

/// The caller's upload, not expired; anyone else's is not found.
fn mine(app: &App, key: &KeyRow, id: &str) -> ApiResult<(Upload, Arc<Library>)> {
    let u = app
        .db
        .read(|c| Ok(upload(c, id)?))?
        .filter(|u| u.key == key.id && u.expires > util::now_secs())
        .ok_or_else(ApiError::not_found)?;
    let lib = app
        .library(&u.library)
        .cloned()
        .ok_or_else(ApiError::not_found)?;
    Ok((u, lib))
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> ApiResult<T> + Send + 'static,
) -> ApiResult<T> {
    tokio::task::spawn_blocking(f).await?
}

/// One request at a time writes to an upload; another is `423 Locked`, which tus clients retry.
fn lock(id: &str) -> ApiResult<tokio::sync::OwnedMutexGuard<()>> {
    static LOCKS: OnceLock<parking_lot::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
        OnceLock::new();
    let m = LOCKS
        .get_or_init(Default::default)
        .lock()
        .entry(id.to_string())
        .or_default()
        .clone();
    m.try_lock_owned().map_err(|_| ApiError::locked())
}

/// `Upload-Expires`: an HTTP date.
fn http_date(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0)
        .unwrap_or_default()
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string()
}

fn header_u64(headers: &HeaderMap, name: &str) -> Option<u64> {
    headers.get(name)?.to_str().ok()?.trim().parse().ok()
}

/// `Upload-Metadata: key base64,key base64,...` (a value may be absent).
pub fn parse_metadata(v: &str) -> HashMap<String, String> {
    v.split(',')
        .filter_map(|pair| {
            let mut it = pair.trim().splitn(2, ' ');
            let k = it.next()?.trim();
            if k.is_empty() {
                return None;
            }
            let val = match it.next() {
                Some(b) => String::from_utf8(util::b64_decode(b.trim())?).ok()?,
                None => String::new(),
            };
            Some((k.to_string(), val))
        })
        .collect()
}

/// A request's own checksum (`Upload-Checksum: sha1|sha256 <base64>`).
enum Check {
    Sha1(Sha1, Vec<u8>),
    Sha256(Sha256, Vec<u8>),
}

impl Check {
    fn from(headers: &HeaderMap) -> ApiResult<Option<Check>> {
        let Some(v) = headers.get("upload-checksum") else {
            return Ok(None);
        };
        let v = v
            .to_str()
            .map_err(|_| ApiError::invalid("bad Upload-Checksum"))?;
        let (alg, b64) = v
            .trim()
            .split_once(' ')
            .ok_or_else(|| ApiError::invalid("Upload-Checksum is <algorithm> <base64>"))?;
        let want =
            util::b64_decode(b64.trim()).ok_or_else(|| ApiError::invalid("bad Upload-Checksum"))?;
        match alg.to_ascii_lowercase().as_str() {
            "sha1" => Ok(Some(Check::Sha1(Sha1::new(), want))),
            "sha256" => Ok(Some(Check::Sha256(Sha256::new(), want))),
            _ => Err(ApiError::invalid(format!(
                "Upload-Checksum algorithms are {CHECKSUMS}"
            ))),
        }
    }

    fn update(&mut self, b: &[u8]) {
        match self {
            Check::Sha1(h, _) => h.update(b),
            Check::Sha256(h, _) => h.update(b),
        }
    }

    fn matches(self) -> bool {
        match self {
            Check::Sha1(h, want) => h.finalize()[..] == want[..],
            Check::Sha256(h, want) => h.finalize()[..] == want[..],
        }
    }
}

/// How writing a request's bytes went.
enum Wrote {
    /// The whole body: the new offset.
    All(u64),
    /// The connection broke: the new offset, counting what did arrive (none when a checksum was asked for).
    Broken(u64),
    Mismatch,
    TooLong,
}

/// Writes a request body at `offset`. Bytes that arrived before the body broke off count, unless the request
/// had a checksum, which they cannot be checked against; the file never holds more than what counts.
async fn write_at(
    lib: &Library,
    u: &Upload,
    offset: u64,
    body: Body,
    mut check: Option<Check>,
) -> ApiResult<Wrote> {
    let (fs, rel) = (lib.fs.clone(), staging(&u.id));
    let std_file = blocking(move || Ok(fs.open_write(&rel)?)).await?;
    let mut file = tokio::fs::File::from_std(std_file);
    file.seek(SeekFrom::Start(offset)).await?;
    let mut got = 0u64;
    let mut stream = body.into_data_stream();
    let mut broken = false;
    let mut too_long = false;
    while let Some(piece) = stream.next().await {
        let Ok(piece) = piece else {
            broken = true;
            break;
        };
        if offset + got + piece.len() as u64 > u.length {
            too_long = true;
            break;
        }
        if let Some(c) = &mut check {
            c.update(&piece);
        }
        file.write_all(&piece).await?;
        got += piece.len() as u64;
    }
    let keep = if too_long || (broken && check.is_some()) {
        0
    } else if broken || check.take().is_none_or(Check::matches) {
        got
    } else {
        file.set_len(offset).await?;
        return Ok(Wrote::Mismatch);
    };
    file.set_len(offset + keep).await?;
    // On disk before it counts: a restart resumes from what is really there.
    file.sync_data().await?;
    Ok(if too_long {
        Wrote::TooLong
    } else if broken {
        Wrote::Broken(offset + keep)
    } else {
        Wrote::All(offset + keep)
    })
}

fn set_offset(app: &App, id: &str, offset: u64) -> ApiResult<()> {
    app.db.write(|t| {
        t.execute(
            "UPDATE tus SET received = ? WHERE id = ?",
            params![offset as i64, id],
        )?;
        Ok(())
    })?;
    Ok(())
}

/// Removes an upload and its bytes.
fn drop_upload(app: &App, lib: &Library, id: &str) -> ApiResult<()> {
    let _ = lib.fs.remove_file(&staging(id));
    app.db.write(|t| {
        t.execute("DELETE FROM tus WHERE id = ?", [id])?;
        Ok(())
    })?;
    Ok(())
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

/// The headers that say where a finished upload went.
fn outcome_headers(r: &mut Response, entry: &str, name: &str, created: bool) {
    let h = r.headers_mut();
    h.insert(
        HeaderName::from_static("wirephoto-entry"),
        HeaderValue::from_str(entry).unwrap_or(HeaderValue::from_static("")),
    );
    let name =
        percent_encoding::utf8_percent_encode(name, percent_encoding::NON_ALPHANUMERIC).to_string();
    h.insert(
        HeaderName::from_static("wirephoto-entry-name"),
        HeaderValue::from_str(&name).unwrap_or(HeaderValue::from_static("")),
    );
    h.insert(
        HeaderName::from_static("wirephoto-stored"),
        HeaderValue::from_static(if created { "created" } else { "existed" }),
    );
}

/// An upload whose bytes have all arrived: checked against the `sha256` it was created with, then stored like
/// a `PUT` into its folder. Its outcome is kept a while ([`OUTCOME_TTL`]).
async fn finish(
    app: &Arc<App>,
    key: &KeyRow,
    info: Option<crate::audit::RequestInfo>,
    u: &Upload,
    lib: &Arc<Library>,
) -> ApiResult<(String, String, bool)> {
    let started = std::time::Instant::now();
    let (a, k, uu, l) = (app.clone(), key.clone(), u.clone(), lib.clone());
    let (parent, sha) = blocking(move || {
        let rel = staging(&uu.id);
        let len = l.fs.stat(&rel)?.map_or(0, |st| st.size);
        if len != uu.length {
            return Err(ApiError::internal()
                .detail(format!("{len} bytes stored, the upload is {}", uu.length)));
        }
        let sha = hash_file(&l, &rel)?;
        if uu.sha256.as_deref().is_some_and(|want| want != sha) {
            drop_upload(&a, &l, &uu.id)?;
            return Err(ApiError::checksum_mismatch().detail("the file does not match its sha256"));
        }
        let parent = a.db.read(|c| Ok(load(c, &k, &uu.parent)))??;
        if parent.kind != Kind::Folder || parent.is_trashed() {
            return Err(ApiError::not_found().detail("the folder is gone or in the trash"));
        }
        writable(&a, &k, &parent)?;
        folder_on_disk(&a, &l, &parent)?;
        Ok((parent, sha))
    })
    .await?;
    let (entry, created) = store(
        app.clone(),
        key.clone(),
        info,
        parent,
        u.name.clone(),
        lib.clone(),
        staging(&u.id),
        sha,
        None,
        started,
    )
    .await?;
    let (a, id, eid) = (app.clone(), u.id.clone(), entry.id.clone());
    blocking(move || {
        a.db.write(|t| {
            t.execute(
                "UPDATE tus SET entry = ?, created_entry = ?, expires = ? WHERE id = ?",
                params![eid, created, util::now_secs() + OUTCOME_TTL, id],
            )?;
            Ok(())
        })?;
        Ok(())
    })
    .await?;
    Metrics::add(&app.metrics.uploads_completed, 1);
    Ok((entry.id, entry.name, created))
}

/// Whether the request speaks tus 1.0.0 (every tus request but `OPTIONS` must say so).
fn tus_request(headers: &HeaderMap) -> ApiResult<()> {
    match headers.get("tus-resumable").and_then(|v| v.to_str().ok()) {
        Some(VERSION) => Ok(()),
        _ => Err(ApiError::tus_version()),
    }
}

fn octets(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case(OCTETS))
}

/// `OPTIONS /v1/tus`: what the server speaks.
pub async fn options() -> Response {
    let mut r = StatusCode::NO_CONTENT.into_response();
    let h = r.headers_mut();
    h.insert("tus-version", HeaderValue::from_static(VERSION));
    h.insert("tus-extension", HeaderValue::from_static(EXTENSIONS));
    h.insert(
        "tus-checksum-algorithm",
        HeaderValue::from_static(CHECKSUMS),
    );
    r
}

/// `OPTIONS` on the tus endpoints, in front of CORS (which takes every `OPTIONS` for a preflight): a client
/// asking what the server speaks gets [`options`]; a browser's preflight gets CORS's answer, with the same tus
/// headers.
pub async fn discovery(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let is_tus =
        req.method() == axum::http::Method::OPTIONS && req.uri().path().starts_with("/v1/tus");
    if !is_tus {
        return next.run(req).await;
    }
    let preflight = req
        .headers()
        .contains_key(header::ACCESS_CONTROL_REQUEST_METHOD);
    let mut r = if preflight {
        next.run(req).await
    } else {
        options().await
    };
    let h = r.headers_mut();
    h.insert("tus-resumable", HeaderValue::from_static(VERSION));
    h.insert("tus-version", HeaderValue::from_static(VERSION));
    h.insert("tus-extension", HeaderValue::from_static(EXTENSIONS));
    h.insert(
        "tus-checksum-algorithm",
        HeaderValue::from_static(CHECKSUMS),
    );
    r
}

/// Adds `Tus-Resumable` to every response of the tus endpoints.
pub async fn resumable(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let mut r = next.run(req).await;
    r.headers_mut()
        .insert("tus-resumable", HeaderValue::from_static(VERSION));
    r
}

/// `POST /v1/tus`: creates an upload (creation), with its first bytes when the request has a body
/// (creation-with-upload); a file sent whole in it is stored at once.
pub async fn create(
    State(app): State<Arc<App>>,
    Caller(key): Caller,
    info: Option<axum::Extension<crate::audit::RequestInfo>>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Response> {
    tus_request(&headers)?;
    if headers.contains_key("upload-defer-length") {
        return Err(ApiError::invalid(
            "Upload-Defer-Length is not supported: send Upload-Length",
        ));
    }
    let length = header_u64(&headers, "upload-length")
        .ok_or_else(|| ApiError::invalid("Upload-Length is needed"))?;
    let meta = headers
        .get("upload-metadata")
        .and_then(|v| v.to_str().ok())
        .map(parse_metadata)
        .unwrap_or_default();
    let parent_id = meta
        .get("parent")
        .cloned()
        .ok_or_else(|| ApiError::invalid("Upload-Metadata needs parent (the folder's entry id)"))?;
    let name = meta
        .get("filename")
        .cloned()
        .ok_or_else(|| ApiError::invalid("Upload-Metadata needs filename"))?;
    if !fsx::valid_name(&name) {
        return Err(ApiError::invalid("not a valid file name"));
    }
    let sha = match meta.get("sha256") {
        Some(h) => Some(
            hex::decode(h)
                .ok()
                .filter(|b| b.len() == 32)
                .ok_or_else(|| ApiError::invalid("sha256 is 64 hex characters"))?,
        ),
        None => None,
    };
    let with_bytes = octets(&headers);
    let check = if with_bytes {
        Check::from(&headers)?
    } else {
        None
    };
    let (a, k) = (app.clone(), key.clone());
    let (u, lib) = blocking(move || {
        let parent = a.db.read(|c| Ok(load(c, &k, &parent_id)))??;
        if parent.kind != Kind::Folder || parent.is_trashed() {
            return Err(ApiError::invalid("parent is not a folder"));
        }
        let lib = writable(&a, &k, &parent)?;
        folder_on_disk(&a, &lib, &parent)?;
        let id = util::random_id("tus_");
        lib.fs.create_dir_all(DIR)?;
        lib.fs.create_new(&staging(&id))?;
        let now = util::now_secs();
        let u = Upload {
            id,
            key: k.id.clone(),
            library: lib.api_id.clone(),
            parent: parent.id.clone(),
            name,
            length,
            sha256: sha,
            offset: 0,
            expires: now + TTL,
            entry: None,
            created_entry: false,
        };
        a.db.write(|t| {
            t.execute(
                "INSERT INTO tus (id, key, library, parent, name, length, sha256, received, expires, created)
                 VALUES (?, ?, ?, ?, ?, ?, ?, 0, ?, ?)",
                params![u.id, u.key, u.library, u.parent, u.name, u.length as i64, u.sha256, u.expires, now],
            )?;
            Ok(())
        })?;
        Ok((u, lib))
    })
    .await?;
    Metrics::add(&app.metrics.uploads_opened, 1);
    let mut offset = 0;
    let mut problem = None;
    if with_bytes {
        let _guard = lock(&u.id)?;
        match write_at(&lib, &u, 0, body, check).await? {
            Wrote::All(o) | Wrote::Broken(o) => offset = o,
            Wrote::Mismatch => problem = Some(ApiError::checksum_mismatch()),
            Wrote::TooLong => problem = Some(ApiError::invalid("more bytes than Upload-Length")),
        }
        Metrics::add(&app.metrics.upload_bytes, offset);
        let (a, id) = (app.clone(), u.id.clone());
        blocking(move || set_offset(&a, &id, offset)).await?;
    }
    if let Some(p) = problem {
        return Err(p);
    }
    let mut r = StatusCode::CREATED.into_response();
    if offset == length {
        let u = Upload {
            offset,
            ..u.clone()
        };
        let (eid, ename, created) = finish(&app, &key, info.map(|i| i.0), &u, &lib).await?;
        outcome_headers(&mut r, &eid, &ename, created);
    }
    let h = r.headers_mut();
    h.insert(
        header::LOCATION,
        format!("/v1/tus/{}", u.id).parse().unwrap(),
    );
    h.insert("upload-expires", http_date(u.expires).parse().unwrap());
    if with_bytes {
        h.insert("upload-offset", offset.into());
    }
    Ok(r)
}

/// `HEAD /v1/tus/{id}`: how far the upload got, to resume.
pub async fn head(
    State(app): State<Arc<App>>,
    Caller(key): Caller,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    tus_request(&headers)?;
    let (u, _) = blocking(move || mine(&app, &key, &id)).await?;
    let mut r = StatusCode::OK.into_response();
    let h = r.headers_mut();
    h.insert("upload-offset", u.offset.into());
    h.insert("upload-length", u.length.into());
    h.insert("upload-expires", http_date(u.expires).parse().unwrap());
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(r)
}

/// `PATCH /v1/tus/{id}` (or `POST` with `X-HTTP-Method-Override: PATCH`): bytes at the upload's offset, up to
/// the rest of the file in one request; the last ones store the file.
pub async fn patch(
    State(app): State<Arc<App>>,
    Caller(key): Caller,
    info: Option<axum::Extension<crate::audit::RequestInfo>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Response> {
    tus_request(&headers)?;
    if !octets(&headers) {
        return Err(
            ApiError::unsupported_media().detail(format!("Content-Type: {OCTETS} is needed"))
        );
    }
    let at = header_u64(&headers, "upload-offset")
        .ok_or_else(|| ApiError::invalid("Upload-Offset is needed"))?;
    let check = Check::from(&headers)?;
    let _guard = lock(&id)?;
    let (a, k, i) = (app.clone(), key.clone(), id.clone());
    let (u, lib) = blocking(move || mine(&a, &k, &i)).await?;
    if u.entry.is_some() {
        return Err(ApiError::offset_mismatch("the upload is complete"));
    }
    if at != u.offset {
        return Err(ApiError::offset_mismatch(format!(
            "the upload is at {}, not {at}",
            u.offset
        )));
    }
    let wrote = write_at(&lib, &u, at, body, check).await?;
    let offset = match wrote {
        Wrote::All(o) | Wrote::Broken(o) => o,
        Wrote::Mismatch => return Err(ApiError::checksum_mismatch()),
        Wrote::TooLong => return Err(ApiError::invalid("more bytes than Upload-Length")),
    };
    Metrics::add(&app.metrics.upload_bytes, offset - at);
    let (a, i) = (app.clone(), id.clone());
    blocking(move || set_offset(&a, &i, offset)).await?;
    if matches!(wrote, Wrote::Broken(_)) {
        return Err(ApiError::invalid(
            "the upload was interrupted; what arrived is kept",
        ));
    }
    let mut r = StatusCode::NO_CONTENT.into_response();
    if offset == u.length {
        let u = Upload {
            offset,
            ..u.clone()
        };
        let (eid, ename, created) = finish(&app, &key, info.map(|i| i.0), &u, &lib).await?;
        outcome_headers(&mut r, &eid, &ename, created);
    }
    let h = r.headers_mut();
    h.insert("upload-offset", offset.into());
    h.insert("upload-expires", http_date(u.expires).parse().unwrap());
    Ok(r)
}

/// `POST /v1/tus/{id}`: a `PATCH` through a proxy that refuses the method (`X-HTTP-Method-Override: PATCH`).
pub async fn patch_override(
    state: State<Arc<App>>,
    caller: Caller,
    info: Option<axum::Extension<crate::audit::RequestInfo>>,
    id: Path<String>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Response> {
    let overridden = headers
        .get("x-http-method-override")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("PATCH"));
    if !overridden {
        return Err(ApiError::invalid(
            "POST here needs X-HTTP-Method-Override: PATCH",
        ));
    }
    patch(state, caller, info, id, headers, body).await
}

/// `GET /v1/tus/{id}`: where a finished upload went, for a client whose last answer was lost; `404` before.
pub async fn outcome(
    State(app): State<Arc<App>>,
    Caller(key): Caller,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    blocking(move || {
        let (u, _) = mine(&app, &key, &id)?;
        let eid = u
            .entry
            .ok_or_else(|| ApiError::not_found().detail("the upload is not finished"))?;
        let row = app.db.read(|c| Ok(load(c, &key, &eid)))??;
        let entry = app.db.read(|c| Ok(to_api(&app, c, &row, &key)?))?;
        let mut r =
            Json(serde_json::json!({"entry": entry, "created": u.created_entry})).into_response();
        outcome_headers(&mut r, &row.id, &row.name, u.created_entry);
        Ok(r)
    })
    .await
}

/// `DELETE /v1/tus/{id}` (termination): abandons the upload and its bytes.
pub async fn terminate(
    State(app): State<Arc<App>>,
    Caller(key): Caller,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<StatusCode> {
    tus_request(&headers)?;
    let _guard = lock(&id)?;
    blocking(move || {
        let (u, lib) = mine(&app, &key, &id)?;
        drop_upload(&app, &lib, &u.id)?;
        if u.entry.is_none() {
            Metrics::add(&app.metrics.uploads_abandoned, 1);
        }
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

/// Removes expired uploads with their bytes, finished ones whose outcome is old, and bytes no upload owns any
/// more (upkeep). Returns how many unfinished uploads expired.
pub fn purge(app: &App) -> anyhow::Result<usize> {
    let now = util::now_secs();
    let gone: Vec<(String, String, bool)> = app.db.read(|c| {
        let mut st =
            c.prepare("SELECT id, library, entry IS NOT NULL FROM tus WHERE expires <= ?")?;
        let rows = st
            .query_map([now], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })?;
    for (id, library, _) in &gone {
        if let Some(lib) = app.library(library) {
            let _ = lib.fs.remove_file(&staging(id));
        }
        app.db.write(|t| {
            t.execute("DELETE FROM tus WHERE id = ?", [id])?;
            Ok(())
        })?;
    }
    for lib in &app.libraries {
        let Ok(files) = lib.fs.list(DIR) else {
            continue;
        };
        for (name, st) in files {
            let known = app.db.read(|c| Ok(upload(c, &name)?.is_some()))?;
            if !known && st.mtime_ns / 1_000_000_000 < now - TTL {
                let _ = lib.fs.remove_file(&staging(&name));
            }
        }
    }
    let expired = gone.iter().filter(|g| !g.2).count();
    Metrics::add(&app.metrics.uploads_expired, expired as u64);
    Ok(expired)
}

/// Unfinished uploads not expired (`wirephoto status`).
pub fn open_count(app: &App) -> u64 {
    app.db
        .read(|c| {
            Ok(c.query_row(
                "SELECT COUNT(*) FROM tus WHERE entry IS NULL AND expires > ?",
                [util::now_secs()],
                |r| r.get::<_, i64>(0),
            )?)
        })
        .unwrap_or(0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata() {
        let m = parse_metadata("parent ZW50XzE=,filename SU1HXzAwMS5BUlc=,is_confidential");
        assert_eq!(m["parent"], "ent_1");
        assert_eq!(m["filename"], "IMG_001.ARW");
        assert_eq!(m["is_confidential"], "");
    }

    #[test]
    fn dates() {
        assert_eq!(http_date(0), "Thu, 01 Jan 1970 00:00:00 GMT");
    }
}
