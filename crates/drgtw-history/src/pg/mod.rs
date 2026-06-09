//! Real Postgres implementation — compiled only with `--features postgres`.
//!
//! Uses `sqlx::query()` (dynamic form) throughout so that `cargo check
//! --features postgres` works without a live DATABASE_URL or an offline
//! query cache.

use drgtw_events::UsageEvent;
use serde_json::Value;
use sqlx::PgPool;
use tracing::instrument;

use crate::error::HistoryError;
use crate::types::{
    AuditEntry, Bucket, DimCount, PiiDetectionRow, UsageBucket, UsageSummary, UserRow,
    WebhookDeliveryRow,
};

// ── Migrations ────────────────────────────────────────────────────────────────

const MIGRATION_001: &str = include_str!("../../migrations/001_initial.sql");
const MIGRATION_002: &str = include_str!("../../migrations/002_ui_features.sql");

pub(crate) async fn run_migrations(pool: &PgPool) -> Result<(), HistoryError> {
    sqlx::raw_sql(MIGRATION_001).execute(pool).await?;
    sqlx::raw_sql(MIGRATION_002).execute(pool).await?;
    Ok(())
}

// ── Usage events ──────────────────────────────────────────────────────────────

#[instrument(skip_all, fields(request_id = %ev.request_id))]
pub(crate) async fn record_usage(pool: &PgPool, ev: &UsageEvent) -> Result<(), HistoryError> {
    let metadata: Option<Value> = ev
        .metadata
        .as_ref()
        .map(serde_json::to_value)
        .transpose()?;

    sqlx::query(
        r#"
        INSERT INTO usage_events (
            request_id, key_id, endpoint, model, connection, status,
            input_tokens, output_tokens, cost_usd, latency_ms, pii, streamed,
            fallback_attempts, ts_unix_ms, metadata
        ) VALUES (
            $1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15
        )
        ON CONFLICT (request_id) DO NOTHING
        "#,
    )
    .bind(&ev.request_id)
    .bind(&ev.key_id)
    .bind(&ev.endpoint)
    .bind(&ev.model)
    .bind(&ev.connection)
    .bind(ev.status as i32)
    .bind(ev.input_tokens.map(|v| v as i64))
    .bind(ev.output_tokens.map(|v| v as i64))
    .bind(ev.cost_usd)
    .bind(ev.latency_ms as i64)
    .bind(ev.pii)
    .bind(ev.streamed)
    .bind(ev.fallback_attempts as i32)
    .bind(ev.ts_unix_ms as i64)
    .bind(metadata)
    .execute(pool)
    .await?;
    Ok(())
}

