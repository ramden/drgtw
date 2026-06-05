//! Upstream MCP client speaking the streamable-HTTP transport.
//!
//! Each [`UpstreamClient`] talks to one configured MCP server. It performs the
//! lazy `initialize` / `notifications/initialized` handshake on first use,
//! captures the server-issued `Mcp-Session-Id`, and replays it (plus the
//! `MCP-Protocol-Version` header and any static auth headers) on every later
//! request. Both `application/json` and `text/event-stream` response bodies are
//! understood; on an HTTP 404 after a session was established the session is
//! cleared and the request retried once (per the spec's session-expiry rule).

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::json;
use tokio::sync::Mutex;

use crate::jsonrpc::JsonRpcResponse;

/// MCP protocol version negotiated by this client.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// Static description of one upstream MCP server.
#[derive(Debug, Clone)]
pub struct UpstreamServer {
    /// Logical name used to namespace this server's tools.
    pub name: String,
    /// Streamable-HTTP endpoint URL (the `POST`/`GET` target).
    pub url: String,
    /// Pre-computed static headers (auth / extra) sent on every request.
    pub headers: Vec<(String, String)>,
}

/// Errors surfaced by [`UpstreamClient`] operations.
#[derive(Debug, thiserror::Error)]
pub enum UpstreamError {
    /// A non-success HTTP status was returned by the upstream.
    #[error("upstream returned HTTP status {status}")]
    Http {
        /// The HTTP status code.
        status: u16,
    },
    /// A transport-level failure (connect, TLS, body read, …).
    #[error("transport error: {0}")]
    Transport(#[from] reqwest::Error),
    /// The upstream returned a JSON-RPC error object.
    #[error("upstream JSON-RPC error {code}: {message}")]
    Rpc {
        /// JSON-RPC error code.
        code: i64,
        /// JSON-RPC error message.
        message: String,
    },
    /// A protocol violation (missing/garbled body, no matching response, …).
    #[error("protocol error: {0}")]
    Protocol(String),
}

/// Mutable per-client session state.
#[derive(Debug, Default)]
struct SessionState {
    /// Session id captured from the `Mcp-Session-Id` response header.
    session_id: Option<String>,
    /// Whether the `initialize` handshake has completed.
    initialized: bool,
}

/// Client for a single upstream MCP server.
#[derive(Debug)]
pub struct UpstreamClient {
    http: reqwest::Client,
    server: UpstreamServer,
    session: Mutex<SessionState>,
    next_id: AtomicU64,
}

impl UpstreamClient {
    /// Build a client for `server` using the shared reqwest `client`.
    pub fn new(server: UpstreamServer, client: reqwest::Client) -> Self {
        Self {
            http: client,
            server,
            session: Mutex::new(SessionState::default()),
            next_id: AtomicU64::new(1),
        }
    }

    /// The configured server name (used by the gateway for namespacing).
    pub fn name(&self) -> &str {
        &self.server.name
    }

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Apply the static auth/extra headers to a request builder.
    fn with_static_headers(&self, mut req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        for (k, v) in &self.server.headers {
            req = req.header(k, v);
        }
        req
    }

    /// Ensure the `initialize` handshake has run, capturing the session id.
    ///
    /// Idempotent: a no-op once the session is initialized.
    pub async fn ensure_initialized(&self) -> Result<(), UpstreamError> {
        {
            let state = self.session.lock().await;
            if state.initialized {
                return Ok(());
            }
        }
        self.initialize().await
    }

