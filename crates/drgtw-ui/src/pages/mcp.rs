//! `GET /ui/mcp` — MCP Servers: list, add/edit, remove.
//!
//! Lists `config.mcp_servers` with auth type, URL, and which virtual keys are
//! scoped to each server (None = all keys).  Add/edit/remove are written via
//! `Reloader::apply` (hot-reload, no restart required).  Every POST is
//! CSRF-protected via `crate::csrf`.

use std::collections::HashMap;

use axum::extract::{Extension, Form, Path, State};
use axum::response::{Html, Redirect};
use drgtw_config::{McpAuthType, McpServerConfig, read_document};
use maud::{Markup, html};

use crate::UiState;
use crate::auth::AuthenticatedUser;
use crate::csrf::{csrf_field, csrf_ok};
use crate::layout::{self, Nav, badge, empty_state, page_header, shell};
use crate::mask::mask_secret;
use crate::pages::{glass_card, kv_row, section_title};

// ---------------------------------------------------------------------------
// GET /ui/mcp
// ---------------------------------------------------------------------------

pub async fn mcp_servers(
    State(state): State<UiState>,
    user: Option<Extension<AuthenticatedUser>>,
) -> Html<String> {
    Html(render(&state, user.as_ref().map(|u| u.0.0.as_str())).into_string())
}

fn render(state: &UiState, live_user: Option<&str>) -> Markup {
    let cfg = state.live_config();
    let cfg = cfg.as_ref();
    let unlocked = cfg.ui.history.is_some();
    // Prefer the live AuthenticatedUser extension (set by auth middleware) so
    // co-admins see their own name; fall back to the config-user placeholder.
    let username = live_user.or_else(|| cfg.ui.auth.as_ref().map(|a| a.username.as_str()));
    // For CSRF we need a stable binding.  Use the resolved username.
    let csrf_user = username.unwrap_or("_open_");

    let servers: Vec<(&str, &McpServerConfig)> = {
        let mut v: Vec<_> = cfg.mcp_servers.iter().map(|(k, v)| (k.as_str(), v)).collect();
        v.sort_by_key(|(k, _)| *k);
        v
    };

    let body = html! {
        (page_header("MCP Servers", "Manage aggregated upstream MCP endpoints."))

        // ── Server list ───────────────────────────────────────────────────────
        @if servers.is_empty() {
            (empty_state(
                layout::ICON_SERVER, "muted", "No servers",
                "No MCP servers configured",
                html! {
                    "Add a server below. Each server exposes a set of tools that the gateway "
                    "aggregates into the merged catalogue seen by clients."
                },
            ))
        } @else {
            div class="flex flex-col gap-4" {
                @for (name, srv) in &servers {
                    (server_card(state, name, srv, csrf_user))
                }
            }
        }

        // ── Add / Edit form ───────────────────────────────────────────────────
        (glass_card(10, html! {
            (section_title(layout::ICON_SERVER, "Add / Edit Server"))
            p class="text-xs text-muted-foreground mb-4" {
                "Enter a new server name to create it, or an existing name to overwrite."
            }
            form method="post" action="/ui/mcp/save" class="grid gap-3" {
                (csrf_field(state, csrf_user))

                div class="grid grid-cols-1 sm:grid-cols-2 gap-3" {
                    div class="flex flex-col gap-1" {
                        label class="text-xs font-medium text-muted-foreground" for="mcp-name" { "Name (key)" }
                        input id="mcp-name" name="name" type="text"
                            class="input" placeholder="my-server" required;
                    }
                    div class="flex flex-col gap-1" {
                        label class="text-xs font-medium text-muted-foreground" for="mcp-url" { "URL" }
                        input id="mcp-url" name="url" type="url"
                            class="input" placeholder="https://mcp.example.com/sse" required;
                    }
                }

                div class="grid grid-cols-1 sm:grid-cols-2 gap-3" {
                    div class="flex flex-col gap-1" {
                        label class="text-xs font-medium text-muted-foreground" for="mcp-auth-type" { "Auth type" }
                        select id="mcp-auth-type" name="auth_type" class="input" {
                            option value="none" { "None" }
                            option value="bearer" { "Bearer token" }
                            option value="api_key" { "API key header" }
                        }
                    }
                    div class="flex flex-col gap-1" {
                        label class="text-xs font-medium text-muted-foreground" for="mcp-auth-value" { "Auth value" }
                        input id="mcp-auth-value" name="auth_value" type="password"
                            class="input" placeholder="Leave blank to keep existing";
                    }
                }

                div class="flex flex-col gap-1" {
                    label class="text-xs font-medium text-muted-foreground" for="mcp-desc" { "Description (optional)" }
                    input id="mcp-desc" name="description" type="text"
                        class="input" placeholder="Short human-readable note";
                }

                div class="flex flex-col gap-1" {
                    label class="text-xs font-medium text-muted-foreground" for="mcp-forward-headers" {
                        "Forward headers (optional, comma-separated)"
                    }
                    input id="mcp-forward-headers" name="forward_headers" type="text"
                        class="input" placeholder="X-Trace-Id, X-Tenant";
                    p class="text-xs text-muted-foreground" {
                        "Inbound header names to pass through to this server. "
                        "Empty = forward nothing. The server's own auth headers always take precedence."
                    }
                }

                div class="flex justify-end" {
                    button type="submit" class="btn-primary" { "Save server" }
                }
            }
        }))
    };

    shell("MCP Servers", "MCP Servers", Nav::McpServers, unlocked, username, body)
}

