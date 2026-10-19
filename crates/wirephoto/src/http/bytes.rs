//! Reading bytes: originals and previews (api.md section 5).

use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use wirephoto_api::{Kind, PreviewsRequest};

use super::auth::{ByteCaller, Caller};
use super::entries::{etag_header, header_tag, load};
use super::error::{ApiError, ApiResult};
use crate::app::App;
use crate::db::EntryRow;
use crate::preview::{self, Source};
use crate::util;

pub const MAX_PREVIEWS: usize = 500;

async fn load_file(app: &Arc<App>, key: crate::auth::KeyRow, id: String) -> ApiResult<EntryRow> {
    let a = app.clone();
    let row =
        tokio::task::spawn_blocking(move || a.db.read(|c| Ok(load(c, &key, &id)))).await???;
    if row.kind == Kind::Folder {
        return Err(ApiError::invalid("a folder has no content"));
    }
    Ok(row)
}

/// A single `bytes=start-end` range against `len`; `None` when absent, multiple or unsatisfiable (then the whole
/// file is sent, which RFC 9110 allows).
fn parse_range(v: &str, len: u64) -> Option<(u64, u64)> {
    let spec = v.strip_prefix("bytes=")?;
    if spec.contains(',') || len == 0 {
        return None;
    }
    let (a, b) = spec.split_once('-')?;
    let (start, end) = if a.is_empty() {
        let n: u64 = b.parse().ok()?;
        (len.saturating_sub(n), len - 1)
    } else {
        let s: u64 = a.parse().ok()?;
        let e: u64 = if b.is_empty() {
            len - 1
        } else {
            b.parse::<u64>().ok()?.min(len - 1)
        };
        (s, e)
    };
    (start <= end && start < len).then_some((start, end))
}

pub async fn get_content(
    State(app): State<Arc<App>>,
    ByteCaller(key): ByteCaller,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let row = load_file(&app, key, id).await?;
    let lib = app
        .library(&row.library)
        .ok_or_else(ApiError::not_found)?
        .clone();
    let etag = row.etag();
    if header_tag(&headers, header::IF_NONE_MATCH).is_some_and(|t| t == etag || t == "*") {
        let mut r = StatusCode::NOT_MODIFIED.into_response();
        r.headers_mut().insert(header::ETAG, etag_header(&etag));
        return Ok(r);
    }
    let (fs, path) = (lib.fs.clone(), row.path.clone());
    let std_file = tokio::task::spawn_blocking(move || fs.open_file(&path)).await??;
    let len = std_file.metadata()?.len();
    let mut file = tokio::fs::File::from_std(std_file);

    let range_ok = header_tag(&headers, header::IF_RANGE).is_none_or(|t| t == etag);
    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .filter(|_| range_ok)
        .and_then(|v| parse_range(v, len));
    let mut builder = Response::builder()
        .header(header::ETAG, etag_header(&etag))
        .header(header::ACCEPT_RANGES, "bytes")
        .header(
            header::CONTENT_TYPE,
            row.mime.as_deref().unwrap_or("application/octet-stream"),
        )
        .header(header::CACHE_CONTROL, "private, no-cache");
    if let Some(sha) = &row.sha256 {
        builder = builder.header("repr-digest", format!("sha-256=:{}:", util::b64(sha)));
    }
    let body = match range {
        Some((start, end)) => {
            file.seek(std::io::SeekFrom::Start(start)).await?;
            builder = builder
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{len}"))
                .header(header::CONTENT_LENGTH, end - start + 1);
            Body::from_stream(tokio_util::io::ReaderStream::with_capacity(
                file.take(end - start + 1),
                256 << 10,
            ))
        }
        None => {
            builder = builder
                .status(StatusCode::OK)
                .header(header::CONTENT_LENGTH, len);
            Body::from_stream(tokio_util::io::ReaderStream::with_capacity(file, 256 << 10))
        }
    };
    builder.body(body).map_err(|_| ApiError::internal())
}

#[derive(Debug, Deserialize)]
pub struct PreviewQuery {
    v: Option<String>,
    sig: Option<String>,
    /// `only`: answer from the cache or not at all (`504`), never by rendering from the original.
    cached: Option<String>,
}

fn parse_size(file: &str) -> ApiResult<u32> {
    let size = file
        .strip_suffix(".jpg")
        .and_then(|s| s.parse::<u32>().ok())
        .ok_or_else(ApiError::not_found)?;
    if !preview::valid_size(size) {
        return Err(ApiError::not_found().detail(format!("preview sizes are {:?}", preview::SIZES)));
    }
    Ok(size)
}

