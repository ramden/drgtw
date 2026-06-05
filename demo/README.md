# DRGTW demo

A one-page chat portal that shows PII being pseudonymized on the way to the
provider and restored on the way back, all through the DRGTW gateway.

## Quickstart

1. `export OPENAI_API_KEY=sk-...` (or put it in `../.env`)
2. `docker compose -f demo/compose.yml up --build`
3. Open <http://localhost:8081>
4. Type a message containing a name/email/phone (e.g. `I'm Jane Doe, jane@example.com`)
5. Watch the four panels: what you typed → what the provider received
   (pseudonymized) → the provider's raw response → what you received (restored).

The gateway image is built locally from the repo `Dockerfile`. The portal
(nginx) proxies `/v1/*` to the gateway, so the browser stays same-origin and no
CORS is needed.