    /// Run the `initialize` request followed by `notifications/initialized`.
    async fn initialize(&self) -> Result<(), UpstreamError> {
        let id = self.next_id();
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "drgtw", "version": env!("CARGO_PKG_VERSION") },
            },
        });

        // The initialize request must not carry a session id yet, but should
        // carry the static auth headers.
        let req = self
            .http
            .post(&self.server.url)
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json")
            .json(&body);
        let req = self.with_static_headers(req);
        let resp = req.send().await?;

        if !resp.status().is_success() {
            return Err(UpstreamError::Http {
                status: resp.status().as_u16(),
            });
        }

        // Capture the session id, if the server issued one.
        let session_id = resp
            .headers()
            .get("Mcp-Session-Id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);

        // Validate the initialize response itself (drain body, surface RPC error).
        let _ = self.read_response_for_id(resp, id).await?;

        {
            let mut state = self.session.lock().await;
            state.session_id = session_id.clone();
            state.initialized = true;
        }

        // Send the initialized notification (no id ⇒ notification).
        self.send_initialized_notification(session_id).await?;
        Ok(())
    }

    /// POST the `notifications/initialized` notification.
    async fn send_initialized_notification(
        &self,
        session_id: Option<String>,
    ) -> Result<(), UpstreamError> {
        let body = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
        });
        let mut req = self
            .http
            .post(&self.server.url)
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json")
            .header("MCP-Protocol-Version", PROTOCOL_VERSION)
            .json(&body);
        if let Some(sid) = session_id {
            req = req.header("Mcp-Session-Id", sid);
        }
        let req = self.with_static_headers(req);
        let resp = req.send().await?;
        // Notifications expect 202 (or 200); anything else is a protocol issue,
        // but a non-2xx status is reported as HTTP.
        if !resp.status().is_success() {
            return Err(UpstreamError::Http {
                status: resp.status().as_u16(),
            });
        }
        Ok(())
    }

    /// List the tools exposed by this upstream. Returns the `result.tools`
    /// array (each item is a raw tool JSON object).
    pub async fn list_tools(&self) -> Result<Vec<serde_json::Value>, UpstreamError> {
        let result = self.request("tools/list", json!({})).await?;
        let tools = result
            .get("tools")
            .and_then(|t| t.as_array())
            .cloned()
            .ok_or_else(|| {
                UpstreamError::Protocol("tools/list result missing `tools` array".to_string())
            })?;
        Ok(tools)
    }

    /// Invoke `name` with `arguments`. Returns the full `result` object.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, UpstreamError> {
        self.request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )
        .await
    }

    /// Perform a JSON-RPC request, initializing first and retrying once on a
    /// post-session 404 (session expiry).
    async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, UpstreamError> {
        self.ensure_initialized().await?;
        match self.request_once(method, &params).await {
            Err(UpstreamError::Http { status: 404 }) => {
                // Session expired: clear it, re-initialize, retry exactly once.
                {
                    let mut state = self.session.lock().await;
                    state.initialized = false;
                    state.session_id = None;
                }
                self.ensure_initialized().await?;
                self.request_once(method, &params).await
            }
            other => other,
        }
    }

    /// Issue a single JSON-RPC request and return the `result` (or RPC error).
    async fn request_once(
        &self,
        method: &str,
        params: &serde_json::Value,
    ) -> Result<serde_json::Value, UpstreamError> {
        let id = self.next_id();
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        let session_id = { self.session.lock().await.session_id.clone() };

        let mut req = self
            .http
            .post(&self.server.url)
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json")
            .header("MCP-Protocol-Version", PROTOCOL_VERSION)
            .json(&body);
        if let Some(sid) = session_id {
            req = req.header("Mcp-Session-Id", sid);
        }
        let req = self.with_static_headers(req);
        let resp = req.send().await?;

        if !resp.status().is_success() {
            return Err(UpstreamError::Http {
                status: resp.status().as_u16(),
            });
        }

        let response = self.read_response_for_id(resp, id).await?;
        if let Some(err) = response.error {
            return Err(UpstreamError::Rpc {
                code: err.code,
                message: err.message,
            });
        }
        response
            .result
            .ok_or_else(|| UpstreamError::Protocol("response had neither result nor error".into()))
    }

    /// Read an HTTP response body (JSON or SSE) and extract the JSON-RPC
    /// response whose `id` matches `want_id`.
    async fn read_response_for_id(
        &self,
        resp: reqwest::Response,
        want_id: u64,
    ) -> Result<JsonRpcResponse, UpstreamError> {
        let content_type = resp
            .headers()
            .get("Content-Type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        let text = resp.text().await?;

        if content_type.contains("text/event-stream") {
            parse_sse_for_id(&text, want_id)
        } else {
            // Single JSON-RPC object.
            let resp: JsonRpcResponse = serde_json::from_str(&text).map_err(|e| {
                UpstreamError::Protocol(format!("invalid JSON-RPC response body: {e}"))
            })?;
            Ok(resp)
        }
    }
}

/// Parse a `text/event-stream` body, concatenating multi-`data:` lines per
/// event, and return the JSON-RPC response whose `id` equals `want_id`.
fn parse_sse_for_id(text: &str, want_id: u64) -> Result<JsonRpcResponse, UpstreamError> {
    let mut data_buf = String::new();
    let mut candidates: Vec<JsonRpcResponse> = Vec::new();

    let flush = |buf: &mut String, out: &mut Vec<JsonRpcResponse>| {
        if buf.is_empty() {
            return;
        }
        if let Ok(resp) = serde_json::from_str::<JsonRpcResponse>(buf) {
            out.push(resp);
        }
        buf.clear();
    };

    for raw_line in text.lines() {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if line.is_empty() {
            // Event boundary.
            flush(&mut data_buf, &mut candidates);
            continue;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            // A single leading space after the colon is part of the SSE framing.
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            if !data_buf.is_empty() {
                data_buf.push('\n');
            }
            data_buf.push_str(rest);
        }
        // Other SSE fields (event:, id:, retry:, comments) are ignored.
    }
    // Flush any trailing event not terminated by a blank line.
    flush(&mut data_buf, &mut candidates);

    let want = serde_json::Value::from(want_id);
    candidates
        .iter()
        .find(|r| r.id.as_ref() == Some(&want))
        .cloned()
        // Fall back to the sole candidate if ids don't line up but exactly one
        // response was present (some servers echo string ids etc.).
        .or_else(|| {
            if candidates.len() == 1 {
                candidates.into_iter().next()
            } else {
                None
            }
        })
        .ok_or_else(|| {
            UpstreamError::Protocol(format!(
                "no JSON-RPC response with id {want_id} in SSE stream"
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_concatenates_multi_data_lines_and_matches_id() {
        // Two data lines for one event form one JSON object.
        let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":5,\n\
                    data: \"result\":{\"ok\":true}}\n\n";
        let resp = parse_sse_for_id(body, 5).unwrap();
        assert_eq!(resp.id, Some(serde_json::Value::from(5u64)));
        assert_eq!(resp.result.unwrap(), serde_json::json!({ "ok": true }));
    }

    #[test]
    fn sse_picks_event_with_matching_id() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"a\":1}}\n\n\
                    data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"b\":2}}\n\n";
        let resp = parse_sse_for_id(body, 2).unwrap();
        assert_eq!(resp.result.unwrap(), serde_json::json!({ "b": 2 }));
    }

    #[test]
    fn sse_no_matching_id_is_protocol_error() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n\
                    data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{}}\n\n";
        let err = parse_sse_for_id(body, 9).unwrap_err();
        assert!(matches!(err, UpstreamError::Protocol(_)));
    }
}
