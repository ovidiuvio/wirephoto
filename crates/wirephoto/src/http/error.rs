//! Errors as RFC 9457 problem details with a stable `code` (api.md section 9).

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use wirephoto_api::Problem;

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub title: &'static str,
    pub detail: Option<String>,
}

pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    fn new(status: StatusCode, code: &'static str, title: &'static str) -> ApiError {
        ApiError {
            status,
            code,
            title,
            detail: None,
        }
    }

    pub fn detail(mut self, d: impl Into<String>) -> ApiError {
        self.detail = Some(d.into());
        self
    }

    pub fn invalid(d: impl Into<String>) -> ApiError {
        Self::new(
            StatusCode::BAD_REQUEST,
            "invalid-request",
            "The request is not valid",
        )
        .detail(d)
    }
    pub fn digest_mismatch() -> ApiError {
        Self::new(
            StatusCode::BAD_REQUEST,
            "digest-mismatch",
            "The bytes received do not match Content-Digest",
        )
    }
    pub fn unauthenticated() -> ApiError {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
            "A valid API key is required",
        )
    }
    pub fn forbidden(d: impl Into<String>) -> ApiError {
        Self::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "This key may not do that",
        )
        .detail(d)
    }
    pub fn not_found() -> ApiError {
        Self::new(StatusCode::NOT_FOUND, "not-found", "Not found")
    }
    pub fn conflict(d: impl Into<String>) -> ApiError {
        Self::new(
            StatusCode::CONFLICT,
            "conflict",
            "That conflicts with the current state",
        )
        .detail(d)
    }
    pub fn history_expired() -> ApiError {
        Self::new(
            StatusCode::GONE,
            "history-expired",
            "Changes that old are no longer kept; list again",
        )
    }
    pub fn version_mismatch(d: impl Into<String>) -> ApiError {
        Self::new(
            StatusCode::PRECONDITION_FAILED,
            "version-mismatch",
            "The entry changed since you read it",
        )
        .detail(d)
    }
    pub fn too_large(d: impl Into<String>) -> ApiError {
        Self::new(StatusCode::PAYLOAD_TOO_LARGE, "too-large", "Over a limit").detail(d)
    }
    pub fn unsupported_media() -> ApiError {
        Self::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported-media",
            "This file cannot be previewed",
        )
    }
    /// The preview could not be made this time (a decode worker crashed or hung, the file could not be opened):
    /// not a verdict on the file, unlike `unsupported-media`.
    pub fn preview_failed(d: impl Into<String>) -> ApiError {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "preview-failed",
            "The preview could not be made right now; try again",
        )
        .detail(d)
    }
    /// Asked for only what is cached (`only-if-cached`), and it is not.
    pub fn not_cached() -> ApiError {
        Self::new(
            StatusCode::GATEWAY_TIMEOUT,
            "not-cached",
            "That preview is not made yet, and only a cached one was asked for",
        )
    }
    pub fn storage_unavailable(d: impl Into<String>) -> ApiError {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "storage-unavailable",
            "The library's storage cannot be reached",
        )
        .detail(d)
    }
    pub fn internal() -> ApiError {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "Something went wrong on the server",
        )
    }

    pub fn problem(&self) -> Problem {
        Problem {
            kind: format!("https://wirephoto.dev/errors/{}", self.code),
            title: self.title.into(),
            status: self.status.as_u16(),
            code: self.code.into(),
            detail: self.detail.clone(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut r = (self.status, axum::Json(self.problem())).into_response();
        r.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/problem+json"),
        );
        r
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> ApiError {
        if let Some(io) = e.downcast_ref::<std::io::Error>() {
            return io_error(io);
        }
        tracing::error!("{e:#}");
        ApiError::internal()
    }
}

impl From<std::io::Error> for ApiError {
    fn from(e: std::io::Error) -> ApiError {
        io_error(&e)
    }
}

impl From<rusqlite::Error> for ApiError {
    fn from(e: rusqlite::Error) -> ApiError {
        tracing::error!("database: {e}");
        ApiError::internal()
    }
}

impl From<tokio::task::JoinError> for ApiError {
    fn from(e: tokio::task::JoinError) -> ApiError {
        tracing::error!("task: {e}");
        ApiError::internal()
    }
}

fn io_error(e: &std::io::Error) -> ApiError {
    use std::io::ErrorKind::*;
    match e.kind() {
        NotFound => ApiError::not_found().detail("the file is not on disk"),
        AlreadyExists => ApiError::conflict("the name is taken"),
        PermissionDenied | ReadOnlyFilesystem => {
            ApiError::forbidden(format!("the server may not write there: {e}"))
        }
        _ => {
            tracing::error!("io: {e}");
            ApiError::storage_unavailable(e.to_string())
        }
    }
}
