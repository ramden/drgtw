//! `GET /ui` — the dashboard. The one page with the most life: hero metric
//! cards, an interactive Chart.js area chart (range toggle via the
//! `/ui/api/timeseries` JSON API, theme-aware, line draw on mount), and a
//! recent-requests glass table.
//!
//! All numbers are real, pulled from the Postgres history store via
//! [`crate::UiState::history`]. When no store is connected (or a query errors)
//! the page renders zero/empty state — never a 500.

use axum::extract::State;
use axum::response::Html;
use maud::{Markup, PreEscaped, html};

use drgtw_history::UsageSummary;

use crate::pages::{
    fmt_cost, fmt_int, fmt_latency, fmt_ts, status_kind, timeseries_json,
};
use crate::layout::{self, Nav, badge, page_header, shell};
use crate::{UiState, range_window};

/// Async handler: queries the last-24h summary, the 24h/Hour timeseries (for
/// the chart's initial render), and the 8 most recent requests.
pub async fn dashboard(State(state): State<UiState>) -> Html<String> {
    let (since, until, bucket) = range_window("24h");

    let (summary, traffic_json, recent) = match state.history() {
        Some(h) => {
            let summary = h.usage_summary(since, until).await.unwrap_or_else(|_| zero_summary());
            let buckets = h.usage_timeseries(since, until, bucket).await.unwrap_or_default();
            let traffic_json = timeseries_json(&buckets, bucket).to_string();
            let recent = h.recent_usage(8).await.unwrap_or_default();
            (summary, traffic_json, recent)
        }
        None => (zero_summary(), timeseries_json(&[], bucket).to_string(), Vec::new()),
    };

    Html(render(&state, &summary, &traffic_json, &recent).into_string())
}

fn zero_summary() -> UsageSummary {
    UsageSummary {
        requests: 0,
        input_tokens: 0,
        output_tokens: 0,
        cost_usd: 0.0,
        avg_latency_ms: 0.0,
        pii_count: 0,
        error_count: 0,
    }
}

