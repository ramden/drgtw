//! # drgtw-bench — gateway benchmark harness
//!
//! Two modes, selected as subcommands:
//!
//! ## `latency` (default subcommand)
//!
//! In-process overhead benchmark.  Both the mock upstream and the real gateway
//! run **in-process** on ephemeral loopback ports:
//!
//! ```text
//!  load-generator
//!       │
//!       ├──► gateway (drgtw_proxy::router, ephemeral port)
//!       │         │
//!       │         └──► mock upstream (axum, ephemeral port)
//!       │
//!       └──► mock upstream (same server, direct, baseline)
//! ```
//!
//! **Overhead = gateway latency distribution − baseline latency distribution**
//!
//! The load generator fires requests on a fixed-interval tokio `interval`
//! ticker (open-loop pacing), bounded by a semaphore (`--concurrency`).
//! Per-request latency is the wall-clock duration from `send()` to the first
//! byte of a complete response.
//!
//! Two scenarios:
//! - **passthrough** – PII disabled; measures pure routing + key-swap overhead.
//! - **pii**         – PII enabled, request body contains 2 emails + 1 phone;
//!   the deterministic regex recognizers do real scanning work.  NER is
//!   intentionally excluded to keep bench deps light.
//!
//! Stats: sorted `Vec<Duration>` percentiles (p50/p90/p99/p999/max), mean,
//! achieved RPS, and error count.  HDR histograms are overkill at ≤100 k
//! samples; a sorted array is exact and allocation-simple.
//!
//! ## `memory`
//!
//! RSS-vs-concurrency benchmark.  Spawns the **real** gateway binary as a child
//! process and sweeps concurrency levels, measuring BASELINE / PEAK / SETTLED /
//! IDLE-AFTER RSS.  See [`memory`] module docs for full details and repro
//! commands.

pub mod memory;

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::routing::post;
use clap::{Parser, Subcommand};
use drgtw_config::{ApiFormat, Config, Connection, PiiConfig, ServerConfig, VirtualKey};
use drgtw_proxy::{ProxyState, router as proxy_router};
use serde::Serialize;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio::time;
use tracing_subscriber::EnvFilter;

// ─────────────────────────────────────────────────────────────────────────────
// CLI
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "drgtw-bench",
    about = "Gateway benchmark harness (latency overhead + memory-vs-concurrency)"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// In-process latency overhead benchmark (default mode).
    Latency(LatencyArgs),
    /// RSS-vs-concurrency benchmark: spawns the real gateway binary.
    Memory(memory::MemoryArgs),
}

#[derive(clap::Args, Debug)]
struct LatencyArgs {
    /// Target request rate (requests per second, open-loop).
    #[arg(long, default_value_t = 1000)]
    rps: u64,

    /// Benchmark duration in seconds.
    #[arg(long, default_value_t = 10)]
    duration_secs: u64,

    /// Maximum in-flight requests at any time (semaphore cap).
    #[arg(long, default_value_t = 256)]
    concurrency: usize,

    /// Which scenario(s) to run.
    #[arg(long, default_value = "both", value_parser = parse_scenario)]
    scenario: Scenario,

    /// Artificial latency added by the mock upstream (milliseconds).
    #[arg(long, default_value_t = 0)]
    upstream_delay_ms: u64,

    /// Write JSON results to this file path (optional).
    #[arg(long)]
    json: Option<std::path::PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scenario {
    Passthrough,
    Pii,
    Both,
}

fn parse_scenario(s: &str) -> Result<Scenario, String> {
    match s {
        "passthrough" => Ok(Scenario::Passthrough),
        "pii" => Ok(Scenario::Pii),
        "both" => Ok(Scenario::Both),
        other => Err(format!(
            "unknown scenario '{other}'; use passthrough|pii|both"
        )),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Mock upstream server
// ─────────────────────────────────────────────────────────────────────────────

/// Fixed ~800-byte OpenAI-style chat completion response.
const MOCK_RESPONSE_BODY: &str = r#"{
  "id": "chatcmpl-bench0000000000000",
  "object": "chat.completion",
  "created": 1700000000,
  "model": "gpt-4o-mini",
  "choices": [
    {
      "index": 0,
      "message": {
        "role": "assistant",
        "content": "This is a fixed benchmark response from the mock upstream. It is intentionally padded to approximately 800 bytes so that response-body handling overhead is representative of real traffic. The response contains no actual LLM output — it exists solely to measure gateway latency introduced between the caller and a real upstream provider."
      },
      "finish_reason": "stop"
    }
  ],
  "usage": {
    "prompt_tokens": 16,
    "completion_tokens": 64,
    "total_tokens": 80
  }
}"#;

async fn mock_chat_handler(
    axum::extract::State(delay_ms): axum::extract::State<u64>,
    _body: axum::body::Bytes,
) -> axum::response::Response {
    if delay_ms > 0 {
        time::sleep(Duration::from_millis(delay_ms)).await;
    }
    axum::response::Response::builder()
        .status(200)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(MOCK_RESPONSE_BODY))
        .expect("static response always valid")
}

/// Spawn the mock upstream on an ephemeral port; return its base URL.
async fn spawn_mock_upstream(delay_ms: u64) -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock upstream");
    let addr: SocketAddr = listener.local_addr().expect("local addr");