pub(crate) async fn record_usage_batch(
    pool: &PgPool,
    evs: &[UsageEvent],
) -> Result<(), HistoryError> {
    let mut tx = pool.begin().await?;
    for ev in evs {
        let metadata: Option<Value> = ev
            .metadata
            .as_ref()
            .map(serde_json::to_value)
            .transpose()?;

        sqlx::query(
            r#"
            INSERT INTO usage_events (
                request_id, key_id, endpoint, model, connection, status,
                input_tokens, output_tokens, cost_usd, latency_ms, pii, streamed,
                fallback_attempts, ts_unix_ms, metadata
            ) VALUES (
                $1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15
            )
            ON CONFLICT (request_id) DO NOTHING
            "#,
        )
        .bind(&ev.request_id)
        .bind(&ev.key_id)
        .bind(&ev.endpoint)
        .bind(&ev.model)
        .bind(&ev.connection)
        .bind(ev.status as i32)
        .bind(ev.input_tokens.map(|v| v as i64))
        .bind(ev.output_tokens.map(|v| v as i64))
        .bind(ev.cost_usd)
        .bind(ev.latency_ms as i64)
        .bind(ev.pii)
        .bind(ev.streamed)
        .bind(ev.fallback_attempts as i32)
        .bind(ev.ts_unix_ms as i64)
        .bind(metadata)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub(crate) async fn recent_usage(
    pool: &PgPool,
    limit: i64,
) -> Result<Vec<UsageEvent>, HistoryError> {
    use sqlx::Row;

    let rows = sqlx::query(
        r#"
        SELECT request_id, key_id, endpoint, model, connection, status,
               input_tokens, output_tokens, cost_usd, latency_ms, pii, streamed,
               fallback_attempts, ts_unix_ms, metadata
        FROM usage_events
        ORDER BY ts_unix_ms DESC
        LIMIT $1
        "#,
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;

    rows.iter()
        .map(|r| {
            let metadata_val: Option<Value> = r.try_get("metadata")?;
            let metadata = metadata_val
                .map(serde_json::from_value)
                .transpose()
                .map_err(HistoryError::Json)?;
            Ok(UsageEvent {
                request_id: r.try_get("request_id")?,
                key_id: r.try_get("key_id")?,
                endpoint: r.try_get("endpoint")?,
                model: r.try_get("model")?,
                connection: r.try_get("connection")?,
                status: r.try_get::<i32, _>("status")? as u16,
                input_tokens: r.try_get::<Option<i64>, _>("input_tokens")?.map(|v| v as u64),
                output_tokens: r.try_get::<Option<i64>, _>("output_tokens")?.map(|v| v as u64),
                cost_usd: r.try_get("cost_usd")?,
                latency_ms: r.try_get::<i64, _>("latency_ms")? as u64,
                pii: r.try_get("pii")?,
                streamed: r.try_get("streamed")?,
                fallback_attempts: r.try_get::<i32, _>("fallback_attempts")? as u32,
                ts_unix_ms: r.try_get::<i64, _>("ts_unix_ms")? as u64,
                metadata,
            })
        })
        .collect()
}

pub(crate) async fn usage_timeseries(
    pool: &PgPool,
    since_ms: i64,
    until_ms: i64,
    bucket: Bucket,
) -> Result<Vec<UsageBucket>, HistoryError> {
    use sqlx::Row;

    let trunc = bucket.pg_trunc();
    let sql = format!(
        r#"
        SELECT
            EXTRACT(EPOCH FROM date_trunc('{trunc}',
                to_timestamp(ts_unix_ms / 1000.0)))::bigint * 1000 AS ts_ms,
            COUNT(*)::bigint                         AS requests,
            COALESCE(SUM(input_tokens),0)::bigint   AS input_tokens,
            COALESCE(SUM(output_tokens),0)::bigint  AS output_tokens,
            COALESCE(SUM(cost_usd),0.0)             AS cost_usd,
            -- AVG over a BIGINT column yields NUMERIC; cast so it decodes into f64.
            AVG(latency_ms)::double precision        AS avg_latency_ms
        FROM usage_events
        WHERE ts_unix_ms >= $1 AND ts_unix_ms < $2
        GROUP BY 1
        ORDER BY 1
        "#,
        trunc = trunc,
    );

    let rows = sqlx::query(&sql)
        .bind(since_ms)
        .bind(until_ms)
        .fetch_all(pool)
        .await?;

    Ok(rows
        .iter()
        .map(|r| UsageBucket {
            ts_ms: r.try_get("ts_ms").unwrap_or(0),
            requests: r.try_get("requests").unwrap_or(0),
            input_tokens: r.try_get("input_tokens").unwrap_or(0),
            output_tokens: r.try_get("output_tokens").unwrap_or(0),
            cost_usd: r.try_get("cost_usd").unwrap_or(0.0),
            avg_latency_ms: r.try_get("avg_latency_ms").unwrap_or(0.0),
        })
        .collect())
}

#[instrument(skip_all)]
pub(crate) async fn usage_summary(
    pool: &PgPool,
    since_ms: i64,
    until_ms: i64,
) -> Result<UsageSummary, HistoryError> {
    use sqlx::Row;

    let row = sqlx::query(
        r#"
        SELECT
            COUNT(*)::bigint                                  AS requests,
            COALESCE(SUM(input_tokens),0)::bigint            AS input_tokens,
            COALESCE(SUM(output_tokens),0)::bigint           AS output_tokens,
            COALESCE(SUM(cost_usd),0.0)                       AS cost_usd,
            COALESCE(AVG(latency_ms)::double precision, 0.0)  AS avg_latency_ms,
            COUNT(*) FILTER (WHERE pii)::bigint               AS pii_count,
            COUNT(*) FILTER (WHERE status >= 400)::bigint     AS error_count
        FROM usage_events
        WHERE ts_unix_ms >= $1 AND ts_unix_ms < $2
        "#,
    )
    .bind(since_ms)
    .bind(until_ms)
    .fetch_one(pool)
    .await?;

    Ok(UsageSummary {
        requests: row.try_get("requests").unwrap_or(0),
        input_tokens: row.try_get("input_tokens").unwrap_or(0),
        output_tokens: row.try_get("output_tokens").unwrap_or(0),
        cost_usd: row.try_get("cost_usd").unwrap_or(0.0),
        avg_latency_ms: row.try_get("avg_latency_ms").unwrap_or(0.0),
        pii_count: row.try_get("pii_count").unwrap_or(0),
        error_count: row.try_get("error_count").unwrap_or(0),
    })
}

#[instrument(skip_all)]
pub(crate) async fn usage_by(
    pool: &PgPool,
    since_ms: i64,
    until_ms: i64,
    column: &'static str,
) -> Result<Vec<DimCount>, HistoryError> {
    use sqlx::Row;

    // `column` is interpolated, never bound — so it MUST be a whitelisted
    // identifier (same safe pattern as `Bucket::pg_trunc`). Reject anything else.
    assert!(
        matches!(column, "model" | "connection" | "endpoint" | "key_id"),
        "usage_by: disallowed column `{column}`"
    );

    let sql = format!(
        r#"
        SELECT
            {column}                                 AS label,
            COUNT(*)::bigint                         AS requests,
            COALESCE(SUM(input_tokens),0)::bigint   AS input_tokens,
            COALESCE(SUM(output_tokens),0)::bigint  AS output_tokens,
            COALESCE(SUM(cost_usd),0.0)             AS cost_usd
        FROM usage_events
        WHERE ts_unix_ms >= $1 AND ts_unix_ms < $2
        GROUP BY {column}
        ORDER BY requests DESC
        LIMIT 50
        "#,
        column = column,
    );

    let rows = sqlx::query(&sql)
        .bind(since_ms)
        .bind(until_ms)
        .fetch_all(pool)
        .await?;

    Ok(rows
        .iter()
        .map(|r| DimCount {
            label: r.try_get("label").unwrap_or_default(),
            requests: r.try_get("requests").unwrap_or(0),
            input_tokens: r.try_get("input_tokens").unwrap_or(0),
            output_tokens: r.try_get("output_tokens").unwrap_or(0),
            cost_usd: r.try_get("cost_usd").unwrap_or(0.0),
        })
        .collect())
}

// ── Audit log ─────────────────────────────────────────────────────────────────

pub(crate) async fn append_audit(
    pool: &PgPool,
    entry: &AuditEntry,
) -> Result<(), HistoryError> {
    sqlx::query(
        r#"
        INSERT INTO audit_log (ts_unix_ms, actor, action, target, detail)
        VALUES ($1, $2, $3, $4, $5)
        "#,
    )
    .bind(entry.ts_unix_ms)
    .bind(&entry.actor)
    .bind(&entry.action)
    .bind(&entry.target)
    .bind(&entry.detail)
    .execute(pool)
    .await?;
    Ok(())
}

pub(crate) async fn recent_audit(
    pool: &PgPool,
    limit: i64,
) -> Result<Vec<AuditEntry>, HistoryError> {
    use sqlx::Row;

    let rows = sqlx::query(
        r#"
        SELECT ts_unix_ms, actor, action, target, detail
        FROM audit_log
        ORDER BY ts_unix_ms DESC
        LIMIT $1
        "#,
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|r| AuditEntry {
            ts_unix_ms: r.try_get("ts_unix_ms").unwrap_or(0),
            actor: r.try_get("actor").unwrap_or_default(),
            action: r.try_get("action").unwrap_or_default(),
            target: r.try_get("target").unwrap_or_default(),
            detail: r.try_get("detail").unwrap_or(Value::Null),
        })
        .collect())
}

// ── Users ─────────────────────────────────────────────────────────────────────

pub(crate) async fn create_user(
    pool: &PgPool,
    username: &str,
    password_hash: &str,
) -> Result<i64, HistoryError> {
    use sqlx::Row;

    let row = sqlx::query(
        r#"
        INSERT INTO users (username, password_hash)
        VALUES ($1, $2)
        RETURNING id
        "#,
    )
    .bind(username)
    .bind(password_hash)
    .fetch_one(pool)
    .await?;
    Ok(row.try_get("id")?)
}

pub(crate) async fn find_user(
    pool: &PgPool,
    username: &str,
) -> Result<Option<UserRow>, HistoryError> {
    use sqlx::Row;

    let row = sqlx::query(
        r#"SELECT id, username, password_hash FROM users WHERE username = $1"#,
    )
    .bind(username)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| UserRow {
        id: r.try_get("id").unwrap_or(0),
        username: r.try_get("username").unwrap_or_default(),
        password_hash: r.try_get("password_hash").unwrap_or_default(),
    }))
}

