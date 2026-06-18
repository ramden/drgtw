//! Shared page shell: `<head>`, app sidebar, top header bar, and the reusable
//! UI primitives (glass cards, badges, page headers, empty states) every page
//! is built from.
//!
//! Design system lives in `styles/theme.css` (compiled into `app.css`). This
//! module only emits markup + class names. Dark-only; no theme toggle.

use maud::{DOCTYPE, Markup, PreEscaped, html};

/// Which sidebar entry is active (drives the highlighted nav link).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Nav {
    Dashboard,
    Configuration,
    Connections,
    VirtualKeys,
    Analytics,
    Traces,
    PiiInsights,
    AuditLog,
    CostBudgets,
    RateLimits,
    McpServers,
    Webhooks,
    TeamAccess,
    Settings,
}

/// Lifecycle state of a nav item — drives badge + locked styling.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Backed by real data.
    Live,
    /// Polished empty state, feature not yet built.
    Soon,
    /// Needs Postgres (`[ui.history]`); locked until configured.
    Postgres,
}

/// Render a full page: doctype, head, sidebar, header bar, and `body` content.
///
/// `history_unlocked` mirrors `config.ui.history.is_some()` — when `true` the
/// Postgres-gated nav entries (Analytics/Traces/Audit) unlock visually.
///
/// `username` is shown in the sidebar footer when auth is enabled; `None` shows
/// the static placeholder (open mode with no auth configured).
pub fn shell(
    title: &str,
    breadcrumb: &str,
    active: Nav,
    history_unlocked: bool,
    username: Option<&str>,
    body: Markup,
) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { "drgtw — " (title) }
                meta name="color-scheme" content="dark";
                // Pre-paint background: inlined so it applies before app.css
                // arrives — paints the document background immediately so
                // navigation never shows a white UA frame.
                style { (PreEscaped(PREPAINT_CSS)) }
                // `?v=` cache-bust: the asset filenames are stable, so browsers
                // pin the old stylesheet across a redesign. Bump on visual change.
                link rel="stylesheet" href="/ui/assets/vendor/app.css?v=teal2";
                script src="/ui/assets/vendor/basecoat-all.min.js" defer {}
                link rel="stylesheet" href="/ui/assets/vendor/uPlot.min.css?v=teal2";
                script src="/ui/assets/vendor/uPlot.iife.min.js?v=teal2" {}
                script type="module" src="/ui/assets/vendor/datastar.js" {}
            }
            body class="bg-background text-foreground antialiased" {
                // Sidebar collapse state lives in a single Datastar signal so the
                // header toggle and the aside width stay in sync.
                // #app-root is the persistent client-nav swap region: the
                // pjax router replaces its children on navigation, so the page
                // never does a full reload (no flash/blink). The collapse signal
                // lives here and survives swaps.
                div id="app-root" class="flex min-h-screen" data-signals="{collapsed: false}" {
                    (sidebar(active, history_unlocked, username))
                    div class="flex-1 min-w-0 flex flex-col" {
                        (header_bar(title, breadcrumb))
                        // Spacious: wide column, generous padding. `space-y-8`
                        // gives a calm vertical rhythm between page sections.
                        main id="app-main" class="flex-1 px-10 py-9 max-w-[1440px] w-full mx-auto" {
                            div class="space-y-8" { (body) }
                        }
                    }
                }
                script { (PreEscaped(PJAX_JS)) }
            }
        }
    }
}

// --------------------------------------------------------------------- sidebar

