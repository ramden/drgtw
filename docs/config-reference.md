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
| `format` | enum string | yes | — | Wire protocol. One of `"open_ai"` or `"anthropic"`. |
| `models` | array of strings | no | `[]` | Model names served by this connection. Used for routing and allowlist checks. |

### `format` values

| TOML value | Upstream protocol |
|------------|-------------------|
| `"open_ai"` | OpenAI Chat Completions API |
| `"anthropic"` | Anthropic Messages API |

---

## `[[virtual_keys]]`

Array of credentials issued to downstream callers. Callers supply these as Bearer tokens (or in the `x-api-key` header for Anthropic-format requests) using stock OpenAI or Anthropic SDKs.

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `key` | string | yes | — | The key value. Must start with `sk-drgtw-`. Must be unique across all virtual keys. |
| `connections` | array of strings | yes | — | One or more connection `name` values this key may route to. Must be non-empty; all names must resolve to a defined connection. |
| `models` | array of strings | no | `null` (all) | Optional model allowlist. When omitted, all models of the listed connections are permitted. When present, requests for any other model are rejected with 403. |

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
4. `format` must be exactly `"open_ai"` or `"anthropic"`.
5. Virtual key `key` values must start with `sk-drgtw-` and be unique.
6. Each name in `[[virtual_keys]].connections` must match a defined connection `name`.
7. `[[virtual_keys]].connections` must be non-empty.
8. Each `[mcp_servers.<name>]` server name must be non-empty and ASCII `[a-zA-Z0-9_-]` only.
9. `[mcp_servers.<name>].url` must be an absolute `http://` or `https://` URL with no query string or fragment.
10. `[mcp_servers.<name>].auth_value` must be non-empty iff `auth_type != "none"`.
11. Every `[mcp_servers.<name>].extra_headers` key must be a valid HTTP header name.

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

[pii]
enabled_by_default = false

[tracing]
enabled = true
retention_days = 90
```
