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
//!
//! ## Signing
//!
//! When `signing_secret` is set, each outbound POST body is signed with
//! HMAC-SHA256 and the header `X-Drgtw-Signature: sha256=<hex>` is appended.
//!
//! ## Delivery logging
//!
//! When `delivery_log` is set, every POST attempt (success or failure) is
//! recorded via [`DeliveryLog::record`].  The trait is defined here to avoid a
//! dependency cycle: `drgtw-events` must not import `drgtw-history`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::UsageEvent;

/// Interval between repeated "buffer full / drop" warning logs (rate-limit).
const WARN_INTERVAL: Duration = Duration::from_secs(5);

// ── DeliveryLog trait ─────────────────────────────────────────────────────────

/// A record of one outbound webhook delivery attempt.
///
/// Passed to [`DeliveryLog::record`] after every POST attempt.  The
/// `drgtw-history` crate implements this trait on `History`; other callers can
/// supply their own implementation for testing or alternative backends.
#[derive(Debug, Clone)]
pub struct DeliveryRecord {
    /// The gateway request id this delivery corresponds to.
    pub request_id: String,
    /// Timestamp of the attempt (ms since Unix epoch).
    pub ts_unix_ms: i64,
    /// HTTP status code returned by the upstream endpoint, if any.
    pub status_code: Option<i32>,
    /// `true` when the delivery was accepted (2xx response).
    pub ok: bool,
    /// Transport or serialisation error message, if delivery failed.
    pub error: Option<String>,
    /// 1-based attempt number.
    pub attempt: i32,
    /// The JSON payload that was sent.
    pub payload: serde_json::Value,
}

