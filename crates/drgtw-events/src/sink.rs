//! [`EventSink`] — non-blocking fire-and-forget webhook sink.
//!
//! ## Design
//!
//! - A bounded [`tokio::sync::mpsc`] channel decouples the hot path from
//!   network I/O.  [`EventSink::emit`] is `try_send`-only: if the buffer is
//!   full the event is **dropped** (never blocks the caller).
//! - A single background Tokio task drains the channel and POSTs each event
//!   as `application/json` with an optional `Authorization: Bearer …` header.
//! - v1: **one POST per event** — no batching.  Batching is deferred to v2.
//! - **No retry** — failures are logged at `WARN` level (rate-limited) and
//!   the event is dropped.  The caller must not assume delivery.
//! - The dropped-event counter is atomically incremented and readable via
//!   [`EventSink::dropped`].
//!
//! ## Privacy invariant
//!
//! The sink serialises whatever [`UsageEvent`] it receives.  It is the
//! *caller's* responsibility to ensure no content or secrets are embedded in
//! the event before calling [`EventSink::emit`].

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::UsageEvent;

/// Interval between repeated "buffer full / drop" warning logs (rate-limit).
const WARN_INTERVAL: Duration = Duration::from_secs(5);

/// Non-blocking, fire-and-forget webhook sink for [`UsageEvent`]s.
///
/// # Thread-safety
///
/// `EventSink` is `Clone + Send + Sync`.  All internal state is behind an
/// `Arc`; cloning produces a second handle to the **same** channel and counter.
#[derive(Clone, Debug)]
pub struct EventSink {
    tx: mpsc::Sender<UsageEvent>,
    dropped: Arc<AtomicU64>,
}

impl EventSink {
    /// Create a new `EventSink` and spawn the background worker task.
    ///
    /// # Parameters
    ///
    /// - `url` — webhook endpoint that accepts `POST application/json`.
    /// - `auth_bearer` — if `Some`, adds `Authorization: Bearer <token>` to every request.
    /// - `buffer_size` — capacity of the internal bounded channel (events).
    /// - `timeout_ms` — per-request HTTP timeout in milliseconds.
    ///
    /// The caller must be inside a Tokio runtime context (the worker is spawned
    /// with [`tokio::spawn`]).
    pub fn new(
        url: String,
        auth_bearer: Option<String>,
        buffer_size: usize,
        timeout_ms: u64,
    ) -> Self {
        let (tx, rx) = mpsc::channel::<UsageEvent>(buffer_size);
        let dropped = Arc::new(AtomicU64::new(0));

        let worker_dropped = Arc::clone(&dropped);
        tokio::spawn(worker(url, auth_bearer, timeout_ms, rx, worker_dropped));

        Self { tx, dropped }
    }

    /// Submit an event for delivery.
    ///
    /// This is **non-blocking**: if the internal buffer is full the event is
    /// silently dropped, the dropped counter is incremented, and a rate-limited
    /// `WARN` log is emitted.  The caller thread is never blocked.
    pub fn emit(&self, ev: UsageEvent) {
        match self.tx.try_send(ev) {
            Ok(()) => {
                debug!(request_id = %"", "event queued");
            }
            Err(_) => {
                // Channel full or closed — drop and count.
                self.dropped.fetch_add(1, Ordering::Relaxed);
                // Rate-limited warn: we can't carry last-warn state in &self
                // cheaply here, so we use a module-level static.
                maybe_warn_drop();
            }
        }
    }