// ── Sessions ──────────────────────────────────────────────────────────────────

pub(crate) async fn create_session(
    pool: &PgPool,
    session_id: &str,
    user_id: i64,
    expires_ms: i64,
) -> Result<(), HistoryError> {
    sqlx::query(
        r#"
        INSERT INTO sessions (session_id, user_id, expires_ms)
        VALUES ($1, $2, $3)
        "#,
    )
    .bind(session_id)
    .bind(user_id)
    .bind(expires_ms)
    .execute(pool)
    .await?;
    Ok(())
}

pub(crate) async fn get_session(
    pool: &PgPool,
    session_id: &str,
) -> Result<Option<(i64, i64)>, HistoryError> {
    use sqlx::Row;

    let row = sqlx::query(
        r#"SELECT user_id, expires_ms FROM sessions WHERE session_id = $1"#,
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| {
        let user_id: i64 = r.try_get("user_id").unwrap_or(0);
        let expires_ms: i64 = r.try_get("expires_ms").unwrap_or(0);
        (user_id, expires_ms)
    }))
}

pub(crate) async fn delete_session(
    pool: &PgPool,
    session_id: &str,
) -> Result<(), HistoryError> {
    sqlx::query(r#"DELETE FROM sessions WHERE session_id = $1"#)
        .bind(session_id)
        .execute(pool)
        .await?;
    Ok(())
}

// ── Per-key usage queries ─────────────────────────────────────────────────────

#[instrument(skip_all)]
pub(crate) async fn usage_summary_by_key(
    pool: &PgPool,
    key_id: &str,
    since_ms: i64,
    until_ms: i64,
) -> Result<UsageSummary, HistoryError> {
    use sqlx::Row;

    let row = sqlx::query(
        r#"
        SELECT
            COUNT(*)::bigint                                  AS requests,
            COALESCE(SUM(input_tokens),0)::bigint            AS input_tokens,
            COALESCE(SUM(output_tokens),0)::bigint           AS output_tokens,
            COALESCE(SUM(cost_usd),0.0)                       AS cost_usd,
            COALESCE(AVG(latency_ms)::double precision, 0.0)  AS avg_latency_ms,
            COUNT(*) FILTER (WHERE pii)::bigint               AS pii_count,
            COUNT(*) FILTER (WHERE status >= 400)::bigint     AS error_count
        FROM usage_events
        WHERE key_id = $1 AND ts_unix_ms >= $2 AND ts_unix_ms < $3
        "#,
    )
    .bind(key_id)
    .bind(since_ms)
    .bind(until_ms)
    .fetch_one(pool)
    .await?;

    Ok(UsageSummary {
        requests: row.try_get("requests").unwrap_or(0),
        input_tokens: row.try_get("input_tokens").unwrap_or(0),
        output_tokens: row.try_get("output_tokens").unwrap_or(0),
        cost_usd: row.try_get("cost_usd").unwrap_or(0.0),
        avg_latency_ms: row.try_get("avg_latency_ms").unwrap_or(0.0),
        pii_count: row.try_get("pii_count").unwrap_or(0),
        error_count: row.try_get("error_count").unwrap_or(0),
    })
}

#[instrument(skip_all)]
pub(crate) async fn usage_timeseries_by_key(
    pool: &PgPool,
    key_id: &str,
    since_ms: i64,
    until_ms: i64,
    bucket: Bucket,
) -> Result<Vec<UsageBucket>, HistoryError> {
    use sqlx::Row;

    let trunc = bucket.pg_trunc();
    let sql = format!(
        r#"
        SELECT
            EXTRACT(EPOCH FROM date_trunc('{trunc}',
                to_timestamp(ts_unix_ms / 1000.0)))::bigint * 1000 AS ts_ms,
            COUNT(*)::bigint                         AS requests,
            COALESCE(SUM(input_tokens),0)::bigint   AS input_tokens,
            COALESCE(SUM(output_tokens),0)::bigint  AS output_tokens,
            COALESCE(SUM(cost_usd),0.0)             AS cost_usd,
            AVG(latency_ms)::double precision        AS avg_latency_ms
        FROM usage_events
        WHERE key_id = $1 AND ts_unix_ms >= $2 AND ts_unix_ms < $3
        GROUP BY 1
        ORDER BY 1
        "#,
        trunc = trunc,
    );

    let rows = sqlx::query(&sql)
        .bind(key_id)
        .bind(since_ms)
        .bind(until_ms)
        .fetch_all(pool)
        .await?;

    Ok(rows
        .iter()
        .map(|r| UsageBucket {
            ts_ms: r.try_get("ts_ms").unwrap_or(0),
            requests: r.try_get("requests").unwrap_or(0),
            input_tokens: r.try_get("input_tokens").unwrap_or(0),
            output_tokens: r.try_get("output_tokens").unwrap_or(0),
            cost_usd: r.try_get("cost_usd").unwrap_or(0.0),
            avg_latency_ms: r.try_get("avg_latency_ms").unwrap_or(0.0),
        })
        .collect())
}

/// Group all requests in `[since_ms, until_ms)` by `key_id`, returning one
/// [`DimCount`] per key (label = key_id).
#[instrument(skip_all)]
pub(crate) async fn usage_by_key(
    pool: &PgPool,
    since_ms: i64,
    until_ms: i64,
) -> Result<Vec<DimCount>, HistoryError> {
    usage_by(pool, since_ms, until_ms, "key_id").await
}

// ── User management ───────────────────────────────────────────────────────────

pub(crate) async fn list_users(pool: &PgPool) -> Result<Vec<UserRow>, HistoryError> {
    use sqlx::Row;

    let rows = sqlx::query(r#"SELECT id, username, password_hash FROM users ORDER BY id"#)
        .fetch_all(pool)
        .await?;

    Ok(rows
        .iter()
        .map(|r| UserRow {
            id: r.try_get("id").unwrap_or(0),
            username: r.try_get("username").unwrap_or_default(),
            password_hash: r.try_get("password_hash").unwrap_or_default(),
        })
        .collect())
}

pub(crate) async fn delete_user(pool: &PgPool, id: i64) -> Result<(), HistoryError> {
    sqlx::query(r#"DELETE FROM users WHERE id = $1"#)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

// ── PII detections ────────────────────────────────────────────────────────────

pub(crate) async fn record_pii_detections(
    pool: &PgPool,
    rows: &[PiiDetectionRow],
) -> Result<(), HistoryError> {
    let mut tx = pool.begin().await?;
    for r in rows {
        sqlx::query(
            r#"
            INSERT INTO pii_detections (request_id, key_id, entity_kind, count, ts_unix_ms)
            VALUES ($1, $2, $3, $4, $5)
            "#,
        )
        .bind(&r.request_id)
        .bind(&r.key_id)
        .bind(&r.entity_kind)
        .bind(r.count)
        .bind(r.ts_unix_ms)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Aggregate PII detections in `[since_ms, until_ms)` grouped by `entity_kind`.
/// Returns one [`DimCount`] per kind; `requests` = total entity occurrences,
/// `input_tokens` / `output_tokens` / `cost_usd` are zero (not meaningful here).
#[instrument(skip_all)]
pub(crate) async fn pii_by_kind(
    pool: &PgPool,
    since_ms: i64,
    until_ms: i64,
) -> Result<Vec<DimCount>, HistoryError> {
    use sqlx::Row;

    let rows = sqlx::query(
        r#"
        SELECT
            entity_kind                        AS label,
            SUM(count)::bigint                 AS requests,
            0::bigint                          AS input_tokens,
            0::bigint                          AS output_tokens,
            0.0::double precision              AS cost_usd
        FROM pii_detections
        WHERE ts_unix_ms >= $1 AND ts_unix_ms < $2
        GROUP BY entity_kind
        ORDER BY requests DESC
        "#,
    )
    .bind(since_ms)
    .bind(until_ms)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|r| DimCount {
            label: r.try_get("label").unwrap_or_default(),
            requests: r.try_get("requests").unwrap_or(0),
            input_tokens: r.try_get("input_tokens").unwrap_or(0),
            output_tokens: r.try_get("output_tokens").unwrap_or(0),
            cost_usd: r.try_get("cost_usd").unwrap_or(0.0),
        })
        .collect())
}

/// Time-series of total PII detection counts in `[since_ms, until_ms)`,
/// bucketed by `bucket` granularity.  Each [`UsageBucket`]'s `requests` field
/// holds the summed entity count; token/cost fields are zero.
#[instrument(skip_all)]
pub(crate) async fn pii_timeseries(
    pool: &PgPool,
    since_ms: i64,
    until_ms: i64,
    bucket: Bucket,
) -> Result<Vec<UsageBucket>, HistoryError> {
    use sqlx::Row;

    let trunc = bucket.pg_trunc();
    let sql = format!(
        r#"
        SELECT
            EXTRACT(EPOCH FROM date_trunc('{trunc}',
                to_timestamp(ts_unix_ms / 1000.0)))::bigint * 1000 AS ts_ms,
            SUM(count)::bigint  AS requests,
            0::bigint           AS input_tokens,
            0::bigint           AS output_tokens,
            0.0                 AS cost_usd,
            0.0                 AS avg_latency_ms
        FROM pii_detections
        WHERE ts_unix_ms >= $1 AND ts_unix_ms < $2
        GROUP BY 1
        ORDER BY 1
        "#,
        trunc = trunc,
    );

    let rows = sqlx::query(&sql)
        .bind(since_ms)
        .bind(until_ms)
        .fetch_all(pool)
        .await?;

    Ok(rows
        .iter()
        .map(|r| UsageBucket {
            ts_ms: r.try_get("ts_ms").unwrap_or(0),
            requests: r.try_get("requests").unwrap_or(0),
            input_tokens: 0,
            output_tokens: 0,
            cost_usd: 0.0,
            avg_latency_ms: 0.0,
        })
        .collect())
}

// ── Webhook deliveries ────────────────────────────────────────────────────────

pub(crate) async fn record_webhook_delivery(
    pool: &PgPool,
    row: &WebhookDeliveryRow,
) -> Result<i64, HistoryError> {
    use sqlx::Row;

    let r = sqlx::query(
        r#"
        INSERT INTO webhook_deliveries
            (request_id, ts_unix_ms, status_code, ok, error, attempt, payload)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        RETURNING id
        "#,
    )
    .bind(&row.request_id)
    .bind(row.ts_unix_ms)
    .bind(row.status_code)
    .bind(row.ok)
    .bind(&row.error)
    .bind(row.attempt)
    .bind(&row.payload)
    .fetch_one(pool)
    .await?;

    Ok(r.try_get("id")?)
}

pub(crate) async fn recent_webhook_deliveries(
    pool: &PgPool,
    limit: u32,
) -> Result<Vec<WebhookDeliveryRow>, HistoryError> {
    use sqlx::Row;

    let rows = sqlx::query(
        r#"
        SELECT id, request_id, ts_unix_ms, status_code, ok, error, attempt, payload
        FROM webhook_deliveries
        ORDER BY ts_unix_ms DESC
        LIMIT $1
        "#,
    )
    .bind(limit as i64)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|r| WebhookDeliveryRow {
            id: r.try_get("id").ok(),
            request_id: r.try_get("request_id").unwrap_or_default(),
            ts_unix_ms: r.try_get("ts_unix_ms").unwrap_or(0),
            status_code: r.try_get("status_code").unwrap_or(None),
            ok: r.try_get("ok").unwrap_or(false),
            error: r.try_get("error").unwrap_or(None),
            attempt: r.try_get("attempt").unwrap_or(0),
            payload: r.try_get("payload").unwrap_or(Value::Null),
        })
        .collect())
}

pub(crate) async fn get_webhook_delivery(
    pool: &PgPool,
    id: i64,
) -> Result<Option<WebhookDeliveryRow>, HistoryError> {
    use sqlx::Row;

    let row = sqlx::query(
        r#"
        SELECT id, request_id, ts_unix_ms, status_code, ok, error, attempt, payload
        FROM webhook_deliveries
        WHERE id = $1
        "#,
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| WebhookDeliveryRow {
        id: r.try_get("id").ok(),
        request_id: r.try_get("request_id").unwrap_or_default(),
        ts_unix_ms: r.try_get("ts_unix_ms").unwrap_or(0),
        status_code: r.try_get("status_code").unwrap_or(None),
        ok: r.try_get("ok").unwrap_or(false),
        error: r.try_get("error").unwrap_or(None),
        attempt: r.try_get("attempt").unwrap_or(0),
        payload: r.try_get("payload").unwrap_or(Value::Null),
    }))
}
