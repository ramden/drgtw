-- usage_events: one row per gateway request.
-- ts_unix_ms is milliseconds since Unix epoch (stored as BIGINT, no chrono dep).
-- metadata is arbitrary JSONB operator-supplied routing metadata.

CREATE TABLE IF NOT EXISTS usage_events (
    request_id       TEXT        NOT NULL PRIMARY KEY,
    key_id           TEXT        NOT NULL,
    endpoint         TEXT        NOT NULL,
    model            TEXT        NOT NULL,
    connection       TEXT        NOT NULL,
    status           INTEGER     NOT NULL,
    input_tokens     BIGINT,
    output_tokens    BIGINT,
    cost_usd         DOUBLE PRECISION,
    latency_ms       BIGINT      NOT NULL,
    pii              BOOLEAN     NOT NULL DEFAULT FALSE,
    streamed         BOOLEAN     NOT NULL DEFAULT FALSE,
    fallback_attempts INTEGER    NOT NULL DEFAULT 0,
    ts_unix_ms       BIGINT      NOT NULL,
    metadata         JSONB
);

CREATE INDEX IF NOT EXISTS usage_events_ts ON usage_events (ts_unix_ms);
CREATE INDEX IF NOT EXISTS usage_events_key ON usage_events (key_id);

-- audit_log: append-only operator audit trail.

CREATE TABLE IF NOT EXISTS audit_log (
    id          BIGSERIAL   PRIMARY KEY,
    ts_unix_ms  BIGINT      NOT NULL,
    actor       TEXT        NOT NULL,
    action      TEXT        NOT NULL,
    target      TEXT        NOT NULL,
    detail      JSONB
);

CREATE INDEX IF NOT EXISTS audit_log_ts ON audit_log (ts_unix_ms);

-- users: simple local user store for DB-mode authentication.

CREATE TABLE IF NOT EXISTS users (
    id            BIGSERIAL   PRIMARY KEY,
    username      TEXT        NOT NULL UNIQUE,
    password_hash TEXT        NOT NULL
);

-- sessions: opaque session tokens bound to a user.

CREATE TABLE IF NOT EXISTS sessions (
    session_id  TEXT    NOT NULL PRIMARY KEY,
    user_id     BIGINT  NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    expires_ms  BIGINT  NOT NULL
);

CREATE INDEX IF NOT EXISTS sessions_user ON sessions (user_id);
