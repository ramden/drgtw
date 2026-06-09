//! Tool aggregation and routing across multiple upstream MCP servers.
//!
//! The [`McpGateway`] fans `tools/list` out to every configured upstream
//! concurrently, prefixes each tool's `name` with `"{server}-"` so names stay
//! unique, and routes `tools/call` back to the owning upstream by longest
//! server-name prefix match.

use serde_json::Value;

use crate::upstream::{UpstreamClient, UpstreamError, UpstreamServer};

/// Separator between server name and bare tool name in a prefixed tool name.
pub const SEPARATOR: &str = "-";

/// Aggregating gateway over a fixed set of upstream MCP servers.
#[derive(Debug)]
pub struct McpGateway {
    clients: Vec<UpstreamClient>,
}

/// Error returned by [`McpGateway::call`].
#[derive(Debug, thiserror::Error)]
pub enum GatewayCallError {
    /// No configured server owns the given prefixed tool name.
    #[error("unknown tool")]
    UnknownTool,
    /// The upstream call failed.
    #[error(transparent)]
    Upstream(#[from] UpstreamError),
}

impl McpGateway {
    /// Build a gateway over `servers`, sharing the given reqwest `client`.
    pub fn new(servers: Vec<UpstreamServer>, client: reqwest::Client) -> Self {
        let clients = servers
            .into_iter()
            .map(|s| UpstreamClient::new(s, client.clone()))
            .collect();
        Self { clients }
    }

    /// Number of configured upstream servers.
    pub fn server_count(&self) -> usize {
        self.clients.len()
    }

    /// Aggregate tools from every upstream concurrently.
    ///
    /// Each returned tool object has its `name` rewritten to the prefixed
    /// `"{server}-{tool}"` form; all other fields are left untouched. A server
    /// that fails to respond is logged at `warn` level and skipped.
    ///
    /// `inbound` is the list of (name, value) header pairs from the incoming
    /// request. Each upstream filters them through its own `forward_headers`
    /// allowlist.
    pub async fn aggregate_tools(&self, inbound: &[(String, String)]) -> Vec<Value> {
        let futures = self.clients.iter().map(|client| async move {
            match client.list_tools(inbound).await {
                Ok(tools) => {
                    let prefix = client.name().to_string();
                    tools
                        .into_iter()
                        .map(|mut tool| {
                            if let Some(name) = tool.get("name").and_then(Value::as_str) {
                                let prefixed = format!("{prefix}{SEPARATOR}{name}");
                                tool["name"] = Value::String(prefixed);
                            }
                            tool
                        })
                        .collect::<Vec<_>>()
                }
                Err(err) => {
                    tracing::warn!(
                        server = %client.name(),
                        error = %err,
                        "skipping upstream MCP server that failed tools/list"
                    );
                    Vec::new()
                }
            }
        });

        let per_server = futures::future::join_all(futures).await;
        per_server.into_iter().flatten().collect()
    }

    /// Resolve a prefixed tool name to its owning client and bare tool name.
    ///
    /// Uses longest server-name prefix match: when one server name is itself a
    /// prefix of another (e.g. `git` and `git-hub`), the longer match wins.
    pub fn route(&self, prefixed: &str) -> Option<(&UpstreamClient, String)> {
        let mut best: Option<(&UpstreamClient, String)> = None;
        let mut best_len = 0usize;
        for client in &self.clients {
            let needle = format!("{}{SEPARATOR}", client.name());
            if let Some(bare) = prefixed.strip_prefix(&needle)
                && !bare.is_empty()
                && client.name().len() > best_len
            {
                best_len = client.name().len();
                best = Some((client, bare.to_string()));
            }
        }
        best
    }

    /// Route and invoke a prefixed tool with `arguments`.
    ///
    /// `inbound` is passed to the owning upstream client for header forwarding.
    pub async fn call(
        &self,
        prefixed: &str,
        arguments: Value,
        inbound: &[(String, String)],
    ) -> Result<Value, GatewayCallError> {
        let (client, bare) = self.route(prefixed).ok_or(GatewayCallError::UnknownTool)?;
        Ok(client.call_tool(&bare, arguments, inbound).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gateway_with(names: &[&str]) -> McpGateway {
        let servers = names
            .iter()
            .map(|n| UpstreamServer {
                name: (*n).to_string(),
                url: format!("http://localhost/{n}"),
                headers: vec![],
                forward_headers: vec![],
            })
            .collect();
        McpGateway::new(servers, reqwest::Client::new())
    }

    #[test]
    fn route_exact_match() {
        let gw = gateway_with(&["git"]);
        let (client, bare) = gw.route("git-status").unwrap();
        assert_eq!(client.name(), "git");
        assert_eq!(bare, "status");
    }

    #[test]
    fn route_longest_prefix_wins() {
        // "git" prefixes "git-hub"; "git-hub-search" must route to "git-hub".
        let gw = gateway_with(&["git", "git-hub"]);
        let (client, bare) = gw.route("git-hub-search").unwrap();
        assert_eq!(client.name(), "git-hub");
        assert_eq!(bare, "search");
    }

    #[test]
    fn route_preserves_dashes_in_tool_name() {
        let gw = gateway_with(&["fs"]);
        let (client, bare) = gw.route("fs-read-file-range").unwrap();
        assert_eq!(client.name(), "fs");
        assert_eq!(bare, "read-file-range");
    }

    #[test]
    fn route_unknown_prefix_is_none() {
        let gw = gateway_with(&["git"]);
        assert!(gw.route("svn-status").is_none());
    }

    #[test]
    fn route_bare_server_name_without_tool_is_none() {
        let gw = gateway_with(&["git"]);
        // "git-" with empty tool must not match.
        assert!(gw.route("git-").is_none());
        // exact server name without separator must not match.
        assert!(gw.route("git").is_none());
    }
}
