//! `GET /ui` — the dashboard. The one page with the most life: four hero metric
//! cards, an interactive Chart.js area chart (range toggle, theme-aware, line
//! draw on mount, latest point ticks off the existing /events SSE), and a
//! recent-requests glass table.
//!
//! All numbers are synthesized and clearly labeled "demo data" — the gateway
//! ships no historical store in this concept.

use maud::{Markup, PreEscaped, html};

use crate::UiState;
use crate::layout::{self, Nav, badge, page_header, shell};

pub fn dashboard(state: &UiState) -> Markup {
    let cfg = &state.config;
    let unlocked = cfg.ui.history.is_some();
    let model_count: usize = cfg.connections.iter().map(|c| c.models.len()).sum();

    let body = html! {
        (page_header("Dashboard", "Live gateway status and traffic at a glance."))

        // Datastar: `uptime` ticks from /ui/events; the chart hook reads it to
        // nudge the latest data point so the graph feels alive.
        div data-signals="{uptime: '0s'}" data-init="@get('/ui/events')" {
            // Hidden probe bound to the live `uptime` signal patched by the SSE
            // stream; the chart reads it to tick the latest point in real time.
            span id="uptimeProbe" class="sr-only" data-text="$uptime" {}

            // --- hero metric cards ---
            div class="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-4 gap-4 mb-6" {
                (metric_card(1, layout::ICON_BOLT, "Requests · 24h", "48,213", "▲ 12.4%", true, "vs. previous day"))
                (metric_card(2, layout::ICON_TOKENS, "Tokens processed", "31.8M", "▲ 8.1%", true, "in + out, last 24h"))
                (metric_card(3, layout::ICON_SHIELD, "PII entities redacted", "9,742", "▲ 3.6%", true, "across all connections"))
                (metric_card(4, layout::ICON_GAUGE, "Avg latency", "412ms", "▼ 5.2%", true, "p50 upstream round-trip"))
            }

            // --- traffic chart ---
            div class="glass lift rise p-5 mb-6" style="--i:5" {
                div class="flex flex-wrap items-center justify-between gap-3 mb-4" {
                    div {
                        div class="flex items-center gap-2" {
                            h3 class="text-base font-semibold" { "Gateway traffic" }
                            (badge("muted", "demo data"))
                        }
                        p class="text-xs text-muted-foreground mt-0.5" { "Requests per interval · live tail" }
                    }
                    // Segmented range control (Datastar signal drives the chart).
                    div data-signals="{range: '24h'}" class="inline-flex rounded-lg border border-border bg-card/40 p-0.5 text-xs" {
                        (range_btn("24h"))
                        (range_btn("7d"))
                        (range_btn("30d"))
                    }
                }
                div class="relative h-[280px]" {
                    canvas id="trafficChart" {}
                }
            }

            // --- inventory strip ---
            div class="grid grid-cols-2 sm:grid-cols-4 gap-4 mb-6" {
                (count_card(6, "Connections", cfg.connections.len(), "/ui/connections"))
                (count_card(7, "Virtual keys", cfg.virtual_keys.len(), "/ui/keys"))
                (count_card(8, "Models", model_count, "/ui/connections"))
                (count_card(9, "MCP servers", cfg.mcp_servers.len(), "/ui/mcp"))
            }

            // --- recent requests ---
            div class="glass rise overflow-hidden" style="--i:10" {
                div class="flex items-center justify-between px-5 py-3.5 border-b border-border" {
                    div class="flex items-center gap-2" {
                        h3 class="text-base font-semibold" { "Recent requests" }
                        (badge("muted", "demo data"))
                    }
                    div class="text-xs text-muted-foreground flex items-center gap-2" {
                        span class="live-dot" {} "streaming"
                    }
                }
                div class="overflow-x-auto" {
                    table class="w-full text-sm" {
                        thead {
                            tr class="text-left text-[11px] uppercase tracking-wide text-muted-foreground border-b border-border" {
                                th class="font-medium px-5 py-2.5" { "Time" }
                                th class="font-medium px-5 py-2.5" { "Virtual key" }
                                th class="font-medium px-5 py-2.5" { "Model" }
                                th class="font-medium px-5 py-2.5 text-right" { "Tokens" }
                                th class="font-medium px-5 py-2.5 text-right" { "Latency" }
                                th class="font-medium px-5 py-2.5" { "PII" }
                                th class="font-medium px-5 py-2.5" { "Status" }
                            }
                        }
                        tbody {
                            (req_row("12:04:51", "sk-…001", "gpt-4o", "1,284", "388ms", "3", "ok", "200"))
                            (req_row("12:04:47", "sk-…002", "claude-sonnet-4-5", "2,019", "511ms", "0", "ok", "200"))
                            (req_row("12:04:39", "sk-…001", "gpt-4o-mini", "642", "201ms", "1", "ok", "200"))
                            (req_row("12:04:30", "sk-…002", "claude-opus-4-5", "3,771", "904ms", "5", "warn", "200"))
                            (req_row("12:04:18", "sk-…001", "gpt-4o", "0", "44ms", "—", "down", "429"))
                            (req_row("12:04:02", "sk-…001", "claude-sonnet-4-5", "1,506", "473ms", "2", "ok", "200"))
                        }
                    }
                }
            }
        }

        // Chart.js bootstrap. Reads colors from CSS vars via getComputedStyle so
        // it tracks the theme; synthesizes the series per range; animates the
        // line draw on mount; ticks the latest point off the live `uptime` signal.
        script { (PreEscaped(CHART_JS)) }
    };

    shell("Dashboard", "Dashboard", Nav::Dashboard, unlocked, cfg.ui.auth.as_ref().map(|a| a.username.as_str()), body)
}

