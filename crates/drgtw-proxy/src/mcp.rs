//! MCP gateway HTTP handler (WP-C).
//!
//! Serves the streamable-HTTP MCP transport at `POST /mcp`. The handler
//! authenticates with the same virtual keys as the other endpoints, then speaks
//! JSON-RPC 2.0:
//!
//! - `initialize`  → capabilities + serverInfo; issues an `Mcp-Session-Id`.
//! - `ping`        → empty result.
//! - notifications (no `id`) → `202 Accepted`, empty body.
//! - `tools/list`  → merged, prefixed tools from every configured upstream.
//! - `tools/call`  → routed to the owning upstream by tool-name prefix.
//! - any other method (with an `id`) → JSON-RPC `-32601`.
//!
//! Malformed request JSON yields a `200 OK` JSON-RPC parse error (`-32700`,
//! `id: null`) rather than an HTTP error, per the JSON-RPC convention. Only
//! authentication failures escape the JSON-RPC envelope (they reuse the
//! standard OpenAI-style 401 produced by [`ProxyError`]).
//!
//! Session handling in v1 is stateless: a fresh UUID is minted on every
//! `initialize` and any client-supplied `Mcp-Session-Id` is accepted without
//! server-side validation.
//!
//! ## PII
//! Pseudonymization of tool arguments / results is OUT OF SCOPE for v1
//! (documented in the design doc); request and response bodies pass through
//! verbatim.

use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use drgtw_mcp::{
    GatewayCallError, INTERNAL_ERROR, INVALID_PARAMS, INVALID_REQUEST, JsonRpcRequest,
    JsonRpcResponse, METHOD_NOT_FOUND, PARSE_ERROR, PROTOCOL_VERSION, UpstreamError,
};
use drgtw_trace::{McpMeta, TraceEntry, TraceKind};
use serde_json::{Value, json};

use crate::ProxyState;
use crate::error::{ErrorFormat, ProxyError};

/// `GET`/`DELETE /mcp` → 405. v1 supports only the `POST` streamable-HTTP
/// transport (no SSE server-push, no session teardown).
pub async fn method_not_allowed() -> Response {
    (StatusCode::METHOD_NOT_ALLOWED, "method not allowed").into_response()
}

/// `POST /mcp` — the MCP JSON-RPC entry point.
pub async fn handle_post(State(state): State<Arc<ProxyState>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();

    // 1. Authenticate exactly like the other endpoints. Auth failures escape the
    //    JSON-RPC envelope and reuse the standard 401 error shape.
    let resolved = match state.keys.authenticate(&parts.headers) {
        Ok(r) => r,
        Err(e) => return ProxyError::from(e).into_response_fmt(ErrorFormat::OpenAi),
    };
    let request_id = resolve_request_id(&parts.headers);
    let key_id = resolved.key_id.clone();

    // 2. Buffer the body (enforce the configured max-body limit).
    let max = state.config.server.max_body_bytes;
    let raw: Bytes = match to_bytes(body, max).await {
        Ok(b) => b,
        Err(_) => return ProxyError::BodyTooLarge.into_response_fmt(ErrorFormat::OpenAi),
    };

    // 3. Parse the JSON-RPC request in two steps. Syntactically invalid JSON →
    //    200 with a -32700 parse error; valid JSON that is not a well-formed
    //    JSON-RPC request → 200 with a -32600 invalid-request error. Both carry
    //    a null id, per the JSON-RPC convention.
    let value: Value = match serde_json::from_slice(&raw) {
        Ok(v) => v,
        Err(_) => {
            return json_rpc_response(JsonRpcResponse::error(
                None,
                PARSE_ERROR,
                "parse error: request body is not valid JSON",
            ));
        }
    };
    let rpc: JsonRpcRequest = match serde_json::from_value(value) {
        Ok(r) => r,
        Err(_) => {
            return json_rpc_response(JsonRpcResponse::error(
                None,
                INVALID_REQUEST,
                "invalid request: body is not a valid JSON-RPC request",
            ));
        }
    };

    // 4. Notifications carry no id and expect no response → 202, empty body.
    if rpc.is_notification() {
        return StatusCode::ACCEPTED.into_response();
    }

    let id = rpc.id.clone();

    // 5. Dispatch by method.
    let method = rpc.method.clone();
    match method.as_str() {
        "initialize" => initialize_response(id),
        "ping" => json_rpc_response(JsonRpcResponse::success(id, json!({}))),
        "tools/list" => {
            let started = Instant::now();
            let tools = state.mcp.aggregate_tools().await;
            // Method-only trace (no args/output for listing).
            emit_trace_mcp(
                &state,
                &request_id,
                &key_id,
                "tools/list",
                None,
                None,
                None,
                None,
                200,
                started.elapsed().as_millis() as u64,
                None,
            );
            json_rpc_response(JsonRpcResponse::success(id, json!({ "tools": tools })))
        }
        "tools/call" => tools_call(&state, id, rpc.params, &request_id, &key_id).await,
        _ => json_rpc_response(JsonRpcResponse::error(
            id,
            METHOD_NOT_FOUND,
            "method not found",
        )),
    }
}

