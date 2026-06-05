//! Error response helpers for both OpenAI-style and Anthropic-style endpoints.

use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::json;

// ---------------------------------------------------------------------------
// Error format selector — controls which wire format the error body uses.
// ---------------------------------------------------------------------------

/// Which wire format should be used when serialising an error response.
///
/// Both variants are part of the public API — `OpenAi` is used by the
/// `/v1/chat/completions` handler's explicit `into_response_fmt` call path
/// and by future callers; `Anthropic` by `/v1/messages`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorFormat {
    /// OpenAI-style: `{"error": {"message": ..., "type": ..., "code": ...}}`
    OpenAi,
    /// Anthropic-style: `{"type":"error","error":{"type":..., "message":...}}`
    Anthropic,
}

// ---------------------------------------------------------------------------
// ProxyError — all error conditions a handler may encounter.
// ---------------------------------------------------------------------------

/// All errors that handlers can return. Each variant carries the HTTP status
/// and a message that will appear in the endpoint-native JSON error body.
#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    // --- auth ---
    #[error("missing API key")]
    MissingKey,
    #[error("invalid API key")]
    UnknownKey,

    // --- routing ---
    #[error("model `{0}` is not allowed for this key")]
    ModelNotAllowed(String),
    #[error("no configured connection serves model `{0}`")]
    UnknownModel(String),

    // --- request ---
    #[error("field `model` is required")]
    MissingModel,
    /// Request body exceeded the configured `server.max_body_bytes` limit.
    #[error("request body too large")]
    BodyTooLarge,

    // --- format enforcement (WP 2.1) ---
    /// Model's connection uses a format incompatible with the called endpoint.
    #[error("{0}")]
    FormatMismatch(String),

    // --- rate limiting (WP 2.1) ---
    /// Gateway-level rate limit hit for this virtual key.
    #[error("rate limit exceeded")]
    RateLimited {
        retry_after_secs: u64,
        limit: u32,
    },

    // --- budget (WP 8.3) ---
    /// Per-key spend budget exhausted for the current window.
    #[error("budget exceeded: spend limit of ${max_usd} USD reached for this window")]
    BudgetExhausted {
        max_usd: f64,
        retry_after_secs: u64,
    },

    // --- PII (WP 3.4) ---
    /// `x-drgtw-pii` header value is not "on" or "off".
    #[error("invalid x-drgtw-pii header value; expected \"on\" or \"off\"")]
    InvalidPiiHeader {
        /// Which wire format to use for the error response body.
        fmt: ErrorFormat,
    },

    // --- PII unavailable (WP 4.4, fail_mode = closed) ---
    /// PII scan failed and fail-closed mode forbids forwarding unmasked data.
    #[error("PII processing unavailable: {0}")]
    PiiUnavailable(String),
    /// Same, raised from the Anthropic endpoint (anthropic-format body).
    #[error("PII processing unavailable: {0}")]
    PiiUnavailableAnthropic(String),

    // --- upstream ---
    #[error("upstream error: {0}")]
    Upstream(#[from] reqwest::Error),
}

impl ProxyError {
    pub fn status(&self) -> StatusCode {
        match self {
            ProxyError::MissingKey | ProxyError::UnknownKey => StatusCode::UNAUTHORIZED,
            ProxyError::ModelNotAllowed(_) => StatusCode::FORBIDDEN,
            ProxyError::UnknownModel(_) => StatusCode::NOT_FOUND,
            ProxyError::BodyTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            ProxyError::MissingModel
            | ProxyError::FormatMismatch(_)
            | ProxyError::InvalidPiiHeader { .. } => StatusCode::BAD_REQUEST,
            ProxyError::RateLimited { .. } | ProxyError::BudgetExhausted { .. } => {
                StatusCode::TOO_MANY_REQUESTS
            }
            ProxyError::PiiUnavailable(_) | ProxyError::PiiUnavailableAnthropic(_) => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            ProxyError::Upstream(_) => StatusCode::BAD_GATEWAY,
        }
    }

    // ---- OpenAI error shape helpers ----------------------------------------

