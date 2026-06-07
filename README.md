# DRGTW

Teams want to put LLM providers to work, but their data won't fit through the
front door: customer names, emails, IBANs, and card numbers can't be shipped off
to a third-party cloud without tripping over compliance and GDPR — so adoption
stalls. DRGTW sits between your application and the provider and quietly closes
that gap. The provider never sees the real values; your application gets back the
real values it expected, and never notices anything happened in between.

**A privacy-first LLM gateway: a single Rust binary that pseudonymizes PII out of your prompts before they reach a provider, then restores the originals in the response — including streaming.**

Point a stock OpenAI or Anthropic SDK at DRGTW instead of the provider. Sensitive
entities in the outbound text (`max.mustermann@example.com`, `Max Mustermann`) are
replaced with stable placeholders (`EMAIL_1`, `PERSON_1`) before the request
leaves the box. The upstream provider only ever sees the placeholders. The
response — full body or token-by-token SSE stream — is restored before it reaches
your caller.

- **Reversible pseudonymization, not redaction.** The provider sees `PERSON_1`; your application sees `Max Mustermann`. The mapping lives in request-scoped memory — never logged, and never persisted unless you explicitly opt into the encrypted vault.
- **Layered detection.** Deterministic recognizers (email, phone, IBAN, credit card — checksum-validated), your own regex rules from config, and optional ONNX NER for names/orgs/locations.
- **Stock SDKs, no code changes.** Works behind the OpenAI Python SDK (`base_url + Bearer`) and the Anthropic SDK (`base_url + x-api-key`).
- **One binary, one TOML file.** No database, no control plane, no UI. Virtual keys, upstream connections, model allowlists, rate limits, and budgets all live in config.
- **Streaming-aware.** Placeholders split across SSE chunk boundaries (`PERSO` + `N_1`) are reassembled and restored correctly.
- **Automatic failover.** When a model is served by multiple connections, retriable upstream failures fall over to the next candidate — transparent to the caller.
- **Cost accounting and usage events.** Per-connection model costs feed a webhook event per request — token counts, USD cost, latency, PII flag, fallback count. Point it at any HTTP receiver to build billing or dashboards.
- **Optional persistent vault.** Turn on `[pii.vault]` and placeholders become stable across requests *and* restarts — backed by SQLite, values encrypted with AES-256-GCM and looked up via a keyed blind index (plaintext never hits the DB). This is what makes embeddings/RAG consistent and lets later responses restore placeholders from earlier requests.
- **Embeddings endpoint.** `/v1/embeddings` pseudonymizes the input before it reaches the provider, works with the stock OpenAI SDK `embeddings.create`, and leaves token-id array inputs untouched.
- **MCP gateway.** A central `/mcp` endpoint aggregating Model Context Protocol servers; tools are namespaced per server (`<server>-<tool>`) and it works with stock MCP clients (Claude Code, Cursor) using your existing virtual keys.
- **Filesystem tracing.** On by default: JSONL traces are written to a `traces/` directory with automatic rotation and `tar.gz` archiving (logrotate-style) and a 90-day retention default. LLM endpoints trace metadata only (never prompt/response bodies); MCP tool calls trace arguments and outputs. Disable with `[tracing] enabled = false`.

> Status: pre-1.0, active development. The PII pipeline, NER, the persistent
> encrypted vault, the `/v1/embeddings` endpoint, virtual keys, rate limiting,
> cross-connection failover, cost tracking, usage events, per-key budgets, and
> the OpenAI/Anthropic proxy paths are implemented and tested
> (300+ tests, including property tests for stream chunk-splitting). There is a
> runnable demo (see below) but no production UI, and no users/teams/SSO — by
> design. Authentication is virtual keys only.

---

<p align="center">
  <img src="docs/images/demo.png" width="640"
       alt="DRGTW demo — the provider only sees PERSON_1 / EMAIL_1 placeholders while you chat with real values" />
</p>

## How it works

```mermaid
sequenceDiagram
    participant Client as Client<br/>(stock OpenAI/Anthropic SDK)
    participant Gateway as DRGTW Gateway
    participant Provider as LLM Provider

    Client->>Gateway: request with real PII (virtual key)
    note over Gateway: detect entities (regex + checksum + NER)<br/>build per-request entity map
    Gateway->>Provider: pseudonymized request (PERSON_1, EMAIL_1...) with upstream key
    Provider-->>Gateway: response / SSE stream containing placeholders
    note over Gateway: restore placeholders (streaming-safe,<br/>split-across-events handled)
    Gateway-->>Client: response with original values
    note over Gateway: entity map lives only in request memory
```

Only the user-visible text fields of the request body are scanned and rewritten
(`messages[].content` and, for Anthropic, the top-level `system` block and
`tool_result` content). Tool schemas, parameters, and every other field pass
through byte-identical. When PII is off, the request body is forwarded
byte-for-byte with no re-serialization — preserving field order, whitespace, and
unknown fields exactly.