fn sidebar(active: Nav, history_unlocked: bool, username: Option<&str>) -> Markup {
    html! {
        aside
            // Static `w-[15.5rem]` is the pre-paint default so the sidebar has
            // its correct width before Datastar initialises — otherwise it sizes
            // to content then snaps to width on every page load (visible flash of
            // the nav dots/locks). `data-style` then drives only the collapse
            // toggle; its inline width wins over the class and matches the static
            // value when expanded, so there is no width delta to animate at load.
            // `sticky top-0 h-screen self-start`: pin the sidebar to the viewport
            // and bound it to exactly one screen tall. `self-start` stops the flex
            // row from stretching it to full document height — that bound is what
            // lets the inner `nav` (flex-1 overflow-y-auto) scroll on its own and
            // keeps the user footer fixed at the bottom, always visible.
            class="shrink-0 w-[15.5rem] sticky top-0 h-screen self-start border-r border-border bg-card flex flex-col transition-all duration-200"
            data-style="{width: $collapsed ? '4.25rem' : '15.5rem'}"
        {
            // Brand mark.
            div class="flex items-center gap-3 px-4 h-16 border-b border-border" {
                div class="brand-gradient shimmer size-9 shrink-0 rounded-xl grid place-items-center text-white font-bold text-lg shadow-lg" {
                    "d"
                }
                div data-show="!$collapsed" class="min-w-0" {
                    div class="font-semibold leading-tight brand-text text-[15px]" { "drgtw" }
                    div class="text-[11px] text-muted-foreground truncate" { "LLM privacy gateway" }
                }
            }

            nav class="flex-1 overflow-y-auto px-3 py-4 flex flex-col gap-5" {
                (nav_group("Operate", &[
                    (NavItem { href: "/ui", label: "Dashboard", icon: ICON_GAUGE, state: State::Live, active: active == Nav::Dashboard }),
                    (NavItem { href: "/ui/config", label: "Configuration", icon: ICON_SLIDERS, state: State::Live, active: active == Nav::Configuration }),
                    (NavItem { href: "/ui/connections", label: "Connections", icon: ICON_PLUG, state: State::Live, active: active == Nav::Connections }),
                    (NavItem { href: "/ui/keys", label: "Virtual Keys", icon: ICON_KEY, state: State::Live, active: active == Nav::VirtualKeys }),
                ], history_unlocked))

                (nav_group("Observe", &[
                    (NavItem { href: "/ui/analytics", label: "Analytics", icon: ICON_CHART, state: State::Postgres, active: active == Nav::Analytics }),
                    (NavItem { href: "/ui/traces", label: "Traces", icon: ICON_ROUTE, state: State::Postgres, active: active == Nav::Traces }),
                    (NavItem { href: "/ui/pii", label: "PII Insights", icon: ICON_SHIELD, state: State::Soon, active: active == Nav::PiiInsights }),
                    (NavItem { href: "/ui/audit", label: "Audit Log", icon: ICON_SCROLL, state: State::Postgres, active: active == Nav::AuditLog }),
                ], history_unlocked))

                (nav_group("Govern", &[
                    (NavItem { href: "/ui/budgets", label: "Cost & Budgets", icon: ICON_COINS, state: State::Soon, active: active == Nav::CostBudgets }),
                    (NavItem { href: "/ui/limits", label: "Rate Limits", icon: ICON_GAUGE2, state: State::Soon, active: active == Nav::RateLimits }),
                    (NavItem { href: "/ui/mcp", label: "MCP Servers", icon: ICON_SERVER, state: State::Soon, active: active == Nav::McpServers }),
                    (NavItem { href: "/ui/webhooks", label: "Webhooks", icon: ICON_WEBHOOK, state: State::Soon, active: active == Nav::Webhooks }),
                ], history_unlocked))

                (nav_group("Admin", &[
                    (NavItem { href: "/ui/team", label: "Team & Access", icon: ICON_USERS, state: State::Soon, active: active == Nav::TeamAccess }),
                    (NavItem { href: "/ui/settings", label: "Settings", icon: ICON_COG, state: State::Live, active: active == Nav::Settings }),
                ], history_unlocked))
            }

            (user_footer(username))
        }
    }
}

struct NavItem {
    href: &'static str,
    label: &'static str,
    icon: &'static str,
    state: State,
    active: bool,
}

fn nav_group(title: &str, items: &[NavItem], history_unlocked: bool) -> Markup {
    html! {
        div class="flex flex-col gap-0.5" {
            div data-show="!$collapsed" class="px-3 mb-1 text-[10.5px] font-semibold uppercase tracking-[0.12em] text-muted-foreground/70" {
                (title)
            }
            @for item in items {
                (nav_link(item, history_unlocked))
            }
        }
    }
}

