//! Lightweight request-ID middleware.
//!
//! Generates a unique `x-drgtw-request-id` header value and:
//!  - sets it on every **response**
//!  - enters a tracing span so the id appears in all log lines for that request
//!
//! The ID is formed from `<unix_nanos_hex>-<monotonic_counter>` — no heavy
//! dependencies (no UUID crate).  It is not cryptographically random, but it
//! is unique within a single process lifetime and cheap to produce.

use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::http::{Request, Response};
use tower::{Layer, Service};
use tracing::{Instrument as _, info_span};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generate a new request ID string.
///
/// Format: `<16-hex-nanos>-<8-hex-counter>`, e.g. `00000197bfe2a3c0-00000001`.
fn new_request_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:016x}-{seq:08x}")
}

// ---------------------------------------------------------------------------
// Layer
// ---------------------------------------------------------------------------

/// [`tower::Layer`] that wraps a service with [`RequestIdService`].
#[derive(Clone, Debug, Default)]
pub struct RequestIdLayer;

impl<S> Layer<S> for RequestIdLayer {
    type Service = RequestIdService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RequestIdService { inner }
    }
}

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct RequestIdService<S> {
    inner: S,
}

impl<S, ReqBody> Service<Request<ReqBody>> for RequestIdService<S>
where
    S: Service<Request<ReqBody>, Response = Response<Body>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    ReqBody: Send + 'static,
{
    type Response = Response<Body>;
    type Error = S::Error;
    type Future = std::pin::Pin<Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request<ReqBody>) -> Self::Future {
        let id = new_request_id();
        let id_clone = id.clone();
        // Inject the id into the REQUEST so downstream handlers (proxy usage
        // events) read the same id that is echoed on the response. We do not
        // honour any client-supplied value — the gateway always assigns its own.
        if let Ok(hv) = id.parse() {
            req.headers_mut().insert("x-drgtw-request-id", hv);
        }
        let span = info_span!("request", request_id = %id);
        let fut = self.inner.call(req);
        Box::pin(
            async move {
                let mut resp = fut.await?;
                resp.headers_mut().insert(
                    "x-drgtw-request-id",
                    id_clone.parse().expect("request id is always valid header value"),
                );
                Ok(resp)
            }
            .instrument(span),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_request_id_format() {
        let id = new_request_id();
        // Should be two hex segments separated by '-'
        let parts: Vec<&str> = id.splitn(2, '-').collect();
        assert_eq!(parts.len(), 2, "id = {id}");
        assert_eq!(parts[0].len(), 16, "nanos part length, id = {id}");
        assert_eq!(parts[1].len(), 8, "counter part length, id = {id}");
    }

    #[test]
    fn consecutive_ids_are_distinct() {
        let a = new_request_id();
        let b = new_request_id();
        assert_ne!(a, b);
    }
}