On the response path, only `2xx` JSON responses are restored; any other response
is relayed verbatim. Streaming responses are restored chunk-by-chunk: a placeholder
that lands at a chunk boundary is held back until enough bytes arrive to decide,
and placeholder-shaped tokens the gateway never issued (e.g. a model hallucinating
`EMAIL_99`) pass through unchanged.

---

## Try it in 2 minutes

The [`demo/`](demo/) folder is a self-contained walkthrough: docker compose
builds the gateway image locally from the repo `Dockerfile` (never pulled) and
serves a small chat portal next to it. The portal (nginx) reverse-proxies
`/v1/*` to the gateway, so the browser only ever talks to one origin — no CORS.

**Prerequisites:** Docker (with Compose) and an OpenAI API key.

```bash
export OPENAI_API_KEY=sk-...
docker compose -f demo/compose.yml up --build
# open http://localhost:8081
```

Type a message containing a name, email, or phone number (e.g. `I'm Jane Doe,
jane@example.com`). Each exchange renders **four panels** side by side:

1. **What you typed** — your message, verbatim.
2. **What was sent to the provider** — the pseudonymized request body, with real
   values replaced by placeholders (`PERSON_1`, `EMAIL_1`, …).
3. **Raw provider response** — what the model wrote back, still referencing the
   placeholders.
4. **What you received** — the restored response, with the real values put back.

An **entities badge** on each exchange shows how many distinct entities were
detected and mapped.