    let app = axum::Router::new()
        .route("/v1/chat/completions", post(mock_chat_handler))
        .with_state(delay_ms);

    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("mock upstream serve");
    });

    format!("http://{addr}")
}

// ─────────────────────────────────────────────────────────────────────────────
// Gateway server (in-process, latency mode only)
// ─────────────────────────────────────────────────────────────────────────────

const BENCH_VIRTUAL_KEY: &str = "sk-drgtw-bench00000001";

/// Build a gateway Config pointing at the mock upstream.
fn build_config(mock_base_url: &str, pii_enabled: bool) -> Arc<Config> {
    let pii = if pii_enabled {
        PiiConfig {
            enabled_by_default: true,
            ..PiiConfig::default()
        }
    } else {
        PiiConfig::default()
    };

    Arc::new(Config {
        server: ServerConfig {
            bind_addr: "127.0.0.1:0".parse().expect("valid addr"),
            ..Default::default()
        },
        connections: vec![Connection {
            name: "bench-upstream".into(),
            base_url: format!("{}/v1", mock_base_url),
            api_key: "mock-upstream-key".into(),
            format: ApiFormat::OpenAi,
            models: vec!["gpt-4o-mini".into()],
            model_costs: Default::default(),
            region: None,
            aws_access_key_id: None,
            aws_secret_access_key: None,
            aws_session_token: None,
        }],
        virtual_keys: vec![VirtualKey {
            key: BENCH_VIRTUAL_KEY.into(),
            connections: vec!["bench-upstream".into()],
            models: Some(vec!["gpt-4o-mini".into()]),
            rate_limit: None,
            budget: None,
            mcp_servers: None,
        }],
        pii,
        events: None,
        fallback: Default::default(),
        mcp_servers: Default::default(),
        tracing: Default::default(),
        model_aliases: Default::default(),
        otel: Default::default(),
        ui: Default::default(),
        guardrails: Default::default(),
    })
}

/// Spawn the gateway on an ephemeral port; return its base URL.
async fn spawn_gateway(config: Arc<Config>) -> String {
    let state = Arc::new(ProxyState::new(config, Path::new(".")).expect("ProxyState::new"));
    let app = proxy_router(state);

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind gateway");
    let addr: SocketAddr = listener.local_addr().expect("local addr");

    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("gateway serve");
    });

    format!("http://{addr}")
}

// ─────────────────────────────────────────────────────────────────────────────
// Load generator
// ─────────────────────────────────────────────────────────────────────────────

/// Request body for passthrough scenario (tiny, no PII tokens).
const PASSTHROUGH_BODY: &str =
    r#"{"model":"gpt-4o-mini","messages":[{"role":"user","content":"ping"}]}"#;

/// Request body for PII scenario: 2 emails + 1 phone — triggers regex scans.
const PII_BODY: &str = r#"{"model":"gpt-4o-mini","messages":[{"role":"user","content":"Contact alice@example.com or bob@corp.io, phone +49 89 12345678"}]}"#;

struct LoadResult {
    latencies: Vec<Duration>,
    error_count: u64,
}

