//! Page bodies and handlers for the embedded admin UI.
//!
//! Pure render functions and (for the config page) async POST handlers. Live
//! pages (dashboard, config, connections, keys, settings) derive real values
//! from the loaded config; secrets are masked via [`crate::mask`]. Every other
//! nav entry renders a polished empty state (coming-soon or Postgres-gated).

mod analytics;
mod audit;
mod budgets;
mod config;
mod connections;
mod dashboard;
mod keys;
mod limits;
mod mcp;
mod pii;
mod settings;
mod setup;
mod team;
mod traces;
mod webhooks;

pub use analytics::analytics;
pub use audit::audit_log;
pub use budgets::cost_budgets;
pub use limits::rate_limits;
pub use mcp::{mcp_delete, mcp_save, mcp_servers};
pub use team::{team_access, team_create, team_delete};
pub use webhooks::{webhooks, webhooks_replay, webhooks_rotate};
pub use pii::pii_insights;
pub use setup::setup_page;
pub use config::{config_save, config_view};
pub use connections::connections;
pub use dashboard::dashboard;
pub use keys::{key_detail, keys_create, keys_delete, keys_update, virtual_keys};
pub use settings::settings;
pub use traces::traces;

use drgtw_history::{Bucket, UsageBucket};
use maud::{Markup, PreEscaped, html};

/// A reusable glass card with optional fade-rise stagger index.
pub(crate) fn glass_card(stagger: usize, inner: Markup) -> Markup {
    html! {
        // `.rise` (transform) on the wrapper, never on the filtered `.glass`
        // element — moving a backdrop-filtered box re-samples its backdrop each
        // frame and shimmers. `grid` makes the single child stretch to fill the
        // wrapper in any layout (block / grid cell / flex item).
        div class="rise grid" style=(format!("--i:{stagger}")) {
            div class="glass lift p-5" { (inner) }
        }
    }
}

/// A key/value definition row used by the config + detail views.
pub(crate) fn kv_row(key: &str, value: Markup) -> Markup {
    html! {
        div class="grid grid-cols-[10rem_1fr] gap-x-4 gap-y-1 py-1.5 border-b border-border/60 last:border-0 text-sm" {
            div class="text-muted-foreground font-mono text-[12.5px]" { (key) }
            div class="min-w-0 break-words" { (value) }
        }
    }
}

/// Section heading inside a page body.
pub(crate) fn section_title(icon: &str, title: &str) -> Markup {
    html! {
        div class="flex items-center gap-2 mb-3 mt-1" {
            span class="size-4 grid place-items-center text-primary" { (PreEscaped(icon)) }
            h3 class="text-sm font-semibold uppercase tracking-wide text-muted-foreground" { (title) }
        }
    }
}

// ---------------------------------------------------------------------------
// Shared formatting helpers (used by dashboard / analytics / traces)
// ---------------------------------------------------------------------------

/// Format an integer with thousands separators, e.g. `48213` → `"48,213"`.
pub(crate) fn fmt_int(n: i64) -> String {
    let neg = n < 0;
    let digits = n.unsigned_abs().to_string();
    let bytes = digits.as_bytes();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    let len = bytes.len();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    if neg { format!("-{out}") } else { out }
}

/// Format a latency in ms as `"412ms"` (or `"1.2s"` past a second).
pub(crate) fn fmt_latency(ms: f64) -> String {
    if ms >= 1000.0 {
        format!("{:.1}s", ms / 1000.0)
    } else {
        format!("{}ms", ms.round() as i64)
    }
}

/// Format a USD cost as `"$x.xx"` (four decimals under a cent so small spends
/// are still visible).
pub(crate) fn fmt_cost(usd: f64) -> String {
    if usd > 0.0 && usd < 0.01 {
        format!("${usd:.4}")
    } else {
        format!("${usd:.2}")
    }
}

/// Render an epoch-ms timestamp as a compact UTC `YYYY-MM-DD HH:MM:SS` string,
/// without pulling in a date crate.
pub(crate) fn fmt_ts(ts_unix_ms: i64) -> String {
    let secs = ts_unix_ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (h, m, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    let (y, mo, d) = civil_from_days(days);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}:{s:02}")
}

/// Days since the Unix epoch → (year, month, day) in the proleptic Gregorian
/// calendar (Howard Hinnant's `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Map an HTTP status code to a badge kind (`"ok"|"warn"|"down"`).
pub(crate) fn status_kind(status: u16) -> &'static str {
    match status {
        200..=299 => "ok",
        300..=399 => "warn",
        _ => "down",
    }
}

/// Serialise usage buckets into the parallel-array JSON the Chart.js client
/// consumes (`window.__traffic` + the `/ui/api/timeseries` endpoint share this
/// shape). `labels` are short, bucket-appropriate time strings.
pub(crate) fn timeseries_json(buckets: &[UsageBucket], bucket: Bucket) -> serde_json::Value {
    let labels: Vec<String> = buckets.iter().map(|b| fmt_bucket_label(b.ts_ms, bucket)).collect();
    serde_json::json!({
        "labels": labels,
        "requests": buckets.iter().map(|b| b.requests).collect::<Vec<_>>(),
        "input_tokens": buckets.iter().map(|b| b.input_tokens).collect::<Vec<_>>(),
        "output_tokens": buckets.iter().map(|b| b.output_tokens).collect::<Vec<_>>(),
        "cost_usd": buckets.iter().map(|b| b.cost_usd).collect::<Vec<_>>(),
        "avg_latency_ms": buckets.iter().map(|b| b.avg_latency_ms).collect::<Vec<_>>(),
    })
}

/// Axis label for one bucket: `HH:00` for hourly, `MM-DD` for daily.
fn fmt_bucket_label(ts_ms: i64, bucket: Bucket) -> String {
    let secs = ts_ms.div_euclid(1000);
    match bucket {
        Bucket::Hour => {
            let h = secs.rem_euclid(86_400) / 3600;
            format!("{h:02}:00")
        }
        Bucket::Day => {
            let (_, mo, d) = civil_from_days(secs.div_euclid(86_400));
            format!("{mo:02}-{d:02}")
        }
    }
}
