# Roadmap

DRGTW is pre-1.0 and under active development. This page tracks what shipped
in each release and what is planned next. Dates are intentionally absent;
scope is the commitment, not the calendar.

## v0.0.2-alpha (current cycle)

Shipped in this release:

- **Model aliasing** — global `[model_aliases]` config table mapping an alias
  (e.g. `fast`) to a real model name. Resolution is one-level and happens
  before routing, allowlists, cost lookup, and usage events, so all downstream
  logic sees the resolved model.
- **Usage-event metadata passthrough** — per-agent / per-session cost
  attribution. Request body `metadata` objects and `x-drgtw-meta-*` request
  headers are merged (headers win) into a capped `metadata` map on every
  usage event. Headers are stripped before the request is forwarded upstream.
- **OpenTelemetry support** — opt-in `[otel]` config section exporting OTLP
  traces and metrics (GenAI semantic conventions). A strict privacy allow-list
  guarantees spans and metrics never carry prompt/response content or PII.
- **AWS Bedrock support** —
  - Bedrock's OpenAI-compatible Chat Completions endpoint works as a regular
    `open_ai` connection with a Bedrock API key (bearer auth), including SSE
    streaming.
  - Native `format = "bedrock"` connections proxy the Anthropic Messages
    surface to `InvokeModel` (bearer auth, non-streaming; streaming requests
    are rejected with a clear error).
- **Multi-arch container images** — releases now publish `linux/amd64` and
  `linux/arm64` images under one manifest.

## Planned (v0.0.3+)

- Bedrock: SigV4 authentication and native streaming
  (`InvokeModelWithResponseStream`, AWS event-stream framing).
- In-memory budgets keyed by attribution metadata (per-agent / per-session
  spend caps, building on the metadata passthrough above).
- Usage-event batching and at-least-once delivery for the webhook sink.
- Response-side sanitization of newly introduced PII.
- Cross-request placeholder restore for *streaming* responses (the persistent
  vault already covers non-streaming).
