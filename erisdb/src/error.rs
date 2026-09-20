use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// Every failure the API can express, mapped to a status + stable error code.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("missing or invalid capability token")]
    Unauthorized,
    #[error("capability does not grant {permission}")]
    Forbidden { permission: String },
    #[error("no such item")]
    NotFound,
    #[error("facet {0} is not registered")]
    UnknownFacet(String),
    #[error("body violates the {facet} schema: {detail}")]
    SchemaViolation { facet: String, detail: String },
    #[error("plugin {0} is not installed")]
    PluginNotFound(String),
    #[error("plugin {plugin} has no operation {operation}")]
    PluginOperationNotFound { plugin: String, operation: String },
    #[error("input violates the {plugin}.{operation} schema: {detail}")]
    PluginSchemaViolation { plugin: String, operation: String, detail: String },
    #[error("plugin unavailable: {0}")]
    PluginUnavailable(String),
    #[error("plugin failed: {0}")]
    PluginFailed(String),
    #[error("revision conflict")]
    RevisionConflict,
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("too many requests")]
    TooManyRequests,
    #[error("capacity reached; retry shortly")]
    Unavailable,
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
            Error::PluginNotFound(_) | Error::PluginOperationNotFound { .. } => {
                (StatusCode::NOT_FOUND, "plugin_not_found")
            }
            Error::PluginSchemaViolation { .. } => {
                (StatusCode::UNPROCESSABLE_ENTITY, "plugin_schema_violation")
            }
            Error::PluginUnavailable(_) => (StatusCode::SERVICE_UNAVAILABLE, "plugin_unavailable"),
            Error::PluginFailed(_) => (StatusCode::BAD_GATEWAY, "plugin_failed"),
            Error::RevisionConflict => (StatusCode::CONFLICT, "revision_conflict"),
            Error::Conflict(_) => (StatusCode::CONFLICT, "conflict"),
            Error::BadRequest(_) => (StatusCode::BAD_REQUEST, "bad_request"),
            Error::TooManyRequests => (StatusCode::TOO_MANY_REQUESTS, "too_many_requests"),
            Error::Unavailable => (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
            Error::Db(_) | Error::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        }
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        // Unique-index violations surface as conflicts, not 500s.
        let this = match self {
            Error::Db(sqlx::Error::Database(e)) if e.is_unique_violation() => {
                tracing::warn!(error = %e, "unique violation");
                Error::Conflict("that value already exists".into())
            }
            other => other,
        };
        let (status, code) = this.code();
        // A store or internal failure is the operator's to read, not the
        // caller's: the raw text carries constraint names, column names and
        // query fragments. It goes to the log; the caller gets the code.
        let detail = match &this {
            Error::Db(e) => {
                tracing::error!(error = %e, "store failure");
                "the store failed; see the server log".to_string()
            }
            Error::Internal(e) => {
                tracing::error!(error = %e, "internal failure");
                "internal failure; see the server log".to_string()
            }
            Error::PluginUnavailable(e) => {
                tracing::error!(error = %e, "plugin unavailable");
                "plugin unavailable; see the server log".to_string()
            }
            Error::PluginFailed(e) => {
                tracing::error!(error = %e, "plugin failure");
                "plugin failed; see the server log".to_string()
            }
            other => other.to_string(),
        };
        (status, Json(json!({ "error": code, "detail": detail }))).into_response()
    }
}

pub type Result<T> = std::result::Result<T, Error>;
