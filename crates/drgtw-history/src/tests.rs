//! Tests that run without a live database (default features only).
//!
//! Postgres integration tests are in the `pg_integration` module below and
//! are gated on both the `postgres` feature and a `DATABASE_URL` env var —
//! they skip automatically when unset so CI stays green without a DB.

use drgtw_events::UsageEvent;
use serde_json::json;

use crate::error::HistoryError;
use crate::handle::History;
use crate::types::{AuditEntry, Bucket, UsageBucket};

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
        let ev = sample_event();
        h.record_usage(&ev).await.expect("record_usage");
        let rows = h.recent_usage(5).await.expect("recent_usage");
        assert!(rows.iter().any(|r| r.request_id == ev.request_id));
    }

    #[tokio::test]
    async fn pg_record_batch() {
        let Some(h) = connect_or_skip().await else {
            return;
        };
        let mut ev1 = sample_event();
        ev1.request_id = "batch-1".to_owned();
        let mut ev2 = sample_event();
        ev2.request_id = "batch-2".to_owned();
        h.record_usage_batch(&[ev1.clone(), ev2.clone()])
            .await
            .expect("batch");
        let rows = h.recent_usage(10).await.expect("recent");
        assert!(rows.iter().any(|r| r.request_id == "batch-1"));
        assert!(rows.iter().any(|r| r.request_id == "batch-2"));
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
        let entry = sample_audit();
        h.append_audit(&entry).await.expect("append_audit");
        let rows = h.recent_audit(5).await.expect("recent_audit");
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

    fn uuid_v4_simple() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
            .to_string()
    }
}
