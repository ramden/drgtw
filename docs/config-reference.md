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
| `api_key` | string | yes¹ | — | Upstream API key. Supports `${ENV_VAR}` references (see [Env-var resolution](#env-var-resolution)). |
| `format` | enum string | yes | — | Wire protocol. One of `"open_ai"`, `"anthropic"`, `"bedrock"`, or `"bedrock_converse"`. |
| `models` | array of strings | no | `[]` | Model names served by this connection. Used for routing and allowlist checks. |

¹ Required for every format **except** `"bedrock_converse"`, where an empty `api_key` is allowed if SigV4 credentials are supplied instead (see [SigV4 credential fields](#sigv4-credential-fields-bedrock_converse-only)).

### `format` values

| TOML value | Upstream protocol |
|------------|-------------------|
| `"open_ai"` | OpenAI Chat Completions API (`POST {base_url}/chat/completions`) |
| `"anthropic"` | Anthropic Messages API (`POST {base_url}/v1/messages`) |
| `"bedrock"` | AWS Bedrock native `InvokeModel` (`POST {base_url}/model/{model}/invoke`), non-streaming, bearer auth, Anthropic-shaped body. Served on the **`/v1/messages`** endpoint surface. See [AWS Bedrock](#aws-bedrock). |
| `"bedrock_converse"` | AWS Bedrock **Converse / ConverseStream** (`POST {base_url}/model/{model}/converse[-stream]`). Served on the **`/v1/chat/completions`** endpoint surface (callers use the OpenAI body); **streaming supported**; SigV4 or bearer auth. Covers non-Anthropic models (Nova, Llama, Titan, …) as well as Anthropic. See [AWS Bedrock](#aws-bedrock). |

### SigV4 credential fields (`bedrock_converse` only)

These optional fields are only meaningful when `format = "bedrock_converse"`. All `${ENV_VAR}` references are expanded at startup.

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `region` | string | conditional | `null` | AWS region (e.g. `"eu-central-1"`). **Required when SigV4 credentials are present.** |
| `aws_access_key_id` | string | no | `null` | SigV4 access key id. Supports `${ENV_VAR}`. |
| `aws_secret_access_key` | string | no | `null` | SigV4 secret access key. Supports `${ENV_VAR}`. Must be set together with `aws_access_key_id`. |
| `aws_session_token` | string | no | `null` | Optional STS session token. Supports `${ENV_VAR}`. Requires the access-key/secret pair. |

Auth resolution per request: if both `aws_access_key_id` and `aws_secret_access_key` are set, the request is SigV4-signed (adding `Authorization: AWS4-HMAC-SHA256 …`, `x-amz-date`, and — when a session token is present — `x-amz-security-token`); otherwise the non-empty `api_key` is sent as `Authorization: Bearer`.

---

## `[[virtual_keys]]`

Array of credentials issued to downstream callers. Callers supply these as Bearer tokens (or in the `x-api-key` header for Anthropic-format requests) using stock OpenAI or Anthropic SDKs.

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `key` | string | yes | — | The key value. Must start with `sk-drgtw-`. Must be unique across all virtual keys. |
| `connections` | array of strings | yes | — | One or more connection `name` values this key may route to. Must be non-empty; all names must resolve to a defined connection. |
| `models` | array of strings | no | `null` (all) | Optional model allowlist. When omitted, all models of the listed connections are permitted. When present, requests for any other model are rejected with 403. |
| `allow_pii_bypass` | bool | no | `false` | When `true`, this key may disable PII scanning per request via `x-drgtw-pii: off`. Keys without it have the bypass header ignored (fail-closed — PII still scans). See [PII → Per-request PII bypass](pii.md#per-request-pii-bypass-x-drgtw-pii). |

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

Optional. PII pseudonymization. **Fully live** — the engine runs in the request path: detected PII is replaced with stable pseudonyms before the prompt is forwarded upstream, and the original values are restored in the response. See the [PII deployment guide](pii.md) for an operator-focused walkthrough.

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `enabled_by_default` | bool | no | `true` | When `true`, pseudonymization runs on every request unless the caller opts out via `x-drgtw-pii: off`. When `false`, it runs only when the caller sends `x-drgtw-pii: on`. |
| `disabled_recognizers` | array of strings | no | `[]` | Built-in recognizer names to turn off (e.g. `["DATE_TIME"]`). |
| `custom_recognizers` | array of tables | no | `[]` | Extra pattern-based recognizers. See [`[[pii.custom_recognizers]]`](#pii-custom_recognizers). |
| `entities` | array of strings | no | *(absent → keep all)* | Optional allow-list of entity **kinds** to keep. See [Entity allow-list](#entity-allow-list). |
| `require_ner` | bool | no | `false` | When `true` and **no** `[pii.ner]` block is configured, the gateway **refuses to boot** (fail-closed for GDPR). See [NER boot behaviour](#ner-and-missing-model-boot-behaviour). |
| `embeddings_require_vault` | bool | no | `false` | When `true`, embeddings requests require a configured `[pii.vault]` (so pseudonyms are reversible) or the request is rejected. |

### Entity allow-list

`entities` is an optional list of the entity **kinds** that pseudonymization should keep. Names are **Presidio-style, case-insensitive**, and accept short aliases. When `entities` is **absent**, all kinds are kept. An **empty list is rejected** at validation (an empty allow-list would silently disable all masking — express that as `enabled_by_default = false` instead). Filtering is **post-scan**: every recognizer still runs; detections whose kind is not in the list are simply dropped before pseudonymization.

| Canonical name | Aliases | Detected by default? |
|----------------|---------|----------------------|
| `PERSON` | — | NER only |
| `LOCATION` | `LOC` | NER only |
| `ORGANIZATION` | `ORG` | NER only |
| `EMAIL_ADDRESS` | `EMAIL` | yes (regex) |
| `PHONE_NUMBER` | `PHONE` | yes (regex) |
| `CREDIT_CARD` | `CC`, `CARD` | yes (regex) |
| `IBAN_CODE` | `IBAN` | yes (regex) |
| `IP_ADDRESS` | `IP` | yes (regex, IPv4 + IPv6) |
| `DATE_TIME` | `DATE`, `DATETIME` | yes (regex, see note) |
| `NATIONAL_ID` | — | **no built-in detector** |
| `NRP` | — | **no built-in detector** |

A `custom_recognizers` recognizer `name` is also a valid entry in `entities`.

- **`IP_ADDRESS`** matches IPv4 and IPv6 literals.
- **`DATE_TIME`** matches German `DD.MM.YYYY` and ISO `YYYY-MM-DD`. It is **best-effort / high recall**: it validates only `month ≤ 12` and `day ≤ 31`, so it will over-match (e.g. version-like `12.10.2024`). If false positives are unacceptable, drop `DATE_TIME` from `entities` (or add it to `disabled_recognizers`).
- **`PERSON` / `ORGANIZATION` / `LOCATION`** require a NER model (`[pii.ner]`). Without one they are **never masked** — see the boot warning below.
- **`NATIONAL_ID` / `NRP`** have **no built-in detector** in this release. They are reachable only via `custom_recognizers` (a pattern) or a future NER model; listing them in `entities` alone masks nothing.

### `[pii.ner]`

Optional. Named-entity recognition for `PERSON` / `ORGANIZATION` / `LOCATION` (and any other label the model emits). Runs an ONNX multilingual model in a bounded worker pool.

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `model_dir` | string | yes | — | Directory holding the model artifacts (`model_quantized.onnx`, `tokenizer.json`, `config.json`). Supports `${ENV_VAR}`. |
| `score_threshold` | float | no | `0.5` | Minimum confidence (`0.0..=1.0`) for an entity to be kept. |
| `fail_mode` | enum string | no | `"open"` | Behaviour on a **runtime** NER error. `"open"` logs a warning and returns no NER detections (request proceeds, names may pass through). `"closed"` propagates the error to the caller (request fails). |
| `timeout_ms` | int | no | `5000` | Per-inference deadline in milliseconds. Must be `> 0`. |
| `workers` | int | no | `2` | NER worker threads. Must be `> 0`. |
| `queue_capacity` | int | no | `64` | Max queued inference jobs before backpressure. Must be `> 0`. |
| `scan_roles` | array of string | no | — (all roles) | Restrict the NER model to chat messages of these roles (`"system"`, `"user"`, `"assistant"`, `"developer"`, `"tool"`; matched case-insensitively). The Anthropic top-level `system` field counts as role `system`. When omitted, NER runs on every role. **Regex recognizers (email, phone, IBAN, …) always run on every role regardless of this setting** — only the NER model is scoped. An empty list is rejected. |
| `cache_capacity` | int | no | `0` (disabled) | Size of the in-memory NER verdict cache, in distinct input texts. When `> 0`, NER results for byte-identical text are reused across requests with LRU eviction. The key is a 128-bit hash of the input — **no plaintext is stored**, only span offsets/kinds/scores. |

#### Scoping NER by message role (`scan_roles`)

In multi-agent stacks every user turn fans out into many gateway calls, and each call typically repeats a large, static, developer-authored system prompt. NER inference is the dominant cost of a PII scan, so re-running it on an unchanged, PII-free system prompt on every call wastes time and can trigger timeouts.

`scan_roles` lets you skip the NER model on roles you control and know to be PII-free:

```toml
[pii.ner]
model_dir  = "models/ner-multilingual"
scan_roles = ["user", "assistant"]   # NER skips the system prompt
```

This keeps full structured-identifier masking (the cheap regex recognizers still scan every role, including `system`), so compliance for emails/IBANs/cards is unaffected — only the expensive person/org/location model is scoped out.

#### Caching NER verdicts (`cache_capacity`)

For large repeated content — an unchanged system prompt, or a long conversation prefix re-sent across multi-round calls — `cache_capacity` reuses prior NER verdicts instead of re-running inference:

```toml
[pii.ner]
model_dir      = "models/ner-multilingual"
cache_capacity = 1024                # reuse verdicts for repeated text
```

The cache is keyed on a 128-bit hash of the input text; it stores only detected span offsets/kinds/scores, never the text itself. Only successful inferences are cached — a timeout or full-queue error is never cached, so a transient failure cannot poison later requests. `scan_roles` and `cache_capacity` are independent and compose: scope NER off the system prompt *and* cache the verdicts for any other repeated content.

#### Model packaging

The NER model is **not** in the repo and **not** in the base/slim image (it is gitignored and dockerignored). Get it onto a box in one of two ways:

1. **Mount a volume** with the slim image and point `model_dir` at the mount:
   ```
   docker run -v /host/ner-multilingual:/app/models/ner-multilingual ... <user>/drgtw:<version>
   ```
   then set `model_dir = "/app/models/ner-multilingual"`.
2. **Use the model-bundled image tag** `<user>/drgtw:<version>-models` (v0.0.8+), which bakes the model into `/app/models/ner-multilingual`. Zero extra setup — just set `model_dir = "/app/models/ner-multilingual"`.

The Dockerfiles declare `VOLUME ["/app/models", "/app/data"]`.

#### NER and missing-model boot behaviour

- A `[pii.ner]` block whose `model_dir` is **missing or invalid** is a **hard boot failure** (fail-closed) — the gateway will not start with a half-configured masker.
- `require_ner = true` with **no** `[pii.ner]` block is also a hard boot failure.
- `enabled_by_default = true` with **no** `[pii.ner]` block and `require_ner = false` boots, but logs a **warning**: *"PERSON/ORGANIZATION/LOCATION names are NOT masked"*. Regex entities (email, phone, IBAN, credit card, IP, date) still mask.
- `fail_mode` (`open` | `closed`) covers **runtime** NER errors only — inference timeout, full queue, model failure. It does **not** cover the missing-model case above, which always fails at boot regardless of `fail_mode`.

### `[[pii.custom_recognizers]]`

Optional, repeatable. Each block adds a pattern-based recognizer. The recognizer's `name` becomes a valid entity kind for `entities` and for `disabled_recognizers`. Use these for site-specific identifiers (employee numbers, ticket ids) and for `NATIONAL_ID` / `NRP`, which have no built-in detector.

### `[pii.vault]`

Optional. Persistent, reversible pseudonym store. When configured, pseudonym↔original mappings are encrypted at rest, so restoration survives restarts and the same value always maps to the same pseudonym.

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `path` | string | yes | — | SQLite file path (mount `/app/data` in containers). Non-empty. |
| `key` | string | yes | — | Encryption key: **64 hex characters** (32 bytes). Supports `${ENV_VAR}` — keep the literal key out of the file. |

### Worked example (GDPR / EMEA)

```toml
[pii]
enabled_by_default = true          # mask every request unless caller opts out
require_ner        = true          # refuse to boot if NER is misconfigured
entities           = [             # keep only these kinds (case-insensitive, aliases ok)
  "PERSON", "ORG", "LOCATION",
  "EMAIL", "PHONE", "IBAN", "CREDIT_CARD",
]

[pii.ner]
model_dir       = "/app/models/ner-multilingual"
score_threshold = 0.5
fail_mode       = "closed"         # a runtime NER error fails the request, never leaks
timeout_ms      = 5000
workers         = 2

[pii.vault]
path = "/app/data/pii-vault.db"
key  = "${DRGTW_PII_VAULT_KEY}"    # 64 hex chars, supplied via env
```

---

## `[[guardrails.rules]]`

Optional, repeatable. Content-filter rules applied to prompts (pre-call) and/or responses (post-call). Each `[[guardrails.rules]]` block is one rule.

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `name` | string | yes | — | Unique label for the rule (used in logs/traces). Must be non-empty and unique. |
| `kind` | enum string | yes | — | `"prompt_injection"`, `"banned_content"`, or `"contact_info"`. See below. |
| `phase` | enum string | no | `"pre"` | When the rule runs: `"pre"` (request), `"post"` (response), or `"both"`. |
| `action` | enum string | no | `"block"` | What to do on a match: `"block"`, `"redact"`, or `"flag"`. See below. |
| `patterns` | array of strings | no | `[]` | Regex strings. For `prompt_injection` these are **extra** heuristics on top of the built-ins; for `banned_content` they are the blocklist. Ignored by `contact_info`. |
| `entities` | array of strings | no | `[]` | Entity names (same vocabulary as `[pii].entities`). Used by `contact_info` only. |

### Kinds

| `kind` | Matches | Uses |
|--------|---------|------|
| `prompt_injection` | Jailbreak / prompt-injection attempts. Has **built-in** heuristics; `patterns` add more. | `patterns` |
| `banned_content` | Disallowed content by regex blocklist. | `patterns` |
| `contact_info` | Contact PII (email, phone, …) by entity kind. | `entities` |

`prompt_injection` pre-call rules run on the **raw prompt before pseudonymization**, so they see the original text.

### Actions

| `action` | Behaviour (v0.0.8) |
|----------|--------------------|
| `block` | **Enforced.** Returns HTTP `403` — OpenAI body with `content_filter` code, Anthropic `invalid_request_error`. No upstream call. |
| `flag` | **Enforced.** Logs/traces the match and continues. |
| `redact` | **Detect-and-log only.** The match is detected and logged but the body is **not yet mutated** — body-redaction enforcement is a fast-follow. Do not rely on `redact` to strip content in this release. |

### Limitations (v0.0.8)

- **`redact` does not yet mutate the body** — it only detects and logs (see table above). Use `block` for hard enforcement.
- **Post-call guardrails run on non-streaming responses only.** Streaming responses are **not** scanned. If you need response scanning, disable streaming for those routes or rely on pre-call rules.

### Worked example

```toml
[[guardrails.rules]]
name   = "jailbreak"
kind   = "prompt_injection"
phase  = "pre"
action = "block"                   # 403 before any upstream call
patterns = ['(?i)ignore (all )?previous instructions']

[[guardrails.rules]]
name   = "no-secrets"
kind   = "banned_content"
phase  = "both"
action = "block"
patterns = ['(?i)AKIA[0-9A-Z]{16}']   # leaked AWS access key ids

[[guardrails.rules]]
name   = "outbound-contact-info"
kind   = "contact_info"
phase  = "post"                    # non-streaming responses only
action = "flag"                    # log; redact does not yet mutate the body
entities = ["EMAIL", "PHONE"]
```

---

## Strict config (`--strict-config`)

By default, unknown or misplaced TOML keys are **logged as a warning and ignored** so an old config keeps booting against a newer binary. Passing `--strict-config` on the CLI turns any unknown key into a **hard error** — boot fails.

Use it in production, especially for PII. A silently-ignored key can disable protection without any visible failure:

- A misplaced `[ner]` table instead of `[pii.ner]` — the NER block never binds, so `PERSON`/`ORG`/`LOCATION` are never masked.
- `score_threshold` set at `[pii]` level instead of under `[pii.ner]` — silently dropped, NER runs with the default threshold.

Without `--strict-config` these only produce a warning in the log (key paths are reported dotted, e.g. `pii.ner.workers`); with it, the gateway refuses to start until the key is in the right place.

```
drgtw --config drgtw.toml --strict-config
```

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
| `resource_attributes` | table | no | `{}` | Extra resource attributes merged into the exported `Resource`, alongside `service.name`/`service.version`. Use for vendor attributes — e.g. set `"openinference.project.name"` to route spans to a specific project in observability backends that key off it. |

#### Resource attributes & project routing

Some backends route spans by a resource attribute rather than `service.name`. For example, backends built on the OpenInference convention group traces by `openinference.project.name`; without it, every span lands in the backend's `default` project. Set it three ways (highest precedence last):

1. **Config** — `[otel.resource_attributes]` table:
   ```toml
   [otel.resource_attributes]
   "openinference.project.name" = "my-project"
   ```
2. **`OTEL_RESOURCE_ATTRIBUTES` env** — the standard OTLP form, comma-separated `key=value` pairs; overrides config per key:
   ```
   OTEL_RESOURCE_ATTRIBUTES=openinference.project.name=my-project,deployment.environment=prod
   ```
3. **`PHOENIX_PROJECT_NAME` env** — convenience that sets `openinference.project.name` only; overrides both of the above.

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
4. `format` must be exactly `"open_ai"`, `"anthropic"`, `"bedrock"`, or `"bedrock_converse"`.
4a. For `format = "bedrock_converse"`: at least one of (a) **both** `aws_access_key_id` and `aws_secret_access_key`, or (b) a non-empty `api_key`, must be set. `aws_access_key_id`/`aws_secret_access_key` must be set **together**. When SigV4 credentials are present, `region` is **required**. `aws_session_token` requires the access-key/secret pair. (The universal "`api_key` non-empty" rule in (3) is relaxed for this format only.)
5. Virtual key `key` values must start with `sk-drgtw-` and be unique.
6. Each name in `[[virtual_keys]].connections` must match a defined connection `name`.
7. `[[virtual_keys]].connections` must be non-empty.
8. Each `[mcp_servers.<name>]` server name must be non-empty and ASCII `[a-zA-Z0-9_-]` only.
9. `[mcp_servers.<name>].url` must be an absolute `http://` or `https://` URL with no query string or fragment.
10. `[mcp_servers.<name>].auth_value` must be non-empty iff `auth_type != "none"`.
11. Every `[mcp_servers.<name>].extra_headers` key must be a valid HTTP header name.

---

## AWS Bedrock

DRGTW reaches Amazon Bedrock through one of **three** connection options. Two use
a **Bedrock API key** (bearer token — generate one in the Bedrock console, the
conventional env var is `AWS_BEARER_TOKEN_BEDROCK`); the third (Converse) also
accepts **AWS SigV4** static credentials. The region is encoded in the hostname.

| Option | `format` | Endpoint surface | Wire API | Auth | Streaming |
|--------|----------|------------------|----------|------|-----------|
| A — OpenAI-compat | `open_ai` | `/v1/chat/completions` | OpenAI Chat Completions | Bearer | yes (SSE) |
| B — native InvokeModel | `bedrock` | `/v1/messages` | `InvokeModel` (Anthropic body) | Bearer | **no** |
| C — Converse | `bedrock_converse` | `/v1/chat/completions` | `Converse` / `ConverseStream` | SigV4 **or** Bearer | **yes** |

Pick by which wire format you want to speak and whether you need SigV4.

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
  (Anthropic error body) and **no upstream call is made**. Use Option A or C for
  streaming.

### Option C — Converse / ConverseStream (`format = "bedrock_converse"`)

A `format = "bedrock_converse"` connection serves the **`/v1/chat/completions`
endpoint surface** (callers use the stock OpenAI body) and dispatches to
Bedrock's normalised **Converse** / **ConverseStream** APIs. This covers
non-Anthropic Bedrock models (Nova, Llama, Titan, …) as well as Anthropic
through one schema, and — unlike Option B — **supports `stream: true`**. The base
URL has **no `/v1` suffix**.

SigV4 (static AWS credentials):

```toml
[[connections]]
name = "bedrock-converse-sigv4"
base_url = "https://bedrock-runtime.eu-central-1.amazonaws.com"
api_key = ""                                    # unused under SigV4
format = "bedrock_converse"
region = "eu-central-1"
aws_access_key_id = "${AWS_ACCESS_KEY_ID}"
aws_secret_access_key = "${AWS_SECRET_ACCESS_KEY}"
aws_session_token = "${AWS_SESSION_TOKEN}"      # optional (STS)
models = ["eu.amazon.nova-pro-v1:0", "eu.meta.llama3-1-70b-instruct-v1:0"]

[connections.model_costs."eu.amazon.nova-pro-v1:0"]
input_per_1m = 0.8
output_per_1m = 3.2
```

Bearer (Bedrock API key) Converse:

```toml
[[connections]]
name = "bedrock-converse-bearer"
base_url = "https://bedrock-runtime.us-east-1.amazonaws.com"
api_key = "${AWS_BEARER_TOKEN_BEDROCK}"
format = "bedrock_converse"
region = "us-east-1"
models = ["us.amazon.titan-text-premier-v1:0"]
```

Behaviour:

- The gateway translates the OpenAI request into a Converse body: `system`
  messages lift to a top-level `system[]` array; `user`/`assistant` messages map
  to `messages[].content[].text`; `max_tokens`/`max_completion_tokens`,
  `temperature`, `top_p`, and `stop` map into `inferenceConfig`. The `model` is
  moved into the URL path (`:` percent-encoded to `%3A`).
- It issues `POST {base_url}/model/{model}/converse` (or `/converse-stream` when
  `stream: true`). Auth is SigV4 when AWS credentials are present, else the
  `api_key` as `Authorization: Bearer` (see
  [SigV4 credential fields](#sigv4-credential-fields-bedrock_converse-only)).
- The non-streaming Converse response is translated back into an OpenAI
  `chat.completion`; `stopReason` maps to `finish_reason`
  (`end_turn`/`stop_sequence` → `stop`, `max_tokens` → `length`,
  `tool_use` → `tool_calls`, `content_filtered`/`guardrail_intervened` →
  `content_filter`). Usage and cost come from `usage.{inputTokens,outputTokens}`.
- For streaming, the binary `application/vnd.amazon.eventstream` response is
  re-framed into OpenAI SSE chunks (the client receives `text/event-stream`
  ending with `data: [DONE]`); usage is captured from the trailing `metadata`
  event — no `stream_options.include_usage` flag is needed.
- **Limitations this release (documented):** tool / function calling
  (`tools`/`tool_choice`) is dropped from the request; non-text content
  (image/audio/document blocks) is rejected with a 400 `unsupported_content`
  error before any upstream call; prompt/response cache token fields are not
  surfaced. PII pseudonymisation/restore runs on the OpenAI shape (before/after
  translation), so privacy guarantees are unchanged.

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
enabled_by_default = true
entities = ["PERSON", "ORG", "LOCATION", "EMAIL", "PHONE", "IBAN", "CREDIT_CARD"]

[pii.ner]
model_dir = "/app/models/ner-multilingual"
fail_mode = "closed"

[[guardrails.rules]]
name = "jailbreak"
kind = "prompt_injection"
action = "block"

[tracing]
enabled = true
retention_days = 90
```
