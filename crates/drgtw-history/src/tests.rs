//! Tests that run without a live database (default features only).
//!
//! Postgres integration tests are in the `pg_integration` module below and
//! are gated on both the `postgres` feature and a `DATABASE_URL` env var —
//! they skip automatically when unset so CI stays green without a DB.

use drgtw_events::UsageEvent;
use serde_json::json;

use crate::error::HistoryError;
use crate::handle::History;
use crate::types::{AuditEntry, Bucket, PiiDetectionRow, UsageBucket, WebhookDeliveryRow};

const FULL_RANGE: (i64, i64) = (0, i64::MAX);

// ── Helpers ───────────────────────────────────────────────────────────────────

fn sample_event() -> UsageEvent {
    UsageEvent {
        request_id: "req-test-1".to_owned(),
        key_id: "key-1".to_owned(),
        endpoint: "chat_completions".to_owned(),
        model: "gpt-4o".to_owned(),
        connection: "openai-main".to_owned(),
        status: 200,
        input_tokens: Some(100),
        output_tokens: Some(50),
        cost_usd: Some(0.001),
        latency_ms: 300,
        pii: false,
        streamed: false,
        fallback_attempts: 0,
        ts_unix_ms: 1_700_000_000_000,
        metadata: None,
    }
}

fn sample_audit() -> AuditEntry {
    AuditEntry {
        ts_unix_ms: 1_700_000_000_000,
        actor: "admin".to_owned(),
        action: "key.rotate".to_owned(),
        target: "key-1".to_owned(),
        detail: json!({"reason": "scheduled"}),
    }
}

// ── Disabled-handle behaviour ─────────────────────────────────────────────────

#[tokio::test]
async fn disabled_record_usage_returns_disabled_err() {
    let h = History::disabled();
    let err = h.record_usage(&sample_event()).await.unwrap_err();
    assert!(
        matches!(err, HistoryError::Disabled),
        "expected Disabled, got {err:?}"
    );
}

#[tokio::test]
async fn disabled_record_usage_batch_returns_disabled_err() {
    let h = History::disabled();
    let err = h
        .record_usage_batch(&[sample_event()])
        .await
        .unwrap_err();
    assert!(matches!(err, HistoryError::Disabled));
}

#[tokio::test]
async fn disabled_append_audit_returns_disabled_err() {
    let h = History::disabled();
    let err = h.append_audit(&sample_audit()).await.unwrap_err();
    assert!(matches!(err, HistoryError::Disabled));
}

#[tokio::test]
async fn disabled_recent_usage_returns_empty() {
    let h = History::disabled();
    let rows = h.recent_usage(10).await.expect("should not error");
    assert!(rows.is_empty());
}

#[tokio::test]
async fn disabled_recent_audit_returns_empty() {
    let h = History::disabled();
    let rows = h.recent_audit(10).await.expect("should not error");
    assert!(rows.is_empty());
}

#[tokio::test]
async fn disabled_usage_timeseries_returns_empty() {
    let h = History::disabled();
    let rows = h
        .usage_timeseries(0, i64::MAX, Bucket::Hour)
        .await
        .expect("should not error");
    assert!(rows.is_empty());
}

#[tokio::test]
async fn disabled_create_user_returns_disabled_err() {
    let h = History::disabled();
    let err = h.create_user("alice", "hash").await.unwrap_err();
    assert!(matches!(err, HistoryError::Disabled));
}

#[tokio::test]
async fn disabled_find_user_returns_none() {
    let h = History::disabled();
    let row = h.find_user("alice").await.expect("should not error");
    assert!(row.is_none());
}

#[tokio::test]
async fn disabled_create_session_returns_disabled_err() {
    let h = History::disabled();
    let err = h.create_session("tok", 1, 9999).await.unwrap_err();
    assert!(matches!(err, HistoryError::Disabled));
}

#[tokio::test]
async fn disabled_get_session_returns_none() {
    let h = History::disabled();
    let row = h.get_session("tok").await.expect("should not error");
    assert!(row.is_none());
}

#[tokio::test]
async fn disabled_delete_session_returns_disabled_err() {
    let h = History::disabled();
    let err = h.delete_session("tok").await.unwrap_err();
    assert!(matches!(err, HistoryError::Disabled));
}