    fn openai_type(&self) -> &'static str {
        match self {
            ProxyError::MissingKey
            | ProxyError::UnknownKey
            | ProxyError::ModelNotAllowed(_)
            | ProxyError::UnknownModel(_)
            | ProxyError::MissingModel
            | ProxyError::BodyTooLarge
            | ProxyError::FormatMismatch(_)
            | ProxyError::InvalidPiiHeader { .. } => "invalid_request_error",
            ProxyError::RateLimited { .. } | ProxyError::BudgetExhausted { .. } => {
                "rate_limit_error"
            }
            ProxyError::PiiUnavailable(_) | ProxyError::PiiUnavailableAnthropic(_) => "api_error",
            ProxyError::Upstream(_) => "upstream_error",
        }
    }

    fn openai_code(&self) -> &'static str {
        match self {
            ProxyError::MissingKey | ProxyError::UnknownKey => "invalid_api_key",
            ProxyError::ModelNotAllowed(_) => "model_not_allowed",
            ProxyError::UnknownModel(_) => "model_not_found",
            ProxyError::MissingModel => "missing_model",
            ProxyError::BodyTooLarge => "request_too_large",
            ProxyError::FormatMismatch(_) => "format_mismatch",
            ProxyError::InvalidPiiHeader { .. } => "invalid_pii_header",
            ProxyError::RateLimited { .. } => "rate_limit_exceeded",
            ProxyError::BudgetExhausted { .. } => "insufficient_budget",
            ProxyError::PiiUnavailable(_) | ProxyError::PiiUnavailableAnthropic(_) => {
                "pii_unavailable"
            }
            ProxyError::Upstream(_) => "upstream_error",
        }
    }

    // ---- Anthropic error shape helpers -------------------------------------

    fn anthropic_type(&self) -> &'static str {
        match self {
            ProxyError::MissingKey | ProxyError::UnknownKey => "authentication_error",
            ProxyError::ModelNotAllowed(_) => "permission_error",
            ProxyError::UnknownModel(_) => "not_found_error",
            ProxyError::MissingModel
            | ProxyError::BodyTooLarge
            | ProxyError::FormatMismatch(_)
            | ProxyError::InvalidPiiHeader { .. } => "invalid_request_error",
            ProxyError::RateLimited { .. } | ProxyError::BudgetExhausted { .. } => {
                "rate_limit_error"
            }
            ProxyError::PiiUnavailable(_) | ProxyError::PiiUnavailableAnthropic(_) => "api_error",
            ProxyError::Upstream(_) => "api_error",
        }
    }

    // ---- Rendered responses ------------------------------------------------

    /// Build an HTTP response in the requested wire format.
    ///
    /// For `InvalidPiiHeader` the embedded `fmt` field overrides the caller's
    /// `fmt` argument — the error knows which endpoint it came from.
    pub fn into_response_fmt(self, fmt: ErrorFormat) -> Response {
        let effective_fmt = if let ProxyError::InvalidPiiHeader { fmt: inner_fmt } = self {
            return ProxyError::InvalidPiiHeader { fmt: inner_fmt }.into_response_with(inner_fmt);
        } else {
            fmt
        };
        self.into_response_with(effective_fmt)
    }

    fn into_response_with(self, fmt: ErrorFormat) -> Response {
        match fmt {
            ErrorFormat::OpenAi => self.into_openai_response(),
            ErrorFormat::Anthropic => self.into_anthropic_response(),
        }
    }

    /// Client-facing message. Internal failure details (upstream errors, PII
    /// engine errors) are logged server-side and replaced with generic text —
    /// `reqwest::Error` can embed upstream URLs and connection metadata, and
    /// NER errors can leak implementation details.
    fn client_message(&self) -> String {
        match self {
            ProxyError::Upstream(e) => {
                tracing::warn!(error = %e, "upstream request failed");
                "upstream request failed".to_owned()
            }
            ProxyError::PiiUnavailable(detail) | ProxyError::PiiUnavailableAnthropic(detail) => {
                tracing::warn!(detail = %detail, "PII processing unavailable");
                "PII processing unavailable; request not forwarded".to_owned()
            }
            other => other.to_string(),
        }
    }

    fn into_openai_response(self) -> Response {
        let status = self.status();
        let extra_headers = rate_limit_headers_for(&self);
        let body = json!({
            "error": {
                "message": self.client_message(),
                "type": self.openai_type(),
                "code": self.openai_code(),
            }
        });
        let mut resp = (status, axum::Json(body)).into_response();
        for (k, v) in extra_headers {
            resp.headers_mut().insert(k, v);
        }
        resp
    }

    fn into_anthropic_response(self) -> Response {
        let status = self.status();
        let extra_headers = rate_limit_headers_for(&self);
        let body = json!({
            "type": "error",
            "error": {
                "type": self.anthropic_type(),
                "message": self.client_message(),
            }
        });
        let mut resp = (status, axum::Json(body)).into_response();
        for (k, v) in extra_headers {
            resp.headers_mut().insert(k, v);
        }
        resp
    }
}

/// Build rate-limit response headers for a `RateLimited` error.
/// Returns an empty vec for any other error variant.
fn rate_limit_headers_for(err: &ProxyError) -> Vec<(HeaderName, HeaderValue)> {
    let mut out = Vec::new();
    match err {
        ProxyError::RateLimited { retry_after_secs, limit } => {
            if let Ok(v) = HeaderValue::from_str(&retry_after_secs.to_string()) {
                out.push((HeaderName::from_static("retry-after"), v));
            }
            if let Ok(v) = HeaderValue::from_str(&limit.to_string()) {
                out.push((HeaderName::from_static("x-ratelimit-limit"), v));
            }
            out.push((
                HeaderName::from_static("x-ratelimit-remaining"),
                HeaderValue::from_static("0"),
            ));
        }
        ProxyError::BudgetExhausted { retry_after_secs, .. } => {
            // Budget exhaustion is a 429 with a retry-after; we do not emit
            // x-ratelimit-* headers (those describe the request-rate limiter).
            if let Ok(v) = HeaderValue::from_str(&retry_after_secs.to_string()) {
                out.push((HeaderName::from_static("retry-after"), v));
            }
        }
        _ => {}
    }
    out
}

/// Default `IntoResponse` uses OpenAI format so existing behaviour is unchanged.
/// `InvalidPiiHeader` uses its embedded format.
impl IntoResponse for ProxyError {
    fn into_response(self) -> Response {
        match self {
            ProxyError::InvalidPiiHeader { fmt } => {
                ProxyError::InvalidPiiHeader { fmt }.into_response_with(fmt)
            }
            other => other.into_openai_response(),
        }
    }
}

impl From<drgtw_keys::AuthError> for ProxyError {
    fn from(e: drgtw_keys::AuthError) -> Self {
        match e {
            drgtw_keys::AuthError::MissingKey => ProxyError::MissingKey,
            drgtw_keys::AuthError::UnknownKey => ProxyError::UnknownKey,
        }
    }
}

impl From<drgtw_keys::RouteError> for ProxyError {
    fn from(e: drgtw_keys::RouteError) -> Self {
        match e {
            drgtw_keys::RouteError::ModelNotAllowed(m) => ProxyError::ModelNotAllowed(m),
            drgtw_keys::RouteError::UnknownModel(m) => ProxyError::UnknownModel(m),
        }
    }
}
