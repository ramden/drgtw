//! # drgtw-mcp
//!
//! MCP-gateway core for the DRGTW gateway: JSON-RPC 2.0 envelope types, an
//! upstream MCP client speaking the streamable-HTTP transport, and a gateway
//! that aggregates and routes tools across many upstream servers.
//!
//! This crate is intentionally **decoupled from `drgtw-config`**: it defines
//! its own input type ([`UpstreamServer`]) so it can be unit-tested in
//! isolation. Wiring from configuration happens later in `drgtw-proxy`.
//!
//! ## Components
//!
//! - [`jsonrpc`] — [`JsonRpcRequest`] / [`JsonRpcResponse`] and the standard
//!   JSON-RPC error codes.
//! - [`upstream`] — [`UpstreamClient`] (lazy handshake, session capture, SSE +
//!   JSON body parsing, 404-retry) over an [`UpstreamServer`].
//! - [`gateway`] — [`McpGateway`]: concurrent `aggregate_tools()`, prefix-based
//!   [`McpGateway::route`] and [`McpGateway::call`].

pub mod gateway;
pub mod jsonrpc;
pub mod upstream;

pub use gateway::{GatewayCallError, McpGateway, SEPARATOR};
pub use jsonrpc::{
    INTERNAL_ERROR, INVALID_PARAMS, INVALID_REQUEST, JsonRpcError, JsonRpcRequest, JsonRpcResponse,
    METHOD_NOT_FOUND, PARSE_ERROR,
};
pub use upstream::{PROTOCOL_VERSION, UpstreamClient, UpstreamError, UpstreamServer};
