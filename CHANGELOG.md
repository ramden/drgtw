# Changelog

All notable changes to drgtw are documented here. The format is loosely based
on [Keep a Changelog](https://keepachangelog.com/); versions are pre-1.0 alpha.

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
