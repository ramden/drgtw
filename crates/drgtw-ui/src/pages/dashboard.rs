//! `GET /ui` — the dashboard. A spacious "Instrument" layout: a hero band
//! pairing one oversized serif headline metric with a wide uPlot traffic chart,
//! a row of KPI cards with inline sparklines, a slim inventory strip, and an
//! airy recent-requests table.
//!
//! All numbers are real, pulled from the Postgres history store via
//! [`crate::UiState::history`]. When no store is connected (or a query errors)
//! the page renders zero/empty state — never a 500.

use axum::extract::State;
use axum::response::Html;
use maud::{Markup, PreEscaped, html};

use drgtw_history::UsageSummary;

use crate::pages::{fmt_cost, fmt_int, fmt_latency, fmt_ts, status_kind, timeseries_json};
use crate::layout::{Nav, badge, page_header, shell};
use crate::{UiState, range_window};

/// Async handler: queries the last-24h summary, the 24h/Hour timeseries (for
/// the chart + sparklines), and the 8 most recent requests.
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
    let success = if s.requests > 0 {
        100.0 * (s.requests.saturating_sub(s.error_count)) as f64 / s.requests as f64
    } else {
        100.0
    };

    let body = html! {
        (page_header("Dashboard", "Live gateway traffic, cost, and privacy at a glance."))

        // Server-rendered series consumed by the chart + sparkline bootstrap.
        script { (PreEscaped(format!("window.__traffic = {traffic_json};"))) }

        // Datastar: `uptime` ticks from /ui/events (live status pill in header).
        div data-signals="{uptime: '0s'}" data-init="@get('/ui/events')" class="space-y-8" {
            span id="uptimeProbe" class="sr-only" data-text="$uptime" {}

            // ---- HERO BAND: oversized serif metric (4) + wide chart (8) ----
            div class="grid grid-cols-1 lg:grid-cols-12 gap-5" {
                // Headline metric.
                div class="lg:col-span-4 glass glass-metric lift p-7 flex flex-col justify-between" {
                    div {
                        div class="flex items-center gap-2 text-[11px] uppercase tracking-[0.14em] text-muted-foreground" {
                            span class="live-dot" {}
                            "Requests · last 24h"
                        }
                        div class="stat-hero text-[5.5rem] mt-3" { (fmt_int(s.requests)) }
                    }
                    div class="grid grid-cols-2 gap-4 mt-6 pt-5 border-t border-border" {
                        (mini_stat("Success rate", &format!("{success:.1}%"), "ok"))
                        (mini_stat("Errors · 24h", &fmt_int(s.error_count), if s.error_count > 0 { "down" } else { "muted" }))
                    }
                }
                // Wide traffic chart.
                div class="lg:col-span-8 glass lift p-6 flex flex-col" {
                    div class="flex flex-wrap items-center justify-between gap-3 mb-5" {
                        div {
                            h3 class="font-display text-xl" { "Gateway traffic" }
                            p class="text-xs text-muted-foreground mt-0.5" { "Requests per interval" }
                        }
                        div data-signals="{range: '24h'}" class="inline-flex rounded-lg border border-border bg-card p-0.5 text-xs" {
                            (range_btn("24h"))
                            (range_btn("7d"))
                            (range_btn("30d"))
                        }
                    }
                    // uPlot injects its own <canvas> here. Fixed height avoids a
                    // flexbox<->canvas feedback loop (a flex-1 container would size
                    // to the canvas, which sizes to the container, growing forever).
                    div id="trafficChart" class="h-[300px] min-w-0" {}
                }
            }

            // ---- KPI ROW: four bigger cards, each with an inline sparkline ----
            div class="grid grid-cols-2 lg:grid-cols-4 gap-5" {
                (kpi_card("Tokens processed", &fmt_int(total_tokens), "in + out, 24h", Some("spark-tokens")))
                (kpi_card("Estimated cost", &fmt_cost(s.cost_usd), "spend, 24h", Some("spark-cost")))
                (kpi_card("Avg latency", &fmt_latency(s.avg_latency_ms), "upstream round-trip", Some("spark-lat")))
                (kpi_card("PII redacted", &fmt_int(s.pii_count), "entities, 24h", None))
            }

            // ---- slim inventory strip ----
            div class="grid grid-cols-2 sm:grid-cols-4 gap-5" {
                (count_card("Connections", cfg.connections.len(), "/ui/connections"))
                (count_card("Virtual keys", cfg.virtual_keys.len(), "/ui/keys"))
                (count_card("Models", model_count, "/ui/connections"))
                (count_card("MCP servers", cfg.mcp_servers.len(), "/ui/mcp"))
            }

            // ---- recent requests (airy) ----
            div class="glass overflow-hidden" {
                div class="flex items-center justify-between px-6 py-4 border-b border-border" {
                    h3 class="font-display text-xl" { "Recent requests" }
                    a href="/ui/traces" class="text-xs text-primary hover:underline" { "View all traces →" }
                }
                div class="overflow-x-auto" {
                    table class="w-full text-sm" {
                        thead {
                            tr class="text-left text-[11px] uppercase tracking-wide text-muted-foreground border-b border-border" {
                                th class="font-medium px-6 py-3" { "Time" }
                                th class="font-medium px-6 py-3" { "Model" }
                                th class="font-medium px-6 py-3" { "Connection" }
                                th class="font-medium px-6 py-3 text-right" { "Tokens" }
                                th class="font-medium px-6 py-3 text-right" { "Latency" }
                                th class="font-medium px-6 py-3" { "PII" }
                                th class="font-medium px-6 py-3" { "Status" }
                            }
                        }
                        tbody {
                            @if recent.is_empty() {
                                tr {
                                    td class="px-6 py-8 text-center text-sm text-muted-foreground" colspan="7" {
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

        // Chart + sparkline bootstrap (pjax-safe: registers teardown).
        script { (PreEscaped(DASH_JS)) }
    };

    shell("Dashboard", "Dashboard", Nav::Dashboard, unlocked, cfg.ui.auth.as_ref().map(|a| a.username.as_str()), body)
}

/// A small label/value pair used in the hero card's footer.
fn mini_stat(label: &str, value: &str, kind: &str) -> Markup {
    let color = match kind {
        "ok" => "color: var(--ok)",
        "down" => "color: var(--down)",
        _ => "color: var(--foreground)",
    };
    html! {
        div {
            div class="text-[11px] uppercase tracking-[0.12em] text-muted-foreground" { (label) }
            div class="text-2xl tnum mt-1.5" style=(color) { (value) }
        }
    }
}

/// A KPI card: label, big tabular number, caption, and an optional sparkline
/// canvas (drawn client-side from `window.__traffic`).
fn kpi_card(label: &str, value: &str, caption: &str, spark_id: Option<&str>) -> Markup {
    html! {
        div class="glass glass-metric lift p-6 flex flex-col" {
            div class="text-[11px] uppercase tracking-[0.12em] text-muted-foreground" { (label) }
            div class="stat-gradient text-4xl font-medium mt-2 leading-none" { (value) }
            @if let Some(id) = spark_id {
                canvas id=(id) class="w-full h-9 mt-4 block" {}
            } @else {
                div class="h-9 mt-4 flex items-end" {
                    div class="text-xs text-muted-foreground" { (caption) }
                }
            }
            @if spark_id.is_some() {
                div class="text-xs text-muted-foreground mt-2" { (caption) }
            }
        }
    }
}

fn count_card(label: &str, n: usize, href: &str) -> Markup {
    html! {
        a href=(href) class="glass lift p-5 block" {
            div class="text-2xl font-medium tnum leading-none" { (n) }
            div class="text-sm text-muted-foreground mt-1.5" { (label) }
        }
    }
}

fn range_btn(label: &str) -> Markup {
    let data_class = format!(
        "{{'btn-brand': $range === '{label}', 'text-muted-foreground hover:text-foreground': $range !== '{label}'}}"
    );
    let on_click = format!("$range = '{label}'");
    html! {
        button type="button"
            class="range-btn px-2.5 py-1 rounded-md transition-colors"
            data-range=(label)
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
            td class="px-6 py-3 font-mono text-[12.5px] text-muted-foreground" { (fmt_ts(ev.ts_unix_ms as i64)) }
            td class="px-6 py-3 font-mono text-[12.5px]" { (ev.model) }
            td class="px-6 py-3 font-mono text-[12.5px] text-muted-foreground" { (ev.connection) }
            td class="px-6 py-3 text-right tnum" { (fmt_int(tokens as i64)) }
            td class="px-6 py-3 text-right tnum text-muted-foreground" { (fmt_latency(ev.latency_ms as f64)) }
            td class="px-6 py-3 tnum text-muted-foreground" { (pii) }
            td class="px-6 py-3" { (badge(status_kind(ev.status), &ev.status.to_string())) }
        }
    }
}

const DASH_JS: &str = r##"
(function () {
  var cleanup = window.__drgtwCleanup || (window.__drgtwCleanup = []);

  function cssVar(name) {
    return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  }
  // Resolve oklch()/any CSS color → "rgb(r,g,b)" via a 1px canvas read-back.
  function resolve(color) {
    var c = document.createElement('canvas');
    c.width = 1; c.height = 1;
    var cx = c.getContext('2d');
    cx.fillStyle = '#000'; cx.fillStyle = color;
    cx.fillRect(0, 0, 1, 1);
    var d = cx.getImageData(0, 0, 1, 1).data;
    return 'rgb(' + d[0] + ',' + d[1] + ',' + d[2] + ')';
  }
  function rgba(rgb, a) {
    var m = rgb.match(/\d+(\.\d+)?/g);
    return m ? 'rgba(' + m[0] + ',' + m[1] + ',' + m[2] + ',' + a + ')' : rgb;
  }

  var accent = resolve(cssVar('--primary') || '#e0a83a');
  var gridColor = rgba(resolve(cssVar('--foreground') || '#fff'), 0.06);
  var tickColor = rgba(resolve(cssVar('--muted-foreground') || '#aaa'), 0.9);

  var t = window.__traffic || {};

  // ---------------------------------------------------------- sparklines ---
  function drawSpark(id, arr) {
    var cv = document.getElementById(id);
    if (!cv || !arr || arr.length < 2) return;
    var dpr = window.devicePixelRatio || 1;
    var w = cv.clientWidth || 160, h = cv.clientHeight || 36;
    cv.width = w * dpr; cv.height = h * dpr;
    var x = cv.getContext('2d'); x.scale(dpr, dpr);
    var min = Math.min.apply(null, arr), max = Math.max.apply(null, arr);
    var rng = (max - min) || 1, pad = 3;
    function px(i) { return (i / (arr.length - 1)) * w; }
    function py(v) { return h - pad - ((v - min) / rng) * (h - pad * 2); }
    x.beginPath();
    arr.forEach(function (v, i) { i ? x.lineTo(px(i), py(v)) : x.moveTo(px(i), py(v)); });
    x.lineTo(w, h); x.lineTo(0, h); x.closePath();
    x.globalAlpha = 0.12; x.fillStyle = accent; x.fill();
    x.globalAlpha = 0.95; x.beginPath();
    arr.forEach(function (v, i) { i ? x.lineTo(px(i), py(v)) : x.moveTo(px(i), py(v)); });
    x.strokeStyle = accent; x.lineWidth = 1.5; x.stroke();
  }
  var tokens = (t.input_tokens || []).map(function (v, i) { return v + ((t.output_tokens || [])[i] || 0); });
  drawSpark('spark-tokens', tokens);
  drawSpark('spark-cost', t.cost_usd || []);
  drawSpark('spark-lat', t.avg_latency_ms || []);

  // ----------------------------------------------------------- main chart ---
  var container = document.getElementById('trafficChart');
  if (!container || typeof uPlot === 'undefined') return;

  function data(o) { return [o.x || [], o.requests || []]; }
  function opts(w, h) {
    return {
      width: w, height: h,
      scales: { x: { time: true } },
      series: [
        {},
        {
          label: 'Requests', stroke: accent, width: 2,
          fill: function (u) {
            var hh = u.ctx.canvas.height || h;
            var g = u.ctx.createLinearGradient(0, 0, 0, hh);
            g.addColorStop(0, rgba(accent, 0.20));
            g.addColorStop(1, rgba(accent, 0));
            return g;
          },
          points: { show: false }
        }
      ],
      axes: [
        { stroke: tickColor, grid: { show: false }, ticks: { show: false }, font: '11px sans-serif' },
        {
          stroke: tickColor, grid: { stroke: gridColor, width: 1 }, ticks: { show: false }, font: '11px sans-serif',
          values: function (_u, v) { return v.map(function (n) { return n >= 1000 ? (n / 1000) + 'k' : String(n); }); }
        }
      ],
      legend: { show: true },
      cursor: { points: { size: 6 } }
    };
  }

  var seed = (t.x) ? t : { x: [], requests: [] };
  var CHART_H = 300; // fixed; matches the container's h-[300px]
  var u = new uPlot(opts(container.clientWidth || 600, CHART_H), data(seed), container);

  // Resize WIDTH only on container change — height stays fixed so there is no
  // flexbox<->canvas growth loop.
  var ro = null;
  if (typeof ResizeObserver !== 'undefined') {
    ro = new ResizeObserver(function () { u.setSize({ width: container.clientWidth || 600, height: CHART_H }); });
    ro.observe(container);
  }

  function load(r) {
    fetch('/ui/api/timeseries?range=' + encodeURIComponent(r), { headers: { Accept: 'application/json' } })
      .then(function (res) { return res.ok ? res.json() : null; })
      .then(function (j) { if (j && j.x) u.setData(data(j)); })
      .catch(function () {});
  }
  document.querySelectorAll('.range-btn').forEach(function (b) {
    b.addEventListener('click', function () { load(b.getAttribute('data-range')); });
  });

  // pjax teardown: client-side nav removes this DOM, so kill the chart +
  // observer to avoid leaks across navigations.
  cleanup.push(function () { try { u.destroy(); } catch (e) {} if (ro) ro.disconnect(); });
})();
"##;
