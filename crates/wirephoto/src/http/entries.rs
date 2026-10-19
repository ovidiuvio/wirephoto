//! Reading entries: one, or a page of a list (api.md section 4).

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::{IntoResponse, Response};
use rusqlite::types::Value;
use serde::Deserialize;
use wirephoto_api::{Counts, Entry, Kind, Media, Page, Urls};

use super::auth::Caller;
use super::error::{ApiError, ApiResult};
use crate::app::App;
use crate::auth::KeyRow;
use crate::db::{self, ENTRY_COLS, ENTRY_FROM, EntryRow};
use crate::{kind, util};

pub const PAGE_DEFAULT: u32 = 200;
pub const PAGE_MAX: u32 = 1000;

/// An entry as the API shows it to `key`.
pub fn to_api(
    app: &App,
    c: &rusqlite::Connection,
    row: &EntryRow,
    key: &KeyRow,
) -> rusqlite::Result<Entry> {
    let is_file = row.kind != Kind::Folder;
    let media = if matches!(row.kind, Kind::Image | Kind::Raw | Kind::Video)
        && row.media_state == db::MEDIA_DONE
    {
        let m = Media {
            width: row.width,
            height: row.height,
            orientation: row.orientation,
            taken: row.taken.clone(),
            duration: row.duration,
        };
        (m.width.is_some() || m.taken.is_some() || m.duration.is_some()).then_some(m)
    } else {
        None
    };
    let urls = is_file.then(|| {
        let sig = app.keys.sign(&key.id, &row.id);
        Urls {
            content: Some(format!("/v1/entries/{}/content?sig={sig}", row.id)),
            preview: row.previewable.then(|| {
                format!(
                    "/v1/entries/{}/preview/{{size}}.jpg?v={}&sig={sig}",
                    row.id,
                    row.etag()
                )
            }),
        }
    });
    let counts = if is_file {
        None
    } else {
        Some(db::counts(c, row.rowid)?).map(|(folders, files)| Counts { folders, files })
    };
    Ok(Entry {
        id: row.id.clone(),
        library: row.library.clone(),
        parent: row.parent_id.clone(),
        kind: row.kind,
        name: row.name.clone(),
        path: format!("/{}", row.path),
        mime: if is_file { row.mime.clone() } else { None },
        size: is_file.then_some(row.size),
        modified: util::rfc3339_ns(row.mtime_ns),
        etag: row.etag(),
        sha256: row.sha256.as_ref().map(hex::encode),
        media,
        trashed: row.is_trashed().then_some(true),
        urls,
        counts,
    })
}

pub fn etag_header(etag: &str) -> HeaderValue {
    HeaderValue::from_str(&format!("\"{etag}\"")).unwrap_or(HeaderValue::from_static("\"\""))
}

/// An entry response with its `ETag`.
pub fn entry_response(e: Entry) -> Response {
    let tag = etag_header(&e.etag);
    let mut r = Json(e).into_response();
    r.headers_mut().insert(header::ETAG, tag);
    r
}

/// The entry, if it exists and the key may see its library.
pub fn load(c: &rusqlite::Connection, key: &KeyRow, id: &str) -> ApiResult<EntryRow> {
    let row = db::entry_by_id(c, id)?.ok_or_else(ApiError::not_found)?;
    if !key.may_use(&row.library) {
        return Err(ApiError::not_found());
    }
    Ok(row)
}

pub async fn get_entry(
    State(app): State<Arc<App>>,
    Caller(key): Caller,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let a = app.clone();
    let e = tokio::task::spawn_blocking(move || -> ApiResult<Entry> {
        a.db.read(|c| Ok(load(c, &key, &id).and_then(|row| Ok(to_api(&a, c, &row, &key)?))))
            .map_err(ApiError::from)?
    })
    .await??;
    Ok(entry_response(e))
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ListQuery {
    parent: Option<String>,
    recursive: Option<bool>,
    library: Option<String>,
    path: Option<String>,
    kind: Option<String>,
    taken_after: Option<String>,
    taken_before: Option<String>,
    sha256: Option<String>,
    size: Option<u64>,
    duplicates: Option<bool>,
    trashed: Option<bool>,
    sort: Option<String>,
    limit: Option<u32>,
    cursor: Option<String>,
}

fn parse_time(s: &str) -> ApiResult<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|d| d.timestamp())
        .map_err(|_| ApiError::invalid(format!("not an RFC 3339 time: {s}")))
}