pub async fn get_preview(
    State(app): State<Arc<App>>,
    ByteCaller(key): ByteCaller,
    Path((id, file)): Path<(String, String)>,
    Query(q): Query<PreviewQuery>,
    headers: axum::http::HeaderMap,
) -> ApiResult<Response> {
    // HTTP's `Cache-Control: only-if-cached`, or `?cached=only` where a header cannot be sent (an <img>).
    let only_cached = q.cached.as_deref() == Some("only")
        || headers
            .get_all(header::CACHE_CONTROL)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .any(|v| {
                v.split(',')
                    .any(|d| d.trim().eq_ignore_ascii_case("only-if-cached"))
            });
    let size = parse_size(&file)?;
    let row = load_file(&app, key, id).await?;
    if !row.previewable {
        return Err(ApiError::unsupported_media());
    }
    let etag = row.preview_version();
    if let Some(v) = &q.v
        && *v != etag
    {
        let mut loc = format!("/v1/entries/{}/preview/{size}.jpg?v={etag}", row.id);
        if let Some(sig) = &q.sig {
            loc.push_str(&format!("&sig={sig}"));
        }
        if only_cached {
            loc.push_str("&cached=only");
        }
        return Ok((StatusCode::TEMPORARY_REDIRECT, [(header::LOCATION, loc)]).into_response());
    }
    let lib = app
        .library(&row.library)
        .ok_or_else(ApiError::not_found)?
        .clone();
    let (bytes, timing) = if only_cached {
        let b = app
            .previews
            .get_cached(&row, size)
            .await
            .ok_or_else(ApiError::not_cached)?;
        (
            b,
            crate::preview::Timing {
                hit: true,
                render_ms: 0.0,
            },
        )
    } else {
        app.previews
            .get(&row, Source::File(lib.fs.clone(), row.path.clone()), size)
            .await
            .map_err(|e| preview_error(&row, &e))?
    };
    let cache = if q.v.is_some() {
        "private, max-age=31536000, immutable"
    } else {
        "private, no-cache"
    };
    let st = if timing.hit {
        "cache;desc=hit".to_string()
    } else {
        format!("cache;desc=miss, render;dur={:.1}", timing.render_ms)
    };
    Ok((
        [
            (header::CONTENT_TYPE, HeaderValue::from_static("image/jpeg")),
            (header::CACHE_CONTROL, HeaderValue::from_static(cache)),
            (header::ETAG, etag_header(&format!("{etag}-{size}"))),
            (
                header::HeaderName::from_static("server-timing"),
                HeaderValue::from_str(&st).unwrap(),
            ),
        ],
        bytes,
    )
        .into_response())
}

/// One frame of `application/vnd.wirephoto.frames`.
fn frame(index: u32, status: u16, payload: &[u8]) -> bytes::Bytes {
    let mut b = Vec::with_capacity(12 + payload.len());
    b.extend_from_slice(&index.to_le_bytes());
    b.extend_from_slice(&status.to_le_bytes());
    b.extend_from_slice(&0u16.to_le_bytes());
    b.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    b.extend_from_slice(payload);
    b.into()
}

fn problem_bytes(e: &ApiError) -> Vec<u8> {
    serde_json::to_vec(&e.problem()).unwrap_or_default()
}

pub async fn post_previews(
    State(app): State<Arc<App>>,
    Caller(key): Caller,
    Json(req): Json<PreviewsRequest>,
) -> ApiResult<Response> {
    if req.ids.len() > MAX_PREVIEWS {
        return Err(ApiError::too_large(format!(
            "at most {MAX_PREVIEWS} previews per request"
        )));
    }
    if !preview::valid_size(req.size) {
        return Err(ApiError::invalid(format!(
            "preview sizes are {:?}",
            preview::SIZES
        )));
    }
    let size = req.size;
    let a = app.clone();
    let ids = req.ids.clone();
    let rows: Vec<ApiResult<EntryRow>> = tokio::task::spawn_blocking(move || {
        a.db.read(|c| Ok(ids.iter().map(|id| load(c, &key, id)).collect::<Vec<_>>()))
    })
    .await??;

    // Order is priority: earlier IDs start first; frames go out as they are ready. Dropping the body (the client
    // went away) drops the stream and with it every rendering not yet finished.
    let parallel = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        * 2;
    let jobs = rows.into_iter().enumerate().map(move |(i, row)| {
        let app = app.clone();
        async move {
            let row = match row {
                Ok(r) => r,
                Err(e) => return frame(i as u32, e.status.as_u16(), &problem_bytes(&e)),
            };
            if !row.previewable {
                let e = ApiError::unsupported_media();
                return frame(i as u32, e.status.as_u16(), &problem_bytes(&e));
            }
            let Some(lib) = app.library(&row.library).cloned() else {
                let e = ApiError::not_found();
                return frame(i as u32, 404, &problem_bytes(&e));
            };
            match app
                .previews
                .get(&row, Source::File(lib.fs.clone(), row.path.clone()), size)
                .await
            {
                Ok((b, _)) => frame(i as u32, 200, &b),
                Err(e) => {
                    let e = preview_error(&row, &e);
                    frame(i as u32, e.status.as_u16(), &problem_bytes(&e))
                }
            }
        }
    });
    let stream = futures::stream::iter(jobs)
        .buffer_unordered(parallel)
        .map(Ok::<_, std::io::Error>);
    Response::builder()
        .header(header::CONTENT_TYPE, "application/vnd.wirephoto.frames")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from_stream(stream))
        .map_err(|_| ApiError::internal())
}

/// A file the decoder cannot read is `415`; anything else (a crashed worker, a file that could not be opened)
/// is `503`, worth trying again, and logged: it says the server has a problem, not the file.
fn preview_error(row: &crate::db::EntryRow, e: &anyhow::Error) -> ApiError {
    if crate::decode::is_decode_failure(e) {
        tracing::debug!(entry = %row.id, path = %row.path, "cannot be previewed: {e:#}");
        ApiError::unsupported_media()
    } else {
        tracing::warn!(entry = %row.id, path = %row.path, "preview failed: {e:#}");
        ApiError::preview_failed(format!("{e:#}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-99", 1000), Some((0, 99)));
        assert_eq!(parse_range("bytes=900-", 1000), Some((900, 999)));
        assert_eq!(parse_range("bytes=-100", 1000), Some((900, 999)));
        assert_eq!(parse_range("bytes=990-2000", 1000), Some((990, 999)));
        assert_eq!(parse_range("bytes=1000-", 1000), None);
        assert_eq!(parse_range("bytes=0-1,5-6", 1000), None);
    }
}