fn metric_card(i: usize, icon: &str, label: &str, value: &str, trend: &str, up: bool, caption: &str) -> Markup {
    let trend_cls = if up { "badge-ok" } else { "badge-down" };
    html! {
        div class="glass glass-metric lift rise p-5" style=(format!("--i:{i}")) {
            div class="flex items-center justify-between mb-3" {
                div class="size-9 rounded-lg icon-orb grid place-items-center text-primary" {
                    span class="size-5 grid place-items-center" { (PreEscaped(icon)) }
                }
                span class=(format!("inline-flex items-center rounded-full px-2 py-0.5 text-[11px] font-medium {trend_cls}")) { (trend) }
            }
            div class="text-3xl font-semibold stat-gradient leading-none" { (value) }
            div class="text-sm font-medium mt-2" { (label) }
            div class="text-xs text-muted-foreground mt-0.5" { (caption) }
        }
    }
}

fn count_card(i: usize, label: &str, n: usize, href: &str) -> Markup {
    html! {
        a href=(href) class="glass lift rise p-5 block" style=(format!("--i:{i}")) {
            div class="text-2xl font-semibold tnum leading-none" { (n) }
            div class="text-sm text-muted-foreground mt-1.5" { (label) }
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

#[allow(clippy::too_many_arguments)]
fn req_row(time: &str, key: &str, model: &str, tokens: &str, latency: &str, pii: &str, status_kind: &str, status: &str) -> Markup {
    html! {
        tr class="row-lift border-b border-border/50 last:border-0" {
            td class="px-5 py-2.5 font-mono text-[12.5px] text-muted-foreground" { (time) }
            td class="px-5 py-2.5 font-mono text-[12.5px]" { (key) }
            td class="px-5 py-2.5 font-mono text-[12.5px]" { (model) }
            td class="px-5 py-2.5 text-right tnum" { (tokens) }
            td class="px-5 py-2.5 text-right tnum text-muted-foreground" { (latency) }
            td class="px-5 py-2.5 tnum text-muted-foreground" { (pii) }
            td class="px-5 py-2.5" { (badge(status_kind, status)) }
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

  // Deterministic pseudo-random so the demo series is stable across reloads.
  function series(n, seed, base, amp) {
    var out = [], s = seed;
    for (var i = 0; i < n; i++) {
      s = (s * 9301 + 49297) % 233280;
      var r = s / 233280;
      var wave = Math.sin(i / (n / 6)) * amp * 0.5;
      out.push(Math.max(0, Math.round(base + wave + (r - 0.5) * amp)));
    }
    return out;
  }
  function labels(range) {
    if (range === '24h') return Array.from({length: 24}, function (_, i) { return ((i) % 24) + ':00'; });
    if (range === '7d')  return ['Mon','Tue','Wed','Thu','Fri','Sat','Sun'];
    return Array.from({length: 30}, function (_, i) { return 'D' + (i + 1); });
  }
  function dataFor(range) {
    if (range === '24h') return series(24, 7, 1800, 1400);
    if (range === '7d')  return series(7, 13, 42000, 18000);
    return series(30, 29, 41000, 22000);
  }

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
        labels: labels(range),
        datasets: [{
          label: 'Requests',
          data: dataFor(range),
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
          y: { grid: { color: grid }, ticks: { color: tick, font: { size: 11 }, callback: function (v) { return v >= 1000 ? (v / 1000) + 'k' : v; } }, border: { display: false } }
        }
      }
    });
  }

  var chart = build();

  function rebuild() { chart.destroy(); chart = build(); }

  // Range toggle: Datastar drives the `btn-brand` class onto the active range
  // button via data-class. We watch which segmented button is active (by class
  // + its label text) and rebuild when it changes. Cheap poll, demo-only.
  var rangeButtons = ['24h', '7d', '30d'];
  setInterval(function () {
    var picked = null;
    document.querySelectorAll('button').forEach(function (b) {
      var t = b.textContent.trim();
      if (rangeButtons.indexOf(t) !== -1 && b.classList.contains('btn-brand')) picked = t;
    });
    if (picked && picked !== range) { range = picked; rebuild(); }
  }, 250);

  // Live tail: nudge the latest point whenever the SSE-driven `uptime` probe
  // changes (once per second from /ui/events). Falls back to a timer so the
  // graph still breathes if the stream is unavailable.
  function tick() {
    var ds = chart.data.datasets[0].data;
    if (!ds.length) return;
    var last = ds[ds.length - 1];
    var jitter = Math.round((Math.random() - 0.4) * Math.max(40, last * 0.04));
    ds[ds.length - 1] = Math.max(0, last + jitter);
    chart.update('none');
  }
  var probe = document.getElementById('uptimeProbe');
  var lastSeen = probe ? probe.textContent : '';
  setInterval(function () {
    if (probe && probe.textContent !== lastSeen) { lastSeen = probe.textContent; tick(); }
  }, 1000);

  // Re-theme the chart when the light/dark toggle fires.
  window.addEventListener('drgtw-theme-change', function () { rebuild(); });
})();
"##;
