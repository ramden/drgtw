# Status & health endpoints

drgtw exposes three cheap, no-LLM endpoints for monitoring and ops dashboards.
`/health` and `/health/ready` are always open; `/info` is open by default but can
be gated with a token (see below). None of them ever emits a secret.

| Endpoint | Purpose | Auth |
|----------|---------|------|
| `GET /health` | Liveness. Always `200 {"status":"ok"}`. | none |
| `GET /health/ready` | Readiness. `200 {"status":"ready","ner_loaded":<bool>}`. | none |
| `GET /info` | Rich, versioned service/config status. | none, or `x-health-token` if `[server] status_token` is set |

## `GET /info`

Cheap (no upstream/LLM call), machine-readable, and **versioned**: fields are
only ever added — never removed or repurposed — so a `schema_version` consumer
can pin to the shape. It reflects the **live** (hot-reload-aware) effective
config, so it is the source of truth for what a running replica is actually
doing.

### Example

```json
{
  "service": "drgtw",
  "schema_version": 1,
  "version": "0.0.16",
  "build": { "git_sha": "1a2b3c4d5e6f", "built_at": "2026-07-09T10:15:00Z" },
  "started_at": "2026-07-09T09:00:00Z",
  "uptime_seconds": 4500,
  "models": [
    { "model": "gpt-4.1-mini", "connection": "azure-openai", "format": "open_ai" },
    { "model": "text-embedding-3-small", "connection": "azure-openai", "format": "open_ai" }
  ],
  "model_aliases": { "fast": "gpt-4.1-mini" },
  "pii": {
    "enabled_by_default": true,
    "require_ner": true,
    "embeddings_disable_pii": false,
    "embeddings_require_vault": false,
    "entities": ["PERSON", "EMAIL"],
    "disabled_recognizers": [],
    "custom_recognizers": ["ticket"],
    "vault_configured": true,
    "ner": {
      "loaded": true,
      "model": "ner-multilingual",
      "score_threshold": 0.5,
      "fail_mode": "open",
      "timeout_ms": 5000,
      "workers": 4,
      "queue_capacity": 64,
      "scan_roles": ["user", "assistant"],
      "cache_capacity": 102
    }
  },
  "guardrails": {
    "enabled": true,
    "rules": [
      { "name": "block-injection", "kind": "prompt_injection", "phase": "pre", "action": "block" }
    ]
  },
  "mcp": {
    "enabled": true,
    "server_count": 1,
    "servers": [{ "name": "github", "description": "GitHub tools" }]
  },
  "otel": { "enabled": true, "traces": true, "metrics": true },
  "events": { "enabled": true },
  "tracing": { "enabled": true },
  "config_fingerprint": "sha256:9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
}
```

### Detecting config / env drift

`config_fingerprint` is a SHA-256 over the **fully-resolved** config (after
`${ENV_VAR}` substitution). Two replicas that should be identical but report
different fingerprints have drifted — including drift introduced only by
environment variables, which a file diff would miss.

The most common latency-affecting drift is a `[pii.ner]` block that lost its
`scan_roles` / `cache_capacity` tuning (e.g. a stale env template). Without
`scan_roles`, NER re-scans the full system prompt every turn; without a warm
`cache_capacity`, identical prefixes are re-inferred. Both are visible directly
under `pii.ner`, and any change flips `config_fingerprint`. Poll `/info` across
replicas and alert on fingerprint mismatch.

### What is never exposed

Only names, booleans, counts, numeric knobs, and the one-way fingerprint are
returned. The endpoint never emits: upstream API keys or base URLs, AWS
credentials, the entity-vault path or key, UI password hash / session key,
event-sink URL or bearer, MCP server URLs or auth, the OTLP endpoint, the
Postgres URL, guardrail or custom-recognizer **regex patterns**, the NER model
**path** (only its basename), or the listen address.

### Optional token gate

Set `[server] status_token` (supports `${ENV_VAR}`) to require callers to send a
matching `x-health-token` header on `/info`; mismatches get `401`. `/health` and
`/health/ready` stay open so orchestration probes never need a secret.

```bash
curl -fsS http://localhost:8080/info                              # open
curl -fsS -H "x-health-token: $TOKEN" http://localhost:8080/info  # gated
```

## `GET /health/ready`

Returns `200` once the gateway is serving. `ner_loaded` reports whether a NER
model is configured and was loaded at boot, letting orchestrators distinguish
"up but PII-degraded" from fully ready. Note: ONNX first-inference warmup after a
restart can still add latency to the first few requests; `started_at` /
`uptime_seconds` on `/info` flag a recently-restarted replica.

## Prometheus `/metrics`

Not yet exposed as a scrape endpoint. drgtw already exports metrics via OTLP —
enable `[otel] metrics = true` and point it at your collector. A native
`/metrics` endpoint is on the roadmap.
