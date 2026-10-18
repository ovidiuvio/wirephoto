//! `POST /v1/batch` (api.md section 7): ordinary JSON requests run one after another through the same router,
//! so each behaves exactly as if it were sent alone.

use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Request, header};
use http_body_util::BodyExt;
use tower::ServiceExt;
use wirephoto_api::{BatchRequest, BatchResponse, SubResponse};

use super::auth::Caller;
use super::error::{ApiError, ApiResult};
use crate::app::App;

pub const MAX_BATCH: usize = 100;

fn allowed(method: &Method, path: &str) -> bool {
    let p = path.split('?').next().unwrap_or("");
    let entries = p == "/v1/entries" || p.starts_with("/v1/entries/");
    let bytes = p.contains("/content") || p.contains("/preview/") || p.contains("/children/");
    entries
        && !bytes
        && matches!(
            *method,
            Method::GET | Method::POST | Method::PATCH | Method::DELETE
        )
}

pub async fn post_batch(
    State(app): State<Arc<App>>,
    _caller: Caller,
    info: Option<axum::Extension<crate::audit::RequestInfo>>,
    headers: HeaderMap,
    Json(req): Json<BatchRequest>,
) -> ApiResult<Json<BatchResponse>> {
    if req.requests.len() > MAX_BATCH {
        return Err(ApiError::too_large(format!(
            "at most {MAX_BATCH} requests per batch"
        )));
    }
    let router = app.router.get().ok_or_else(ApiError::internal)?.clone();
    let mut responses = Vec::with_capacity(req.requests.len());
    for sub in req.requests {
        let method = Method::from_bytes(sub.method.to_ascii_uppercase().as_bytes())
            .unwrap_or(Method::OPTIONS);
        if !allowed(&method, &sub.path) {
            let e = ApiError::invalid(format!("{} {} cannot be batched", sub.method, sub.path));
            responses.push(SubResponse {
                status: e.status.as_u16(),
                body: serde_json::to_value(e.problem()).ok(),
            });
            continue;
        }
        let mut b = Request::builder().method(method).uri(&sub.path);
        // The batch's own credentials and origin apply to every request in it.
        for h in [header::AUTHORIZATION, header::ORIGIN] {
            if let Some(v) = headers.get(&h) {
                b = b.header(h, v.clone());
            }
        }
        for (k, v) in sub.headers.iter().flatten() {
            let Ok(name) = HeaderName::from_bytes(k.as_bytes()) else {
                continue;
            };
            if name == header::AUTHORIZATION || name == header::ORIGIN || name == header::HOST {
                continue;
            }
            if let Ok(v) = HeaderValue::from_str(v) {
                b = b.header(name, v);
            }
        }
        let body = match &sub.body {
            Some(v) => {
                b = b.header(header::CONTENT_TYPE, "application/json");
                Body::from(serde_json::to_vec(v).unwrap_or_default())
            }
            None => Body::empty(),
        };
        // Who is asking, for the audit trail of each request in it (they skip the access log's layer).
        if let Some(info) = &info {
            b = b.extension(info.0.clone());
        }
        let Ok(request) = b.body(body) else {
            let e = ApiError::invalid("malformed request");
            responses.push(SubResponse {
                status: 400,
                body: serde_json::to_value(e.problem()).ok(),
            });
            continue;
        };
        let resp = router
            .clone()
            .oneshot(request)
            .await
            .map_err(|_| ApiError::internal())?;
        let status = resp.status().as_u16();
        let bytes = resp
            .into_body()
            .collect()
            .await
            .map(|c| c.to_bytes())
            .unwrap_or_default();
        let body = if bytes.is_empty() {
            None
        } else {
            serde_json::from_slice(&bytes).ok()
        };
        responses.push(SubResponse { status, body });
    }
    Ok(Json(BatchResponse { responses }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_can_be_batched() {
        assert!(allowed(&Method::PATCH, "/v1/entries/ent_1"));
        assert!(allowed(&Method::GET, "/v1/entries?library=lib_x&sha256=00"));
        assert!(!allowed(&Method::PUT, "/v1/entries/ent_1/content"));
        assert!(!allowed(&Method::GET, "/v1/entries/ent_1/content"));
        assert!(!allowed(&Method::POST, "/v1/batch"));
        assert!(!allowed(&Method::GET, "/v1/changes"));
    }
}
