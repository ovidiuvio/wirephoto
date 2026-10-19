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
        uploads: wirephoto_api::UploadsInfo {
            tus: "/v1/tus".into(),
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
            Method::OPTIONS,
        ])
        .allow_headers([
            header::AUTHORIZATION,
            header::CONTENT_TYPE,
            header::IF_MATCH,
            header::IF_NONE_MATCH,
            header::IF_RANGE,
            header::RANGE,
            header::CONTENT_RANGE,
            header::ACCEPT,
            h("content-digest"),
            h("last-event-id"),
            // tus (/v1/tus)
            h("tus-resumable"),
            h("upload-length"),
            h("upload-offset"),
            h("upload-metadata"),
            h("upload-checksum"),
            h("upload-concat"),
            h("upload-defer-length"),
            h("x-http-method-override"),
            h("x-requested-with"),
        ])
        .expose_headers([
            header::ETAG,
            header::LOCATION,
            header::CONTENT_RANGE,
            header::CONTENT_LENGTH,
            h("server-timing"),
            h("repr-digest"),
            // tus (/v1/tus)
            h("upload-offset"),
            h("upload-length"),
            h("upload-expires"),
            h("tus-resumable"),
            h("tus-version"),
            h("tus-extension"),
            h("tus-max-size"),
            h("tus-checksum-algorithm"),
            h("wirephoto-entry"),
            h("wirephoto-entry-name"),
            h("wirephoto-stored"),
        ])
        .max_age(Duration::from_secs(7200))
        .allow_private_network(true)
}

/// The access log: one line per request (`wirephoto::access`, `[log] access`), with how long the response took
/// to start (a long download or stream goes on after), its size when known, the key and the client. Refused
/// requests (401, 403) also go to the audit trail. A URL's signature is never written down.
async fn access_log(
    State(app): State<Arc<App>>,
    mut req: axum::extract::Request,
    next: middleware::Next,
) -> axum::response::Response {
    let started = Instant::now();
    let client = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|c| c.0.ip());
    let info = crate::audit::RequestInfo {
        client,
        ..Default::default()
    };
    req.extensions_mut().insert(info.clone());
    let method = req.method().clone();
    let path = redacted(req.uri());
    let bytes_in = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    // Requests come first: background work slows down while the API is in use. Health checks and the live
    // changes stream (an app keeps it open) do not count.
    let counts = !(path == "/healthz" || path.starts_with("/v1/changes"));
    if counts {
        app.api_active();
    }
    let res = next.run(req).await;
    if counts {
        app.api_active();
    }
    let status = res.status().as_u16();
    let ms = started.elapsed().as_millis() as u64;
    // A whole body knows its length; a stream (batches, live changes) does not yet.
    let bytes = res
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .or_else(|| axum::body::HttpBody::size_hint(res.body()).exact());
    app.metrics.request(
        crate::metrics::kind_of(method.as_str(), &path),
        status,
        bytes,
        bytes_in,
        ms,
    );
    if app.config.log.access {
        let size = bytes.map(human_bytes).unwrap_or_else(|| "-".into());
        tracing::info!(
            target: "wirephoto::access",
            "{method} {path} {status} {ms} ms {size} key={} client={}",
            info.key_name().unwrap_or("-"),
            client.map(|c| c.to_string()).unwrap_or_else(|| "-".into())
        );
    }
    if status == 401 || status == 403 {
        let app = app.clone();
        let detail = format!("{method} {path}");
        let info2 = info.clone();
        tokio::task::spawn_blocking(move || {
            crate::audit::record(
                &app,
                crate::audit::Event::new("denied", detail)
                    .by(Some(&info2), None)
                    .status(status),
            )
        });
    }
    res
}

/// The request's path and query, with any `sig` replaced: a signature in a log is a key to that file.
fn redacted(uri: &axum::http::Uri) -> String {
    match uri.query() {
        None => uri.path().to_string(),
        Some(q) => {
            let q: Vec<String> = q
                .split('&')
                .map(|kv| {
                    if kv.starts_with("sig=") {
                        "sig=…".to_string()
                    } else {
                        kv.to_string()
                    }
                })
                .collect();
            format!("{}?{}", uri.path(), q.join("&"))
        }
    }
}

fn human_bytes(b: u64) -> String {
    match b {
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1u64 << 20) as f64),
        b if b >= 1 << 10 => format!("{:.1} KB", b as f64 / 1024.0),
        b => format!("{b} B"),
    }
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
        .merge(
            Router::new()
                .route(
                    "/v1/tus",
                    post(crate::tus::create).options(crate::tus::options),
                )
                .route(
                    "/v1/tus/{id}",
                    axum::routing::head(crate::tus::head)
                        .patch(crate::tus::patch)
                        .post(crate::tus::patch_override)
                        .get(crate::tus::outcome)
                        .delete(crate::tus::terminate),
                )
                .route_layer(middleware::from_fn(crate::tus::resumable)),
        )
        .route("/v1/batch", post(batch::post_batch))
        .route("/v1/changes", get(changes::get_changes))
        .fallback(|| async { error::ApiError::not_found() })
        .with_state(app.clone());
    let _ = app.router.set(api.clone());
    api.layer(middleware::from_fn(cross_origin_headers))
        .layer(middleware::from_fn_with_state(app.clone(), access_log))
        .layer(cors(app))
        .layer(middleware::from_fn(crate::tus::discovery))
        // Failures are in the access log; tower-http would log every 504 not-cached as an error.
        .layer(tower_http::trace::TraceLayer::new_for_http().on_failure(()))
}