fn render(
    state: &UiState,
    s: &UsageSummary,
    traffic_json: &str,
    recent: &[drgtw_events::UsageEvent],
) -> Markup {
    let cfg = &state.config;
    let unlocked = cfg.ui.history.is_some();
    let model_count: usize = cfg.connections.iter().map(|c| c.models.len()).sum();
    let total_tokens = s.input_tokens + s.output_tokens;

    let body = html! {
        (page_header("Dashboard", "Live gateway status and traffic at a glance."))

        // Server-rendered initial chart series. The Chart.js bootstrap consumes
        // this on mount; the range toggle refetches from /ui/api/timeseries.
        script { (PreEscaped(format!("window.__traffic = {traffic_json};"))) }

        // Datastar: `uptime` ticks from /ui/events; the chart hook reads it to
        // nudge the latest data point so the graph feels alive.
        // `space-y-6` here (not just on the shell wrapper) because all the
        // dashboard sections live INSIDE this Datastar SSE container — the
        // shell's spacer only reaches this div, not its children.
        div data-signals="{uptime: '0s'}" data-init="@get('/ui/events')" class="space-y-6" {
            // Hidden probe bound to the live `uptime` signal patched by the SSE
            // stream; the chart reads it to tick the latest point in real time.
            span id="uptimeProbe" class="sr-only" data-text="$uptime" {}

            // --- metric cards (last 24h) — one uniform grid: 2-up on mobile,
            // 3-up on desktop, all cells equal size (no half-width voids).
            div class="grid grid-cols-2 lg:grid-cols-3 gap-4" {
                (metric_card(1, layout::ICON_BOLT, "Requests · 24h", &fmt_int(s.requests), "last 24h"))
                (metric_card(2, layout::ICON_TOKENS, "Tokens processed", &fmt_int(total_tokens), "in + out, last 24h"))
                (metric_card(3, layout::ICON_COINS, "Cost · 24h", &fmt_cost(s.cost_usd), "estimated spend"))
                (metric_card(4, layout::ICON_GAUGE, "Avg latency", &fmt_latency(s.avg_latency_ms), "upstream round-trip"))
                (metric_card(5, layout::ICON_SHIELD, "PII entities redacted", &fmt_int(s.pii_count), "across all connections, 24h"))
                (metric_card(6, layout::ICON_GAUGE2, "Errors · 24h", &fmt_int(s.error_count), "non-2xx responses"))
            }

            // --- traffic chart ---
            div class="rise grid" style="--i:7" {
              div class="glass lift p-5 min-w-0" {
                div class="flex flex-wrap items-center justify-between gap-3 mb-4" {
                    div {
                        h3 class="text-base font-semibold" { "Gateway traffic" }
                        p class="text-xs text-muted-foreground mt-0.5" { "Requests per interval" }
                    }
                    // Segmented range control (Datastar signal drives the chart).
                    div data-signals="{range: '24h'}" class="inline-flex rounded-lg border border-border bg-card/40 p-0.5 text-xs" {
                        (range_btn("24h"))
                        (range_btn("7d"))
                        (range_btn("30d"))
                    }
                }
                div class="relative h-[280px] min-w-0" {
                    canvas id="trafficChart" {}
                }
              }
            }

            // --- inventory strip ---
            div class="grid grid-cols-2 sm:grid-cols-4 gap-4" {
                (count_card(8, "Connections", cfg.connections.len(), "/ui/connections"))
                (count_card(9, "Virtual keys", cfg.virtual_keys.len(), "/ui/keys"))
                (count_card(10, "Models", model_count, "/ui/connections"))
                (count_card(11, "MCP servers", cfg.mcp_servers.len(), "/ui/mcp"))
            }

            // --- recent requests ---
            div class="rise grid" style="--i:12" {
              div class="glass overflow-hidden" {
                div class="flex items-center justify-between px-5 py-3.5 border-b border-border" {
                    h3 class="text-base font-semibold" { "Recent requests" }
                    a href="/ui/traces" class="text-xs text-primary hover:underline" { "View all traces →" }
                }
                div class="overflow-x-auto" {
                    table class="w-full text-sm" {
                        thead {
                            tr class="text-left text-[11px] uppercase tracking-wide text-muted-foreground border-b border-border" {
                                th class="font-medium px-5 py-2.5" { "Time" }
                                th class="font-medium px-5 py-2.5" { "Model" }
                                th class="font-medium px-5 py-2.5" { "Connection" }
                                th class="font-medium px-5 py-2.5 text-right" { "Tokens" }
                                th class="font-medium px-5 py-2.5 text-right" { "Latency" }
                                th class="font-medium px-5 py-2.5" { "PII" }
                                th class="font-medium px-5 py-2.5" { "Status" }
                            }
                        }
                        tbody {
                            @if recent.is_empty() {
                                tr {
                                    td class="px-5 py-6 text-center text-sm text-muted-foreground" colspan="7" {
                                        "No requests recorded yet."
                                    }
                                }
                            } @else {
                                @for ev in recent {
                                    (req_row(ev))
                                }
                            }
                        }
                    }
                }
              }
            }
        }

        // Chart.js bootstrap. Reads colors from CSS vars via getComputedStyle so
        // it tracks the theme; consumes the server-rendered `window.__traffic`
        // series; refetches from /ui/api/timeseries on range change; animates the
        // line draw on mount; ticks the latest point off the live `uptime` signal.
        script { (PreEscaped(CHART_JS)) }
    };

    shell("Dashboard", "Dashboard", Nav::Dashboard, unlocked, cfg.ui.auth.as_ref().map(|a| a.username.as_str()), body)
}