/// Build the `initialize` success response and attach a fresh `Mcp-Session-Id`.
fn initialize_response(id: Option<Value>) -> Response {
    let result = json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "drgtw", "version": env!("CARGO_PKG_VERSION") },
    });
    let mut resp = json_rpc_response(JsonRpcResponse::success(id, result));
    if let Ok(value) = HeaderValue::from_str(&uuid::Uuid::new_v4().to_string()) {
        resp.headers_mut().insert("mcp-session-id", value);
    }
    resp
}

/// Handle `tools/call`: validate params, route to the upstream, and translate
/// gateway errors into JSON-RPC error responses. Emits an `mcp` trace entry
/// carrying the tool name, routed server, arguments, and output/status.
async fn tools_call(
    state: &ProxyState,
    id: Option<Value>,
    params: Option<Value>,
    request_id: &str,
    key_id: &str,
) -> Response {
    let started = Instant::now();

    // params must be an object carrying a string `name`.
    let Some(params) = params.as_ref().and_then(Value::as_object) else {
        emit_trace_mcp(
            state, request_id, key_id, "tools/call", None, None, None, None, 200,
            started.elapsed().as_millis() as u64,
            Some("invalid params: expected object with `name`".to_owned()),
        );
        return json_rpc_response(JsonRpcResponse::error(
            id,
            INVALID_PARAMS,
            "invalid params: expected an object with a `name` field",
        ));
    };
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        emit_trace_mcp(
            state, request_id, key_id, "tools/call", None, None, None, None, 200,
            started.elapsed().as_millis() as u64,
            Some("invalid params: missing `name`".to_owned()),
        );
        return json_rpc_response(JsonRpcResponse::error(
            id,
            INVALID_PARAMS,
            "invalid params: missing string field `name`",
        ));
    };
    // arguments is optional; default to an empty object.
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    // Capture the routed upstream server name (longest-prefix match) before the
    // call. `None` when no server owns the prefix (unknown tool).
    let server = state.mcp.route(name).map(|(c, _)| c.name().to_owned());
    let args_for_trace = Some(arguments.clone());

    match state.mcp.call(name, arguments).await {
        Ok(result) => {
            emit_trace_mcp(
                state,
                request_id,
                key_id,
                "tools/call",
                Some(name),
                server.as_deref(),
                args_for_trace,
                Some(result.clone()),
                200,
                started.elapsed().as_millis() as u64,
                None,
            );
            json_rpc_response(JsonRpcResponse::success(id, result))
        }
        Err(GatewayCallError::UnknownTool) => {
            emit_trace_mcp(
                state,
                request_id,
                key_id,
                "tools/call",
                Some(name),
                server.as_deref(),
                args_for_trace,
                None,
                200,
                started.elapsed().as_millis() as u64,
                Some(format!("unknown tool: `{name}`")),
            );
            json_rpc_response(JsonRpcResponse::error(
                id,
                INVALID_PARAMS,
                format!("unknown tool: `{name}`"),
            ))
        }
        Err(GatewayCallError::Upstream(UpstreamError::Rpc { code, message })) => {
            // Pass the upstream JSON-RPC error through verbatim.
            emit_trace_mcp(
                state,
                request_id,
                key_id,
                "tools/call",
                Some(name),
                server.as_deref(),
                args_for_trace,
                None,
                200,
                started.elapsed().as_millis() as u64,
                Some(format!("upstream rpc error {code}: {message}")),
            );
            json_rpc_response(JsonRpcResponse::error(id, code, message))
        }
        Err(GatewayCallError::Upstream(err)) => {
            // Transport / HTTP / protocol failure: log server-side, return a
            // brief internal error that leaks no upstream detail.
            tracing::warn!(tool = %name, error = %err, "MCP upstream call failed");
            emit_trace_mcp(
                state,
                request_id,
                key_id,
                "tools/call",
                Some(name),
                server.as_deref(),
                args_for_trace,
                None,
                200,
                started.elapsed().as_millis() as u64,
                Some(format!("upstream tool call failed: {err}")),
            );
            json_rpc_response(JsonRpcResponse::error(
                id,
                INTERNAL_ERROR,
                "internal error: upstream tool call failed",
            ))
        }
    }
}