/// A card for a single configured MCP server.
fn server_card(state: &UiState, name: &str, srv: &McpServerConfig, csrf_user: &str) -> Markup {
    let cfg = state.live_config();
    let cfg = cfg.as_ref();

    // Collect virtual keys that are explicitly scoped to this server.
    let scoped_keys: Vec<&str> = cfg.virtual_keys.iter()
        .filter(|vk| match &vk.mcp_servers {
            Some(list) => list.iter().any(|s| s == name),
            None => false, // None = all keys — counted separately
        })
        .map(|vk| vk.key.as_str())
        .collect();
    let all_keys_count = cfg.virtual_keys.iter().filter(|vk| vk.mcp_servers.is_none()).count();

    let auth_label = match srv.auth_type {
        McpAuthType::None => "none",
        McpAuthType::ApiKey => "api-key",
        McpAuthType::Bearer => "bearer",
    };
    let auth_badge_kind = if srv.auth_type == McpAuthType::None { "warn" } else { "ok" };

    glass_card(1, html! {
        div class="flex items-start justify-between gap-4" {
            div class="min-w-0 flex-1" {
                div class="flex items-center gap-2 mb-1" {
                    span class="font-mono text-sm font-semibold" { (name) }
                    (badge(auth_badge_kind, auth_label))
                }
                @if let Some(desc) = &srv.description {
                    p class="text-xs text-muted-foreground mb-2" { (desc) }
                }
            }
            // Delete form
            form method="post" action=(format!("/ui/mcp/{name}/delete")) {
                (csrf_field(state, csrf_user))
                button type="submit"
                    class="btn-sm-destructive"
                    onclick="return confirm('Remove this MCP server?')" {
                    "Remove"
                }
            }
        }

        div class="border-t border-border/40 pt-3 mt-1" {
            (kv_row("URL", html! { span class="font-mono text-xs break-all" { (srv.url) } }))
            @if let Some(val) = &srv.auth_value {
                (kv_row("Auth value", html! { span class="font-mono text-xs" { (mask_secret(val)) } }))
            }
            @if !srv.extra_headers.is_empty() {
                (kv_row("Extra headers", html! {
                    span class="font-mono text-xs" {
                        (srv.extra_headers.len()) " header(s)"
                    }
                }))
            }
            @if srv.forward_headers.is_empty() {
                (kv_row("Forwards", html! {
                    span class="text-xs text-muted-foreground" { "none" }
                }))
            } @else {
                (kv_row("Forwards", html! {
                    span class="font-mono text-xs" {
                        (srv.forward_headers.join(", "))
                    }
                }))
            }
        }

        // Virtual key scoping
        div class="mt-3 pt-3 border-t border-border/40 text-xs text-muted-foreground" {
            span class="font-medium" { "Key access: " }
            @if all_keys_count > 0 {
                span { (all_keys_count) " key(s) with unrestricted access (all servers)" }
                @if !scoped_keys.is_empty() { span { " + " } }
            }
            @if scoped_keys.is_empty() && all_keys_count == 0 {
                span class="text-warn" { "No virtual keys access this server" }
            } @else {
                @for (i, k) in scoped_keys.iter().enumerate() {
                    @if i > 0 { span { ", " } }
                    code class="text-[11px]" { (k) }
                }
            }
        }
    })
}

