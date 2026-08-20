use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// Every failure the API can express, mapped to a status + stable error code.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("missing or invalid capability token")]
    Unauthorized,
    #[error("capability does not cover {facet}:{verb}")]
    Forbidden { facet: String, verb: String },
    #[error("no such item")]
    NotFound,
    #[error("facet {0} is not registered")]
    UnknownFacet(String),
    #[error("body violates the {facet} schema: {detail}")]
    SchemaViolation { facet: String, detail: String },
    #[error("revision conflict")]
    RevisionConflict,
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error("internal: {0}")]
    Internal(String),
}

impl Error {
    fn code(&self) -> (StatusCode, &'static str) {
        match self {
            Error::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized"),
            Error::Forbidden { .. } => (StatusCode::FORBIDDEN, "forbidden"),
            Error::NotFound => (StatusCode::NOT_FOUND, "not_found"),
            Error::UnknownFacet(_) => (StatusCode::UNPROCESSABLE_ENTITY, "unknown_facet"),
            Error::SchemaViolation { .. } => (StatusCode::UNPROCESSABLE_ENTITY, "schema_violation"),
            Error::RevisionConflict => (StatusCode::CONFLICT, "revision_conflict"),
            Error::Conflict(_) => (StatusCode::CONFLICT, "conflict"),
            Error::BadRequest(_) => (StatusCode::BAD_REQUEST, "bad_request"),
            Error::Db(_) | Error::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        }
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        // Unique-index violations surface as conflicts, not 500s.
        let this = match self {
            Error::Db(sqlx::Error::Database(e)) if e.is_unique_violation() => {
                Error::Conflict(e.message().to_string())
            }
            other => other,
        };
        let (status, code) = this.code();
        (status, Json(json!({ "error": code, "detail": this.to_string() }))).into_response()
    }
}

pub type Result<T> = std::result::Result<T, Error>;