#[tokio::test]
async fn disabled_usage_summary_returns_zero() {
    let h = History::disabled();
    let (since, until) = FULL_RANGE;
    let s = h.usage_summary(since, until).await.expect("should not error");
    assert_eq!(s.requests, 0);
    assert_eq!(s.input_tokens, 0);
    assert_eq!(s.output_tokens, 0);
    assert_eq!(s.cost_usd, 0.0);
    assert_eq!(s.avg_latency_ms, 0.0);
    assert_eq!(s.pii_count, 0);
    assert_eq!(s.error_count, 0);
}

#[tokio::test]
async fn disabled_usage_by_model_returns_empty() {
    let h = History::disabled();
    let (since, until) = FULL_RANGE;
    let rows = h.usage_by_model(since, until).await.expect("should not error");
    assert!(rows.is_empty());
}

#[tokio::test]
async fn disabled_usage_by_connection_returns_empty() {
    let h = History::disabled();
    let (since, until) = FULL_RANGE;
    let rows = h
        .usage_by_connection(since, until)
        .await
        .expect("should not error");
    assert!(rows.is_empty());
}

#[tokio::test]
async fn disabled_usage_by_endpoint_returns_empty() {
    let h = History::disabled();
    let (since, until) = FULL_RANGE;
    let rows = h
        .usage_by_endpoint(since, until)
        .await
        .expect("should not error");
    assert!(rows.is_empty());
}

// ── New disabled-handle tests ─────────────────────────────────────────────────

fn sample_pii_row() -> PiiDetectionRow {
    PiiDetectionRow {
        request_id: "req-pii-1".to_owned(),
        key_id: "key-1".to_owned(),
        entity_kind: "EMAIL".to_owned(),
        count: 2,
        ts_unix_ms: 1_700_000_000_000,
    }
}

fn sample_webhook_row() -> WebhookDeliveryRow {
    WebhookDeliveryRow {
        id: None,
        request_id: "req-hook-1".to_owned(),
        ts_unix_ms: 1_700_000_000_000,
        status_code: Some(200),
        ok: true,
        error: None,
        attempt: 1,
        payload: json!({"event": "test"}),
    }
}

#[tokio::test]
async fn disabled_usage_summary_by_key_returns_zero() {
    let h = History::disabled();
    let s = h
        .usage_summary_by_key("key-1", 0, i64::MAX)
        .await
        .expect("should not error");
    assert_eq!(s.requests, 0);
}

#[tokio::test]
async fn disabled_usage_timeseries_by_key_returns_empty() {
    let h = History::disabled();
    let rows = h
        .usage_timeseries_by_key("key-1", 0, i64::MAX, Bucket::Day)
        .await
        .expect("should not error");
    assert!(rows.is_empty());
}

#[tokio::test]
async fn disabled_usage_by_key_returns_empty() {
    let h = History::disabled();
    let rows = h
        .usage_by_key(0, i64::MAX)
        .await
        .expect("should not error");
    assert!(rows.is_empty());
}

#[tokio::test]
async fn disabled_list_users_returns_empty() {
    let h = History::disabled();
    let rows = h.list_users().await.expect("should not error");
    assert!(rows.is_empty());
}

#[tokio::test]
async fn disabled_delete_user_returns_disabled_err() {
    let h = History::disabled();
    let err = h.delete_user(1).await.unwrap_err();
    assert!(matches!(err, HistoryError::Disabled));
}

#[tokio::test]
async fn disabled_record_pii_detections_returns_disabled_err() {
    let h = History::disabled();
    let err = h
        .record_pii_detections(&[sample_pii_row()])
        .await
        .unwrap_err();
    assert!(matches!(err, HistoryError::Disabled));
}

#[tokio::test]
async fn disabled_pii_by_kind_returns_empty() {
    let h = History::disabled();
    let rows = h.pii_by_kind(0, i64::MAX).await.expect("should not error");
    assert!(rows.is_empty());
}

#[tokio::test]
async fn disabled_pii_timeseries_returns_empty() {
    let h = History::disabled();
    let rows = h
        .pii_timeseries(0, i64::MAX, Bucket::Hour)
        .await
        .expect("should not error");
    assert!(rows.is_empty());
}

#[tokio::test]
async fn disabled_record_webhook_delivery_returns_disabled_err() {
    let h = History::disabled();
    let err = h
        .record_webhook_delivery(&sample_webhook_row())
        .await
        .unwrap_err();
    assert!(matches!(err, HistoryError::Disabled));
}