async fn run_load(
    base_url: String,
    auth_header: String,
    body: &'static str,
    rps: u64,
    duration: Duration,
    concurrency: usize,
) -> LoadResult {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("build reqwest client");

    let sem = Arc::new(Semaphore::new(concurrency));
    let url = format!("{base_url}/v1/chat/completions");

    let mut interval = time::interval(Duration::from_nanos(1_000_000_000 / rps.max(1)));
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

    let deadline = Instant::now() + duration;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Result<Duration, ()>>();

    loop {
        interval.tick().await;
        if Instant::now() >= deadline {
            break;
        }

        let permit = Arc::clone(&sem);
        let client = client.clone();
        let url = url.clone();
        let auth = auth_header.clone();
        let tx = tx.clone();

        tokio::spawn(async move {
            let _guard = permit.acquire_owned().await.expect("semaphore closed");
            let t0 = Instant::now();
            let result = client
                .post(&url)
                .header("Authorization", auth)
                .header("Content-Type", "application/json")
                .body(body)
                .send()
                .await;
            let elapsed = t0.elapsed();
            match result {
                Ok(resp) if resp.status().is_success() => {
                    // Drain body so the connection is returned to pool.
                    let _ = resp.bytes().await;
                    let _ = tx.send(Ok(elapsed));
                }
                _ => {
                    let _ = tx.send(Err(()));
                }
            }
        });
    }

    // Drop our sender copy so the channel closes when all spawned tasks finish.
    drop(tx);

    let mut latencies = Vec::new();
    let mut error_count = 0u64;
    while let Some(result) = rx.recv().await {
        match result {
            Ok(d) => latencies.push(d),
            Err(()) => error_count += 1,
        }
    }

    LoadResult {
        latencies,
        error_count,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Statistics
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct Stats {
    pub count: usize,
    pub errors: u64,
    pub achieved_rps: f64,
    pub mean_ms: f64,
    pub p50_ms: f64,
    pub p90_ms: f64,
    pub p99_ms: f64,
    pub p999_ms: f64,
    pub max_ms: f64,
}

/// Compute stats from a sorted (ascending) slice of durations.
/// The caller must sort before passing.
pub fn compute_stats(sorted: &[Duration], errors: u64, wall_secs: f64) -> Stats {
    if sorted.is_empty() {
        return Stats {
            count: 0,
            errors,
            achieved_rps: 0.0,
            mean_ms: 0.0,
            p50_ms: 0.0,
            p90_ms: 0.0,
            p99_ms: 0.0,
            p999_ms: 0.0,
            max_ms: 0.0,
        };
    }

    let count = sorted.len();
    let sum_ns: u128 = sorted.iter().map(|d| d.as_nanos()).sum();
    let mean_ms = (sum_ns as f64 / count as f64) / 1_000_000.0;
    let achieved_rps = count as f64 / wall_secs;

    Stats {
        count,
        errors,
        achieved_rps,
        mean_ms,
        p50_ms: percentile_ms(sorted, 50.0),
        p90_ms: percentile_ms(sorted, 90.0),
        p99_ms: percentile_ms(sorted, 99.0),
        p999_ms: percentile_ms(sorted, 99.9),
        max_ms: sorted
            .last()
            .map(|d| d.as_nanos() as f64 / 1_000_000.0)
            .unwrap_or(0.0),
    }
}

/// Return the p-th percentile of a **sorted** duration slice in milliseconds.
/// Uses the nearest-rank method: index = ceil(p/100 * n) - 1, clamped.
pub fn percentile_ms(sorted: &[Duration], p: f64) -> f64 {
    assert!(!sorted.is_empty(), "percentile_ms called on empty slice");
    let n = sorted.len();
    // Nearest-rank: rank = ceil(p/100 * n), 1-based → index = rank - 1
    let rank = ((p / 100.0) * n as f64).ceil() as usize;
    let idx = rank.saturating_sub(1).min(n - 1);
    sorted[idx].as_nanos() as f64 / 1_000_000.0
}

// ─────────────────────────────────────────────────────────────────────────────
// Reporting
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct BenchResult {
    scenario: String,
    target_rps: u64,
    duration_secs: u64,
    upstream_delay_ms: u64,
    baseline: Stats,
    gateway: Stats,
    overhead_p50_ms: f64,
    overhead_p99_ms: f64,
}

fn print_table(results: &[BenchResult]) {
    println!();
    println!(
        "{:<12} {:>8} {:>8} {:>8} {:>8} {:>8} {:>10} {:>8} {:>8}",
        "run", "count", "errs", "rps", "p50ms", "p99ms", "p999ms", "maxms", "overhead-p99ms"
    );
    println!("{}", "-".repeat(92));

    for r in results {
        println!(
            "{:<12} {:>8} {:>8} {:>8.0} {:>8.2} {:>8.2} {:>10.2} {:>8.2} {:>14}",
            format!("{}/base", r.scenario),
            r.baseline.count,
            r.baseline.errors,
            r.baseline.achieved_rps,
            r.baseline.p50_ms,
            r.baseline.p99_ms,
            r.baseline.p999_ms,
            r.baseline.max_ms,
            "",
        );
        println!(
            "{:<12} {:>8} {:>8} {:>8.0} {:>8.2} {:>8.2} {:>10.2} {:>8.2} {:>14.2}",
            format!("{}/gw", r.scenario),
            r.gateway.count,
            r.gateway.errors,
            r.gateway.achieved_rps,
            r.gateway.p50_ms,
            r.gateway.p99_ms,
            r.gateway.p999_ms,
            r.gateway.max_ms,
            r.overhead_p99_ms,
        );
        println!();
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Scenario runner
// ─────────────────────────────────────────────────────────────────────────────

async fn run_scenario(
    label: &str,
    mock_url: &str,
    pii: bool,
    rps: u64,
    duration_secs: u64,
    concurrency: usize,
    request_body: &'static str,
) -> BenchResult {
    let dur = Duration::from_secs(duration_secs);

    eprintln!("[bench] {label}: spawning gateway (pii={pii})...");
    let gw_config = build_config(mock_url, pii);
    let gw_url = spawn_gateway(gw_config).await;

    let gw_auth = format!("Bearer {BENCH_VIRTUAL_KEY}");
    let direct_auth = "Bearer mock-upstream-key".to_string();

    // --- baseline: direct to mock upstream ---
    eprintln!("[bench] {label}: baseline run ({rps} rps, {duration_secs}s)...");
    let t0 = Instant::now();
    let base_result = run_load(
        mock_url.to_string(),
        direct_auth,
        request_body,
        rps,
        dur,
        concurrency,
    )
    .await;
    let base_wall = t0.elapsed().as_secs_f64();

    let mut base_lat = base_result.latencies;
    base_lat.sort_unstable();
    let baseline = compute_stats(&base_lat, base_result.error_count, base_wall);

    // --- gateway run ---
    eprintln!("[bench] {label}: gateway run ({rps} rps, {duration_secs}s)...");
    let t1 = Instant::now();
    let gw_result = run_load(gw_url, gw_auth, request_body, rps, dur, concurrency).await;
    let gw_wall = t1.elapsed().as_secs_f64();

    let mut gw_lat = gw_result.latencies;
    gw_lat.sort_unstable();
    let gateway = compute_stats(&gw_lat, gw_result.error_count, gw_wall);

    let overhead_p50_ms = gateway.p50_ms - baseline.p50_ms;
    let overhead_p99_ms = gateway.p99_ms - baseline.p99_ms;

    BenchResult {
        scenario: label.to_string(),
        target_rps: rps,
        duration_secs,
        upstream_delay_ms: 0,
        baseline,
        gateway,
        overhead_p50_ms,
        overhead_p99_ms,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Entry point
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    match cli.command {
        Commands::Latency(args) => run_latency(args).await,
        Commands::Memory(args) => memory::run(args).await,
    }
}

async fn run_latency(cli: LatencyArgs) {
    eprintln!(
        "[bench] target={} rps  duration={}s  concurrency={}  upstream_delay={}ms",
        cli.rps, cli.duration_secs, cli.concurrency, cli.upstream_delay_ms
    );

    let mock_url = spawn_mock_upstream(cli.upstream_delay_ms).await;
    eprintln!("[bench] mock upstream at {mock_url}");

    let mut results: Vec<BenchResult> = Vec::new();

    if matches!(cli.scenario, Scenario::Passthrough | Scenario::Both) {
        let r = run_scenario(
            "passthrough",
            &mock_url,
            false,
            cli.rps,
            cli.duration_secs,
            cli.concurrency,
            PASSTHROUGH_BODY,
        )
        .await;
        results.push(r);
    }

    if matches!(cli.scenario, Scenario::Pii | Scenario::Both) {
        let r = run_scenario(
            "pii",
            &mock_url,
            true,
            cli.rps,
            cli.duration_secs,
            cli.concurrency,
            PII_BODY,
        )
        .await;
        results.push(r);
    }

    print_table(&results);

    if let Some(json_path) = &cli.json {
        let json = serde_json::to_string_pretty(&results).expect("serialize results");
        std::fs::write(json_path, json).expect("write JSON output");
        eprintln!("[bench] JSON results written to {}", json_path.display());
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Unit tests — stats math
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    // --- percentile_ms ---

    #[test]
    fn percentile_single_element() {
        let v = vec![ms(42)];
        assert!((percentile_ms(&v, 50.0) - 42.0).abs() < 0.001);
        assert!((percentile_ms(&v, 99.9) - 42.0).abs() < 0.001);
    }

    #[test]
    fn percentile_uniform_distribution() {
        // 100 elements: 1ms, 2ms, ... 100ms
        let v: Vec<Duration> = (1u64..=100).map(ms).collect();
        // p50 nearest-rank: ceil(50/100 * 100) = 50th element = 50ms
        assert!((percentile_ms(&v, 50.0) - 50.0).abs() < 0.001);
        // p90: ceil(90/100 * 100) = 90th = 90ms
        assert!((percentile_ms(&v, 90.0) - 90.0).abs() < 0.001);
        // p99: ceil(99/100 * 100) = 99th = 99ms
        assert!((percentile_ms(&v, 99.0) - 99.0).abs() < 0.001);
        // p100 (nearest rank clamps to last): 100ms
        assert!((percentile_ms(&v, 100.0) - 100.0).abs() < 0.001);
    }

    #[test]
    fn percentile_two_elements() {
        let v = vec![ms(10), ms(20)];
        // p50: ceil(0.5 * 2) = 1 → index 0 → 10ms
        assert!((percentile_ms(&v, 50.0) - 10.0).abs() < 0.001);
        // p99: ceil(0.99 * 2) = 2 → index 1 → 20ms
        assert!((percentile_ms(&v, 99.0) - 20.0).abs() < 0.001);
    }

    #[test]
    fn percentile_known_skewed_distribution() {
        // 9 fast + 1 slow → p99 should hit the slow one
        let mut v: Vec<Duration> = (0..9).map(|_| ms(1)).collect();
        v.push(ms(1000));
        v.sort_unstable();
        // p99: ceil(0.99 * 10) = 10 → index 9 → 1000ms
        assert!((percentile_ms(&v, 99.0) - 1000.0).abs() < 0.001);
        // p50: ceil(0.50 * 10) = 5 → index 4 → 1ms
        assert!((percentile_ms(&v, 50.0) - 1.0).abs() < 0.001);
    }

    // --- compute_stats ---

    #[test]
    fn stats_empty_input() {
        let s = compute_stats(&[], 3, 10.0);
        assert_eq!(s.count, 0);
        assert_eq!(s.errors, 3);
        assert_eq!(s.achieved_rps as u64, 0);
        assert_eq!(s.mean_ms as u64, 0);
    }

    #[test]
    fn stats_single_request() {
        let v = vec![ms(100)];
        let s = compute_stats(&v, 0, 1.0);
        assert_eq!(s.count, 1);
        assert!((s.mean_ms - 100.0).abs() < 0.001);
        assert!((s.p50_ms - 100.0).abs() < 0.001);
        assert!((s.p99_ms - 100.0).abs() < 0.001);
        assert!((s.max_ms - 100.0).abs() < 0.001);
        assert!((s.achieved_rps - 1.0).abs() < 0.001);
    }

    #[test]
    fn stats_mean_and_rps() {
        // 4 requests of 10ms each over 2 seconds
        let v = vec![ms(10), ms(10), ms(10), ms(10)];
        let s = compute_stats(&v, 0, 2.0);
        assert!((s.mean_ms - 10.0).abs() < 0.001);
        assert!((s.achieved_rps - 2.0).abs() < 0.001);
    }

    #[test]
    fn stats_error_count_propagated() {
        let v = vec![ms(5)];
        let s = compute_stats(&v, 42, 1.0);
        assert_eq!(s.errors, 42);
    }

    #[test]
    fn stats_p999_on_1000_uniform() {
        // 1000 elements 1..=1000ms; p99.9: ceil(0.999 * 1000) = 1000 → index 999 → 1000ms
        let v: Vec<Duration> = (1u64..=1000).map(ms).collect();
        let s = compute_stats(&v, 0, 1.0);
        assert!((s.p999_ms - 1000.0).abs() < 0.001);
    }
}