fn nav_link(item: &NavItem, history_unlocked: bool) -> Markup {
    // Postgres-gated items unlock only when history is configured.
    let locked = matches!(item.state, State::Postgres) && !history_unlocked;

    let base = "group relative flex items-center gap-3 rounded-lg px-3 py-2 text-sm transition-colors";

    if locked {
        return html! {
            span
                class=(format!("{base} text-muted-foreground/50 cursor-not-allowed"))
                title="Requires PostgreSQL — configure [ui.history] in drgtw.toml"
            {
                span class="shrink-0 size-4 grid place-items-center" { (PreEscaped(item.icon)) }
                span data-show="!$collapsed" class="flex-1 min-w-0 truncate" { (item.label) }
                span data-show="!$collapsed" class="shrink-0 text-[11px]" aria-hidden="true" { "🔒" }
            }
        };
    }

    let cls = if item.active {
        format!("{base} nav-active text-foreground font-medium")
    } else {
        format!("{base} text-muted-foreground hover:text-foreground hover:bg-accent/60")
    };

    html! {
        a href=(item.href) class=(cls) {
            @if item.active { span class="nav-bar" {} }
            span class="shrink-0 size-4 grid place-items-center" { (PreEscaped(item.icon)) }
            span data-show="!$collapsed" class="flex-1 min-w-0 truncate" { (item.label) }
            @if !item.active {
                (state_dot(item.state))
            }
        }
    }
}

/// Tiny trailing glyph that hints at item state (only on inactive items).
fn state_dot(state: State) -> Markup {
    match state {
        State::Live => html! {
            span data-show="!$collapsed" title="Live" class="shrink-0 size-1.5 rounded-full" style="background: var(--ok)" {}
        },
        State::Soon => html! {
            span data-show="!$collapsed" title="Coming soon" class="shrink-0 text-[11px] text-muted-foreground/60" aria-hidden="true" { "◐" }
        },
        State::Postgres => html! {
            // Unlocked-but-not-active Postgres item: neutral marker.
            span data-show="!$collapsed" title="Requires PostgreSQL" class="shrink-0 size-1.5 rounded-full" style="background: var(--warn)" {}
        },
    }
}

fn user_footer(username: Option<&str>) -> Markup {
    // Self-contained dropdown driven by a local Datastar `menu` signal — avoids
    // Basecoat's dropdown component contract (which expects a specific trigger
    // structure). Click-away closes it.
    let display_name = username.unwrap_or("Operator");
    // Avatar initials: first two uppercase chars of the display name.
    let initials: String = display_name
        .chars()
        .filter(|c| c.is_alphabetic())
        .take(2)
        .map(|c| c.to_ascii_uppercase())
        .collect();
    let initials = if initials.is_empty() { "OP".to_owned() } else { initials };
    let signed_in_label = username
        .map(|u| format!("Signed in as {u}"))
        .unwrap_or_else(|| "Signed in as operator".to_owned());

    html! {
        div class="border-t border-border p-3" {
            div class="relative" data-signals="{menu: false}" data-on:click__outside="$menu = false" {
                button type="button"
                    class="flex w-full items-center gap-3 rounded-lg px-2 py-2 hover:bg-accent/60 transition-colors text-left"
                    aria-haspopup="menu"
                    data-on:click="evt.stopPropagation(); $menu = !$menu"
                {
                    div class="brand-gradient size-8 shrink-0 rounded-full grid place-items-center text-white text-xs font-semibold" { (initials) }
                    div data-show="!$collapsed" class="min-w-0 flex-1" {
                        div class="text-sm font-medium truncate" { (display_name) }
                        div class="text-[11px] text-muted-foreground truncate" { "Operator" }
                    }
                    span data-show="!$collapsed" class="shrink-0 text-muted-foreground" { (PreEscaped(ICON_CHEVRON_UPDOWN)) }
                }
                div data-show="$menu" style="display:none" class="absolute bottom-full mb-2 left-0 w-56 glass rounded-lg p-1 z-20" role="menu" {
                    div class="px-3 py-2 text-xs text-muted-foreground" { (signed_in_label) }
                    div class="h-px bg-border my-1" {}
                    a href="/ui/settings" role="menuitem" class="w-full flex items-center gap-2 rounded-md px-3 py-2 text-sm hover:bg-accent/60 transition-colors" {
                        span class="size-4 grid place-items-center" { (PreEscaped(ICON_COG)) } "Settings"
                    }
                    div class="h-px bg-border my-1" {}
                    // Logout: POST /ui/logout (form submit so it works without JS too).
                    form method="post" action="/ui/logout" {
                        button type="submit" role="menuitem" class="w-full flex items-center gap-2 rounded-md px-3 py-2 text-sm text-muted-foreground hover:bg-accent/60 transition-colors" {
                            span class="size-4 grid place-items-center" { (PreEscaped(ICON_LOGOUT)) } "Sign out"
                        }
                    }
                }
            }
        }
    }
}

