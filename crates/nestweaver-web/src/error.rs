use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
    /// A coded body to send instead of `{"error": message}`.
    pub body: Option<serde_json::Value>,
    retry_after: Option<axum::http::HeaderValue>,
}

impl ApiError {
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: msg.into(),
            body: None,
            retry_after: None,
        }
    }

    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: msg.into(),
            body: None,
            retry_after: None,
        }
    }

    /// 409 with a coded body (`{"error": <code>, ...}`), for a request the
    /// graph cannot answer in its current state.
    pub fn conflict_with_body(msg: impl Into<String>, body: serde_json::Value) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: msg.into(),
            body: Some(body),
            retry_after: None,
        }
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: msg.into(),
            body: None,
            retry_after: None,
        }
    }

    pub fn unavailable(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: msg.into(),
            body: None,
            retry_after: None,
        }
    }

    /// Deadline classification is confined to context's read boundary. Native
    /// context-edge queries retain Ladybug's interruption message inside a
    /// typed Query error; unrelated query/database failures remain internal.
    pub fn from_context_read(err: anyhow::Error) -> Self {
        let timed_out =
            err.chain().any(
                |cause| match cause.downcast_ref::<nestweaver_store::StoreError>() {
                    Some(nestweaver_store::StoreError::Cancelled(
                        nestweaver_store::CancelReason::Timeout,
                    )) => true,
                    Some(nestweaver_store::StoreError::Query(message)) => {
                        message.starts_with("context edges")
                            && message.ends_with("Runtime exception: Query interrupted.")
                    }
                    _ => false,
                },
            );
        if timed_out {
            tracing::info!(error = %err, "context read deadline exceeded");
            let message =
                "Context read exceeded its deadline; retry shortly or narrow the context seeds.";
            return Self {
                status: StatusCode::GATEWAY_TIMEOUT,
                message: message.into(),
                body: Some(
                    json!({"error": "context_timeout", "message": message, "retryable": true}),
                ),
                retry_after: Some(axum::http::HeaderValue::from_static("1")),
            };
        }
        Self::from_ranking(err)
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
        let body = self
            .body
            .unwrap_or_else(|| json!({ "error": self.message }));
        let mut response = (self.status, axum::Json(body)).into_response();
        if let Some(retry_after) = self.retry_after {
            response
                .headers_mut()
                .insert(axum::http::header::RETRY_AFTER, retry_after);
        }
        response
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
