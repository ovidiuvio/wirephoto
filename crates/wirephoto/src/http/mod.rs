//! The HTTP API (docs/api.md).

pub mod auth;
pub mod batch;
pub mod bytes;
pub mod changes;
pub mod entries;
pub mod error;

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderName, HeaderValue, Method, header};
use axum::routing::{get, post, put};
use axum::{Router, middleware};
use tower_http::cors::{AllowOrigin, CorsLayer};
use wirephoto_api::{KeyInfo, Library, Limits, PreviewInfo, ServerInfo, Watch};

use crate::app::App;
use crate::auth::Keys;
use crate::preview;
use auth::{Caller, MaybeCaller};
use error::ApiResult;

async fn server_info(MaybeCaller(key): MaybeCaller) -> Json<ServerInfo> {
    Json(ServerInfo {
        name: "wirephoto".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        previews: PreviewInfo {
            sizes: preview::SIZES.to_vec(),
        },
        limits: Limits {
            page_size: entries::PAGE_MAX,
            batch: batch::MAX_BATCH as u32,
            previews: bytes::MAX_PREVIEWS as u32,
        },
        key: key.map(|k| KeyInfo {
            name: k.name,
            scope: k.scope,
            libraries: k.libraries,
        }),
    })
}

fn library_json(lib: &crate::app::Library) -> Library {
    Library {
        id: lib.api_id.clone(),
        name: lib.cfg.name.clone(),
        root: lib.root_id.read().clone(),
        read_only: lib.cfg.read_only,
        trash_days: lib.cfg.trash_days,
        indexing: lib.status.get(),
        watch: if lib.live.load(std::sync::atomic::Ordering::Relaxed) {
            Watch::Live
        } else {
            Watch::Polling
        },
    }
}

async fn libraries(State(app): State<Arc<App>>, Caller(key): Caller) -> Json<Vec<Library>> {
    Json(
        app.libraries
            .iter()
            .filter(|l| key.may_use(&l.api_id))
            .map(|l| library_json(l))
            .collect(),
    )
}

async fn library(
    State(app): State<Arc<App>>,
    Caller(key): Caller,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> ApiResult<Json<Library>> {
    let lib = app
        .library(&id)
        .filter(|l| key.may_use(&l.api_id))
        .ok_or_else(error::ApiError::not_found)?;
    Ok(Json(library_json(lib)))
}

async fn healthz() -> &'static str {
    "ok"
}

/// Origins holding a key, re-read at most every few seconds (keys are managed by another process).
async fn origin_allowed(app: Arc<App>, origin: HeaderValue) -> bool {
    let Some(origin) = origin.to_str().ok().and_then(crate::auth::normalize_origin) else {
        return false;
    };
    {
        let cache = app.origins.read();
        if cache.0.elapsed() < Duration::from_secs(5) {
            return cache.1.contains(&origin);
        }
    }
    let a = app.clone();
    let fresh = tokio::task::spawn_blocking(move || Keys::origins(&a.db))
        .await
        .ok()
        .and_then(|r| r.ok())
        .unwrap_or_default();
    let ok = fresh.contains(&origin);
    *app.origins.write() = (Instant::now(), fresh);
    ok
}

fn cors(app: Arc<App>) -> CorsLayer {
    let h = |s: &'static str| HeaderName::from_static(s);
    CorsLayer::new()
        .allow_origin(AllowOrigin::async_predicate(move |origin, _parts| {
            origin_allowed(app.clone(), origin)
        }))
        .allow_methods([
            Method::GET,
            Method::HEAD,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
        ])
        .allow_headers([
            header::AUTHORIZATION,
            header::CONTENT_TYPE,
            header::IF_MATCH,
            header::IF_NONE_MATCH,
            header::IF_RANGE,
            header::RANGE,
            header::ACCEPT,
            h("content-digest"),
            h("last-event-id"),
        ])
        .expose_headers([
            header::ETAG,
            header::LOCATION,
            header::CONTENT_RANGE,
            header::CONTENT_LENGTH,
            h("server-timing"),
            h("repr-digest"),
        ])
        .max_age(Duration::from_secs(7200))
        .allow_private_network(true)
}

async fn cross_origin_headers(
    req: axum::extract::Request,
    next: middleware::Next,
) -> axum::response::Response {
    let mut r = next.run(req).await;
    let h = r.headers_mut();
    // Lets apps on other origins read Server-Timing in their own performance tools.
    h.insert(
        HeaderName::from_static("timing-allow-origin"),
        HeaderValue::from_static("*"),
    );
    // Lets a cross-origin isolated page (COEP require-corp) show previews in <img> by their signed URLs; the
    // signature, not the embedding origin, is what guards them.
    h.insert(
        HeaderName::from_static("cross-origin-resource-policy"),
        HeaderValue::from_static("cross-origin"),
    );
    r
}

pub fn router(app: Arc<App>) -> Router {
    let api = Router::new()
        .route("/healthz", get(healthz))
        .route("/v1", get(server_info))
        .route("/v1/libraries", get(libraries))
        .route("/v1/libraries/{id}", get(library))
        .route(
            "/v1/entries",
            get(entries::list_entries).post(crate::write::post_entry),
        )
        .route(
            "/v1/entries/{id}",
            get(entries::get_entry)
                .patch(crate::write::patch_entry)
                .delete(crate::write::delete_entry),
        )
        .route(
            "/v1/entries/{id}/content",
            get(bytes::get_content).put(crate::write::put_content),
        )
        .route("/v1/entries/{id}/preview/{file}", get(bytes::get_preview))
        .route(
            "/v1/entries/{folder}/children/{name}",
            put(crate::write::put_child),
        )
        .route("/v1/previews", post(bytes::post_previews))
        .route("/v1/batch", post(batch::post_batch))
        .route("/v1/changes", get(changes::get_changes))
        .fallback(|| async { error::ApiError::not_found() })
        .with_state(app.clone());
    let _ = app.router.set(api.clone());
    api.layer(middleware::from_fn(cross_origin_headers))
        .layer(cors(app))
        .layer(tower_http::trace::TraceLayer::new_for_http())
}