fn parse_sha(s: &str) -> ApiResult<Vec<u8>> {
    hex::decode(s)
        .ok()
        .filter(|b| b.len() == 32)
        .ok_or_else(|| ApiError::invalid("sha256 is 64 hex characters"))
}

pub async fn list_entries(
    State(app): State<Arc<App>>,
    Caller(key): Caller,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Page<Entry>>> {
    let a = app.clone();
    let page = tokio::task::spawn_blocking(move || list(&a, &key, q)).await??;
    Ok(Json(page))
}

fn list(app: &Arc<App>, key: &KeyRow, q: ListQuery) -> ApiResult<Page<Entry>> {
    let limit = q.limit.unwrap_or(PAGE_DEFAULT);
    if !(1..=PAGE_MAX).contains(&limit) {
        return Err(ApiError::invalid(format!("limit is 1 to {PAGE_MAX}")));
    }
    let mut wh: Vec<String> = vec!["e.missing = 0".into()];
    let mut args: Vec<Value> = Vec::new();

    // Scope: exactly one of parent / library.
    match (&q.parent, &q.library) {
        (Some(pid), None) => {
            let parent = app.db.read(|c| Ok(load(c, key, pid)))??;
            if parent.kind != Kind::Folder {
                return Err(ApiError::invalid("parent is not a folder"));
            }
            if q.recursive.unwrap_or(false) {
                wh.push("e.library = ?".into());
                args.push(Value::Text(parent.library.clone()));
                if parent.path.is_empty() {
                    wh.push("e.path != ''".into());
                } else {
                    wh.push("substr(e.path, 1, ?) = ?".into());
                    let prefix = format!("{}/", parent.path);
                    args.push(Value::Integer(prefix.len() as i64));
                    args.push(Value::Text(prefix));
                }
            } else {
                wh.push("e.parent = ?".into());
                args.push(Value::Integer(parent.rowid));
            }
        }
        (None, Some(lib)) => {
            if app.library(lib).is_none() || !key.may_use(lib) {
                return Err(ApiError::not_found());
            }
            wh.push("e.library = ?".into());
            args.push(Value::Text(lib.clone()));
            if let Some(p) = &q.path {
                wh.push("e.path = ?".into());
                args.push(Value::Text(
                    p.trim_start_matches('/').trim_end_matches('/').to_string(),
                ));
            }
        }
        _ => return Err(ApiError::invalid("give exactly one of parent or library")),
    }
    if q.path.is_some() && q.library.is_none() {
        return Err(ApiError::invalid("path needs library"));
    }
    if q.trashed.unwrap_or(false) {
        wh.push("e.trash_root = e.rowid".into());
    } else {
        wh.push("e.trashed_at IS NULL".into());
    }
    if let Some(k) = &q.kind {
        let kinds = k
            .split(',')
            .map(|s| {
                kind::parse(s.trim()).ok_or_else(|| ApiError::invalid(format!("unknown kind {s}")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        wh.push(format!("e.kind IN ({})", vec!["?"; kinds.len()].join(",")));
        args.extend(kinds.into_iter().map(|k| Value::Integer(kind::to_i64(k))));
    }
    if let Some(t) = &q.taken_after {
        wh.push("e.sort_taken >= ?".into());
        args.push(Value::Integer(parse_time(t)?));
    }
    if let Some(t) = &q.taken_before {
        wh.push("e.sort_taken < ?".into());
        args.push(Value::Integer(parse_time(t)?));
    }
    if let Some(s) = &q.sha256 {
        let sha = parse_sha(s)?;
        // Files of the given size not hashed yet are hashed now, so a lookup never misses them.
        if let (Some(size), Some(lib)) = (q.size, &q.library) {
            hash_candidates(app, lib, size)?;
        }
        wh.push("e.sha256 = ?".into());
        args.push(Value::Blob(sha));
    }
    // A lookup by hash answers "the server has these bytes": only with files on disk as indexed.
    let lookup = q.sha256.is_some();
    let duplicates = q.duplicates.unwrap_or(false);
    if duplicates {
        wh.push(
            "e.sha256 IN (SELECT sha256 FROM entries d WHERE d.library = e.library AND d.sha256 IS NOT NULL \
             AND d.trashed_at IS NULL AND d.missing = 0 GROUP BY d.sha256 HAVING COUNT(*) > 1)"
                .into(),
        );
    }

    // Order and keyset paging.
    let (expr, desc) = if duplicates {
        ("hex(e.sha256)", false)
    } else {
        let s = q.sort.as_deref().unwrap_or("name");
        let (name, desc) = s.strip_prefix('-').map(|n| (n, true)).unwrap_or((s, false));
        let expr = match name {
            "name" => "e.name",
            "taken" => "e.sort_taken",
            "modified" => "e.mtime_ns",
            "size" => "e.size",
            _ => return Err(ApiError::invalid(format!("unknown sort {name}"))),
        };
        (expr, desc)
    };
    if let Some(cur) = &q.cursor {
        let (v, rowid) = decode_cursor(cur).ok_or_else(|| ApiError::invalid("bad cursor"))?;
        wh.push(format!(
            "({expr}, e.rowid) {} (?, ?)",
            if desc { "<" } else { ">" }
        ));
        args.push(v);
        args.push(Value::Integer(rowid));
    }
    let dir = if desc { "DESC" } else { "ASC" };
    let sql = format!(
        "SELECT {ENTRY_COLS}, {expr} FROM {ENTRY_FROM} WHERE {} ORDER BY {expr} {dir}, e.rowid {dir} LIMIT {}",
        wh.join(" AND "),
        limit + 1
    );
    app.db
        .read(|c| {
            let mut st = c.prepare(&sql)?;
            let mut rows: Vec<(EntryRow, Value)> = st
                .query_map(rusqlite::params_from_iter(args.iter()), |r| {
                    Ok((EntryRow::from_row(r)?, r.get::<_, Value>(26)?))
                })?
                .collect::<Result<_, _>>()?;
            if lookup {
                rows.retain(|(r, _)| {
                    let Some(lib) = app.library(&r.library) else {
                        return false;
                    };
                    let there = crate::indexer::on_disk(lib, r);
                    if !there {
                        crate::indexer::forget_later(app, lib, &r.path);
                    }
                    there
                });
            }
            let next = if rows.len() > limit as usize {
                rows.truncate(limit as usize);
                rows.last().map(|(r, v)| encode_cursor(v, r.rowid))
            } else {
                None
            };
            let items = rows
                .iter()
                .map(|(r, _)| to_api(app, c, r, key))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Page { items, next })
        })
        .map_err(ApiError::from)
}

/// Hashes up to 64 unhashed files of `size` in a library (dedup lookups).
fn hash_candidates(app: &App, library: &str, size: u64) -> ApiResult<()> {
    let rows: Vec<EntryRow> = app.db.read(|c| {
        let mut st = c.prepare(&format!(
            "SELECT {ENTRY_COLS} FROM {ENTRY_FROM} WHERE e.library = ? AND e.size = ? AND e.sha256 IS NULL AND e.kind != 0 \
             AND e.trashed_at IS NULL AND e.missing = 0 LIMIT 64"
        ))?;
        let rows = st.query_map(rusqlite::params![library, size as i64], EntryRow::from_row)?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })?;
    for row in &rows {
        if let Err(e) = crate::indexer::hash_entry(app, row) {
            tracing::debug!(entry = %row.id, "hash failed: {e:#}");
        }
    }
    Ok(())
}

fn encode_cursor(v: &Value, rowid: i64) -> String {
    let jv = match v {
        Value::Integer(i) => serde_json::json!(i),
        Value::Text(s) => serde_json::json!(s),
        Value::Real(f) => serde_json::json!(f),
        _ => serde_json::Value::Null,
    };
    util::b64url(serde_json::json!([jv, rowid]).to_string().as_bytes())
}

fn decode_cursor(s: &str) -> Option<(Value, i64)> {
    let v: serde_json::Value = serde_json::from_slice(&util::b64url_decode(s)?).ok()?;
    let arr = v.as_array()?;
    let rowid = arr.get(1)?.as_i64()?;
    let val = match arr.first()? {
        serde_json::Value::Number(n) if n.is_i64() => Value::Integer(n.as_i64()?),
        serde_json::Value::Number(n) => Value::Real(n.as_f64()?),
        serde_json::Value::String(s) => Value::Text(s.clone()),
        _ => return None,
    };
    Some((val, rowid))
}

/// The value of an `If-Match` / `If-None-Match` header without quotes or a weak marker.
pub fn header_tag(headers: &HeaderMap, name: header::HeaderName) -> Option<String> {
    let v = headers.get(name)?.to_str().ok()?.trim();
    Some(v.trim_start_matches("W/").trim_matches('"').to_string())
}