#[tokio::test]
async fn disabled_recent_webhook_deliveries_returns_empty() {
    let h = History::disabled();
    let rows = h
        .recent_webhook_deliveries(10)
        .await
        .expect("should not error");
    assert!(rows.is_empty());
}

#[tokio::test]
async fn disabled_get_webhook_delivery_returns_none() {
    let h = History::disabled();
    let row = h.get_webhook_delivery(1).await.expect("should not error");
    assert!(row.is_none());
}

// ── Pure-Rust helpers ─────────────────────────────────────────────────────────

#[test]
fn bucket_ms_hour() {
    assert_eq!(Bucket::Hour.ms(), 3_600_000);
}

#[test]
fn bucket_ms_day() {
    assert_eq!(Bucket::Day.ms(), 86_400_000);
}

#[test]
fn usage_bucket_serde_roundtrip() {
    let b = UsageBucket {
        ts_ms: 1_700_000_000_000,
        requests: 42,
        input_tokens: 1000,
        output_tokens: 500,
        cost_usd: 0.05,
        avg_latency_ms: 123.4,
    };
    let json = serde_json::to_string(&b).unwrap();
    let back: UsageBucket = serde_json::from_str(&json).unwrap();
    assert_eq!(back.requests, b.requests);
    assert_eq!(back.ts_ms, b.ts_ms);
}

#[test]
fn audit_entry_serde_roundtrip() {
    let entry = sample_audit();
    let json = serde_json::to_string(&entry).unwrap();
    let back: AuditEntry = serde_json::from_str(&json).unwrap();
    assert_eq!(back.actor, entry.actor);
    assert_eq!(back.action, entry.action);
}

#[test]
fn history_error_disabled_display() {
    let e = HistoryError::Disabled;
    assert_eq!(e.to_string(), "history store is disabled");
}

// ── Postgres integration tests (skipped when DATABASE_URL unset) ──────────────

#[cfg(feature = "postgres")]
mod pg_integration {
    use super::*;
    use crate::handle::History;

    async fn connect_or_skip() -> Option<History> {
        let url = match std::env::var("DATABASE_URL") {
            Ok(u) if !u.is_empty() => u,
            _ => {
                eprintln!("DATABASE_URL not set — skipping Postgres integration tests");
                return None;
            }
        };
        match History::connect(&url).await {
            Ok(h) => Some(h),
            Err(e) => {
                eprintln!("Could not connect to Postgres ({e}) — skipping integration tests");
                None
            }
        }
    }

    #[tokio::test]
    async fn pg_record_and_recent_usage() {
        let Some(h) = connect_or_skip().await else {
            return;
        };
        let mut ev = sample_event();
        ev.request_id = format!("rec-{}", uuid_v4_simple());
        ev.ts_unix_ms = recent_unique_ts() as u64;
        h.record_usage(&ev).await.expect("record_usage");
        let rows = h.recent_usage(1000).await.expect("recent_usage");
        assert!(rows.iter().any(|r| r.request_id == ev.request_id));
    }

    #[tokio::test]
    async fn pg_record_batch() {
        let Some(h) = connect_or_skip().await else {
            return;
        };
        let tag = uuid_v4_simple();
        let ts = recent_unique_ts();
        let id1 = format!("batch-{tag}-1");
        let id2 = format!("batch-{tag}-2");
        let mut ev1 = sample_event();
        ev1.request_id = id1.clone();
        ev1.ts_unix_ms = ts as u64;
        let mut ev2 = sample_event();
        ev2.request_id = id2.clone();
        ev2.ts_unix_ms = ts as u64 + 1;
        h.record_usage_batch(&[ev1.clone(), ev2.clone()])
            .await
            .expect("batch");
        let rows = h.recent_usage(1000).await.expect("recent");
        assert!(rows.iter().any(|r| r.request_id == id1));
        assert!(rows.iter().any(|r| r.request_id == id2));
    }

    #[tokio::test]
    async fn pg_timeseries_returns_buckets() {
        let Some(h) = connect_or_skip().await else {
            return;
        };
        let buckets = h
            .usage_timeseries(0, i64::MAX, Bucket::Day)
            .await
            .expect("timeseries");
        // As long as at least one row was inserted above, we get ≥1 bucket.
        assert!(!buckets.is_empty());
    }

