//! JSON-RPC 2.0 request/response types shared by the MCP gateway.
//!
//! These types model the subset of the [JSON-RPC 2.0] envelope used by the
//! Model Context Protocol streamable-HTTP transport. A request with `id: None`
//! is a *notification* (no response is expected).
//!
//! [JSON-RPC 2.0]: https://www.jsonrpc.org/specification

use serde::{Deserialize, Serialize};

/// Standard JSON-RPC error codes (see the JSON-RPC 2.0 specification).
/// Invalid JSON was received by the server.
pub const PARSE_ERROR: i64 = -32700;
/// The JSON sent is not a valid Request object.
pub const INVALID_REQUEST: i64 = -32600;
/// The method does not exist / is not available.
pub const METHOD_NOT_FOUND: i64 = -32601;
/// Invalid method parameter(s).
pub const INVALID_PARAMS: i64 = -32602;
/// Internal JSON-RPC error.
pub const INTERNAL_ERROR: i64 = -32603;

/// A JSON-RPC 2.0 request (or notification when [`id`](Self::id) is `None`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    /// Protocol version marker, always `"2.0"` for valid requests.
    pub jsonrpc: String,
    /// Request identifier. `None` marks this as a notification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<serde_json::Value>,
    /// The method to invoke.
    pub method: String,
    /// Optional structured parameters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

impl JsonRpcRequest {
    /// Returns `true` when this request is a notification (carries no `id`).
    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }
}

/// The `error` member of a JSON-RPC error response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcError {
    /// Numeric error code.
    pub code: i64,
    /// Human-readable error message.
    pub message: String,
    /// Optional additional error data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// A JSON-RPC 2.0 response — either a success (`result`) or an `error`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    /// Protocol version marker, always `"2.0"`.
    pub jsonrpc: String,
    /// Identifier echoed from the originating request.
    pub id: Option<serde_json::Value>,
    /// Success payload. Present iff this is a success response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    /// Error payload. Present iff this is an error response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl JsonRpcResponse {
    /// Construct a success response echoing `id` and carrying `result`.
    pub fn success(id: Option<serde_json::Value>, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    /// Construct an error response with the given `code` and `message`.
    pub fn error(id: Option<serde_json::Value>, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_round_trips() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(7)),
            method: "tools/list".to_string(),
            params: Some(json!({})),
        };
        let wire = serde_json::to_value(&req).unwrap();
        let back: JsonRpcRequest = serde_json::from_value(wire).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn notification_has_no_id_and_is_detected() {
        let raw = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        let req: JsonRpcRequest = serde_json::from_value(raw).unwrap();
        assert!(req.is_notification());
        assert_eq!(req.id, None);

        // A request with an id is not a notification.
        let raw2 = json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" });
        let req2: JsonRpcRequest = serde_json::from_value(raw2).unwrap();
        assert!(!req2.is_notification());
    }

    #[test]
    fn notification_serializes_without_id_field() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: None,
            method: "notifications/initialized".to_string(),
            params: None,
        };
        let wire = serde_json::to_value(&req).unwrap();
        assert!(wire.get("id").is_none());
        assert!(wire.get("params").is_none());
    }

    #[test]
    fn success_response_round_trips() {
        let resp = JsonRpcResponse::success(Some(json!(1)), json!({ "ok": true }));
        assert_eq!(resp.jsonrpc, "2.0");
        assert!(resp.error.is_none());
        let wire = serde_json::to_value(&resp).unwrap();
        assert!(wire.get("error").is_none());
        let back: JsonRpcResponse = serde_json::from_value(wire).unwrap();
        assert_eq!(resp, back);
    }

    #[test]
    fn error_response_round_trips() {
        let resp = JsonRpcResponse::error(Some(json!(2)), METHOD_NOT_FOUND, "no such method");
        assert!(resp.result.is_none());
        let err = resp.error.clone().unwrap();
        assert_eq!(err.code, METHOD_NOT_FOUND);
        assert_eq!(err.message, "no such method");
        let wire = serde_json::to_value(&resp).unwrap();
        assert!(wire.get("result").is_none());
        let back: JsonRpcResponse = serde_json::from_value(wire).unwrap();
        assert_eq!(resp, back);
    }

    #[test]
    fn standard_codes_have_expected_values() {
        assert_eq!(PARSE_ERROR, -32700);
        assert_eq!(INVALID_REQUEST, -32600);
        assert_eq!(METHOD_NOT_FOUND, -32601);
        assert_eq!(INVALID_PARAMS, -32602);
        assert_eq!(INTERNAL_ERROR, -32603);
    }
}
