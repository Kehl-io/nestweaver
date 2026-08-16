use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

impl ApiError {
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: msg.into(),
        }
    }

    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: msg.into(),
        }
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: msg.into(),
        }
    }

    pub fn unavailable(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: msg.into(),
        }
    }

    /// Classify a failed ranking query. A dirty-publication refusal
    /// (`StoreError::RankingUnavailable` — the store fails ranking closed;
    /// see the `nestweaver-store` ranking module contract) is transient, so
    /// it maps to 503 "ranking unavailable" — never to a successful-looking
    /// empty result. Any other error maps to 500 as usual. The error itself
    /// is classified (not a re-check of the dirty flag), so an unrelated
    /// failure during a publication window is still reported as 500.
    pub fn from_ranking(err: anyhow::Error) -> Self {
        if let Some(nestweaver_store::StoreError::RankingUnavailable) =
            err.downcast_ref::<nestweaver_store::StoreError>()
        {
            tracing::info!(error = %err, "ranking query refused: index publication in flight");
            return Self::unavailable(
                "ranking temporarily unavailable — index publication in progress",
            );
        }
        Self::from(err)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({ "error": self.message });
        (self.status, axum::Json(body)).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        tracing::error!(error = %err, "internal error");
        Self::internal(err.to_string())
    }
}

impl From<nestweaver_store::StoreError> for ApiError {
    fn from(err: nestweaver_store::StoreError) -> Self {
        tracing::error!(error = %err, "store error");
        Self::internal(err.to_string())
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(err: serde_json::Error) -> Self {
        Self::bad_request(format!("JSON error: {err}"))
    }
}