    #[tokio::test]
    async fn pg_audit_append_and_recent() {
        let Some(h) = connect_or_skip().await else {
            return;
        };
        let mut entry = sample_audit();
        entry.actor = format!("actor-{}", uuid_v4_simple());
        entry.ts_unix_ms = recent_unique_ts();
        h.append_audit(&entry).await.expect("append_audit");
        let rows = h.recent_audit(1000).await.expect("recent_audit");
        assert!(rows.iter().any(|r| r.actor == entry.actor));
    }

    #[tokio::test]
    async fn pg_user_create_and_find() {
        let Some(h) = connect_or_skip().await else {
            return;
        };
        let username = format!("testuser_{}", uuid_v4_simple());
        let id = h
            .create_user(&username, "argon2hash")
            .await
            .expect("create_user");
        assert!(id > 0);
        let found = h.find_user(&username).await.expect("find_user");
        assert!(found.is_some());
        assert_eq!(found.unwrap().username, username);
    }

    #[tokio::test]
    async fn pg_session_lifecycle() {
        let Some(h) = connect_or_skip().await else {
            return;
        };
        let username = format!("sessuser_{}", uuid_v4_simple());
        let user_id = h
            .create_user(&username, "hash")
            .await
            .expect("create_user");
        let sid = format!("sess_{}", uuid_v4_simple());
        h.create_session(&sid, user_id, i64::MAX)
            .await
            .expect("create_session");
        let got = h.get_session(&sid).await.expect("get_session");
        assert!(got.is_some());
        assert_eq!(got.unwrap().0, user_id);
        h.delete_session(&sid).await.expect("delete_session");
        let gone = h.get_session(&sid).await.expect("get after delete");
        assert!(gone.is_none());
    }

    #[tokio::test]
    async fn pg_usage_summary_and_by_dims() {
        let Some(h) = connect_or_skip().await else {
            return;
        };

        // Carve out a unique, isolated time window so aggregates are exact
        // regardless of any other rows already in the table.
        let since = isolated_past_window();
        let until = since + 1_000;
        let tag = uuid_v4_simple();

        // Build a controlled set of events within [since, until):
        //   model A: 3 requests (one pii, one status 500)
        //   model B: 1 request
        //   connections: conn-x (3), conn-y (1)
        //   endpoints:   ep-1 (2), ep-2 (2)
        let mut events = Vec::new();
        let mk = |rid: String, model: &str, conn: &str, ep: &str, status: u16, pii: bool, ts: i64| {
            let mut e = sample_event();
            e.request_id = rid;
            e.model = model.to_owned();
            e.connection = conn.to_owned();
            e.endpoint = ep.to_owned();
            e.status = status;
            e.pii = pii;
            e.input_tokens = Some(100);
            e.output_tokens = Some(50);
            e.cost_usd = Some(0.001);
            e.latency_ms = 200;
            e.ts_unix_ms = ts as u64;
            e
        };
        events.push(mk(format!("sum-{tag}-1"), "model-A", "conn-x", "ep-1", 200, false, since + 1));
        events.push(mk(format!("sum-{tag}-2"), "model-A", "conn-x", "ep-1", 200, true, since + 2));
        events.push(mk(format!("sum-{tag}-3"), "model-A", "conn-x", "ep-2", 500, false, since + 3));
        events.push(mk(format!("sum-{tag}-4"), "model-B", "conn-y", "ep-2", 200, false, since + 4));

        h.record_usage_batch(&events).await.expect("batch insert");

        // ── summary ──
        let s = h.usage_summary(since, until).await.expect("usage_summary");
        assert_eq!(s.requests, 4, "requests");
        assert_eq!(s.input_tokens, 400, "input_tokens");
        assert_eq!(s.output_tokens, 200, "output_tokens");
        assert!((s.cost_usd - 0.004).abs() < 1e-9, "cost_usd = {}", s.cost_usd);
        assert!((s.avg_latency_ms - 200.0).abs() < 1e-9, "avg_latency_ms = {}", s.avg_latency_ms);
        assert_eq!(s.pii_count, 1, "pii_count");
        assert_eq!(s.error_count, 1, "error_count (status>=400)");

        // ── by model ──
        let by_model = h.usage_by_model(since, until).await.expect("usage_by_model");
        assert_eq!(by_model.len(), 2, "two distinct models");
        // ORDER BY requests DESC → model-A (3) first, model-B (1) second.
        assert_eq!(by_model[0].label, "model-A");
        assert_eq!(by_model[0].requests, 3);
        assert_eq!(by_model[1].label, "model-B");
        assert_eq!(by_model[1].requests, 1);

        // ── by connection ──
        let by_conn = h
            .usage_by_connection(since, until)
            .await
            .expect("usage_by_connection");
        assert_eq!(by_conn.len(), 2);
        assert_eq!(by_conn[0].label, "conn-x");
        assert_eq!(by_conn[0].requests, 3);

        // ── by endpoint ──
        let by_ep = h
            .usage_by_endpoint(since, until)
            .await
            .expect("usage_by_endpoint");
        assert_eq!(by_ep.len(), 2);
        // ep-1 and ep-2 both have 2 requests; assert the set of labels.
        let mut labels: Vec<String> = by_ep.iter().map(|d| d.label.clone()).collect();
        labels.sort();
        assert_eq!(labels, vec!["ep-1".to_owned(), "ep-2".to_owned()]);
    }

