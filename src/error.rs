use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use ts_rs::TS;

/// Every machine-readable code this application emits.
///
/// An enum rather than a free string so the generated TypeScript is a closed
/// union a client can exhaustively match, and so the list in `docs/api.md`
/// cannot claim a code the code base does not produce. Doorman keeps its own
/// codes on its own routes; this type deliberately does not try to mirror them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum ErrorCode {
    ValidationError,
    InvalidJson,
    NotFound,
    MethodNotAllowed,
    PreconditionRequired,
    PreconditionFailed,
    LifecycleConflict,
    HandleUnavailable,
    FavoritesExists,
    Forbidden,
    ModerationRestricted,
    ReportLimit,
    RequestRejected,
    InternalError,
    DatabaseUnavailable,
    IgdbNotConfigured,
    IgdbUnavailable,
    IgdbRateLimited,
    CatalogBusy,
}

impl ErrorCode {
    /// Every variant, so a test can assert the documented list is complete.
    /// A new code added above without a line in `docs/api.md` fails there.
    pub const ALL: [Self; 19] = [
        Self::ValidationError,
        Self::InvalidJson,
        Self::NotFound,
        Self::MethodNotAllowed,
        Self::PreconditionRequired,
        Self::PreconditionFailed,
        Self::LifecycleConflict,
        Self::HandleUnavailable,
        Self::FavoritesExists,
        Self::Forbidden,
        Self::ModerationRestricted,
        Self::ReportLimit,
        Self::RequestRejected,
        Self::InternalError,
        Self::DatabaseUnavailable,
        Self::IgdbNotConfigured,
        Self::IgdbUnavailable,
        Self::IgdbRateLimited,
        Self::CatalogBusy,
    ];

    /// The wire spelling, for logs and for tests that assert on a code.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ValidationError => "validation_error",
            Self::InvalidJson => "invalid_json",
            Self::NotFound => "not_found",
            Self::MethodNotAllowed => "method_not_allowed",
            Self::PreconditionRequired => "precondition_required",
            Self::PreconditionFailed => "precondition_failed",
            Self::HandleUnavailable => "handle_unavailable",
            Self::Forbidden => "forbidden",
            Self::ModerationRestricted => "moderation_restricted",
            Self::ReportLimit => "report_limit",
            Self::FavoritesExists => "favorites_exists",
            Self::LifecycleConflict => "lifecycle_conflict",
            Self::RequestRejected => "request_rejected",
            Self::InternalError => "internal_error",
            Self::DatabaseUnavailable => "database_unavailable",
            Self::IgdbNotConfigured => "igdb_not_configured",
            Self::IgdbUnavailable => "igdb_unavailable",
            Self::IgdbRateLimited => "igdb_rate_limited",
            Self::CatalogBusy => "catalog_busy",
        }
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why one field of a request body was rejected.
///
/// Deliberately small and about the *shape* of the failure rather than the
/// field it happened to, so a form can style by category and a future field
/// reuses an existing variant instead of adding one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum FieldErrorCode {
    /// The field is missing, empty, or null where a value is required.
    Required,
    /// A number outside its permitted range.
    OutOfRange,
    /// Longer than the field permits.
    TooLong,
    /// Not parseable, or containing characters the field forbids.
    InvalidFormat,
    /// A finish date earlier than the start date it is paired with.
    BeforeStart,
    /// A field the route does not accept.
    UnknownField,
}

/// One field's rejection.
///
/// `field` is the JSON name as sent, so a client can address the control that
/// owns it without translating from Rust identifiers.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct FieldError {
    pub field: String,
    pub code: FieldErrorCode,
    pub message: String,
}

impl FieldError {
    pub fn new(field: impl Into<String>, code: FieldErrorCode, message: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            code,
            message: message.into(),
        }
    }
}

/// The body of every application error response.
#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct ErrorBody {
    pub error: ErrorPayload,
}

#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct ErrorPayload {
    pub code: ErrorCode,
    /// A human-readable summary. Always present, including when `fields` is,
    /// so a caller that renders nothing per-field still has something to show.
    pub message: String,
    /// Present only when individual fields were rejected. Absent rather than
    /// empty, so `fields` in a response always carries at least one entry.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[ts(optional, as = "Option<Vec<FieldError>>")]
    pub fields: Vec<FieldError>,
}

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: ErrorCode,
    pub message: String,
    pub fields: Vec<FieldError>,
}

impl ApiError {
    pub fn new(status: StatusCode, code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            fields: Vec::new(),
        }
    }
    /// A validation failure with no single owning field: a query parameter, a
    /// path segment, or a rule that spans the whole request.
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, ErrorCode::ValidationError, message)
    }
    /// A validation failure attributable to named fields of the request body.
    ///
    /// Every field a request got wrong is reported at once: a form that fixes
    /// one problem per round trip is a worse form, and the checks are cheap.
    pub fn fields(fields: Vec<FieldError>) -> Self {
        debug_assert!(!fields.is_empty(), "a field error needs at least one field");
        let message = match fields.as_slice() {
            [only] => only.message.clone(),
            many => format!("{} fields are invalid", many.len()),
        };
        Self {
            status: StatusCode::BAD_REQUEST,
            code: ErrorCode::ValidationError,
            message,
            fields,
        }
    }
    pub fn not_found() -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            ErrorCode::NotFound,
            "Resource not found",
        )
    }
    pub fn upstream() -> Self {
        Self::new(
            StatusCode::BAD_GATEWAY,
            ErrorCode::IgdbUnavailable,
            "The game provider is unavailable; try again later",
        )
    }
    /// Attaches the field a failure belongs to, for checks that are written
    /// once but reported per field.
    pub fn with_field(mut self, field: FieldError) -> Self {
        self.fields.push(field);
        self
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorBody {
                error: ErrorPayload {
                    code: self.code,
                    message: self.message,
                    fields: self.fields,
                },
            }),
        )
            .into_response()
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(error: sqlx::Error) -> Self {
        if error
            .as_database_error()
            .is_some_and(|e| e.constraint() == Some("moderation_restricted"))
        {
            return Self::new(
                StatusCode::FORBIDDEN,
                ErrorCode::ModerationRestricted,
                "Public participation is restricted; private editing remains available",
            );
        }
        tracing::error!(%error, "Database operation failed");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::InternalError,
            "An internal error occurred",
        )
    }
}

pub type ApiResult<T> = Result<T, ApiError>;
