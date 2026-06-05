# DRGTW Configuration Reference

DRGTW is configured via a single TOML file (default: `drgtw.toml`).
All sections except `[[connections]]` and `[[virtual_keys]]` are optional.

---

## `[server]`

Optional. Configures the gateway listener.

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `bind_addr` | string (SocketAddr) | no | `"127.0.0.1:8080"` | Address and port the gateway listens on. Any value accepted by Rust's `SocketAddr` parser. |

---

## `[[connections]]`

Array of upstream provider connections. Each entry is a TOML array-of-tables block.

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `name` | string | yes | — | Unique identifier. Referenced by `[[virtual_keys]].connections`. |
| `base_url` | string (URL) | yes | — | Absolute `http://` or `https://` base URL. No query string or fragment allowed. Supports `${ENV_VAR}` references. |
| `api_key` | string | yes | — | Upstream API key. Supports `${ENV_VAR}` references (see [Env-var resolution](#env-var-resolution)). |
| `format` | enum string | yes | — | Wire protocol. One of `"open_ai"`, `"anthropic"`, or `"bedrock"`. |
| `models` | array of strings | no | `[]` | Model names served by this connection. Used for routing and allowlist checks. |

### `format` values

| TOML value | Upstream protocol |
|------------|-------------------|
| `"open_ai"` | OpenAI Chat Completions API (`POST {base_url}/chat/completions`) |
| `"anthropic"` | Anthropic Messages API (`POST {base_url}/v1/messages`) |
| `"bedrock"` | AWS Bedrock native `InvokeModel` (`POST {base_url}/model/{model}/invoke`), non-streaming, bearer auth, Anthropic-shaped body. See [AWS Bedrock](#aws-bedrock). |

---

## `[[virtual_keys]]`

Array of credentials issued to downstream callers. Callers supply these as Bearer tokens (or in the `x-api-key` header for Anthropic-format requests) using stock OpenAI or Anthropic SDKs.

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `key` | string | yes | — | The key value. Must start with `sk-drgtw-`. Must be unique across all virtual keys. |
| `connections` | array of strings | yes | — | One or more connection `name` values this key may route to. Must be non-empty; all names must resolve to a defined connection. |
| `models` | array of strings | no | `null` (all) | Optional model allowlist. When omitted, all models of the listed connections are permitted. When present, requests for any other model are rejected with 403. |

---

## `[model_aliases]`

Optional. A flat table mapping an **alias** model name to a **target** model name. Maps to `Config.model_aliases: HashMap<String, String>` (serde default: empty).

| Form | Type | Required | Default | Description |
|------|------|----------|---------|-------------|
| `<alias> = "<target>"` | string → string | no | `{}` | When a request's `model` equals `<alias>`, it is rewritten to `<target>` before any routing decision. |

When an incoming request's `model` matches an alias key, the gateway rewrites the request body's `model` field to the target **before** connection routing, the virtual-key model allowlist check, cost-table lookup, and usage-event emission — so every downstream component sees the resolved model, and the resolved model name is what is forwarded upstream.

**Resolution is one level only.** If a target is itself an alias key, it is **not** re-resolved (no recursive chains). For example, with `a = "b"` and `b = "c"`, a request for `a` resolves to `b` (not `c`).

```toml
[model_aliases]
fast  = "gpt-4o-mini"
smart = "gpt-4o"
```

---

## `[events]`

Optional. Usage-event webhook. The gateway POSTs one JSON event per completed proxied request. Events carry **metadata only** (tokens, cost, latency, model, connection, key id) — never request/response content or API keys. Delivery is fire-and-forget: if the receiver is slow or unreachable, events are dropped and the request path is never blocked.

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `url` | string (URL) | yes | — | Ingest endpoint events are POSTed to. Supports `${ENV_VAR}`. |
| `auth_bearer` | string | no | — | Optional Bearer token sent as `Authorization: Bearer <value>`. Supports `${ENV_VAR}`. |
| `buffer_size` | int | no | `1024` | In-memory queue depth. |
| `timeout_ms` | int | no | `5000` | Per-POST timeout in milliseconds. |

### Attribution metadata

For per-agent / per-session cost attribution, each event may carry a `metadata` object (`map<string,string>`). It is populated from two sources, **merged with headers winning on key collision**:

1. **Request body** — the top-level `metadata` object (the OpenAI / Anthropic `metadata` field, including Anthropic `user_id`). String values are taken verbatim; non-string values are JSON-stringified. After harvesting, the `metadata` object is **stripped from the forwarded body**: several OpenAI-compatible upstreams (e.g. Azure OpenAI) reject unknown parameters with `400`. If the provider itself must see a metadata field (e.g. Anthropic abuse-detection `user_id`), use the headers below for gateway attribution and accept that body `metadata` does not pass through.
2. **Request headers** — any header prefixed `x-drgtw-meta-`. The prefix is stripped and the remainder lowercased to form the key (`x-drgtw-meta-session-id: abc` → `session-id = "abc"`). These headers are **stripped before the request is forwarded upstream**, so they never leak to the provider.

**Caps** (applied after merge): at most **16 keys**; keys longer than **64 characters** are dropped; values are truncated to **256 characters**. When more than 16 keys remain, the excess is dropped deterministically in sorted (ascending) key order. When no metadata is supplied, the `metadata` field is **omitted** from the serialized event (backward compatible with receivers that predate this feature).

---

## `[pii]`

Optional. PII pseudonymization settings. The engine is a Phase 3 deliverable; this section is parsed and validated now but has no runtime effect until Phase 3.

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `enabled_by_default` | bool | no | `false` | When `true`, PII pseudonymization is applied to every request unless the caller opts out via `x-drgtw-pii: off`. When `false`, it is applied only when the caller sends `x-drgtw-pii: on`. |

---

## `[tracing]`

Optional. Filesystem request tracing, logrotate-style. **On by default** — set `enabled = false` to turn it off entirely.

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `enabled` | bool | no | `true` | Master switch. When `false`, no tracing runs and no files are written. |
| `dir` | string | no | `"traces"` | Directory where trace files are written. A relative path resolves against the config file's directory. |
| `retention_days` | int | no | `90` | Archives (and any stray rotated `.jsonl`) older than this many days are deleted. |
| `rotate_max_bytes` | int | no | `52428800` | Size threshold (bytes) at which the active trace file rotates. Default `52428800` = 50 MiB. |
| `archive_after_files` | int | no | `10` | Number of rotated files that accumulate before they are bundled into a single `tar.gz` archive. |

### Behavior

Traces are written as JSONL (one JSON object per line), like system logs. The active file is `drgtw-trace.jsonl` in `dir`; one line is appended per traced request.

- **Rotation.** When the active file reaches `rotate_max_bytes`, it is renamed with a UTC timestamp (`drgtw-trace-<yyyymmdd-HHMMSS>.jsonl`) and a fresh active file is opened.
- **Archiving.** Once `archive_after_files` rotated files have accumulated, they are bundled into a single `tar.gz` archive (`traces-<yyyymmdd-HHMMSS>.tar.gz`) in the same directory and the originals are removed — the same lifecycle as `logrotate`.
- **Retention.** Archives older than `retention_days` are deleted.

### What is traced

- **LLM endpoints** (chat, messages, embeddings, models) trace **metadata only** — request id, virtual key name (never the secret), status, latency, model, connection, and token counts. Prompt and response **bodies are never written**, consistent with the gateway's PII guarantees.
- **MCP tool calls** (through `/mcp`) trace the **full call**: method, tool name, server, arguments, and outputs. Argument and output fields larger than 64 KiB are truncated with a `…[truncated]` marker.

---

## `[otel]`

Optional. OpenTelemetry OTLP export of traces and metrics. **Off by default** — set `enabled = true` to turn it on. Always compiled in; gated purely at runtime.

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `enabled` | bool | no | `false` | Master switch. When `false`, nothing is installed and the rest of the section is inert. |
| `endpoint` | string | no | `"http://localhost:4317"` | OTLP collector URL. gRPC conventionally uses port `4317`, HTTP `4318`. The `OTEL_EXPORTER_OTLP_ENDPOINT` env var, if set, overrides this at startup. |
| `protocol` | string | no | `"grpc"` | Transport: `"grpc"` or `"http"` (OTLP/HTTP binary protobuf; the `/v1/traces` / `/v1/metrics` signal paths are appended automatically). |
| `service_name` | string | no | `"drgtw"` | `service.name` resource attribute. |
| `traces` | bool | no | `true` | Export request spans (only takes effect when `enabled`). |
| `metrics` | bool | no | `true` | Export metrics (only takes effect when `enabled`). |
| `sample_ratio` | float | no | `1.0` | Parent-based trace sampling ratio, `0.0..=1.0`. Validated at boot. |
| `export_interval_ms` | int | no | `10000` | Periodic metric push interval. |
| `export_timeout_ms` | int | no | `5000` | Per-export deadline (trace batches and metric pushes). |
| `metrics_include_key_id` | bool | no | `false` | Include `drgtw.key_id` as a **metric** label. Off by default because it multiplies metric cardinality (keys × models × connections × status). Spans always carry `key_id` — spans are not aggregated. |

### Privacy allow-list

Spans and metrics carry **only** allow-listed, content-free metadata: model (request/response), connection name, upstream host/port, status / error class, token counts, cost USD, latency, time-to-first-chunk, `key_id` (virtual-key *name*, never the secret), `pii` flag, `request_id` (spans only), endpoint/operation, and fallback attempts. Prompt/response content, PII values, pseudonyms, and secrets are never emitted, and there is no configuration switch that enables content capture. This is enforced by construction (the telemetry type has no field that can hold content) and guarded by tests.

### Signals

- **Spans.** The per-request `proxy_request` span is enriched with [GenAI semantic-convention](https://opentelemetry.io/docs/specs/semconv/gen-ai/) attributes (`gen_ai.operation.name`, `gen_ai.provider.name`, `gen_ai.request.model`, `gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens`, …) plus `drgtw.*`-namespaced gateway attributes (connection, cost, pii flag, fallback attempts). For streaming responses the span closes at response handoff (status known, token counts not yet).
- **Metrics.** GenAI histograms `gen_ai.client.operation.duration` and `gen_ai.client.token.usage` (split by `gen_ai.token.type`), plus gateway counters `drgtw.requests`, `drgtw.tokens.input`, `drgtw.tokens.output`, `drgtw.cost.usd`, and `drgtw.pii.redactions`. Labels are cardinality-controlled: `request_id` is never a metric label; `key_id` only with `metrics_include_key_id = true`. Token/cost metrics for streaming responses are recorded at stream completion.

---

## `[mcp_servers.<name>]`

Optional. Each table declares one upstream [Model Context Protocol](https://modelcontextprotocol.io)
server aggregated behind the gateway's `/mcp` endpoint. The `<name>` in the table
key is the server name; it becomes the prefix on every tool that server exposes
(`<name>-<tool>`). Maps to `Config.mcp_servers: HashMap<String, McpServerConfig>`
(serde default: empty). See the [MCP gateway guide](mcp.md) for routing semantics
and client setup.

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `url` | string (URL) | yes | — | Upstream MCP endpoint. Absolute `http://` or `https://`, no query string or fragment. Streamable HTTP transport. Supports `${ENV_VAR}`. |
| `description` | string | no | — | Free-text label. Informational only. |
| `auth_type` | enum string | no | `"none"` | Upstream auth scheme: `"none"`, `"api_key"`, or `"bearer"`. |
| `auth_value` | string | conditional | — | Credential value. Required iff `auth_type != "none"`. Supports `${ENV_VAR}`. |
| `extra_headers` | table of string→string | no | `{}` | Static headers added to every upstream request. Values support `${ENV_VAR}`. |

### `auth_type` → upstream header

| TOML value | Header sent to upstream |
|------------|-------------------------|
| `"none"` | (none added) |
| `"api_key"` | `X-API-Key: <auth_value>` |
| `"bearer"` | `Authorization: Bearer <auth_value>` |

Example:

```toml
[mcp_servers.context7]
url = "https://mcp.context7.com/mcp"
description = "library documentation"
auth_type = "none"

[mcp_servers.context7.extra_headers]
CONTEXT7_API_KEY = "${CONTEXT7_API_KEY}"
```

---

## Env-var resolution

`${VAR}` references in `api_key`, `base_url`, `[mcp_servers.<name>].auth_value`, and `[mcp_servers.<name>].extra_headers` values are expanded from the process environment at startup.

- Syntax: `${VARIABLE_NAME}` — curly braces required.
- A bare `$NAME` (no braces) is treated as a literal string, not a variable reference.
- If the referenced variable is not set at startup, the process exits with an error identifying the missing variable and the field that referenced it.
- Multiple references in a single value are each resolved independently.

Example: `api_key = "${MY_KEY}"` — `MY_KEY` must be set in the environment.

---

## Validation rules

Rules enforced by `drgtw_config::load()` at startup:

1. Connection `name` values must be non-empty and unique.
2. `base_url` must be an absolute `http://` or `https://` URL with no query string or fragment.
3. All `${VAR}` references in `api_key` and `base_url` must resolve to non-empty environment variables; unresolved references are a hard startup error.
4. `format` must be exactly `"open_ai"`, `"anthropic"`, or `"bedrock"`.
5. Virtual key `key` values must start with `sk-drgtw-` and be unique.
6. Each name in `[[virtual_keys]].connections` must match a defined connection `name`.
7. `[[virtual_keys]].connections` must be non-empty.
8. Each `[mcp_servers.<name>]` server name must be non-empty and ASCII `[a-zA-Z0-9_-]` only.
9. `[mcp_servers.<name>].url` must be an absolute `http://` or `https://` URL with no query string or fragment.
10. `[mcp_servers.<name>].auth_value` must be non-empty iff `auth_type != "none"`.
11. Every `[mcp_servers.<name>].extra_headers` key must be a valid HTTP header name.

---

## AWS Bedrock

DRGTW reaches Amazon Bedrock with a **Bedrock API key** (bearer token) — no AWS
SigV4 signing. Generate one in the Bedrock console; the conventional environment
variable is `AWS_BEARER_TOKEN_BEDROCK`. The region is encoded in the hostname.

There are two ways to connect, depending on which wire format you want to speak.

### Option A — OpenAI-compatible endpoint (recommended)

Bedrock exposes an OpenAI Chat Completions surface. Point a standard
`format = "open_ai"` connection at the regional Bedrock base URL **including the
`/v1` suffix**. This covers Anthropic, OpenAI-OSS, and other Bedrock-hosted
families through one schema, and supports streaming (standard SSE) as well as
non-streaming.

```toml
[[connections]]
name = "bedrock-eu"
base_url = "https://bedrock-runtime.eu-central-1.amazonaws.com/v1"
api_key = "${AWS_BEARER_TOKEN_BEDROCK}"
format = "open_ai"
models = ["eu.anthropic.claude-sonnet-4-6", "openai.gpt-oss-120b"]

[connections.model_costs."eu.anthropic.claude-sonnet-4-6"]
input_per_1m = 3.0
output_per_1m = 15.0
```

The gateway issues `POST {base_url}/chat/completions` with
`Authorization: Bearer <key>`. For streaming usage accounting, the client must
send `stream_options.include_usage = true` (same as upstream OpenAI).

### Option B — native InvokeModel (`format = "bedrock"`)

A `format = "bedrock"` connection serves the **`/v1/messages` endpoint surface**
(callers use the Anthropic Messages body) and dispatches to Bedrock's native
`InvokeModel` operation. The base URL has **no `/v1` suffix**:

```toml
[[connections]]
name = "bedrock-native-eu"
base_url = "https://bedrock-runtime.eu-central-1.amazonaws.com"
api_key = "${AWS_BEARER_TOKEN_BEDROCK}"
format = "bedrock"
models = ["anthropic.claude-3-5-sonnet-20241022-v2:0"]

[connections.model_costs."anthropic.claude-3-5-sonnet-20241022-v2:0"]
input_per_1m = 3.0
output_per_1m = 15.0
```

Behaviour:

- The gateway issues `POST {base_url}/model/{model}/invoke` with
  `Authorization: Bearer <key>`. The model id is taken from the request body's
  `model` field, moved into the URL path (the `:` in a `...-v2:0` revision
  suffix is percent-encoded to `%3A`), and removed from the body.
- If the body has no `anthropic_version`, the gateway injects
  `"anthropic_version": "bedrock-2023-05-31"`. A client-supplied value is
  preserved.
- Usage and cost come from the Anthropic-shaped response
  (`usage.input_tokens` / `usage.output_tokens`).
- **Streaming is not supported in this release.** A request with
  `"stream": true` against a `bedrock` connection is rejected with HTTP 400
  (Anthropic error body) and **no upstream call is made**. Use Option A for
  streaming.

### Model ids and cost keys

Bedrock model ids and cross-region inference-profile ids are used **verbatim** —
there is no normalisation layer. Examples:

| Form | Example |
|------|---------|
| Foundation model id | `anthropic.claude-3-5-sonnet-20241022-v2:0` (note the `:0` revision) |
| Cross-region inference profile | `eu.anthropic.claude-sonnet-4-6`, `us.anthropic.claude-sonnet-4-6` |
| OpenAI-OSS on Bedrock | `openai.gpt-oss-120b` |

Both `[[virtual_keys]].models` and `[connections.model_costs]` keys match against
the exact id the client sends. Wildcard keys help when the geo prefix or version
suffix varies, e.g. `[connections.model_costs."us.anthropic.claude-*"]`. Bedrock
pricing differs by region and is **not** embedded in the gateway — supply rates
per region in `model_costs`.

---

## Examples

### Minimal configuration

```toml
[[connections]]
name = "openai"
base_url = "https://api.openai.com"
api_key = "${OPENAI_API_KEY}"
format = "open_ai"
models = ["gpt-4o"]

[[virtual_keys]]
key = "sk-drgtw-my-local-key-001"
connections = ["openai"]
```

### Full configuration

```toml
[server]
bind_addr = "0.0.0.0:9090"

[[connections]]
name = "openai-main"
base_url = "https://api.openai.com"
api_key = "${OPENAI_API_KEY}"
format = "open_ai"
models = ["gpt-4o", "gpt-4o-mini", "gpt-4-turbo", "o1-preview", "o1-mini"]

[[connections]]
name = "anthropic-main"
base_url = "https://api.anthropic.com"
api_key = "${ANTHROPIC_API_KEY}"
format = "anthropic"
models = ["claude-opus-4-5", "claude-sonnet-4-5", "claude-haiku-4-5"]

[[virtual_keys]]
key = "sk-drgtw-all-models-example-key-001"
connections = ["openai-main", "anthropic-main"]

[[virtual_keys]]
key = "sk-drgtw-restricted-example-key-002"
connections = ["openai-main"]
models = ["gpt-4o-mini"]

[model_aliases]
fast  = "gpt-4o-mini"
smart = "gpt-4o"

[events]
url = "https://ingest.example.com/llm-usage"

[pii]
enabled_by_default = false

[tracing]
enabled = true
retention_days = 90
```