    #[tokio::test]
    async fn pg_user_list_and_delete() {
        let Some(h) = connect_or_skip().await else {
            return;
        };
        let username = format!("listuser_{}", uuid_v4_simple());
        let id = h
            .create_user(&username, "hash")
            .await
            .expect("create_user");
        let users = h.list_users().await.expect("list_users");
        assert!(users.iter().any(|u| u.id == id && u.username == username));
        h.delete_user(id).await.expect("delete_user");
        let after = h.find_user(&username).await.expect("find_user after delete");
        assert!(after.is_none(), "user should be gone after delete_user");
    }

    #[tokio::test]
    async fn pg_per_key_usage_queries() {
        let Some(h) = connect_or_skip().await else {
            return;
        };

        let since = isolated_past_window();
        let until = since + 1_000;
        let tag = uuid_v4_simple();
        let key_a = format!("key-a-{tag}");
        let key_b = format!("key-b-{tag}");

        // Insert 3 events for key_a and 1 for key_b in the isolated window.
        let mk_key = |rid: String, kid: &str, ts: i64| {
            let mut e = sample_event();
            e.request_id = rid;
            e.key_id = kid.to_owned();
            e.input_tokens = Some(100);
            e.output_tokens = Some(50);
            e.cost_usd = Some(0.001);
            e.latency_ms = 200;
            e.ts_unix_ms = ts as u64;
            e
        };
        let evs = vec![
            mk_key(format!("pk-{tag}-1"), &key_a, since + 1),
            mk_key(format!("pk-{tag}-2"), &key_a, since + 2),
            mk_key(format!("pk-{tag}-3"), &key_a, since + 3),
            mk_key(format!("pk-{tag}-4"), &key_b, since + 4),
        ];
        h.record_usage_batch(&evs).await.expect("batch");

        // usage_summary_by_key — only key_a rows in window
        let s = h
            .usage_summary_by_key(&key_a, since, until)
            .await
            .expect("usage_summary_by_key");
        assert_eq!(s.requests, 3);
        assert_eq!(s.input_tokens, 300);

        // usage_timeseries_by_key — should have ≥1 bucket
        let ts = h
            .usage_timeseries_by_key(&key_a, since, until, Bucket::Day)
            .await
            .expect("usage_timeseries_by_key");
        assert!(!ts.is_empty());

        // usage_by_key — both keys appear; key_a first (more requests)
        let by_key = h.usage_by_key(since, until).await.expect("usage_by_key");
        assert!(by_key.iter().any(|d| d.label == key_a && d.requests == 3));
        assert!(by_key.iter().any(|d| d.label == key_b && d.requests == 1));
    }

