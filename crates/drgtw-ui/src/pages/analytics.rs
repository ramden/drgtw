//! `GET /ui/analytics` — long-range analytics over the Postgres history store.
//!
//! Async handler. Pulls a 7d/Day window: a summary, a timeseries (rendered into
//! `window.__analytics` for the multi-series line chart), and top
//! model/connection/endpoint breakdowns (bar charts). Each canvas gets its own
//! Chart.js init that reuses the theme-resolve pattern from the dashboard.

use axum::extract::State;
use axum::response::Html;
use maud::{Markup, PreEscaped, html};

use drgtw_history::{DimCount, UsageSummary};

use crate::pages::{
    fmt_cost, fmt_int, fmt_latency, timeseries_json,
};
use crate::layout::{self, Nav, empty_state, page_header, shell};
use crate::{UiState, range_window};

pub async fn analytics(State(state): State<UiState>) -> Html<String> {
    let (since, until, bucket) = range_window("7d");

    let (summary, series_json, by_model, by_conn, by_endpoint) = match state.history() {
        Some(h) => {
            let summary = h.usage_summary(since, until).await.unwrap_or_else(|_| zero_summary());
            let buckets = h.usage_timeseries(since, until, bucket).await.unwrap_or_default();
            let series_json = timeseries_json(&buckets, bucket).to_string();
            let by_model = h.usage_by_model(since, until).await.unwrap_or_default();
            let by_conn = h.usage_by_connection(since, until).await.unwrap_or_default();
            let by_endpoint = h.usage_by_endpoint(since, until).await.unwrap_or_default();
            (summary, series_json, by_model, by_conn, by_endpoint)
        }
        None => (
            zero_summary(),
            timeseries_json(&[], bucket).to_string(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ),
    };

    Html(render(&state, &summary, &series_json, &by_model, &by_conn, &by_endpoint).into_string())
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

#[allow(clippy::too_many_arguments)]
fn render(
    state: &UiState,
    s: &UsageSummary,
    series_json: &str,
    by_model: &[DimCount],
    by_conn: &[DimCount],
    by_endpoint: &[DimCount],
) -> Markup {
    let cfg = &state.config;
    let unlocked = cfg.ui.history.is_some();
    let username = cfg.ui.auth.as_ref().map(|a| a.username.as_str());
    let total_tokens = s.input_tokens + s.output_tokens;

    let empty = s.requests == 0
        && by_model.is_empty()
        && by_conn.is_empty()
        && by_endpoint.is_empty();

    // Bar-chart JSON payloads (top 8 each, requests + cost per label).
    let model_json = dim_json(by_model);
    let conn_json = dim_json(by_conn);
    let endpoint_json = dim_json(by_endpoint);

    let body = html! {
        (page_header("Analytics", "Token, cost, and latency trends over the last 7 days."))

        // Server-rendered data for the client charts.
        script { (PreEscaped(format!(
            "window.__analytics = {series_json};\nwindow.__byModel = {model_json};\nwindow.__byConn = {conn_json};\nwindow.__byEndpoint = {endpoint_json};"
        ))) }

        @if empty {
            (empty_state(
                layout::ICON_CHART, "muted", "No analytics yet",
                "Nothing to chart",
                html! { "Analytics populate once the gateway records requests in the history store." }
            ))
        } @else {
            // --- summary stat cards (7d) ---
            div class="grid grid-cols-2 sm:grid-cols-3 lg:grid-cols-5 gap-4" {
                (stat_card(1, "Requests · 7d", &fmt_int(s.requests)))
                (stat_card(2, "Tokens · 7d", &fmt_int(total_tokens)))
                (stat_card(3, "Cost · 7d", &fmt_cost(s.cost_usd)))
                (stat_card(4, "Avg latency", &fmt_latency(s.avg_latency_ms)))
                (stat_card(5, "Errors · 7d", &fmt_int(s.error_count)))
            }

            // --- trends (multi-series line) ---
            div class="rise grid" style="--i:6" {
              div class="glass lift p-5" {
                h3 class="text-base font-semibold mb-1" { "Trends" }
                p class="text-xs text-muted-foreground mb-4" { "Requests, tokens, and cost per day" }
                div class="relative h-[300px]" { canvas id="trendChart" {} }
              }
            }

            // --- breakdowns (bar charts) ---
            div class="grid grid-cols-1 lg:grid-cols-3 gap-4" {
                (bar_card(7, "Top models", "modelChart"))
                (bar_card(8, "Top connections", "connChart"))
                (bar_card(9, "Top endpoints", "endpointChart"))
            }
        }

        script { (PreEscaped(ANALYTICS_JS)) }
    };

    shell("Analytics", "Analytics", Nav::Analytics, unlocked, username, body)
}

/// `{labels, requests, cost_usd}` arrays (top 8) for a bar chart.
fn dim_json(dims: &[DimCount]) -> String {
    let top: Vec<&DimCount> = dims.iter().take(8).collect();
    serde_json::json!({
        "labels": top.iter().map(|d| d.label.clone()).collect::<Vec<_>>(),
        "requests": top.iter().map(|d| d.requests).collect::<Vec<_>>(),
        "cost_usd": top.iter().map(|d| d.cost_usd).collect::<Vec<_>>(),
    })
    .to_string()
}

fn stat_card(i: usize, label: &str, value: &str) -> Markup {
    html! {
        div class="rise grid" style=(format!("--i:{i}")) {
          div class="glass glass-metric lift p-4" {
            div class="text-2xl font-semibold stat-gradient leading-none" { (value) }
            div class="text-xs text-muted-foreground mt-1.5" { (label) }
          }
        }
    }
}

fn bar_card(i: usize, title: &str, canvas_id: &str) -> Markup {
    html! {
        // `min-w-0` lets the grid item shrink below the canvas's intrinsic width
        // so the 3-up breakdown row reflows instead of overflowing horizontally.
        div class="rise grid min-w-0" style=(format!("--i:{i}")) {
          div class="glass lift p-5 min-w-0" {
            h3 class="text-sm font-semibold mb-3" { (title) }
            div class="relative h-[260px] min-w-0" { canvas id=(canvas_id) {} }
          }
        }
    }
}

const ANALYTICS_JS: &str = r##"
(function () {
  if (typeof Chart === 'undefined') return;

  function cssVar(name) {
    return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  }
  function resolve(color) {
    var c = document.createElement('canvas');
    c.width = 1; c.height = 1;
    var x = c.getContext('2d');
    x.fillStyle = '#000';
    x.fillStyle = color;
    x.fillRect(0, 0, 1, 1);
    var d = x.getImageData(0, 0, 1, 1).data;
    return 'rgb(' + d[0] + ',' + d[1] + ',' + d[2] + ')';
  }
  function rgba(rgb, a) {
    var m = rgb.match(/\d+(\.\d+)?/g);
    if (!m) return rgb;
    return 'rgba(' + m[0] + ',' + m[1] + ',' + m[2] + ',' + a + ')';
  }

  var charts = [];
  function destroyAll() { charts.forEach(function (c) { c.destroy(); }); charts = []; }

  function commonScales(tick, grid) {
    return {
      x: { grid: { display: false }, ticks: { color: tick, font: { size: 11 }, maxTicksLimit: 8 }, border: { display: false } },
      y: { grid: { color: grid }, ticks: { color: tick, font: { size: 11 }, callback: function (v) { return v >= 1000 ? (v / 1000) + 'k' : v; } }, border: { display: false } }
    };
  }

  function buildAll() {
    var accent = resolve(cssVar('--primary') || '#7c5cff');
    var ok = resolve(cssVar('--ok') || '#22c55e');
    var warn = resolve(cssVar('--warn') || '#f59e0b');
    var grid = rgba(resolve(cssVar('--foreground') || '#fff'), 0.08);
    var tick = rgba(resolve(cssVar('--muted-foreground') || '#aaa'), 0.95);

    // Trends: requests + tokens + cost (cost on a secondary axis).
    var a = window.__analytics || { labels: [] };
    var trendEl = document.getElementById('trendChart');
    if (trendEl) {
      charts.push(new Chart(trendEl.getContext('2d'), {
        type: 'line',
        data: {
          labels: a.labels || [],
          datasets: [
            { label: 'Requests', data: a.requests || [], borderColor: accent, backgroundColor: rgba(accent, 0.1), borderWidth: 2, tension: 0.35, pointRadius: 0, fill: true, yAxisID: 'y' },
            { label: 'Tokens', data: (a.input_tokens || []).map(function (v, i) { return v + ((a.output_tokens || [])[i] || 0); }), borderColor: ok, borderWidth: 2, tension: 0.35, pointRadius: 0, yAxisID: 'y' },
            { label: 'Cost (USD)', data: a.cost_usd || [], borderColor: warn, borderWidth: 2, tension: 0.35, pointRadius: 0, yAxisID: 'y1' }
          ]
        },
        options: {
          responsive: true, maintainAspectRatio: false,
          animation: { duration: 700 },
          interaction: { intersect: false, mode: 'index' },
          plugins: { legend: { labels: { color: tick, boxWidth: 12, font: { size: 11 } } } },
          scales: {
            x: { grid: { display: false }, ticks: { color: tick, font: { size: 11 }, maxTicksLimit: 8 }, border: { display: false } },
            y: { position: 'left', grid: { color: grid }, ticks: { color: tick, font: { size: 11 }, callback: function (v) { return v >= 1000 ? (v / 1000) + 'k' : v; } }, border: { display: false } },
            y1: { position: 'right', grid: { display: false }, ticks: { color: tick, font: { size: 11 }, callback: function (v) { return '$' + v.toFixed(2); } }, border: { display: false } }
          }
        }
      }));
    }

    function bar(id, payload) {
      var el = document.getElementById(id);
      if (!el || !payload) return;
      charts.push(new Chart(el.getContext('2d'), {
        type: 'bar',
        data: {
          labels: payload.labels || [],
          datasets: [{ label: 'Requests', data: payload.requests || [], backgroundColor: rgba(accent, 0.7), borderColor: accent, borderWidth: 1, borderRadius: 4 }]
        },
        options: {
          responsive: true, maintainAspectRatio: false,
          animation: { duration: 600 },
          plugins: {
            legend: { display: false },
            tooltip: {
              callbacks: {
                afterLabel: function (c) {
                  var cost = (payload.cost_usd || [])[c.dataIndex];
                  return cost != null ? '$' + cost.toFixed(2) : '';
                }
              }
            }
          },
          scales: commonScales(tick, grid)
        }
      }));
    }
    bar('modelChart', window.__byModel);
    bar('connChart', window.__byConn);
    bar('endpointChart', window.__byEndpoint);
  }

  buildAll();
  window.addEventListener('drgtw-theme-change', function () { destroyAll(); buildAll(); });
})();
"##;