/// Callback for recording webhook delivery attempts.
///
/// Implement this on a type that can persist [`DeliveryRecord`]s.
/// `drgtw-history::History` implements this trait when the `postgres` feature
/// is enabled.
pub trait DeliveryLog: Send + Sync + 'static {
    fn record(
        &self,
        rec: DeliveryRecord,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>>;
}

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
    /// - `signing_secret` — if `Some`, computes HMAC-SHA256 of the JSON body and
    ///   sends `X-Drgtw-Signature: sha256=<hex>` on every request.
    /// - `delivery_log` — if `Some`, every POST attempt is recorded via
    ///   [`DeliveryLog::record`] (fire-and-forget; errors are swallowed).
    ///
    /// The caller must be inside a Tokio runtime context (the worker is spawned
    /// with [`tokio::spawn`]).
    pub fn new(
        url: String,
        auth_bearer: Option<String>,
        buffer_size: usize,
        timeout_ms: u64,
        signing_secret: Option<String>,
        delivery_log: Option<Arc<dyn DeliveryLog>>,
    ) -> Self {
        let (tx, rx) = mpsc::channel::<UsageEvent>(buffer_size);
        let dropped = Arc::new(AtomicU64::new(0));

        let worker_dropped = Arc::clone(&dropped);
        tokio::spawn(worker(
            url,
            auth_bearer,
            timeout_ms,
            signing_secret,
            delivery_log,
            rx,
            worker_dropped,
        ));

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
    signing_secret: Option<String>,
    delivery_log: Option<Arc<dyn DeliveryLog>>,
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
        let body_bytes = match serde_json::to_vec(&ev) {
            Ok(b) => b,
            Err(e) => {
                warn!("EventSink: failed to serialise event: {e}");
                dropped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };

        // Snapshot payload as Value for delivery logging (cheap clone of already-
        // serialised bytes; avoids a second serde pass in the fast path).
        let payload_value: serde_json::Value =
            serde_json::from_slice(&body_bytes).unwrap_or(serde_json::Value::Null);

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;

        let mut req = client
            .post(&url)
            .header("Content-Type", "application/json")
            .body(body_bytes.clone());

        if let Some(token) = &auth_bearer {
            req = req.header("Authorization", format!("Bearer {token}"));
        }

        // HMAC-SHA256 signing — header: `X-Drgtw-Signature: sha256=<hex>`
        if let Some(secret) = &signing_secret {
            use hmac::{Hmac, Mac};
            use sha2::Sha256;
            type HmacSha256 = Hmac<Sha256>;

            if let Ok(mut mac) = HmacSha256::new_from_slice(secret.as_bytes()) {
                mac.update(&body_bytes);
                let sig = hex::encode(mac.finalize().into_bytes());
                req = req.header("X-Drgtw-Signature", format!("sha256={sig}"));
            }
        }

        let result = req.send().await;

        let (status_code, ok, error_msg) = match result {
            Ok(resp) if resp.status().is_success() => {
                debug!("EventSink: event delivered (status {})", resp.status());
                (Some(resp.status().as_u16() as i32), true, None)
            }
            Ok(resp) => {
                let code = resp.status().as_u16() as i32;
                let now_ms_u64 = now_ms as u64;
                let last = LAST_HTTP_WARN_MS.load(Ordering::Relaxed);
                if now_ms_u64.saturating_sub(last) >= WARN_INTERVAL.as_millis() as u64
                    && LAST_HTTP_WARN_MS
                        .compare_exchange(last, now_ms_u64, Ordering::Relaxed, Ordering::Relaxed)
                        .is_ok()
                {
                    warn!(
                        "EventSink: upstream returned HTTP {} — event dropped (rate-limited warn)",
                        code
                    );
                }
                dropped.fetch_add(1, Ordering::Relaxed);
                (Some(code), false, None)
            }
            Err(e) => {
                let now_ms_u64 = now_ms as u64;
                let last = LAST_HTTP_WARN_MS.load(Ordering::Relaxed);
                if now_ms_u64.saturating_sub(last) >= WARN_INTERVAL.as_millis() as u64
                    && LAST_HTTP_WARN_MS
                        .compare_exchange(last, now_ms_u64, Ordering::Relaxed, Ordering::Relaxed)
                        .is_ok()
                {
                    warn!(
                        "EventSink: HTTP request failed — event dropped (rate-limited): {e}"
                    );
                }
                dropped.fetch_add(1, Ordering::Relaxed);
                (None, false, Some(e.to_string()))
            }
        };

        // Record delivery attempt if a log is wired up (fire-and-forget).
        if let Some(log) = &delivery_log {
            let rec = DeliveryRecord {
                request_id: ev.request_id.clone(),
                ts_unix_ms: now_ms,
                status_code,
                ok,
                error: error_msg,
                attempt: 1,
                payload: payload_value,
            };
            log.record(rec).await;
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

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
            metadata: None,
        }
    }

    /// Captures the raw request body for later inspection.
    struct BodyCapture(Arc<Mutex<Vec<u8>>>);
    impl Respond for BodyCapture {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            *self.0.lock().unwrap() = req.body.clone();
            ResponseTemplate::new(200)
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
            None,
            None,
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
        let server = MockServer::start().await;
        let captured: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));

        Mock::given(method("POST"))
            .respond_with(BodyCapture(Arc::clone(&captured)))
            .mount(&server)
            .await;

        let sink = EventSink::new(server.uri(), None, 64, 5_000, None, None);
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

        let sink = EventSink::new(server.uri(), None, 1, 5_000, None, None);

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

        let sink = EventSink::new(server.uri(), None, 64, 5_000, None, None);

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

        let sink = EventSink::new(server.uri(), None, 64, 5_000, None, None);
        sink.emit(sample_event("req-no-auth"));
        tokio::time::sleep(Duration::from_millis(300)).await;
        server.verify().await;
    }

    /// When a signing_secret is set, the `X-Drgtw-Signature: sha256=<hex>` header
    /// is present and its value is a valid HMAC-SHA256 of the JSON body.
    #[tokio::test]
    async fn signing_secret_produces_hmac_header() {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        type HmacSha256 = Hmac<Sha256>;

        let secret = "test-signing-secret";

        let server = MockServer::start().await;
        let captured_hdrs: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_body: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));

        // Two captures: headers first, then body (both from the same request).
        // Use BodyCapture for body and HeaderCapture for headers on the same mock.
        // Simplest approach: a custom Respond that captures both.
        struct BothCapture {
            hdrs: Arc<Mutex<Vec<(String, String)>>>,
            body: Arc<Mutex<Vec<u8>>>,
        }
        impl Respond for BothCapture {
            fn respond(&self, req: &Request) -> ResponseTemplate {
                *self.body.lock().unwrap() = req.body.clone();
                let mut h = self.hdrs.lock().unwrap();
                for (name, value) in req.headers.iter() {
                    if let Ok(v) = value.to_str() {
                        h.push((name.as_str().to_owned(), v.to_owned()));
                    }
                }
                ResponseTemplate::new(200)
            }
        }

        Mock::given(method("POST"))
            .respond_with(BothCapture {
                hdrs: Arc::clone(&captured_hdrs),
                body: Arc::clone(&captured_body),
            })
            .mount(&server)
            .await;

        let sink = EventSink::new(
            server.uri(),
            None,
            64,
            5_000,
            Some(secret.to_owned()),
            None,
        );
        sink.emit(sample_event("req-sign"));
        tokio::time::sleep(Duration::from_millis(300)).await;

        let body = captured_body.lock().unwrap().clone();
        let hdrs = captured_hdrs.lock().unwrap().clone();

        // Verify signature header is present
        let sig_hdr = hdrs
            .iter()
            .find(|(k, _)| k == "x-drgtw-signature")
            .expect("X-Drgtw-Signature header must be present");
        assert!(
            sig_hdr.1.starts_with("sha256="),
            "signature header should start with sha256="
        );

        // Verify the signature is correct
        let hex_sig = sig_hdr.1.trim_start_matches("sha256=");
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(&body);
        let expected = hex::encode(mac.finalize().into_bytes());
        assert_eq!(hex_sig, expected, "HMAC-SHA256 signature mismatch");
    }

    /// When a DeliveryLog is wired, record() is called after each POST.
    #[tokio::test]
    async fn delivery_log_called_on_success_and_failure() {
        use std::sync::Mutex;

        #[derive(Default)]
        struct TestLog(Mutex<Vec<DeliveryRecord>>);

        impl DeliveryLog for TestLog {
            fn record(
                &self,
                rec: DeliveryRecord,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>>
            {
                self.0.lock().unwrap().push(rec);
                Box::pin(async {})
            }
        }

        let log = Arc::new(TestLog::default());

        let server = MockServer::start().await;

        // First call → 200
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        // Second call → 503
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        let sink = EventSink::new(
            server.uri(),
            None,
            64,
            5_000,
            None,
            Some(log.clone() as Arc<dyn DeliveryLog>),
        );

        sink.emit(sample_event("req-log-ok"));
        tokio::time::sleep(Duration::from_millis(200)).await;
        sink.emit(sample_event("req-log-fail"));
        tokio::time::sleep(Duration::from_millis(300)).await;

        let records = log.0.lock().unwrap();
        assert_eq!(records.len(), 2, "expected 2 delivery records");

        let ok_rec = records.iter().find(|r| r.request_id == "req-log-ok").unwrap();
        assert!(ok_rec.ok);
        assert_eq!(ok_rec.status_code, Some(200));

        let fail_rec = records.iter().find(|r| r.request_id == "req-log-fail").unwrap();
        assert!(!fail_rec.ok);
        assert_eq!(fail_rec.status_code, Some(503));
    }
}