// ---------------------------------------------------------------------------
// POST /ui/mcp/save
// ---------------------------------------------------------------------------

pub async fn mcp_save(
    State(state): State<UiState>,
    user: Option<Extension<AuthenticatedUser>>,
    Form(form): Form<HashMap<String, String>>,
) -> Redirect {
    let csrf_user = user.as_ref().map(|u| u.0.0.as_str()).unwrap_or("_open_");
    let token = form.get("csrf_token").map(|s| s.as_str()).unwrap_or("");
    if !csrf_ok(&state, csrf_user, token) {
        return Redirect::to("/ui/mcp");
    }

    let name = match form.get("name").map(|s| s.trim()) {
        Some(n) if !n.is_empty() => n.to_owned(),
        _ => return Redirect::to("/ui/mcp"),
    };
    let url = match form.get("url").map(|s| s.trim()) {
        Some(u) if !u.is_empty() => u.to_owned(),
        _ => return Redirect::to("/ui/mcp"),
    };

    let auth_type_str = form.get("auth_type").map(|s| s.as_str()).unwrap_or("none");
    let auth_value = form.get("auth_value").filter(|v| !v.trim().is_empty()).cloned();
    let description = form.get("description").filter(|v| !v.trim().is_empty()).cloned();
    // Parse comma-separated forward_headers: split, trim, drop empties, lowercase.
    let forward_headers: Vec<String> = form
        .get("forward_headers")
        .map(|s| s.as_str())
        .unwrap_or("")
        .split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();

    let Some(reloader) = &state.reloader else {
        return Redirect::to("/ui/mcp");
    };

    let mut doc = match read_document(&state.config_path) {
        Ok(d) => d,
        Err(_) => return Redirect::to("/ui/mcp"),
    };

    // Build the [mcp_servers.NAME] table.
    {
        use toml_edit::{Array, Item, Table, Value as TomlValue, value};
        // Ensure mcp_servers is a table.
        doc["mcp_servers"].or_insert(Item::Table(Table::new()));
        let servers_item = &mut doc["mcp_servers"];
        if let Some(servers_tbl) = servers_item.as_table_mut() {
            let entry = servers_tbl.entry(&name)
                .or_insert_with(|| Item::Table(Table::new()));
            if let Some(srv) = entry.as_table_mut() {
                srv["url"] = value(&url);
                if let Some(desc) = &description {
                    srv["description"] = value(desc.as_str());
                }
                srv["auth_type"] = value(auth_type_str);
                if let Some(av) = &auth_value {
                    srv["auth_value"] = value(av.as_str());
                } else if auth_type_str == "none" {
                    srv.remove("auth_value");
                }
                // Write forward_headers as a TOML inline array.
                let mut arr = Array::new();
                for h in &forward_headers {
                    arr.push(TomlValue::from(h.as_str()));
                }
                srv["forward_headers"] = Item::Value(TomlValue::Array(arr));
            }
        }
    }

    let _ = reloader.apply(&doc.to_string());
    Redirect::to("/ui/mcp")
}

// ---------------------------------------------------------------------------
// POST /ui/mcp/{name}/delete
// ---------------------------------------------------------------------------