fn metric_card(i: usize, icon: &str, label: &str, value: &str, caption: &str) -> Markup {
    html! {
        div class="rise grid" style=(format!("--i:{i}")) {
          div class="glass glass-metric lift p-5" {
            div class="flex items-center justify-between mb-3" {
                div class="size-9 rounded-lg icon-orb grid place-items-center text-primary" {
                    span class="size-5 grid place-items-center" { (PreEscaped(icon)) }
                }
            }
            div class="text-3xl font-semibold stat-gradient leading-none" { (value) }
            div class="text-sm font-medium mt-2" { (label) }
            div class="text-xs text-muted-foreground mt-0.5" { (caption) }
          }
        }
    }
}

fn count_card(i: usize, label: &str, n: usize, href: &str) -> Markup {
    html! {
        div class="rise grid" style=(format!("--i:{i}")) {
          a href=(href) class="glass lift p-5 block" {
            div class="text-2xl font-semibold tnum leading-none" { (n) }
            div class="text-sm text-muted-foreground mt-1.5" { (label) }
          }
        }
    }
}

fn range_btn(label: &str) -> Markup {
    let data_class = format!(
        "{{'btn-brand text-white': $range === '{label}', 'text-muted-foreground hover:text-foreground': $range !== '{label}'}}"
    );
    let on_click = format!("$range = '{label}'");
    html! {
        button type="button"
            class="px-2.5 py-1 rounded-md transition-colors"
            data-class=(data_class)
            data-on:click=(on_click)
        { (label) }
    }
}

fn req_row(ev: &drgtw_events::UsageEvent) -> Markup {
    let tokens = ev.input_tokens.unwrap_or(0) + ev.output_tokens.unwrap_or(0);
    let pii = if ev.pii { "yes" } else { "—" };
    html! {
        tr class="row-lift border-b border-border/50 last:border-0" {
            td class="px-5 py-2.5 font-mono text-[12.5px] text-muted-foreground" { (fmt_ts(ev.ts_unix_ms as i64)) }
            td class="px-5 py-2.5 font-mono text-[12.5px]" { (ev.model) }
            td class="px-5 py-2.5 font-mono text-[12.5px] text-muted-foreground" { (ev.connection) }
            td class="px-5 py-2.5 text-right tnum" { (fmt_int(tokens as i64)) }
            td class="px-5 py-2.5 text-right tnum text-muted-foreground" { (fmt_latency(ev.latency_ms as f64)) }
            td class="px-5 py-2.5 tnum text-muted-foreground" { (pii) }
            td class="px-5 py-2.5" { (badge(status_kind(ev.status), &ev.status.to_string())) }
        }
    }
}

