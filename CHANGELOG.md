# Changelog

All notable changes to drgtw are documented here. The format is loosely based
on [Keep a Changelog](https://keepachangelog.com/); versions are pre-1.0 alpha.

## [0.0.10-alpha] — 2026-06-25

### Added — smaller NER model image
- **`:<version>-models-small`** image variant: bundles a distilled multilingual
  NER model (distilbert-base-multilingual-cased, hrl) instead of the larger
  default. ~2× faster NER inference at near-identical recall (benchmarked 0.97
  vs 0.98 recall, ~4.2ms vs ~8.5ms/sentence on a neutral multilingual set);
  same `PER`/`ORG`/`LOC` masking and the same `/app/models/ner-multilingual`
  path, so it is a drop-in for latency-sensitive deployments. The larger
  `:<version>-models` image still ships unchanged.

### Changed — model image workflow
- **`models.yml`** is now parameterized with `asset_pattern` and `tag_suffix`
  inputs (defaults `ner-multilingual.tar.gz` / `-models`), so any model variant
  can be layered onto a slim base from its own release asset. The default asset
  pattern is now an exact name (was a glob) so multiple model tarballs can
  coexist on one release without ambiguity.

## [0.0.9-alpha] — 2026-06-25

### Added — NER performance
- **`pii.ner.scan_roles`**: restrict the NER model to specific chat message
  roles (e.g. `["user", "assistant"]`), matched case-insensitively against the
  message `role` (the Anthropic top-level `system` field counts as `system`).
  Lets deployments skip NER on a large, static, PII-free system prompt. The
  regex recognizers (email/phone/IBAN/card/IP/date) still scan **every** role —
  only the NER model is scoped, so structured-identifier masking is unchanged.
  Absent = all roles (backward compatible); an empty list is rejected; a
  missing/unknown role is always scanned.
- **`pii.ner.cache_capacity`**: in-memory LRU cache of NER verdicts keyed on a
  128-bit hash of the input text. `0` (default) disables it; `> 0` reuses
  results for byte-identical text across requests. No plaintext is stored (only
  span offsets/kinds/scores), and only successful inferences are cached so a
  timeout/queue-full error cannot poison later requests. Composes with
  `scan_roles`.

## [0.0.8-alpha] — unreleased

### Added — PII entity coverage
- **`pii.entities`** allow-list: restrict pseudonymization to specific entity
  kinds. Presidio-style, case-insensitive, with aliases (e.g. `EMAIL` →
  `EMAIL_ADDRESS`, `CC` → `CREDIT_CARD`, `IP` → `IP_ADDRESS`). Absent = all
  kinds (backward compatible); an empty list is rejected. A `custom_recognizers`
  name is also a valid entity. Filtering is post-scan.
- **Built-in `IP_ADDRESS` recognizer** (IPv4 + IPv6, validated).
- **Built-in `DATE_TIME` recognizer** (German `DD.MM.YYYY` + ISO `YYYY-MM-DD`,
  best-effort; high recall — drop `DATE_TIME` from `entities` if false positives
  are unacceptable).
- New `EntityKind`s `IpAddress`, `DateTime`, `NationalId`, `Nrp` (placeholders
  `IP`/`DATE`/`NID`/`NRP`). `NATIONAL_ID`/`NRP` have no built-in detector — reach
  them via `custom_recognizers` or a future NER model.

### Added — fail-closed & discoverability
- **`pii.require_ner`**: when `true` and no `[pii.ner]` model is configured, the
  gateway refuses to boot (fail-closed for GDPR). Default `false` logs a boot
  **warning** instead ("PERSON/ORGANIZATION/LOCATION names are NOT masked").
- **`--strict-config`** flag: unknown/misplaced TOML keys become a hard boot
  error instead of being silently ignored. Without it, unknown keys now log a
  warning (previously: silent). This closes the silent-leak path where a
  misplaced `[ner]` (vs `[pii.ner]`) block left NER disabled with no signal.

### Added — content guardrails
- **`[guardrails]`** surface with `[[guardrails.rules]]` (kinds:
  `prompt_injection`, `banned_content`, `contact_info`; phases `pre`/`post`/`both`;
  actions `block`/`redact`/`flag`). New `drgtw-guardrails` crate. A `block`
  returns HTTP 403 (`content_filter`). Pre-call rules run on the raw prompt
  before pseudonymization.
  - **Limitations (v0.0.8):** `block` and `flag` are enforced end-to-end;
    `redact` currently detects and logs but does not yet mutate the body. Post-call
    guardrails run on non-streaming responses only (streaming is not scanned).

### Added — model packaging
- **`<user>/drgtw:<version>-models`** image variant bakes the multilingual NER
  model into `/app/models/ner-multilingual` (no volume mount needed). Built by CI
  when the `NER_MODEL_URL` repo variable is set (`Dockerfile.models`).

### Changed
- `docs/config-reference.md` `[pii]` section rewritten (the old "Placeholder
  until Phase 3, no runtime effect" stub was false — the engine is live). New
  `docs/pii.md` deployment guide.

### Notes
- Missing NER model with a `[pii.ner]` block is a hard boot failure (already the
  case). `fail_mode` (`open`/`closed`) covers runtime NER errors only.