/// Resolve the request id from the `x-drgtw-request-id` header (set by the bin
/// middleware) or generate a short fallback — mirrors `handlers::resolve_request_id`.
fn resolve_request_id(headers: &HeaderMap) -> String {
    if let Some(id) = headers.get("x-drgtw-request-id").and_then(|v| v.to_str().ok())
        && !id.is_empty()
    {
        return id.to_owned();
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!("drgtw-{nanos:016x}")
}

/// Emit an `mcp` trace entry when tracing is enabled. The crate truncates
/// `arguments`/`output` past 64 KiB itself; this only assembles the entry.
#[allow(clippy::too_many_arguments)]
fn emit_trace_mcp(
    state: &ProxyState,
    request_id: &str,
    key_id: &str,
    method: &str,
    tool: Option<&str>,
    server: Option<&str>,
    arguments: Option<Value>,
    output: Option<Value>,
    status: u16,
    latency_ms: u64,
    error: Option<String>,
) {
    let Some(trace) = &state.trace else { return };
    trace.emit(TraceEntry {
        ts: rfc3339_now(),
        request_id: request_id.to_owned(),
        virtual_key: Some(key_id.to_owned()),
        status: Some(status),
        latency_ms: Some(latency_ms),
        error,
        detail: TraceKind::Mcp(McpMeta {
            method: method.to_owned(),
            tool: tool.map(str::to_owned),
            server: server.map(str::to_owned),
            arguments,
            output,
        }),
    });
}

/// RFC3339 (UTC) timestamp for trace entries (mirrors `handlers::rfc3339_now`).
fn rfc3339_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (y, mo, d, h, mi, s) = civil_from_unix(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Convert unix seconds to a UTC civil date-time tuple (Hinnant's inverse).
fn civil_from_unix(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let hour = (rem / 3600) as u32;
    let min = ((rem % 3600) / 60) as u32;
    let sec = (rem % 60) as u32;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d, hour, min, sec)
}

/// Serialize a [`JsonRpcResponse`] into a `200 OK` `application/json` response.
fn json_rpc_response(resp: JsonRpcResponse) -> Response {
    let body = serde_json::to_vec(&resp).unwrap_or_else(|_| b"{}".to_vec());
    let mut response = Response::new(Body::from(body));
    response
        .headers_mut()
        .insert("content-type", HeaderValue::from_static("application/json"));
    response
}