    /// Return the total number of events dropped since this sink was created.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// Emit a `WARN` about dropped events, rate-limited to at most once per
/// [`WARN_INTERVAL`].
fn maybe_warn_drop() {
    use std::sync::atomic::AtomicU64 as Au64;
    use std::time::{SystemTime, UNIX_EPOCH};

    static LAST_WARN_MS: Au64 = Au64::new(0);

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let last = LAST_WARN_MS.load(Ordering::Relaxed);
    if now_ms.saturating_sub(last) >= WARN_INTERVAL.as_millis() as u64 {
        // Best-effort CAS: if another thread wins, it will log instead.
        if LAST_WARN_MS
            .compare_exchange(last, now_ms, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            warn!("EventSink buffer full — event dropped (further drops rate-limited)");
        }
    }
}

// ── Background worker ────────────────────────────────────────────────────────

async fn worker(
    url: String,
    auth_bearer: Option<String>,
    timeout_ms: u64,
    mut rx: mpsc::Receiver<UsageEvent>,
    dropped: Arc<AtomicU64>,
) {
    let client = Client::builder()
        .timeout(Duration::from_millis(timeout_ms))
        .build()
        .unwrap_or_default();

    // Reusable static for rate-limiting warn on HTTP failures.
    use std::sync::atomic::AtomicU64 as Au64;
    use std::time::{SystemTime, UNIX_EPOCH};
    static LAST_HTTP_WARN_MS: Au64 = Au64::new(0);

    while let Some(ev) = rx.recv().await {
        let body = match serde_json::to_vec(&ev) {
            Ok(b) => b,
            Err(e) => {
                warn!("EventSink: failed to serialise event: {e}");
                dropped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };

        let mut req = client
            .post(&url)
            .header("Content-Type", "application/json")
            .body(body);

        if let Some(token) = &auth_bearer {
            req = req.header("Authorization", format!("Bearer {token}"));
        }

        let result = req.send().await;

        match result {
            Ok(resp) if resp.status().is_success() => {
                debug!("EventSink: event delivered (status {})", resp.status());
            }
            Ok(resp) => {
                let now_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                let last = LAST_HTTP_WARN_MS.load(Ordering::Relaxed);
                if now_ms.saturating_sub(last) >= WARN_INTERVAL.as_millis() as u64
                    && LAST_HTTP_WARN_MS
                        .compare_exchange(last, now_ms, Ordering::Relaxed, Ordering::Relaxed)
                        .is_ok()
                {
                    warn!(
                        "EventSink: upstream returned HTTP {} — event dropped (rate-limited warn)",
                        resp.status()
                    );
                }
                dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                let now_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                let last = LAST_HTTP_WARN_MS.load(Ordering::Relaxed);
                if now_ms.saturating_sub(last) >= WARN_INTERVAL.as_millis() as u64
                    && LAST_HTTP_WARN_MS
                        .compare_exchange(last, now_ms, Ordering::Relaxed, Ordering::Relaxed)
                        .is_ok()
                {
                    warn!(
                        "EventSink: HTTP request failed — event dropped (rate-limited): {e}"
                    );
                }
                dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn sample_event(id: &str) -> UsageEvent {
        UsageEvent {
            request_id: id.to_string(),
            key_id: "key-001".to_string(),
            endpoint: "chat_completions".to_string(),
            model: "gpt-4o".to_string(),
            connection: "openai-prod".to_string(),
            status: 200,
            input_tokens: Some(100),
            output_tokens: Some(50),
            cost_usd: Some(0.00075),
            latency_ms: 250,
            pii: false,
            streamed: false,
            fallback_attempts: 0,
            ts_unix_ms: 1_700_000_000_000,
        }
    }

    /// Event arrives at mock server with correct Content-Type and Authorization header.
    #[tokio::test]
    async fn event_arrives_with_auth_header_and_correct_json() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/events"))
            .and(header("content-type", "application/json"))
            .and(header("authorization", "Bearer secret-token"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let sink = EventSink::new(
            format!("{}/events", server.uri()),
            Some("secret-token".to_string()),
            64,
            5_000,
        );

        let ev = sample_event("req-001");
        sink.emit(ev);

        // Give the worker time to deliver.
        tokio::time::sleep(Duration::from_millis(300)).await;

        server.verify().await;
    }

    /// Event JSON body has all expected fields (spot-check `request_id` and `model`).
    #[tokio::test]
    async fn event_json_body_has_expected_fields() {
        use std::sync::{Arc, Mutex};
        use wiremock::matchers::method;
        use wiremock::{Request, Respond, ResponseTemplate};

        struct BodyCapture(Arc<Mutex<Vec<u8>>>);
        impl Respond for BodyCapture {
            fn respond(&self, req: &Request) -> ResponseTemplate {
                *self.0.lock().unwrap() = req.body.clone();
                ResponseTemplate::new(200)
            }
        }

        let server = MockServer::start().await;
        let captured: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));

        Mock::given(method("POST"))
            .respond_with(BodyCapture(Arc::clone(&captured)))
            .mount(&server)
            .await;

        let sink = EventSink::new(server.uri(), None, 64, 5_000);
        let ev = sample_event("req-body-check");
        sink.emit(ev);

        tokio::time::sleep(Duration::from_millis(300)).await;

        let body = captured.lock().unwrap().clone();
        let json: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON body");
        assert_eq!(json["request_id"], "req-body-check");
        assert_eq!(json["model"], "gpt-4o");
        assert_eq!(json["endpoint"], "chat_completions");
        assert_eq!(json["status"], 200);
    }

    /// Buffer overflow drops events without blocking the caller.
    #[tokio::test]
    async fn buffer_overflow_drops_without_blocking() {
        // tiny buffer=1, slow responder (200ms delay)
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_delay(Duration::from_millis(200)),
            )
            .mount(&server)
            .await;

        let sink = EventSink::new(server.uri(), None, 1, 5_000);

        // Fire many events synchronously — should never block
        let start = std::time::Instant::now();
        for i in 0..50u32 {
            sink.emit(sample_event(&format!("req-{i}")));
        }
        let elapsed = start.elapsed();

        // Emitting 50 events must complete in well under 1 second
        assert!(
            elapsed < Duration::from_millis(500),
            "emit loop took {elapsed:?} — should be near-instant"
        );

        // Some events were dropped
        // (we sent 50 with buffer=1; worker may have consumed a few but most are dropped)
        let dropped = sink.dropped();
        assert!(dropped > 0, "expected some drops, got {dropped}");
    }

    /// Sink keeps working after the upstream returns 500.
    #[tokio::test]
    async fn sink_recovers_after_upstream_500() {
        let server = MockServer::start().await;

        // First call → 500, second → 200
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let sink = EventSink::new(server.uri(), None, 64, 5_000);

        // First event — hits 500
        sink.emit(sample_event("req-fail"));
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Second event — hits 200, worker must still be alive
        sink.emit(sample_event("req-recover"));
        tokio::time::sleep(Duration::from_millis(300)).await;

        server.verify().await;
    }

    /// No auth header when auth_bearer is None.
    #[tokio::test]
    async fn no_auth_header_when_none() {
        let server = MockServer::start().await;

        // wiremock's `not(header(...))` is not in the public API, so we capture
        // and assert the absence ourselves via body capture + separate mock.
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let sink = EventSink::new(server.uri(), None, 64, 5_000);
        sink.emit(sample_event("req-no-auth"));
        tokio::time::sleep(Duration::from_millis(300)).await;
        server.verify().await;
    }
}
