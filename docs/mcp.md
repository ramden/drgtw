# MCP Gateway

DRGTW exposes a central `/mcp` endpoint that aggregates one or more upstream
[Model Context Protocol](https://modelcontextprotocol.io) (MCP) servers behind a
single streamable-HTTP JSON-RPC endpoint. Configured upstreams are merged into
one tool catalog, every tool is namespaced by its server, and clients
authenticate with the same virtual keys they already use for the chat and
embeddings endpoints.

This lets you point any stock MCP client (Claude Code, Cursor, …) at the gateway
once and reach every configured upstream server through it — no per-server client
configuration, one credential.

---

## What it does

- **Aggregation.** Every server under `[mcp_servers.<name>]` is queried and their
  tools are merged into a single `tools/list` response.
- **Per-server namespacing.** Each upstream tool is exposed as
  `<server_name>-<tool_name>`. The prefix is how the gateway routes a
  `tools/call` back to the right upstream.
- **One credential.** Clients present a DRGTW virtual key; the gateway holds the
  per-upstream credentials and injects them when talking to each upstream.
- **Resilient fan-out.** `tools/list` queries upstreams concurrently. A failing
  upstream is logged and skipped — it does not fail the whole request.

---

## Configuration

MCP upstreams are declared in the same TOML config file as everything else, under
`[mcp_servers.<name>]` tables. The section is optional: with no servers
configured the `/mcp` endpoint still works and `tools/list` returns an empty
list.

```toml
[mcp_servers.context7]
url = "https://mcp.context7.com/mcp"   # absolute http(s), no query/fragment
description = "library documentation"   # optional, free text
auth_type = "none"                      # none (default) | api_key | bearer
# auth_value = "${CONTEXT7_API_KEY}"    # required iff auth_type != none; ${VAR} resolved

# Optional static headers sent on every upstream request. Values support ${VAR}.
[mcp_servers.context7.extra_headers]
CONTEXT7_API_KEY = "${CONTEXT7_API_KEY}"
```

### Fields

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `url` | string (URL) | yes | — | Upstream MCP endpoint. Absolute `http://` or `https://`, no query string or fragment. Streamable HTTP transport. |
| `description` | string | no | — | Free-text label. Informational only. |
| `auth_type` | enum string | no | `"none"` | How to authenticate to the upstream: `"none"`, `"api_key"`, or `"bearer"`. |
| `auth_value` | string | conditional | — | Credential value. **Required if and only if** `auth_type` is not `"none"`. Supports `${ENV_VAR}`. |
| `extra_headers` | table of string→string | no | `{}` | Static headers added to every upstream request. Header values support `${ENV_VAR}`. |

### How `auth_type` maps to a header

| `auth_type` | Header sent to upstream |
|-------------|-------------------------|
| `"none"` | (no auth header added) |
| `"api_key"` | `X-API-Key: <auth_value>` |
| `"bearer"` | `Authorization: Bearer <auth_value>` |

`extra_headers` are applied independently of `auth_type`, so a server that wants
its key in a custom header (rather than `X-API-Key` / `Authorization`) can use
`auth_type = "none"` plus an `extra_headers` entry.

### Environment-variable resolution

`auth_value` and every value under `extra_headers` support `${VAR}` references,
resolved from the process environment at startup — the same rules as `api_key`
and `base_url` on connections. A `${VAR}` whose variable is unset is a hard
startup error. A bare `$NAME` (no braces) is a literal, not a reference.

### Validation rules

Enforced by `drgtw_config::load()` at startup:

1. Server name (the `<name>` in `[mcp_servers.<name>]`) must be non-empty and
   ASCII `[a-zA-Z0-9_-]` only.
2. `url` must be an absolute `http://` or `https://` URL with no query string or
   fragment.
3. `auth_value` must be non-empty **iff** `auth_type` is not `"none"`. Setting
   `auth_value` with `auth_type = "none"`, or omitting it with `api_key` /
   `bearer`, is a configuration error.
4. Every `extra_headers` key must be a valid HTTP header name.
5. All `${VAR}` references in `auth_value` and `extra_headers` must resolve to
   non-empty environment variables.

See the [configuration reference](config-reference.md#mcp_serversname) for the
canonical field table.

---

## Tool namespacing and routing

Every upstream tool is exposed under the name `<server_name>-<tool_name>`. For
example, a `get-library-docs` tool on the `context7` server is listed as
`context7-get-library-docs`.

On `tools/call`, the gateway routes by **longest configured-server-name prefix
match** on the `<name>-` boundary: it finds the configured server whose
`<name>-` is the longest prefix of the requested tool name, strips that prefix,
and forwards the bare tool name to that upstream. A tool name that matches no
configured server returns a JSON-RPC `-32602` ("unknown tool") error.

---

## Supported methods

The endpoint speaks JSON-RPC 2.0 over streamable HTTP. Protocol version
`2025-06-18`. On `initialize`, the gateway always responds with protocol
version `2025-06-18` (the version it implements), regardless of what the client
sends; per the MCP spec the client may disconnect if it cannot support it.

| Method | Behavior |
|--------|----------|
| `initialize` | Returns `protocolVersion`, `capabilities.tools`, and `serverInfo` (`name: "drgtw"`). Response includes an `Mcp-Session-Id` header (a UUID). |
| `notifications/*` | Any notification (a JSON-RPC message with no `id`, e.g. `notifications/initialized`) is accepted with HTTP `202` and an empty body. |
| `ping` | Returns an empty result `{}`. |
| `tools/list` | Returns the merged, namespaced tool catalog from all reachable upstreams. Pagination cursors are ignored in v1. |
| `tools/call` | Routes to the upstream by tool-name prefix (see above). An upstream JSON-RPC error is passed through; an upstream transport error returns `-32603`. |
| anything else | JSON-RPC error `-32601` (method not found). |

### HTTP-level behavior

| Request | Result |
|---------|--------|
| `POST /mcp` (valid JSON-RPC) | `200` with `Content-Type: application/json`. |
| `POST /mcp` (notification) | `202`, empty body. |
| `POST /mcp` (unparseable body) | JSON-RPC `-32700` (parse error). |
| `GET /mcp` | `405` — server-push SSE streams are out of scope in v1. |
| `DELETE /mcp` | `405`. |

The gateway issues a fresh `Mcp-Session-Id` (UUID) on each `initialize` and
does not validate the header on subsequent requests in v1 (stateless session
handling).

---

## Authentication

Clients authenticate to `/mcp` with a DRGTW **virtual key** — the same
credential and the same headers as the chat and embeddings endpoints:

```
Authorization: Bearer sk-drgtw-...
```

Any valid virtual key may use `/mcp`; there is no per-key MCP gating in v1. A
missing or invalid key is rejected before any JSON-RPC parsing.

Upstream authentication is separate and per-server: the gateway injects each
upstream's configured credential (`auth_type` / `auth_value` / `extra_headers`).
Upstream credentials are never exposed to the client.

---

## Tracing

When filesystem tracing is enabled (it is on by default), each MCP tool call
through `/mcp` is traced with its method, tool name, upstream server, **arguments,
and outputs** (fields larger than 64 KiB are truncated). Unlike the LLM endpoints
— which trace metadata only — MCP traces include the full call payloads. See the
[`[tracing]` section of the configuration reference](config-reference.md#tracing)
for the directory, rotation, archiving, and retention settings, and to disable it.

---

## Limits (v1)

- **Streamable HTTP only.** No stdio transport, and no SSE server-push from the
  gateway (`GET /mcp` → `405`). The gateway *does* accept `text/event-stream`
  response bodies from upstreams and extracts the matching JSON-RPC response.
- **No PII pseudonymization of tool arguments.** Tool-call arguments and results
  pass through unmodified — the PII pipeline that protects chat/embeddings does
  not yet apply to MCP traffic. Do not send sensitive values through MCP tool
  calls expecting them to be masked.
- **No per-key MCP gating.** Any valid virtual key reaches every configured
  upstream.
- **Pagination cursors on `tools/list` are ignored.**

---

## curl examples

Replace `sk-drgtw-...` with one of your configured virtual keys, and assume the
gateway is listening on `http://localhost:8080`.

### initialize

```bash
curl http://localhost:8080/mcp \
  -H "Authorization: Bearer sk-drgtw-..." \
  -H "Content-Type: application/json" \
  -d '{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "initialize",
    "params": {
      "protocolVersion": "2025-06-18",
      "capabilities": {},
      "clientInfo": {"name": "curl", "version": "1.0"}
    }
  }'
```

### tools/list

```bash
curl http://localhost:8080/mcp \
  -H "Authorization: Bearer sk-drgtw-..." \
  -H "Content-Type: application/json" \
  -d '{
    "jsonrpc": "2.0",
    "id": 2,
    "method": "tools/list"
  }'
```

The result is a merged catalog; tool names are prefixed with the upstream server
name (e.g. `context7-get-library-docs`).

### tools/call

```bash
curl http://localhost:8080/mcp \
  -H "Authorization: Bearer sk-drgtw-..." \
  -H "Content-Type: application/json" \
  -d '{
    "jsonrpc": "2.0",
    "id": 3,
    "method": "tools/call",
    "params": {
      "name": "context7-get-library-docs",
      "arguments": {"context7CompatibleLibraryID": "/vercel/next.js"}
    }
  }'
```

The `name` is the namespaced tool name from `tools/list`; the gateway strips the
`context7-` prefix and forwards `get-library-docs` to the `context7` upstream.

---

## Client configuration

Stock MCP clients that support streamable-HTTP servers with custom headers
(Claude Code, Cursor, …) point at `/mcp` and carry the virtual key in the
`Authorization` header:

```json
{
  "mcpServers": {
    "drgtw": {
      "url": "http://localhost:8080/mcp",
      "headers": {
        "Authorization": "Bearer sk-drgtw-..."
      }
    }
  }
}
```

Every tool from every configured upstream then appears in the client under its
namespaced name.

---

## Example: context7 upstream

[context7](https://context7.com) is a public MCP server for up-to-date library
documentation. It works without authentication; an API key (sent in a custom
header) raises rate limits.

Anonymous (no auth):

```toml
[mcp_servers.context7]
url = "https://mcp.context7.com/mcp"
description = "up-to-date library documentation"
auth_type = "none"
```

With an API key supplied via `extra_headers`:

```toml
[mcp_servers.context7]
url = "https://mcp.context7.com/mcp"
description = "up-to-date library documentation"
auth_type = "none"

[mcp_servers.context7.extra_headers]
CONTEXT7_API_KEY = "${CONTEXT7_API_KEY}"
```

With either block in place, `tools/list` on the gateway includes the context7
tools under the `context7-` prefix.