The portal works because it sends `x-drgtw-debug: on` (documented in
[Per-request control](#per-request-control)), which makes the gateway attach a
`drgtw_debug` object to the response so panels 2 and 3 have something to show.
The entity↔value mapping itself is never exposed — only the count.

---

## Quick start

### 1. Build

```bash
cargo build --release
# binary at ./target/release/drgtw
```

### 2. Minimal config

Create `drgtw.toml`:

```toml
[server]
bind_addr = "127.0.0.1:8080"

[[connections]]
name = "openai-main"
base_url = "https://api.openai.com"
api_key = "${OPENAI_API_KEY}"   # ${VAR} resolved from the environment at startup
format = "open_ai"
models = ["gpt-4o", "gpt-4o-mini"]

[[virtual_keys]]
key = "sk-drgtw-local-dev-key-001"   # keys must start with sk-drgtw-
connections = ["openai-main"]

[pii]
enabled_by_default = true             # scan every request unless the caller opts out
```

See [`drgtw.toml.example`](drgtw.toml.example) for the fully annotated template.

### 3. Validate and run

```bash
export OPENAI_API_KEY=sk-...

# Validate config (and compile custom recognizer regexes) without starting:
./target/release/drgtw --config drgtw.toml --validate-config

# Run:
./target/release/drgtw --config drgtw.toml
# logs to stderr; set RUST_LOG=debug for more detail
```

### 4. Point an SDK at it

Use the **virtual key** as the API key and the gateway as the base URL. The
upstream provider key stays in the gateway's config — your application never sees it.

```python
from openai import OpenAI

client = OpenAI(
    base_url="http://localhost:8080/v1",
    api_key="sk-drgtw-local-dev-key-001",
)

resp = client.chat.completions.create(
    model="gpt-4o-mini",
    messages=[{
        "role": "user",
        "content": "Write an email to Max Mustermann from Example Corp in Munich",
    }],
)
print(resp.choices[0].message.content)
```

Same call with `curl`:

```bash
curl http://localhost:8080/v1/chat/completions \
  -H "Authorization: Bearer sk-drgtw-local-dev-key-001" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "gpt-4o-mini",
    "messages": [
      {"role": "user", "content": "Write an email to Max Mustermann from Example Corp in Munich"}
    ]
  }'
```

**What the provider actually receives** (with NER enabled), the user content is
rewritten to:

```
Write an email to PERSON_1 from ORG_1 in LOCATION_1
```

The provider never sees `Max Mustermann`, `Example Corp`, or `München`. Whatever the
model writes back referencing `PERSON_1`/`ORG_1`/`LOCATION_1` is restored to the
real values before your code receives the response.

---

## Configuration reference

The whole gateway is configured from one TOML file. Sections below summarize the
schema; the annotated [`drgtw.toml.example`](drgtw.toml.example) is the canonical
reference.

### `[server]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `bind_addr` | socket address | `127.0.0.1:8080` | Address the gateway listens on. |

### `[[connections]]` — upstream providers

One table per upstream. Referenced by virtual keys.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `name` | string | yes | Unique identifier referenced by `[[virtual_keys]].connections`. |
| `base_url` | URL | yes | Absolute `http(s)` base URL, no query string or fragment. Supports `${ENV_VAR}`. |
| `api_key` | string | yes | Upstream provider key. Supports `${ENV_VAR}` (unresolved → hard startup error). |
| `format` | `"open_ai"` \| `"anthropic"` | yes | Wire protocol the upstream speaks. |
| `models` | string array | no | Model names served by this connection. Used for routing and allowlist checks. |

Tested upstreams: OpenAI, Azure OpenAI (via its `v1` endpoint — set
`base_url = "https://<resource>.openai.azure.com/openai/v1"`), and
Anthropic-format mocks.

### `[[virtual_keys]]` — credentials you issue to callers

Keys must start with `sk-drgtw-`. Callers present them as `Authorization: Bearer`
(OpenAI SDK) or `x-api-key` (Anthropic SDK).

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `key` | string | yes | Must start with `sk-drgtw-`; unique. |
| `connections` | string array | yes | Connections this key may route to. Non-empty; each must resolve. |
| `models` | string array | no | Optional allowlist. Omit to allow all models of the listed connections. Entries may end with `*` for prefix match (e.g. `gpt-*`). |
| `rate_limit` | table | no | Optional per-key token-bucket limit (see below). |
| `budget` | table | no | Optional per-key USD spend budget (see below). |

**Routing.** A request's `model` is resolved against the key's allowed
connections: an exact model match beats a wildcard, and among wildcards the
longest matching prefix wins. Wildcards permit a single trailing `*` only.

**Rate limit** (`[virtual_keys.rate_limit]`):

| Field | Type | Description |
|-------|------|-------------|
| `requests` | integer > 0 | Maximum requests in the window. |
| `per_seconds` | integer > 0 | Window length in seconds. |

When the bucket is empty the gateway returns `429` with a `retry-after` header
and `x-ratelimit-limit` / `x-ratelimit-remaining` headers, in the wire format of
the endpoint that was called.

### `[pii]` — pseudonymization

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled_by_default` | bool | `true` | Whether PII scanning runs when the caller sends no `x-drgtw-pii` header. Privacy-first: on unless the caller opts out. |
| `disabled_recognizers` | string array | `[]` | Built-in recognizers to turn off, by name: `email`, `phone`, `iban`, `credit_card`. |
| `custom_recognizers` | array of tables | `[]` | User-defined regex recognizers (see [Extending PII detection](#extending-pii-detection)). |
| `ner` | table | absent | Optional ONNX NER configuration (see below). When absent, NER is not loaded. |

Built-in recognizers and their placeholders:

| Recognizer | Validation | Placeholder |
|------------|------------|-------------|
| `email`       | regex                        | `EMAIL_n` |
| `phone`       | regex                        | `PHONE_n` |
| `iban`        | ISO 13616 mod-97 checksum    | `IBAN_n` |
| `credit_card` | Luhn checksum (13–19 digits) | `CARD_n` |

Placeholders are numbered per entity kind and **stable within a request**: the
same value mapped twice gets the same placeholder.

### `[pii.ner]` — ONNX named-entity recognition

Adds `PERSON` / `ORG` / `LOCATION` detection on top of the deterministic layer.
Only `model_dir` is required.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `model_dir` | string | — (required) | Directory holding the model artifacts. Supports `${ENV_VAR}`. |
| `score_threshold` | float `0.0..=1.0` | `0.5` | Minimum mean per-span score to emit a detection. |
| `fail_mode` | `"open"` \| `"closed"` | `"open"` | Behavior when inference errors/times out (see below). |
| `timeout_ms` | integer > 0 | `5000` | Per-request inference timeout. |
| `workers` | integer > 0 | `2` | NER worker threads. |
| `queue_capacity` | integer > 0 | `64` | Max pending requests in the NER queue. |

NER runs on a dedicated worker pool off the request path (via `spawn_blocking`),
so it never blocks plain passthrough traffic.

- **`fail_mode = "open"`** (default): on inference error, log a warning and emit
  no NER detections — the request still goes through (deterministic recognizers
  still applied).
- **`fail_mode = "closed"`**: on inference error, fail the request rather than
  risk forwarding unmasked text. Choose this when leaking PII is worse than
  dropping a request.

### `[pii.vault]` — persistent encrypted entity vault

**Default: off** (the section is absent). Without it, the entity↔placeholder map
lives in request-scoped memory and the same value gets the same placeholder only
*within a single request*. Turn the vault on and placeholders become stable
**across requests and across restarts**.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `path` | string | yes | Path to the SQLite database file. Must be non-empty. Supports `${ENV_VAR}`; a relative path resolves against the config-file directory. |
| `key`  | string | yes | Master key: exactly **64 hex characters** (32 bytes) after `${ENV_VAR}` expansion. Generate with `openssl rand -hex 32`. |

```toml
[pii.vault]
path = "vault.db"
key  = "${DRGTW_VAULT_KEY}"   # 64 hex chars; openssl rand -hex 32
```

**How it's stored.** Original values are encrypted at rest with AES-256-GCM (a
fresh random nonce per value). Lookups go through a keyed blind index
(`HMAC-SHA256`) — the plaintext value is **never** written to the database. The
32-byte master key is split into independent encryption and index subkeys via
domain-separated HMAC; the master key is never used directly as a cipher key. On
open, a key-check row is verified, so pointing the vault at an existing database
with the **wrong key fails loudly** instead of silently corrupting data.

**Placeholder format differs from storeless mode.** Without a vault, placeholders
are readable per-request counters (`EMAIL_1`, `PERSON_2`). With a vault they are
hash-derived — `EMAIL_<hex>` where the suffix is a slice of the keyed blind index
for that value. This is deliberate at vault scale: a global running counter across
millions of persisted entities would leak how many distinct values exist and the
order they were first seen, and invites `PERSON_1`-vs-`PERSON_12` prefix ambiguity.
The hash suffix is deterministic per value, order-free, and contention-free; a
truncation collision with a *different* value automatically lengthens the suffix.

**What it enables.**

- **Embeddings / RAG consistency.** The same entity gets the same placeholder
  every time it appears, across separate requests — so embeddings of
  pseudonymized text remain comparable.
- **Cross-request restore.** A later *non-streaming* response can restore
  placeholders that were assigned in earlier requests, not just the current one.

**Caveats — read these.**

- The vault is the crown jewels. Protect both the **database file** and the
  **key**. Anyone with both can recover every mapped value.
- **Lose the key and the mappings are unrecoverable** — there is no backdoor;
  the key-check row simply refuses to open.
- **Streaming responses still restore within-request only.** Past-request
  placeholders that appear in an SSE stream pass through untouched. Cross-request
  restore from the vault applies to non-streaming responses.

### `/v1/embeddings`

OpenAI-format embeddings, non-streaming. Works with the stock OpenAI SDK
(`client.embeddings.create`) — point its `base_url` at the gateway exactly as
for chat.

- The `input` field (a string or an array of strings) is pseudonymized before it
  reaches the provider, on the same PII pipeline as chat.
- **Token-id array inputs are passed through untouched** — there's no text to
  scan.
- **There is no response restore** — embedding responses are opaque float
  vectors with nothing to put back.
- Usage accounts **input tokens only** (`usage.prompt_tokens`), priced with the
  model's input price.
- Requires an `open_ai`-format connection; a model served only by an
  `anthropic`-format connection returns a format-mismatch error.

With `[pii.vault]` enabled, the placeholders assigned here are stable and shared
with chat requests — the basis of the embeddings/RAG consistency guarantee.

Without a vault, embeddings placeholders are per-request counters that differ
across requests, so the gateway logs a boot-time warning that cross-request
vector consistency is not guaranteed. To require the vault instead, set
`pii.embeddings_require_vault = true`: with no `[pii.vault]` configured the
gateway refuses to start (fail-closed) rather than serving inconsistent
placeholders.

### `[tracing]` — filesystem request tracing

**On by default.** JSONL traces are written to a `traces/` directory (relative to
the config file), the active file rotates at 50 MiB, rotated files are bundled
into `tar.gz` archives logrotate-style, and archives older than the 90-day
retention default are deleted. LLM endpoints trace metadata only — never prompt
or response bodies; MCP tool calls trace arguments and outputs. Turn it off:

```toml
[tracing]
enabled = false
```

See the [configuration reference](docs/config-reference.md#tracing) for every field.

---

## Extending PII detection

Three layers, each independently configurable.

```mermaid
flowchart LR
    A[request body] --> B[deterministic recognizers]
    B --> C[custom regex]
    C --> D[NER]
    D --> E[overlap resolution]
    E --> F[entity map]
    F --> G[upstream]
```

### a) Custom regex recognizers

Add organization-specific patterns under `[[pii.custom_recognizers]]`. The
placeholder prefix is derived from the `name` (uppercased): `name = "ticket"` →
`TICKET_1`, `TICKET_2`, …

```toml
[[pii.custom_recognizers]]
name = "ticket"                       # → TICKET_n
pattern = "JIRA-\\d{3,6}"

[[pii.custom_recognizers]]
name = "employee_id"                  # → EMPLOYEE_ID_n
pattern = "EMP-[0-9]{6}"

[[pii.custom_recognizers]]
name = "project_code"                 # → PROJECT_CODE_n
pattern = "PRJ-[A-Z]{2,4}-\\d{2}"

[[pii.custom_recognizers]]
name = "customer_number"              # → CUSTOMER_NUMBER_n
pattern = "CUST\\d{7}"
```

Patterns use Rust [`regex`](https://docs.rs/regex) syntax. Names must be unique
and ASCII alphanumeric/underscore. Patterns are compiled at startup: an **invalid
regex fails the boot** (caught by `--validate-config`), it is not silently
ignored at runtime.

### b) Disabling built-in recognizers

Turn off any built-in by name. Useful when a checksum-validated recognizer
collides with your domain data, or when you want full control via custom rules.

```toml
[pii]
enabled_by_default = true
disabled_recognizers = ["phone", "credit_card"]   # keep email + iban only
```

Valid names: `email`, `phone`, `iban`, `credit_card`. An empty list (the
default) keeps all built-ins active.

### c) Custom NER models

DRGTW loads **any Hugging Face token-classification model exported to ONNX** with
a BIO label scheme. Point `[pii.ner].model_dir` at a directory containing:

```
model.onnx          # (or model_quantized.onnx) token-classification graph
tokenizer.json      # HF fast-tokenizer file
config.json         # HF config with id2label, e.g.
                    # {0:O, 1:B-PER, 2:I-PER, 3:B-ORG, 4:I-ORG, 5:B-LOC, 6:I-LOC}
```

The gateway maps `PER → Person`, `ORG → Org`, `LOC → Location`; any other label
(e.g. `DATE`, `MISC`) is ignored. It reads the model's declared input names
dynamically and feeds whichever of `input_ids` / `attention_mask` /
`token_type_ids` the graph wants, so both DistilBERT (2 inputs) and BERT-family
(3 inputs) exports work. The multilingual baseline was tested with a quantized
`bert-base-multilingual-cased-ner-hrl`; German and English are verified.

```toml
[pii.ner]
model_dir = "models/ner-multilingual"
score_threshold = 0.7
fail_mode = "closed"
```

#### Training / fine-tuning your own model

The [`training/`](training/) directory is a self-contained
[`uv`](https://docs.astral.sh/uv/) project (`drgtw_training`) that produces
artifacts in exactly the layout above. See [`docs/training-guide.md`](docs/training-guide.md)
for the full workflow.

```bash
cd training
uv sync                       # core: train + export
uv sync --extra annotate      # adds Presidio + spaCy (only for the annotate step)

# 1. Synthetic data — deterministic Faker generation (de_DE + en_US),
#    mixing names / companies / locations into natural sentences:
uv run python -m drgtw_training synth -n 500 --seed 0 --out data/synth.jsonl

# 2. (optional) Pre-annotate your own raw text with Presidio as a labelling aid
#    — review and correct before training:
uv run python -m drgtw_training annotate \
    --input raw.txt --out data/draft.jsonl --spacy-model en_core_web_lg

# 3. Fine-tune a token-classification model (default base:
#    distilbert-base-multilingual-cased). Writes the HF model + tokenizer plus
#    eval_report.json (seqeval per-entity precision/recall/F1):
uv run python -m drgtw_training train \
    --data data/synth.jsonl --out out/model \
    --base-model distilbert-base-multilingual-cased \
    --epochs 3 --lr 5e-5 --batch-size 16 --eval-fraction 0.2 --seed 0

# 4. Export to the gateway artifact layout (model.onnx + tokenizer.json +
#    config.json with BIO id2label):
uv run python -m drgtw_training export \
    --model-dir out/model --out out/gateway/ner-custom
```

Point `[pii.ner].model_dir` at `out/gateway/ner-custom` and restart. To get a
BERT-family export that includes `token_type_ids` (matching the stock
`models/ner-multilingual` artifact), pass a BERT base such as
`bert-base-multilingual-cased` to `train`.

---

## Per-request control

Override the configured default per request with the `x-drgtw-pii` header.

| Header | Value | Effect |
|--------|-------|--------|
| `x-drgtw-pii` | `on`  | Force pseudonymization on for this request. |
| `x-drgtw-pii` | `off` | Force it off — body is forwarded byte-for-byte. |
| `x-drgtw-pii` | (absent) | Use `[pii].enabled_by_default`. |

The value is case-insensitive; any other value returns `400`.

### Debug header (`x-drgtw-debug`)

| Header | Value | Effect |
|--------|-------|--------|
| `x-drgtw-debug` | `on` | Attach a `drgtw_debug` object to the JSON response. |
| `x-drgtw-debug` | (absent / anything else) | No debug object. |

Honoured **only** on non-streaming requests with PII on for that request;
otherwise it is a no-op (streaming responses and PII-off requests never gain the
object). When active, the `2xx` JSON response gains a top-level `drgtw_debug`:

| Field | Contents |
|-------|----------|
| `pseudonymized_request` | The exact request JSON sent upstream, after pseudonymization. |
| `raw_response_text` | The assistant text *before* restore — one entry per OpenAI choice / Anthropic content block. |
| `entities` | The number of distinct entities in the request map. |

The entity↔value mapping itself is **never** included — only the count. This is
the header the [demo portal](#try-it-in-2-minutes) uses to render its four
panels.

Set it as a default header on the SDK client so every call carries it:

```python
from openai import OpenAI

client = OpenAI(
    base_url="http://localhost:8080/v1",
    api_key="sk-drgtw-local-dev-key-001",
    default_headers={"x-drgtw-pii": "on"},
)
```

```python
import anthropic

client = anthropic.Anthropic(
    base_url="http://localhost:8080",
    api_key="sk-drgtw-local-dev-key-001",   # sent as x-api-key by the SDK
    default_headers={"x-drgtw-pii": "on"},
)
```

---

## Resilience & cost accounting

### Cross-connection fallback

When the same model is reachable through multiple connections, the gateway
automatically tries the next candidate on retriable upstream failures. Retriable
conditions: connect/transport error, HTTP 502/503/504, HTTP 429. Candidates are
tried in the order they appear in the virtual key's `connections` list; exact
model-name matches are preferred before wildcard-matched connections. For
streaming responses, failover happens before the first response byte is consumed
— a partially streamed response is never abandoned mid-stream. The
PII-pseudonymized request body is reused byte-for-byte across all attempts
(required so placeholders are restorable regardless of which connection answers).

Fallback is on by default. Disable it globally:

```toml
[fallback]
enabled = false
```

The `x-drgtw-fallback-attempts` response header reports how many attempts were
made before the final response.

### Model costs

Attach a cost table to any connection so the gateway can compute per-request USD
spend. Keys may be exact model names or wildcard patterns ending in `*`; when
both match, the exact key wins, and among wildcards the longest prefix wins.

```toml
[[connections]]
name = "openai-main"
base_url = "https://api.openai.com"
api_key = "${OPENAI_API_KEY}"
format = "open_ai"
models = ["gpt-4o", "gpt-4o-mini"]

[connections.model_costs."gpt-4o"]
input_per_1m  = 2.50    # USD per 1M input tokens
output_per_1m = 10.00

[connections.model_costs."gpt-4o-mini"]
input_per_1m  = 0.15
output_per_1m = 0.60

# wildcard covers any future gpt-* variant not listed above
[connections.model_costs."gpt-*"]
input_per_1m  = 1.00
output_per_1m = 4.00
```

Costs are `null` in events when no matching entry exists or the upstream did not
report token counts.

### Usage events (webhook)

Configure a webhook URL and the gateway POSTs one JSON event per completed
proxied request — successes and upstream errors alike; never on auth failure.

```toml
[events]
url         = "https://ingest.example.com/llm-usage"
auth_bearer = "${EVENTS_TOKEN}"   # optional; ${VAR} expanded from env
buffer_size = 1024                # in-memory queue depth (default 1024)
timeout_ms  = 5000                # per-POST timeout ms (default 5000)
```

The sink is fire-and-forget: a bounded in-memory channel decouples the hot
request path from the webhook POST. If the receiver is slow or unreachable,
events are dropped (counted, logged at WARN) — the gateway never blocks waiting
for the webhook. Works for streaming too (token counts come from the final SSE
`usage` chunk; for OpenAI streams the caller must pass
`stream_options: {"include_usage": true}` to get counts).

**Privacy invariant:** events contain no request or response content and no API
keys — metadata only.

Example event (JSON body POSTed to your receiver):

```json
{
  "request_id":       "01J3KXYZ...",
  "key_id":           "vk-2",
  "endpoint":         "chat_completions",
  "model":            "gpt-4o",
  "connection":       "backup-live",
  "status":           200,
  "input_tokens":     1200,
  "output_tokens":    340,
  "cost_usd":         0.048,
  "latency_ms":       831,
  "pii":              true,
  "streamed":         false,
  "fallback_attempts": 1,
  "ts_unix_ms":       1717500000000
}
```

To count costs externally, point `events.url` at any HTTP receiver — a small
webhook handler, a ClickHouse ingest endpoint, a Vector pipeline — and
aggregate `cost_usd` per `key_id` or `connection`.

### Per-key budgets

Limit a virtual key's total spend within a rolling window:

```toml
[[virtual_keys]]
key         = "sk-drgtw-team-budget-key-003"
connections = ["openai-main"]

[virtual_keys.budget]
max_usd    = 10.00    # reject requests once this is exceeded
per_seconds = 86400   # window length (86400 = 24 h)
```

Spend is computed from the connection's `model_costs`. When the budget is
exhausted the gateway returns `429` with a `retry-after` header indicating
seconds until the window resets.

**Budget counters are in-memory only.** Restarting the gateway resets all
windows. For production enforcement use the usage events to track spend
externally and rotate or revoke keys accordingly.

---

## MCP gateway

A central `/mcp` endpoint aggregates upstream
[Model Context Protocol](https://modelcontextprotocol.io) servers behind one
streamable-HTTP JSON-RPC endpoint. Configured upstreams are merged into a single
tool catalog, each tool namespaced as `<server>-<tool>`, and clients
authenticate with the same virtual keys used everywhere else.

Declare upstreams in the same TOML file:

```toml
[mcp_servers.context7]
url = "https://mcp.context7.com/mcp"   # absolute http(s), no query/fragment
description = "library documentation"   # optional
auth_type = "none"                      # none (default) | api_key | bearer
# auth_value = "${CONTEXT7_API_KEY}"    # required iff auth_type != none
```

Point any stock MCP client at it (Claude Code / Cursor style):

```json
{
  "mcpServers": {
    "drgtw": {
      "url": "http://localhost:8080/mcp",
      "headers": {"Authorization": "Bearer sk-drgtw-local-dev-key-001"}
    }
  }
}
```

List the merged, namespaced tool catalog with `curl`:

```bash
curl http://localhost:8080/mcp \
  -H "Authorization: Bearer sk-drgtw-local-dev-key-001" \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc": "2.0", "id": 1, "method": "tools/list"}'
```

v1 is streamable HTTP only (no stdio, no server-push SSE), and MCP tool
arguments are **not** PII-pseudonymized yet. See
[`docs/mcp.md`](docs/mcp.md) for the full guide.

---

## Privacy guarantees & limits

DRGTW is deliberately explicit about what it does and does not protect.

**Guaranteed**

- By default the entity↔placeholder map is held in **request-scoped memory only**: never written to disk, never logged, dropped when the request completes. When the optional [`[pii.vault]`](#piivault--persistent-encrypted-entity-vault) is enabled, mappings are persisted to SQLite — but encrypted with AES-256-GCM and indexed via a keyed blind index, so the plaintext value is still never written to disk in either case. The map is never logged.
- Outbound request text contains **only placeholders** for everything the detection layers matched — the upstream provider never receives the original values.
- When PII is off, the request body is forwarded byte-identical (no re-serialization), and the gateway client runs without response compression so the byte stream is handled as received.
- Streaming restoration is correct across chunk boundaries (verified by property tests over arbitrary chunk splits), and placeholders the gateway did not issue are passed through untouched.

**Current limits** (pre-1.0)

- **Response-side detection.** Only known placeholders are restored on the way back; *new* PII the model emits in its response is not scanned or masked.
- **Scope of scanning.** Only the documented user-visible text fields are scanned. Tool-call arguments and tool/function schemas are **not** scanned yet.
- **Cross-request placeholder stability requires the vault.** By default mappings are per-request only — the same value gets the same placeholder within one request but not across requests. Enable [`[pii.vault]`](#piivault--persistent-encrypted-entity-vault) for stability across requests and restarts; even then, *streaming* responses restore within-request only.
- Detection is only as good as the configured recognizers and NER model; a custom domain may need custom regex rules and/or a fine-tuned model.

---

## Performance

A loopback benchmark harness ([`bins/drgtw-bench`](bins/drgtw-bench)) measures
**added** gateway latency: it runs the gateway and a baseline path against the
same mock upstream over loopback ports and reports the difference of the latency
distributions (p50/p90/p99/p999/max, achieved RPS, error count). The response
body is padded to ~800 bytes to make body-handling overhead representative.

On a single local machine, the harness shows **sub-0.1 ms p99 added latency at
~2k RPS** over loopback. This is a machine-local, indicative figure — not a
multi-tenant production SLA. Run the harness on your own hardware for numbers
that mean something to you.

### Memory at concurrency

The harness's `memory` mode spawns the real release binary, holds N requests
in-flight simultaneously (mock upstream with a 2 s artificial delay), and
samples the gateway's RSS from `/proc` every 100 ms:

```sh
ulimit -n 65536
./target/release/drgtw-bench memory --concurrency-steps "10,100,500,1000,5000" --pii on
```

Measured (single Linux machine, loopback, release build):

| Concurrent in-flight requests | RSS, PII off | RSS, PII on (regex/checksum) |
|---:|---:|---:|
| idle | 16 MB | 16 MB |
| 10 | 17 MB | 19 MB |
| 100 | 21 MB | 25 MB |
| 500 | 41 MB | 46 MB |
| 1,000 | 65 MB | 72 MB |
| 5,000 | 259 MB | 268 MB |

Zero errors at every step. With the optional NER model loaded
(`[pii.ner]`, quantized multilingual BERT), add a flat ~257 MB for the
resident model + ONNX runtime (~273 MB idle).

### How that compares

Published figures from other gateways — **not measured by us, not under the
same conditions or hardware**; each links to its source. The "vs DRGTW"
columns compare each vendor's published number against our closest measured
data point, so treat them as order-of-magnitude ratios, not lab results:

| Gateway | Published memory | vs DRGTW (memory) | Published latency overhead | vs DRGTW (latency) | Source type |
|---|---|---|---|---|---|
| **DRGTW** | **65 MB @ 1,000 in-flight · 41 MB @ 500 · 259 MB @ 5,000** | — | **+0.05 ms (50 µs) p99 added @ 5k RPS** | — | measured here |
| [Helicone ai-gateway](https://github.com/Helicone/ai-gateway) (Rust) | ~64 MB nominal, <100 MB sustained @ ~3k RPS | **≈ parity** (same class) | P95 < 5 ms full proxy | not comparable (full vs added) | vendor self-reported |
| [Bifrost](https://www.getmaxim.ai/bifrost/resources/benchmarks) (Go) | 120 MB peak @ 500 RPS | **DRGTW ~3× lighter** (41 MB @ 500 in-flight) | 11–59 µs added @ 5k RPS | **≈ parity** (same µs class) | vendor self-reported |
| [LiteLLM proxy](https://docs.litellm.ai/docs/proxy/prod) (Python) | ~1.9–2 GB per pod @ 1k RPS; docs recommend **4 GiB per worker**; [users reported](https://github.com/BerriAI/litellm/issues/12685) 14–20 GB growth before leak fixes | **DRGTW ~30× lighter** (65 MB vs ~2 GB @ ~1k concurrent); **~60× vs recommended provisioning** | 8 ms P95 @ 1k RPS (4 replicas) | **DRGTW ~100× lower added latency** (50 µs vs 8 ms — caveat: theirs is full-path P95) | vendor + GitHub issue reports |
| [TensorZero](https://www.tensorzero.com/docs/gateway/benchmarks) (Rust) | none published | — | <1 ms p99 added @ 10k QPS | same class (sub-ms) — they prove 2× our tested QPS | vendor self-reported |
| Portkey (Node) | none found | — | ~5 ms P50 (third-party) | — | — |

Honest reading of those ratios:

- **vs LiteLLM (the incumbent):** ~30× less memory at comparable concurrency,
  and you provision a 16–65 MB process instead of 4 GiB per worker. Latency is
  microseconds vs milliseconds — two orders of magnitude — though their figure
  is full-path P95 and ours is added overhead, so call it "µs-class vs
  ms-class" rather than an exact multiplier.
- **vs Bifrost / Helicone / TensorZero (the compiled-language class):** memory
  and latency are in the same band — parity to ~3× depending on the data
  point. DRGTW is not claiming to beat them on raw proxy speed; the
  differentiator is that none of them does reversible PII pseudonymization —
  the only restore-capable alternative we found anywhere is a Kong Enterprise
  plugin that requires a separate PII sidecar service from a private registry.
- These are **not controlled comparisons**: different hardware, payloads, and
  load shapes. Run `drgtw-bench` on your own hardware next to any competitor
  before believing any table — including this one.

---

## Development

Cargo workspace:

```
crates/
  drgtw-config   # TOML schema, ${ENV_VAR} resolution, validation
  drgtw-keys     # virtual-key extraction, allowlist, rate-limit + budget checks
  drgtw-pii      # detection, entity map, full-body + streaming restore
  drgtw-vault    # persistent encrypted entity vault (SQLite, AES-256-GCM, blind index)
  drgtw-ner      # ONNX model loading, inference, worker pool
  drgtw-events   # UsageEvent wire type, EventSink, cost calculation, token extraction
  drgtw-proxy    # axum handlers, routing, fallback dispatch, upstream client, error rendering
bins/
  drgtw          # the gateway binary
  drgtw-bench    # loopback latency benchmark
training/        # Python/uv NER training + ONNX export pipeline
```

```bash
# Rust tests (300+, including proptest stream chunk-splitting properties):
cargo test

# Training module tests:
cd training && uv sync --extra dev && uv run pytest
```

---

## Roadmap

See [docs/roadmap.md](docs/roadmap.md) for the full release-by-release plan.

Next up (v0.0.4+):

- Native InvokeModel streaming for the Anthropic-surface Bedrock path.
- In-memory budgets keyed by attribution metadata (per-agent / per-session).
- Usage-event batching and at-least-once delivery for the webhook sink.
- Response-side sanitization of newly introduced PII.
- Cross-request placeholder restore for *streaming* responses (the persistent
  vault already covers non-streaming).

## License

DRGTW uses a dual-license model.

### Community Edition

Everything in this repository is the Community Edition, licensed under the
[Apache License 2.0](LICENSE). Included features:

* OpenAI-compatible API (`/v1/chat/completions`, `/v1/embeddings`, `/v1/models`)
* Anthropic-native API (`/v1/messages`)
* PII detection and pseudonymization (NER + custom recognizers)
* Encrypted entity vault with cross-request restore
* Virtual keys, rate limiting, and budgets
* Provider fallbacks
* Usage events and cost tracking
* MCP gateway (`/mcp`)
* Request tracing with rotation and retention
* Configuration management

### Enterprise Edition

Enterprise Edition components are proprietary software and require a
commercial license — see [LICENSE-ENTERPRISE.md](LICENSE-ENTERPRISE.md).
They are not covered by Apache 2.0.

Enterprise features may include:

* Azure AD / Entra ID SSO
* SAML / OIDC SSO
* Role-Based Access Control (RBAC)
* Multi-tenancy
* Audit logging
* Governance policies
* Compliance reporting
* Enterprise guardrails
* Approval workflows
* Advanced rate limiting
* Departmental cost allocation
* Enterprise administration features

### Copyright

Copyright (c) 2026 Digitlab e.K. See [NOTICE](NOTICE).

### Repository structure

```
drgtw/
├── bins/                  Apache 2.0 — gateway server + bench harness
├── crates/                Apache 2.0 — community core (proxy, PII, vault, MCP, …)
├── docs/                  Apache 2.0 — documentation
├── demo/                  Apache 2.0 — demo stack
├── enterprise/            Commercial License (distributed separately)
├── LICENSE
├── LICENSE-ENTERPRISE.md
├── NOTICE
└── README.md
```
