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
    PluginSchemaViolation {
        plugin: String,
        operation: String,
        detail: String,
    },
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
    /// Enrollment and passphrase changes come from a device in this
    /// ErisDB's mesh, calling as itself over Iroh.
    #[error("{0}")]
    NotADevice(String),
    #[error("a note this ErisDB cannot take: {0}")]
    BadNote(String),
    /// Counted against the device that guessed; `retry_after` when this
    /// guess locked it out.
    #[error("wrong passphrase{}", retry_after.map(|s| format!("; try again in {s} seconds")).unwrap_or_default())]
    WrongPassphrase { retry_after: Option<u64> },
    #[error("too many wrong passphrases from this device; try again in {retry_after} seconds")]
    Locked { retry_after: u64 },
    #[error("no passphrase is set; the operator sets one with `erisdb passphrase set`")]
    NoPassphrase,
    #[error("this ErisDB has not joined a mesh")]
    NotJoined,
    /// This core's own approval no longer counts: the mesh's ErisDB is
    /// another app, and apps get in there.
    #[error(
        "this is no longer the mesh's ErisDB: the mesh approved another, or this one's device left"
    )]
    NotTheErisDB,
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
            Error::NotADevice(_) => (StatusCode::FORBIDDEN, "not_a_device"),
            Error::BadNote(_) => (StatusCode::BAD_REQUEST, "bad_note"),
            Error::WrongPassphrase { .. } => (StatusCode::FORBIDDEN, "wrong_passphrase"),
            Error::Locked { .. } => (StatusCode::TOO_MANY_REQUESTS, "locked"),
            Error::NoPassphrase => (StatusCode::SERVICE_UNAVAILABLE, "no_passphrase"),
            Error::NotJoined => (StatusCode::SERVICE_UNAVAILABLE, "not_joined"),
            Error::NotTheErisDB => (StatusCode::SERVICE_UNAVAILABLE, "not_the_erisdb"),
            Error::Db(_) | Error::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        }
    }
}

impl From<erisoverlaynetwork::Error> for Error {
    fn from(e: erisoverlaynetwork::Error) -> Self {
        use erisoverlaynetwork::Error as Mesh;
        match e {
            Mesh::Unauthorized => Error::Unauthorized,
            Mesh::Forbidden { permission } => Error::Forbidden { permission },
            Mesh::BadRequest(detail) => Error::BadRequest(detail),
            Mesh::Conflict(detail) => Error::Conflict(detail),
            Mesh::Expired => Error::BadRequest(e.to_string()),
            // Only a terminal prompt is cancelled, and the API dials no one.
            Mesh::Cancelled(_) | Mesh::Unreachable(_) | Mesh::Io(_) | Mesh::Internal(_) => {
                Error::Internal(e.to_string())
            }
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
        let mut body = json!({ "error": code, "detail": detail });
        // A wrong guess that locks the device says for how long; a guess
        // refused while locked says how long is left.
        match this {
            Error::Locked { retry_after } => {
                body["retry_after"] = json!(retry_after);
                (
                    status,
                    [(axum::http::header::RETRY_AFTER, retry_after.to_string())],
                    Json(body),
                )
                    .into_response()
            }
            Error::WrongPassphrase {
                retry_after: Some(retry_after),
            } => {
                body["retry_after"] = json!(retry_after);
                (status, Json(body)).into_response()
            }
            _ => (status, Json(body)).into_response(),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