const CHART_JS: &str = r##"
(function () {
  var el = document.getElementById('trafficChart');
  if (!el || typeof Chart === 'undefined') return;

  function cssVar(name) {
    return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  }
  // Resolve any CSS color (incl. oklch(), which modern browsers report
  // verbatim from getComputedStyle) into a concrete "rgb(r,g,b)" string by
  // painting it on a 1px canvas and reading the pixel back. Without this the
  // rgba() parser below mistakes oklch's "0.62 0.21 280" for r,g,b and clamps
  // 280 -> 255, turning the accent and tick labels blue.
  function resolve(color) {
    var c = document.createElement('canvas');
    c.width = 1; c.height = 1;
    var x = c.getContext('2d');
    x.fillStyle = '#000';
    x.fillStyle = color;          // ignored if the browser can't parse it
    x.fillRect(0, 0, 1, 1);
    var d = x.getImageData(0, 0, 1, 1).data;
    return 'rgb(' + d[0] + ',' + d[1] + ',' + d[2] + ')';
  }
  function rgba(rgb, a) {
    var m = rgb.match(/\d+(\.\d+)?/g);
    if (!m) return rgb;
    return 'rgba(' + m[0] + ',' + m[1] + ',' + m[2] + ',' + a + ')';
  }

  // Initial series is server-rendered into window.__traffic; range toggles
  // refetch the JSON API and rebuild.
  var data = (window.__traffic && window.__traffic.labels) ? window.__traffic
           : { labels: [], requests: [] };
  var range = '24h';
  var ctx = el.getContext('2d');

  function gradient() {
    var accent = resolve(cssVar('--primary') || '#7c5cff');
    var g = ctx.createLinearGradient(0, 0, 0, el.height || 280);
    g.addColorStop(0, rgba(accent, 0.38));
    g.addColorStop(1, rgba(accent, 0.0));
    return g;
  }

  function build() {
    var accent = resolve(cssVar('--primary') || '#7c5cff');
    var grid = rgba(resolve(cssVar('--foreground') || '#fff'), 0.08);
    var tick = rgba(resolve(cssVar('--muted-foreground') || '#aaa'), 0.95);
    return new Chart(ctx, {
      type: 'line',
      data: {
        labels: data.labels || [],
        datasets: [{
          label: 'Requests',
          data: data.requests || [],
          borderColor: accent,
          borderWidth: 2,
          fill: true,
          backgroundColor: gradient(),
          tension: 0.4,
          pointRadius: 0,
          pointHoverRadius: 5,
          pointHoverBackgroundColor: accent,
          pointHoverBorderColor: '#fff',
          pointHoverBorderWidth: 2
        }]
      },
      options: {
        responsive: true,
        maintainAspectRatio: false,
        animation: { duration: 900, easing: 'easeOutCubic' },
        // Headroom so the area never touches the card's top edge.
        layout: { padding: { top: 8 } },
        interaction: { intersect: false, mode: 'index' },
        plugins: {
          legend: { display: false },
          tooltip: {
            backgroundColor: rgba(resolve(cssVar('--card') || '#1c1c22'), 0.95),
            borderColor: rgba(accent, 0.4), borderWidth: 1,
            titleColor: tick, bodyColor: resolve(cssVar('--foreground') || '#fff'),
            padding: 10, displayColors: false,
            callbacks: { label: function (c) { return c.parsed.y.toLocaleString() + ' requests'; } }
          }
        },
        scales: {
          x: { grid: { display: false }, ticks: { color: tick, maxTicksLimit: 8, font: { size: 11 } }, border: { display: false } },
          y: { beginAtZero: true, grace: '8%', grid: { color: grid }, ticks: { color: tick, font: { size: 11 }, precision: 0, maxTicksLimit: 6, callback: function (v) { return v >= 1000 ? (v / 1000) + 'k' : v; } }, border: { display: false } }
        }
      }
    });
  }

  var chart = build();

  function rebuild() { chart.destroy(); chart = build(); }

  function load(r) {
    fetch('/ui/api/timeseries?range=' + encodeURIComponent(r), { headers: { 'Accept': 'application/json' } })
      .then(function (res) { return res.ok ? res.json() : null; })
      .then(function (json) { if (json) { data = json; rebuild(); } })
      .catch(function () {});
  }

  // Range toggle: Datastar drives the `btn-brand` class onto the active range
  // button via data-class. Watch which segmented button is active (class +
  // label text) and refetch when it changes.
  var rangeButtons = ['24h', '7d', '30d'];
  setInterval(function () {
    var picked = null;
    document.querySelectorAll('button').forEach(function (b) {
      var t = b.textContent.trim();
      if (rangeButtons.indexOf(t) !== -1 && b.classList.contains('btn-brand')) picked = t;
    });
    if (picked && picked !== range) { range = picked; load(range); }
  }, 250);

  // (Removed the synthetic random "live tail" — it nudged the last point every
  // second with Math.random(), making the chart jump constantly and showing
  // fake data. The chart now reflects only real timeseries from the range load.)

  // Re-theme the chart when the light/dark toggle fires.
  window.addEventListener('drgtw-theme-change', function () { rebuild(); });
})();
"##;