// ------------------------------------------------------------------ header bar

fn header_bar(title: &str, breadcrumb: &str) -> Markup {
    html! {
        header class="sticky top-0 z-10 h-16 shrink-0 border-b border-border bg-background flex items-center gap-4 px-6" {
            button type="button"
                class="shrink-0 size-8 grid place-items-center rounded-md text-muted-foreground hover:text-foreground hover:bg-accent/60 transition-colors"
                data-on:click="$collapsed = !$collapsed"
                title="Toggle sidebar"
            {
                (PreEscaped(ICON_SIDEBAR))
            }
            div class="min-w-0" {
                div class="text-[11px] text-muted-foreground flex items-center gap-1.5" {
                    span { "drgtw" }
                    span class="opacity-50" { "/" }
                    span { (breadcrumb) }
                }
                h1 class="text-[15px] font-semibold leading-tight truncate" { (title) }
            }

            div class="flex-1" {}

            // Decorative command-palette search pill.
            button type="button" class="search-pill hidden md:flex items-center gap-2 rounded-lg px-3 py-1.5 text-sm text-muted-foreground" {
                span class="size-4 grid place-items-center" { (PreEscaped(ICON_SEARCH)) }
                span { "Search…" }
                span class="kbd ml-6" { "⌘K" }
            }

            // Live status pill.
            div class="flex items-center gap-2 rounded-full badge-ok px-3 py-1.5 text-xs font-medium" {
                span class="live-dot" {}
                span { "Operational" }
            }
        }
    }
}

// --------------------------------------------------------------- UI primitives

/// In-page header (title + subtitle), used at the top of each page body.
pub fn page_header(title: &str, subtitle: &str) -> Markup {
    html! {
        div class="mb-8" {
            h2 class="font-display text-[2rem] leading-tight tracking-tight" { (title) }
            p class="text-sm text-muted-foreground mt-1.5" { (subtitle) }
        }
    }
}

/// A status badge. `kind` ∈ {"ok","warn","down","muted","brand"}.
pub fn badge(kind: &str, label: &str) -> Markup {
    let cls = match kind {
        "ok" => "badge-ok",
        "warn" => "badge-warn",
        "down" => "badge-down",
        "brand" => "badge-brand",
        _ => "badge-muted",
    };
    html! {
        span class=(format!("inline-flex items-center gap-1.5 rounded-full px-2.5 py-0.5 text-[11px] font-medium {cls}")) {
            @if kind == "ok" { span class="live-dot" {} }
            (label)
        }
    }
}

/// Centered empty-state card for "coming soon" / "requires Postgres" pages.
pub fn empty_state(icon: &str, badge_kind: &str, badge_label: &str, title: &str, body: Markup) -> Markup {
    html! {
        div class="grid mx-auto max-w-xl" {
          div class="glass lift text-center px-8 py-14" {
            div class="icon-orb mx-auto size-16 rounded-2xl grid place-items-center text-primary mb-5" {
                span class="size-7 grid place-items-center" { (PreEscaped(icon)) }
            }
            div class="mb-4" { (badge(badge_kind, badge_label)) }
            h3 class="text-lg font-semibold mb-2" { (title) }
            div class="text-sm text-muted-foreground leading-relaxed" { (body) }
          }
        }
    }
}

// --------------------------------------------------------------------- scripts

/// Inlined in `<head>` so it applies before `app.css` arrives: paints the
/// document background in the dark theme colour immediately, so a navigation
/// never shows a white UA frame before the stylesheet loads.
const PREPAINT_CSS: &str = "html{background:oklch(0.135 0 0)}";

