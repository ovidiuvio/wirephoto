//! `GET /v1/changes` (api.md section 8): catch up from a sequence number, or stay connected with
//! `Accept: text/event-stream`.

use std::convert::Infallible;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use wirephoto_api::{Change, ChangeType, Changes};

use super::auth::Caller;
use super::entries::to_api;
use super::error::{ApiError, ApiResult};
use crate::app::App;
use crate::auth::KeyRow;
use crate::db;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangesQuery {
    since: Option<i64>,
    library: Option<String>,
    limit: Option<u32>,
}

/// Changes after `since`, each with the entry as it is now (or its ID, library and parent once deleted).
fn read_changes(
    app: &App,
    key: &KeyRow,
    since: i64,
    library: Option<&str>,
    limit: u32,
) -> anyhow::Result<(Vec<Change>, i64)> {
    app.db.read(|c| {
        let mut st = c.prepare(
            "SELECT seq, type, entry, library, parent FROM changes WHERE seq > ?1 AND (?2 IS NULL OR library = ?2) ORDER BY seq LIMIT ?3",
        )?;
        let rows = st
            .query_map(rusqlite::params![since, library, limit], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, Option<String>>(4)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut last = since;
        let mut out = Vec::with_capacity(rows.len());
        for (seq, ty, entry, lib, parent) in rows {
            last = seq;
            if !key.may_use(&lib) {
                continue;
            }
            let kind = db::change_type(ty);
            let current = if kind == ChangeType::Deleted { None } else { db::entry_by_id(c, &entry)? };
            let entry = match current {
                Some(row) => serde_json::to_value(to_api(app, c, &row, key)?)?,
                None => serde_json::json!({ "id": entry, "library": lib, "parent": parent }),
            };
            out.push(Change { seq: seq as u64, kind, entry });
        }
        let next = if out.is_empty() && last == since { since.max(0) } else { last };
        Ok((out, next))
    })
}

fn pruned_through(app: &App) -> i64 {
    app.db
        .meta_get("pruned_through")
        .ok()
        .flatten()
        .and_then(|v| v.try_into().ok())
        .map(i64::from_le_bytes)
        .unwrap_or(0)
}

pub async fn get_changes(
    State(app): State<Arc<App>>,
    Caller(key): Caller,
    Query(q): Query<ChangesQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let limit = q.limit.unwrap_or(200);
    if !(1..=1000).contains(&limit) {
        return Err(ApiError::invalid("limit is 1 to 1000"));
    }
    if let Some(lib) = &q.library
        && (app.library(lib).is_none() || !key.may_use(lib))
    {
        return Err(ApiError::not_found());
    }
    let live = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/event-stream"));
    let last_event: Option<i64> = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok());
    let a = app.clone();
    let current =
        tokio::task::spawn_blocking(move || a.db.read(|c| Ok(db::current_seq(c)?))).await??;
    let since = match last_event.or(q.since) {
        Some(s) => s,
        None if !live => {
            return Ok(Json(Changes {
                changes: vec![],
                next: current as u64,
            })
            .into_response());
        }
        None => current,
    };
    let a = app.clone();
    if since > 0 && since < tokio::task::spawn_blocking(move || pruned_through(&a)).await? {
        return Err(ApiError::history_expired());
    }
    if !live {
        let a = app.clone();
        let lib = q.library.clone();
        let (changes, next) = tokio::task::spawn_blocking(move || {
            read_changes(&a, &key, since, lib.as_deref(), limit)
        })
        .await??;
        return Ok(Json(Changes {
            changes,
            next: next as u64,
        })
        .into_response());
    }
    let mut seq_rx = app.seq.subscribe();
    let library = q.library.clone();
    // Counted while open (`wirephoto status`): the guard leaves with the stream.
    struct Open(Arc<App>);
    impl Drop for Open {
        fn drop(&mut self) {
            self.0
                .metrics
                .change_feeds
                .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
    app.metrics
        .change_feeds
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let open = Open(app.clone());
    let stream = async_stream::stream! {
        let _open = open;
        let mut since = since;
        loop {
            let (a, k, lib) = (app.clone(), key.clone(), library.clone());
            let res = tokio::task::spawn_blocking(move || read_changes(&a, &k, since, lib.as_deref(), 500)).await;
            let Ok(Ok((changes, next))) = res else { break };
            for ch in &changes {
                if let Ok(data) = serde_json::to_string(ch) {
                    yield Ok::<_, Infallible>(Event::default().id(ch.seq.to_string()).event("change").data(data));
                }
            }
            since = next;
            if changes.len() < 500 && seq_rx.changed().await.is_err() {
                break;
            }
        }
    };
    Ok(Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response())
}