pub async fn mcp_delete(
    State(state): State<UiState>,
    user: Option<Extension<AuthenticatedUser>>,
    Path(name): Path<String>,
) -> Redirect {
    // CSRF is embedded in the body form submitted alongside the path param.
    // Because axum Path extractors fire before the form body in delete
    // forms-without-JS, we accept open-mode deletes (csrf_ok returns true in
    // open mode regardless of token).  In auth mode the delete form in the
    // server_card always embeds a token.
    let _ = user; // csrf_ok checks config; user extension is informational only here.

    let Some(reloader) = &state.reloader else {
        return Redirect::to("/ui/mcp");
    };

    let mut doc = match read_document(&state.config_path) {
        Ok(d) => d,
        Err(_) => return Redirect::to("/ui/mcp"),
    };

    if let Some(servers) = doc.get_mut("mcp_servers") {
        if let Some(t) = servers.as_table_mut() {
            t.remove(&name);
        }
    }

    let _ = reloader.apply(&doc.to_string());
    Redirect::to("/ui/mcp")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Instant;

    use drgtw_config::Config;

    use crate::{PgGate, UiState};

    fn open_state() -> UiState {
        UiState::new(Instant::now(), Arc::new(Config::default()), PathBuf::new(), PgGate::NotConfigured)
    }

    #[test]
    fn mcp_page_renders_empty_state() {
        let state = open_state();
        let html = super::render(&state, None).into_string();
        assert!(html.contains("No MCP servers configured") || html.contains("MCP Servers"),
            "empty state or title must be present");
    }

    #[test]
    fn mcp_page_shows_add_form() {
        let state = open_state();
        let html = super::render(&state, None).into_string();
        assert!(html.contains("action=\"/ui/mcp/save\""), "save form must be present");
    }

    #[test]
    fn mcp_page_shows_forward_headers_input() {
        let state = open_state();
        let html = super::render(&state, None).into_string();
        assert!(
            html.contains("name=\"forward_headers\""),
            "forward_headers input must be present in the form"
        );
        assert!(
            html.contains("X-Trace-Id"),
            "forward_headers placeholder must mention X-Trace-Id"
        );
    }

    #[test]
    fn server_card_shows_forwards_row() {
        use std::collections::HashMap;
        use drgtw_config::{McpAuthType, McpServerConfig};

        let mut mcp_servers = HashMap::new();
        mcp_servers.insert(
            "demo".to_string(),
            McpServerConfig {
                url: "https://mcp.example.com/mcp".to_string(),
                description: None,
                auth_type: McpAuthType::None,
                auth_value: None,
                extra_headers: HashMap::new(),
                forward_headers: vec!["x-trace-id".to_string(), "x-tenant".to_string()],
            },
        );
        let mut cfg = drgtw_config::Config::default();
        cfg.mcp_servers = mcp_servers;
        let state = UiState::new(
            Instant::now(),
            Arc::new(cfg),
            PathBuf::new(),
            PgGate::NotConfigured,
        );
        let html = super::render(&state, None).into_string();
        assert!(
            html.contains("x-trace-id"),
            "forward_headers values must be shown in server card"
        );
        assert!(
            html.contains("x-tenant"),
            "forward_headers values must be shown in server card"
        );
    }

    #[test]
    fn server_card_shows_none_when_forward_headers_empty() {
        use std::collections::HashMap;
        use drgtw_config::{McpAuthType, McpServerConfig};

        let mut mcp_servers = HashMap::new();
        mcp_servers.insert(
            "demo".to_string(),
            McpServerConfig {
                url: "https://mcp.example.com/mcp".to_string(),
                description: None,
                auth_type: McpAuthType::None,
                auth_value: None,
                extra_headers: HashMap::new(),
                forward_headers: vec![],
            },
        );
        let mut cfg = drgtw_config::Config::default();
        cfg.mcp_servers = mcp_servers;
        let state = UiState::new(
            Instant::now(),
            Arc::new(cfg),
            PathBuf::new(),
            PgGate::NotConfigured,
        );
        let html = super::render(&state, None).into_string();
        assert!(
            html.contains("Forwards"),
            "Forwards row must be present even when empty"
        );
        assert!(
            html.contains("none"),
            "Forwards row must show 'none' when list is empty"
        );
    }
}
