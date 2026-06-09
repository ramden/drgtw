-- pii_detections: one row per (request_id, entity_kind) detected by the proxy.
-- ts_unix_ms is milliseconds since Unix epoch.

CREATE TABLE IF NOT EXISTS pii_detections (
    request_id  TEXT    NOT NULL,
    key_id      TEXT    NOT NULL,
    entity_kind TEXT    NOT NULL,
    count       INTEGER NOT NULL,
    ts_unix_ms  BIGINT  NOT NULL
);

CREATE INDEX IF NOT EXISTS pii_detections_ts  ON pii_detections (ts_unix_ms);
CREATE INDEX IF NOT EXISTS pii_detections_key ON pii_detections (key_id);

-- webhook_deliveries: one row per outbound webhook attempt made by EventSink.
-- payload is the full JSON body that was sent.

CREATE TABLE IF NOT EXISTS webhook_deliveries (
    id          BIGSERIAL   PRIMARY KEY,
    request_id  TEXT        NOT NULL,
    ts_unix_ms  BIGINT      NOT NULL,
    status_code INTEGER,
    ok          BOOLEAN     NOT NULL,
    error       TEXT,
    attempt     INTEGER     NOT NULL,
    payload     JSONB       NOT NULL
);

CREATE INDEX IF NOT EXISTS webhook_deliveries_ts ON webhook_deliveries (ts_unix_ms);
