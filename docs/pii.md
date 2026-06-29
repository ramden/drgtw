# PII Pseudonymization — Deployment Guide

DRGTW masks personal data **before** it leaves your network. Detected PII is
replaced with stable pseudonyms in the outbound prompt; the original values are
restored in the response on the way back. This guide is operator-focused: what
masks out of the box, what needs a model, how to ship the model, and a complete
GDPR configuration.

For the field-by-field reference see
[config-reference.md → `[pii]`](config-reference.md#pii).

---

## What gets masked

There are two detection layers.

### Regex recognizers — work out of the box, no model

These run with zero extra setup the moment `[pii]` is enabled:

| Entity | Alias | Notes |
|--------|-------|-------|
| `EMAIL_ADDRESS` | `EMAIL` | |
| `PHONE_NUMBER` | `PHONE` | |
| `IBAN_CODE` | `IBAN` | |
| `CREDIT_CARD` | `CC`, `CARD` | |
| `IP_ADDRESS` | `IP` | IPv4 and IPv6 |
| `DATE_TIME` | `DATE`, `DATETIME` | German `DD.MM.YYYY`, ISO `YYYY-MM-DD` |

`DATE_TIME` is **high recall, best-effort**: it validates only `month ≤ 12` and
`day ≤ 31`, so it will over-match things that look like dates (version strings,
ratios). If those false positives are unacceptable, drop `DATE_TIME` from your
`entities` allow-list or add it to `disabled_recognizers`.

### NER recognizer — needs a model

Names of people, organizations, and places are **not** pattern-matchable. They
require the multilingual NER model configured under `[pii.ner]`:

| Entity | Alias |
|--------|-------|
| `PERSON` | — |
| `ORGANIZATION` | `ORG` |
| `LOCATION` | `LOC` |

**Without a `[pii.ner]` block, `PERSON` / `ORGANIZATION` / `LOCATION` are never
masked**, even if you list them in `entities`. The gateway logs a boot warning in
this case (see [Boot warning](#boot-warning)).

### No built-in detector

`NATIONAL_ID` and `NRP` have **no built-in detector** in this release. They are
reachable only via a `[[pii.custom_recognizers]]` pattern or a future NER label.
Listing them in `entities` alone masks nothing.

---

## The entities allow-list

`entities` restricts which **kinds** are kept. It is Presidio-style,
case-insensitive, and accepts the aliases above.

```toml
[pii]
entities = ["PERSON", "ORG", "EMAIL", "IBAN"]
```

- **Absent** → keep every kind (default).
- **Empty list** → **rejected at validation**. An empty allow-list would silently
  mask nothing; express "off" as `enabled_by_default = false` instead.
- Filtering is **post-scan**: recognizers still run, then detections whose kind is
  not in the list are dropped. So narrowing `entities` does not save inference
  cost — it only controls what gets pseudonymized.

---

## Shipping the NER model

The model (`model_quantized.onnx`, `tokenizer.json`, `config.json`) is **not** in
the repo and **not** in the base/slim image — it is gitignored and dockerignored.
There are two supported ways to get it onto a box.

### Option A — mount a volume (slim image)

Place the model on the host and bind-mount it:

```bash
docker run \
  -v /host/ner-multilingual:/app/models/ner-multilingual \
  -v "$PWD/drgtw.toml:/app/drgtw.toml" \
  <user>/drgtw:<version>
```

```toml
[pii.ner]
model_dir = "/app/models/ner-multilingual"
```

The Dockerfiles declare `VOLUME ["/app/models", "/app/data"]`, so the mount point
already exists.

### Option B — model-bundled image (`-models` tag, v0.0.8+)

The `<user>/drgtw:<version>-models` image **bakes the model in** at
`/app/models/ner-multilingual`. No volume needed:

```bash
docker run -v "$PWD/drgtw.toml:/app/drgtw.toml" <user>/drgtw:<version>-models
```

```toml
[pii.ner]
model_dir = "/app/models/ner-multilingual"
```

Use the slim image + volume when you manage model artifacts yourself; use the
`-models` tag for zero-config deploys.

---

## A complete GDPR configuration

Fail-closed, names masked, runtime errors never leak, pseudonyms reversible, plus
a jailbreak guardrail.

```toml
[pii]
enabled_by_default = true          # mask every request unless an authorized caller sends x-drgtw-pii: off
require_ner        = true          # refuse to boot if NER is missing/misconfigured
entities = ["PERSON", "ORG", "LOCATION", "EMAIL", "PHONE", "IBAN", "CREDIT_CARD"]

[pii.ner]
model_dir       = "/app/models/ner-multilingual"
score_threshold = 0.5
fail_mode       = "closed"         # a runtime NER error fails the request — never forwards unmasked
timeout_ms      = 5000
workers         = 2
scan_roles      = ["user", "assistant"]  # skip NER on the static, PII-free system prompt
cache_capacity  = 1024             # reuse NER verdicts for repeated text (no plaintext stored)

[pii.vault]
path = "/app/data/pii-vault.db"    # mount /app/data so the vault survives restarts
key  = "${DRGTW_PII_VAULT_KEY}"    # 64 hex chars, supplied via env

# Block obvious prompt-injection attempts before the prompt is even pseudonymized.
[[guardrails.rules]]
name   = "jailbreak"
kind   = "prompt_injection"
phase  = "pre"
action = "block"                   # returns 403, no upstream call
```

Run it strictly so a misplaced key can't silently disable masking:

```bash
drgtw --config /app/drgtw.toml --strict-config
```

Why `require_ner = true` **and** `--strict-config`? They guard different
failures. `require_ner` catches *"I forgot the `[pii.ner]` block."*
`--strict-config` catches *"I wrote `[ner]` instead of `[pii.ner]`"* — a block that
**looks** present but never binds. Together they make "names silently unmasked"
impossible to ship.

---

## Per-request PII bypass (`x-drgtw-pii`)

Callers can override the PII mode per request with the `x-drgtw-pii` header:

| Header | Effect |
|---|---|
| `x-drgtw-pii: on`  | Force PII scanning on for this request (always allowed). |
| `x-drgtw-pii: off` | Skip PII scanning for this request — **only for authorized keys**. |
| *(absent)*         | Use `pii.enabled_by_default`. |

`off` is **gated per virtual key** and **fail-closed**: a key may disable scanning
only when it is explicitly authorized. An `off` header from an unauthorized key is
**ignored** — the request falls back to the config default (i.e. PII still scans
when `enabled_by_default = true`). This prevents any holder of a valid key from
silently turning masking off.

Authorize a key with `allow_pii_bypass` (defaults to `false`):

```toml
[[virtual_keys]]
key              = "sk-analyzer"
connections      = ["azure"]
allow_pii_bypass = true   # only this key may honor `x-drgtw-pii: off`
```

This lets one key mix scanned and unscanned traffic, decided per call. The common
case: a code/embedding-analysis job sends `x-drgtw-pii: off` on its
`/v1/embeddings` calls — masking would mutate the input text and skew the vectors —
while the same deployment's chat traffic stays scanned. An unauthorized key sending
the same header is silently scanned anyway.

---

## NER performance: scoping and caching

NER inference dominates the cost of a PII scan. Two `[pii.ner]` settings cut that
cost for the common case where the same large text is scanned repeatedly — e.g. a
static, developer-authored system prompt that every call in a multi-agent stack
re-sends:

- **`scan_roles`** — restrict the NER model to specific message roles. With
  `scan_roles = ["user", "assistant"]` the system prompt is never run through
  NER. The cheap regex recognizers (email, phone, IBAN, card, IP, date) still
  scan **every** role, so structured-identifier masking is unchanged — only the
  person/org/location model is scoped. The Anthropic top-level `system` field is
  treated as role `system`. A missing/unknown role is always scanned (scoping
  never silently drops masking).

- **`cache_capacity`** — reuse NER verdicts for byte-identical input across
  requests (LRU, default off). The key is a 128-bit hash of the text; only span
  offsets/kinds/scores are stored, **never plaintext**, so the cache is safe to
  enable on user content as well as the system prompt. Only successful
  inferences are cached — timeouts/queue-full errors are not, so a transient
  failure can't poison later requests.

The two are independent and compose. With NER scoped off a 6 KB system prompt and
verdicts cached for any other repeated context, per-call NER work drops to the
genuinely new text — letting you keep masking on without raising `timeout_ms` or
loosening `fail_mode`.

---

## Boot warning

If `enabled_by_default = true` but there is **no** `[pii.ner]` block and
`require_ner = false`, the gateway boots and logs:

```
PERSON/ORGANIZATION/LOCATION names are NOT masked
```

Regex entities (email, phone, IBAN, credit card, IP, date) still mask — only the
NER-backed kinds are affected. Set `require_ner = true` if a missing NER block
should be a hard failure instead of a warning.

### Boot failures (fail-closed)

| Situation | Result |
|-----------|--------|
| `require_ner = true`, no `[pii.ner]` | **hard boot failure** |
| `[pii.ner]` present, `model_dir` missing/invalid | **hard boot failure** |
| `[pii.ner]` present and valid, runtime inference error | governed by `fail_mode` |

`fail_mode` (`open` | `closed`) covers **only runtime NER errors** — timeout, full
queue, inference failure. It does **not** cover a missing model, which always
fails at boot regardless of `fail_mode`.

---

## Guardrails (content filter)

Guardrails are a separate surface (`[[guardrails.rules]]`) from PII masking but
often deployed alongside it. Three kinds — `prompt_injection`, `banned_content`,
`contact_info` — each with a `phase` (`pre` / `post` / `both`) and an `action`
(`block` / `redact` / `flag`).

Two honest limitations in v0.0.8:

- **`redact` detects and logs but does not yet mutate the body.** Use `block` for
  hard enforcement; body-redaction is a fast-follow.
- **Post-call rules run on non-streaming responses only.** Streaming responses are
  not scanned.

Full reference:
[config-reference.md → `[[guardrails.rules]]`](config-reference.md#guardrailsrules).

---

## Troubleshooting

### "I see `ner=false` in the logs and names aren't masked"

The `[pii.ner]` block didn't bind. The usual cause is a **misplaced or misspelled
key** that was silently ignored — for example:

- `[ner]` written at the top level instead of `[pii.ner]`.
- `score_threshold` placed under `[pii]` instead of `[pii.ner]`.

Re-run with `--strict-config`. Instead of a warning, the gateway will now fail to
boot and name the offending key (dotted, e.g. `pii.ner.workers`), so you can move
it to the correct table.

```bash
drgtw --config drgtw.toml --strict-config
```

For a production lockdown, also set `require_ner = true` — then a missing
`[pii.ner]` is a hard failure rather than a warning you might miss.

### "The gateway won't start"

Check, in order:

1. `require_ner = true` but no `[pii.ner]` block → add the block or set
   `require_ner = false`.
2. `[pii.ner].model_dir` points at a directory without the model artifacts → mount
   the volume or switch to the `-models` image tag.
3. `[pii.vault].key` is not exactly 64 hex characters after `${ENV}` resolution →
   fix the key (32 bytes, hex-encoded).

### "Dates / version numbers are being masked"

`DATE_TIME` is high-recall by design. Drop it from `entities`, or add
`disabled_recognizers = ["DATE_TIME"]`.

### "I need to mask employee numbers / national IDs"

Those have no built-in detector. Add a `[[pii.custom_recognizers]]` block with a
regex pattern; its `name` then becomes a valid `entities` entry.
