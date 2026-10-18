//! Who is calling: the API key from `Authorization: Bearer`, or the `sig` of a URL the server handed out (bytes
//! and previews only). A key tied to an origin is refused from any other browser origin.

use std::sync::Arc;

use axum::extract::FromRequestParts;
use axum::http::{HeaderMap, header, request::Parts};
use wirephoto_api::Scope;

use super::error::ApiError;
use crate::app::App;
use crate::auth::{KeyRow, Keys, normalize_origin};

/// A caller with a valid key.
pub struct Caller(pub KeyRow);

impl Caller {
    pub fn require_write(&self) -> Result<(), ApiError> {
        if self.0.scope == Scope::Write {
            Ok(())
        } else {
            Err(ApiError::forbidden("this key is read-only"))
        }
    }

    pub fn require_library(&self, library: &str) -> Result<(), ApiError> {
        if self.0.may_use(library) {
            Ok(())
        } else {
            Err(ApiError::not_found())
        }
    }
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let v = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = v.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
}

fn check_origin(headers: &HeaderMap, key: &KeyRow) -> Result<(), ApiError> {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return Ok(());
    };
    // Same-origin requests to the server itself carry no key restriction worth enforcing beyond the key's own.
    match (&key.origin, normalize_origin(origin)) {
        (Some(allowed), Some(o)) if *allowed == o => Ok(()),
        (None, _) => Err(ApiError::forbidden("this key is for scripts, not browsers")),
        _ => Err(ApiError::forbidden("this key belongs to another origin")),
    }
}

async fn by_token(app: &Arc<App>, token: String) -> Result<KeyRow, ApiError> {
    let a = app.clone();
    let key = tokio::task::spawn_blocking(move || {
        let k = Keys::by_token(&a.db, &token)?;
        if let Some(k) = &k {
            Keys::touch(&a.db, k);
        }
        anyhow::Ok(k)
    })
    .await??;
    key.ok_or_else(ApiError::unauthenticated)
}

impl FromRequestParts<Arc<App>> for Caller {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        app: &Arc<App>,
    ) -> Result<Self, Self::Rejection> {
        let token = bearer(&parts.headers)
            .ok_or_else(ApiError::unauthenticated)?
            .to_string();
        let key = by_token(app, token).await?;
        check_origin(&parts.headers, &key)?;
        Ok(Caller(key))
    }
}

/// An optional caller, for `GET /v1`.
pub struct MaybeCaller(pub Option<KeyRow>);

impl FromRequestParts<Arc<App>> for MaybeCaller {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        app: &Arc<App>,
    ) -> Result<Self, Self::Rejection> {
        match bearer(&parts.headers) {
            None => Ok(MaybeCaller(None)),
            Some(t) => Ok(MaybeCaller(Some(by_token(app, t.to_string()).await?))),
        }
    }
}

/// A caller of a byte endpoint (`/v1/entries/{id}/content`, `/preview/...`): a key header, or a `sig` valid for
/// that entry.
pub struct ByteCaller(pub KeyRow);

impl FromRequestParts<Arc<App>> for ByteCaller {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        app: &Arc<App>,
    ) -> Result<Self, Self::Rejection> {
        if let Some(t) = bearer(&parts.headers) {
            let key = by_token(app, t.to_string()).await?;
            check_origin(&parts.headers, &key)?;
            return Ok(ByteCaller(key));
        }
        let entry = parts
            .uri
            .path()
            .strip_prefix("/v1/entries/")
            .and_then(|r| r.split('/').next())
            .unwrap_or("");
        let sig = parts
            .uri
            .query()
            .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("sig=")))
            .ok_or_else(ApiError::unauthenticated)?;
        let sig = percent_encoding::percent_decode_str(sig)
            .decode_utf8_lossy()
            .to_string();
        let key_id = app
            .keys
            .verify(&sig, entry)
            .ok_or_else(ApiError::unauthenticated)?;
        let a = app.clone();
        let key = tokio::task::spawn_blocking(move || Keys::by_id(&a.db, &key_id))
            .await??
            .ok_or_else(ApiError::unauthenticated)?;
        if parts.headers.contains_key(header::ORIGIN) {
            check_origin(&parts.headers, &key)?;
        }
        Ok(ByteCaller(key))
    }
}