/// Client-side navigation ("pjax"): intercept same-origin `/ui/*` link clicks,
/// fetch the target, and swap only `#app-root`'s children — the document, its
/// stylesheet, fonts and the chart libs are never reloaded, so navigation has
/// no full-page teardown and therefore no flash/blink. Uses the View
/// Transitions API for a crossfade where supported (instant swap otherwise —
/// still blink-free, since the swap is a single synchronous DOM replacement).
///
/// Page-end scripts (chart bootstraps) register teardown fns on
/// `window.__drgtwCleanup`; the router runs them before each swap so timers /
/// ResizeObservers don't leak across navigations. Parsed `<script>` nodes are
/// inert, so the router re-creates them to execute in document order.
const PJAX_JS: &str = r##"
(function () {
  if (window.__drgtwPjax) return;
  window.__drgtwPjax = true;
  window.__drgtwCleanup = window.__drgtwCleanup || [];

  function cleanup() {
    var fns = window.__drgtwCleanup || [];
    for (var i = 0; i < fns.length; i++) { try { fns[i](); } catch (e) {} }
    window.__drgtwCleanup = [];
  }
  function runScripts(scope) {
    scope.querySelectorAll('script').forEach(function (old) {
      var s = document.createElement('script');
      for (var i = 0; i < old.attributes.length; i++) {
        s.setAttribute(old.attributes[i].name, old.attributes[i].value);
      }
      s.textContent = old.textContent;
      old.parentNode.replaceChild(s, old);
    });
  }
  function apply(doc) {
    var next = doc.getElementById('app-root');
    var cur = document.getElementById('app-root');
    if (!next || !cur) return false;
    cleanup();
    cur.replaceChildren.apply(cur, Array.prototype.slice.call(next.childNodes));
    if (doc.title) document.title = doc.title;
    var main = document.getElementById('app-main') || cur;
    runScripts(main);
    window.scrollTo(0, 0);
    return true;
  }
  function navigate(url, push) {
    fetch(url, { headers: { 'X-Requested-With': 'pjax' }, credentials: 'same-origin' })
      .then(function (r) {
        var ct = r.headers.get('content-type') || '';
        if (!r.ok || ct.indexOf('text/html') === -1) throw 0;
        return r.text();
      })
      .then(function (html) {
        var doc = new DOMParser().parseFromString(html, 'text/html');
        var run = function () { if (!apply(doc)) { location.href = url; } };
        if (push) history.pushState({ pjax: 1 }, '', url);
        if (document.startViewTransition) document.startViewTransition(run); else run();
      })
      .catch(function () { location.href = url; });
  }

  document.addEventListener('click', function (e) {
    if (e.defaultPrevented || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
    var a = e.target.closest ? e.target.closest('a') : null;
    if (!a) return;
    if (a.target || a.hasAttribute('download') || a.getAttribute('rel') === 'external') return;
    if (a.origin !== location.origin) return;
    if (a.pathname.indexOf('/ui') !== 0) return;        // only in-app routes
    if (a.pathname === location.pathname && !a.hash) { e.preventDefault(); return; }
    if (a.hash && a.pathname === location.pathname) return; // let in-page anchors work
    e.preventDefault();
    navigate(a.href, true);
  });
  window.addEventListener('popstate', function () { navigate(location.href, false); });
})();
"##;

// ----------------------------------------------------------------------- icons
// Inline lucide SVGs (https://lucide.dev), MIT. Stroke = currentColor so they
// inherit text color. Kept as &str constants to avoid per-render allocation.

pub const ICON_GAUGE: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m12 14 4-4"/><path d="M3.34 19a10 10 0 1 1 17.32 0"/></svg>"#;
pub const ICON_GAUGE2: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 14v-4"/><path d="M10 8h4"/><circle cx="12" cy="14" r="8"/><path d="M12 2v2"/></svg>"#;
pub const ICON_SLIDERS: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="21" x2="14" y1="4" y2="4"/><line x1="10" x2="3" y1="4" y2="4"/><line x1="21" x2="12" y1="12" y2="12"/><line x1="8" x2="3" y1="12" y2="12"/><line x1="21" x2="16" y1="20" y2="20"/><line x1="12" x2="3" y1="20" y2="20"/><line x1="14" x2="14" y1="2" y2="6"/><line x1="8" x2="8" y1="10" y2="14"/><line x1="16" x2="16" y1="18" y2="22"/></svg>"#;
pub const ICON_PLUG: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 22v-5"/><path d="M9 8V2"/><path d="M15 8V2"/><path d="M18 8v5a4 4 0 0 1-4 4h-4a4 4 0 0 1-4-4V8Z"/></svg>"#;
pub const ICON_KEY: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m15.5 7.5 2.3 2.3a1 1 0 0 0 1.4 0l2.1-2.1a1 1 0 0 0 0-1.4L19 4"/><path d="m21 2-9.6 9.6"/><circle cx="7.5" cy="15.5" r="5.5"/></svg>"#;
pub const ICON_CHART: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 3v16a2 2 0 0 0 2 2h16"/><path d="m19 9-5 5-4-4-3 3"/></svg>"#;
pub const ICON_ROUTE: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="6" cy="19" r="3"/><path d="M9 19h8.5a3.5 3.5 0 0 0 0-7h-11a3.5 3.5 0 0 1 0-7H15"/><circle cx="18" cy="5" r="3"/></svg>"#;
pub const ICON_SHIELD: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M20 13c0 5-3.5 7.5-7.66 8.95a1 1 0 0 1-.67-.01C7.5 20.5 4 18 4 13V6a1 1 0 0 1 1-1c2 0 4.5-1.2 6.24-2.72a1.17 1.17 0 0 1 1.52 0C14.51 3.81 17 5 19 5a1 1 0 0 1 1 1z"/></svg>"#;
pub const ICON_SCROLL: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M19 17V5a2 2 0 0 0-2-2H4"/><path d="M8 21h12a2 2 0 0 0 2-2v-1a1 1 0 0 0-1-1H11a1 1 0 0 0-1 1v1a2 2 0 1 1-4 0V5a2 2 0 1 0-4 0v2a1 1 0 0 0 1 1h3"/></svg>"#;
pub const ICON_COINS: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="8" cy="8" r="6"/><path d="M18.09 10.37A6 6 0 1 1 10.34 18"/><path d="M7 6h1v4"/><path d="m16.71 13.88.7.71-2.82 2.82"/></svg>"#;
pub const ICON_SERVER: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect width="20" height="8" x="2" y="2" rx="2"/><rect width="20" height="8" x="2" y="14" rx="2"/><line x1="6" x2="6.01" y1="6" y2="6"/><line x1="6" x2="6.01" y1="18" y2="18"/></svg>"#;
pub const ICON_WEBHOOK: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M18 16.98h-5.99c-1.1 0-1.95.94-2.48 1.9A4 4 0 0 1 2 17c.01-.7.2-1.4.57-2"/><path d="m6 17 3.13-5.78c.53-.97.1-2.18-.5-3.1a4 4 0 1 1 6.89-4.06"/><path d="m12 6 3.13 5.73C15.66 12.7 16.9 13 18 13a4 4 0 0 1 0 8"/></svg>"#;
pub const ICON_USERS: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2"/><circle cx="9" cy="7" r="4"/><path d="M22 21v-2a4 4 0 0 0-3-3.87"/><path d="M16 3.13a4 4 0 0 1 0 7.75"/></svg>"#;
pub const ICON_COG: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.51a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.39a2 2 0 0 0-.73-2.73l-.15-.08a2 2 0 0 1-1-1.74v-.5a2 2 0 0 1 1-1.74l.15-.09a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2z"/><circle cx="12" cy="12" r="3"/></svg>"#;
pub const ICON_SIDEBAR: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect width="18" height="18" x="3" y="3" rx="2"/><path d="M9 3v18"/></svg>"#;
pub const ICON_SEARCH: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="11" cy="11" r="8"/><path d="m21 21-4.3-4.3"/></svg>"#;
pub const ICON_CHEVRON_UPDOWN: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m7 15 5 5 5-5"/><path d="m7 9 5-5 5 5"/></svg>"#;
pub const ICON_SUN_MOON: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 8a2.83 2.83 0 0 0 4 4 4 4 0 1 1-4-4"/><path d="M12 2v2"/><path d="M12 20v2"/><path d="m4.9 4.9 1.4 1.4"/><path d="m17.7 17.7 1.4 1.4"/><path d="M2 12h2"/><path d="M20 12h2"/><path d="m6.3 17.7-1.4 1.4"/><path d="m19.1 4.9-1.4 1.4"/></svg>"#;
pub const ICON_LOGOUT: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4"/><polyline points="16 17 21 12 16 7"/><line x1="21" x2="9" y1="12" y2="12"/></svg>"#;
pub const ICON_DATABASE: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><ellipse cx="12" cy="5" rx="9" ry="3"/><path d="M3 5V19A9 3 0 0 0 21 19V5"/><path d="M3 12A9 3 0 0 0 21 12"/></svg>"#;