    #[tokio::test]
    async fn pg_pii_detections_record_and_query() {
        let Some(h) = connect_or_skip().await else {
            return;
        };

        let since = isolated_past_window();
        let until = since + 1_000;
        let tag = uuid_v4_simple();

        let rows = vec![
            PiiDetectionRow {
                request_id: format!("pii-{tag}-1"),
                key_id: "key-1".to_owned(),
                entity_kind: "EMAIL".to_owned(),
                count: 3,
                ts_unix_ms: since + 1,
            },
            PiiDetectionRow {
                request_id: format!("pii-{tag}-2"),
                key_id: "key-1".to_owned(),
                entity_kind: "PHONE".to_owned(),
                count: 1,
                ts_unix_ms: since + 2,
            },
            PiiDetectionRow {
                request_id: format!("pii-{tag}-3"),
                key_id: "key-2".to_owned(),
                entity_kind: "EMAIL".to_owned(),
                count: 2,
                ts_unix_ms: since + 3,
            },
        ];
        h.record_pii_detections(&rows)
            .await
            .expect("record_pii_detections");

        // pii_by_kind — EMAIL total = 5, PHONE total = 1
        let by_kind = h.pii_by_kind(since, until).await.expect("pii_by_kind");
        let email = by_kind.iter().find(|d| d.label == "EMAIL").expect("EMAIL");
        assert_eq!(email.requests, 5);
        let phone = by_kind.iter().find(|d| d.label == "PHONE").expect("PHONE");
        assert_eq!(phone.requests, 1);

        // pii_timeseries — ≥1 bucket with sum > 0
        let ts = h
            .pii_timeseries(since, until, Bucket::Day)
            .await
            .expect("pii_timeseries");
        assert!(!ts.is_empty());
        assert!(ts.iter().map(|b| b.requests).sum::<i64>() == 6);
    }

    #[tokio::test]
    async fn pg_webhook_delivery_lifecycle() {
        let Some(h) = connect_or_skip().await else {
            return;
        };

        let ts = recent_unique_ts();
        let row = WebhookDeliveryRow {
            id: None,
            request_id: format!("hook-req-{}", uuid_v4_simple()),
            ts_unix_ms: ts,
            status_code: Some(200),
            ok: true,
            error: None,
            attempt: 1,
            payload: serde_json::json!({"event": "test", "ts": ts}),
        };

        let id = h
            .record_webhook_delivery(&row)
            .await
            .expect("record_webhook_delivery");
        assert!(id > 0);

        // get_webhook_delivery — round-trips fields
        let fetched = h
            .get_webhook_delivery(id)
            .await
            .expect("get_webhook_delivery")
            .expect("should exist");
        assert_eq!(fetched.request_id, row.request_id);
        assert_eq!(fetched.ok, true);
        assert_eq!(fetched.status_code, Some(200));
        assert_eq!(fetched.attempt, 1);
        assert_eq!(fetched.id, Some(id));

        // recent_webhook_deliveries — includes our new row
        let recent = h
            .recent_webhook_deliveries(1000)
            .await
            .expect("recent_webhook_deliveries");
        assert!(recent.iter().any(|r| r.id == Some(id)));

        // A failed delivery record
        let fail_row = WebhookDeliveryRow {
            id: None,
            request_id: format!("hook-fail-{}", uuid_v4_simple()),
            ts_unix_ms: ts + 1,
            status_code: None,
            ok: false,
            error: Some("connection refused".to_owned()),
            attempt: 1,
            payload: serde_json::json!({}),
        };
        let fail_id = h
            .record_webhook_delivery(&fail_row)
            .await
            .expect("record failed delivery");
        let fail_fetched = h
            .get_webhook_delivery(fail_id)
            .await
            .expect("get failed delivery")
            .expect("should exist");
        assert!(!fail_fetched.ok);
        assert_eq!(
            fail_fetched.error.as_deref(),
            Some("connection refused")
        );
    }

    fn uuid_v4_simple() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
            .to_string()
    }

    /// Real current epoch-ms plus a small unique nudge. Rows stamped with this
    /// are genuinely recent (so they appear in `recent_*` queries) and carry a
    /// unique id, so assertions filter by that id with a generous limit — safe to
    /// re-run against a persistent database without polluting other suites.
    fn recent_unique_ts() -> i64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        now + (uuid_v4_simple().parse::<i64>().unwrap_or(0) % 1_000)
    }

    /// An isolated time window far in the PAST (around 2017) with a unique base.
    /// Aggregate assertions (summary / by-dimension counts) need a window that
    /// contains ONLY this test's rows; placing it in the past means it never
    /// overlaps real `now`-stamped data and — being old — its rows never surface
    /// in `recent_*` (newest-N) queries, so it pollutes no other suite.
    fn isolated_past_window() -> i64 {
        let n: i64 = uuid_v4_simple().parse().unwrap_or(0);
        1_500_000_000_000 + (n % 100_000_000) * 1_000
    }
}
