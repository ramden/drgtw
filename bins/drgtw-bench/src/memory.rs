//! # memory — gateway RSS-vs-concurrency benchmark mode
//!
//! Measures how the **real** gateway binary's resident set size (RSS) grows
//! under concurrent in-flight load.  Unlike the latency mode, this mode does
//! **not** run the gateway in-process: it spawns the release binary as a child
//! process so that the OS memory accounting reflects only the gateway's actual
//! allocations.
//!
//! ## Architecture
//!
//! ```text
//!  bench process
//!    │
//!    ├── embedded mock upstream (axum, ephemeral port, long --upstream-delay-ms)
//!    │
//!    ├── child process: drgtw --config /tmp/drgtw-bench-XXXXXX.toml
//!    │       │
//!    │       └── → mock upstream (port written into the temp config)
//!    │
//!    └── load generator (tokio JoinSet, N requests simultaneously)
//!              └── → gateway child (127.0.0.1:<gw_port>)
//! ```
//!
//! ## Measurement protocol (per concurrency step)
//!
//! 1. Sample **BASELINE** RSS (child idle, no load).
//! 2. Fire N requests simultaneously (`tokio::task::JoinSet`).
//! 3. While requests are in-flight, sample RSS every 100 ms; record PEAK and
//!    SETTLED (median of in-flight samples).
//! 4. Await all completions; record error count.
//! 5. 2-second cooldown; sample **IDLE-AFTER** RSS.
//!
//! ## Repro commands
//!
//! ```sh
//! # Build release gateway first (if not done already)
//! cargo build --release -p drgtw
//!
//! # Build bench
//! cargo build --release -p drgtw-bench
//!
//! # PII off — steps 10,100,1000
//! ./target/release/drgtw-bench memory \
//!     --gateway-bin target/release/drgtw \
//!     --concurrency-steps "10,100,1000" \
//!     --upstream-delay-ms 2000 \
//!     --pii off
//!
//! # PII on — same steps
//! ./target/release/drgtw-bench memory \
//!     --gateway-bin target/release/drgtw \
//!     --concurrency-steps "10,100,1000" \
//!     --upstream-delay-ms 2000 \
//!     --pii on
//!
//! # Full default steps with JSON output
//! ./target/release/drgtw-bench memory \
//!     --gateway-bin target/release/drgtw \
//!     --json /tmp/memory-results.json
//!
//! # NOTE: for --concurrency-steps values ≥ ~1000 you may need to raise the
//! # per-process file-descriptor limit first:
//! #   ulimit -n 65536
//! # or permanently in /etc/security/limits.conf:
//! #   * soft nofile 65536
//! #   * hard nofile 65536
//! ```

use std::path::PathBuf;
use std::time::Duration;

use axum::routing::post;
use clap::Args;
use serde::Serialize;
use tokio::net::TcpListener;
use tokio::task::JoinSet;
use tokio::time;
use tracing::{debug, warn};

// ─────────────────────────────────────────────────────────────────────────────
// CLI args (attached to the `memory` subcommand)
// ─────────────────────────────────────────────────────────────────────────────

/// Memory-vs-concurrency benchmark: spawns the real gateway binary as a child
/// process and measures RSS at idle, under load, and after cooldown.
#[derive(Args, Debug)]
pub struct MemoryArgs {
    /// Path to the release gateway binary.
    #[arg(long, default_value = "target/release/drgtw")]
    pub gateway_bin: PathBuf,

    /// Comma-separated list of concurrency levels to sweep.
    #[arg(long, default_value = "10,100,500,1000,5000")]
    pub concurrency_steps: String,

    /// Artificial upstream response delay in milliseconds.
    /// A long delay keeps all N requests in-flight simultaneously so RSS
    /// sampling captures the peak working set.
    #[arg(long, default_value_t = 2000)]
    pub upstream_delay_ms: u64,

    /// PII scenario: `on` = deterministic recognizers active, body contains
    /// 2 emails + 1 phone.  `off` = passthrough only.
    /// Accepted values: on, off, true, false, 1, 0.
    #[arg(long, default_value = "off", value_name = "on|off")]
    pub pii: PiiFlag,

    /// Write JSON results to this path (optional).
    #[arg(long)]
    pub json: Option<PathBuf>,
}

/// Newtype wrapper so clap treats `--pii on|off` as a value, not a flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PiiFlag(pub bool);

impl std::str::FromStr for PiiFlag {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "on" | "true" | "1" => Ok(PiiFlag(true)),
            "off" | "false" | "0" => Ok(PiiFlag(false)),
            other => Err(format!("expected on|off, got '{other}'")),
        }
    }
}

/// Convenience: tests call this directly.
pub fn parse_pii_flag(s: &str) -> Result<bool, String> {
    s.parse::<PiiFlag>().map(|f| f.0)
}

// ─────────────────────────────────────────────────────────────────────────────
// Result types
// ─────────────────────────────────────────────────────────────────────────────

/// One row in the output table.
#[derive(Debug, Clone, Serialize)]
pub struct MemoryRow {
    /// Label: "baseline" or a concurrency number as string.
    pub label: String,
    /// Resident set size before any load (or 0 for baseline row).
    pub baseline_mb: f64,
    /// Peak RSS observed while requests were in-flight (MB).
    pub peak_mb: f64,
    /// Median of in-flight RSS samples — represents steady-state under load.
    pub settled_mb: f64,
    /// RSS after the 2-second cooldown.
    pub idle_after_mb: f64,
    /// Number of requests that failed or timed out.
    pub errors: u64,
}

// ─────────────────────────────────────────────────────────────────────────────
// RSS sampling — /proc/<pid>/status VmRSS
// ─────────────────────────────────────────────────────────────────────────────

/// Parse the `VmRSS` line from `/proc/<pid>/status` content.
/// Returns kilobytes, or `None` if the line is absent / unparseable.
///
/// # Examples
///
/// ```
/// use drgtw_bench::memory::parse_vmrss_kb;
/// let status = "Name:\tdrgtw\nVmRSS:\t  12345 kB\nVmPeak:\t20000 kB\n";
/// assert_eq!(parse_vmrss_kb(status), Some(12345));
/// ```
pub fn parse_vmrss_kb(proc_status: &str) -> Option<u64> {
    for line in proc_status.lines() {
        if line.starts_with("VmRSS:") {
            // Format: "VmRSS:\t  12345 kB"
            let rest = line.trim_start_matches("VmRSS:").trim();
            // rest is like "12345 kB" or just "12345"
            let num_part = rest.split_whitespace().next()?;
            return num_part.parse().ok();
        }
    }
    None
}

/// Sample the RSS of `pid` in kilobytes.  Returns `None` if the process has
/// exited or the file is unreadable.
fn sample_rss_kb(pid: u32) -> Option<u64> {
    let path = format!("/proc/{pid}/status");
    let content = std::fs::read_to_string(&path).ok()?;
    parse_vmrss_kb(&content)
}

// ─────────────────────────────────────────────────────────────────────────────
// Child process guard (kills on Drop, handles panic)
// ─────────────────────────────────────────────────────────────────────────────

/// RAII guard: holds a [`std::process::Child`] and kills it on drop.
/// This ensures the gateway subprocess is cleaned up even if the bench panics.
pub struct ChildGuard(pub std::process::Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        // Best-effort kill: ignore errors (process may have already exited).
        let _ = self.0.kill();
        let _ = self.0.wait(); // reap zombie
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Mock upstream (same as latency mode — long-delay variant)
// ─────────────────────────────────────────────────────────────────────────────

const MOCK_RESPONSE_BODY: &str = r#"{
  "id": "chatcmpl-membench000000000",
  "object": "chat.completion",
  "created": 1700000000,
  "model": "gpt-4o-mini",
  "choices": [
    {
      "index": 0,
      "message": {
        "role": "assistant",
        "content": "Memory benchmark mock response."
      },
      "finish_reason": "stop"
    }
  ],
  "usage": {"prompt_tokens": 8, "completion_tokens": 6, "total_tokens": 14}
}"#;

async fn mock_handler(
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
        .expect("static response")
}

/// Spawn the mock upstream; return its bound address.
async fn spawn_mock_upstream(delay_ms: u64) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock upstream");
    let addr = listener.local_addr().expect("local addr");
    let app = axum::Router::new()
        .route("/v1/chat/completions", post(mock_handler))
        .with_state(delay_ms);
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("mock upstream");
    });
    addr
}

// ─────────────────────────────────────────────────────────────────────────────
// Config file generation
// ─────────────────────────────────────────────────────────────────────────────

const MEMORY_BENCH_KEY: &str = "sk-drgtw-membench00001";

/// Write a temp TOML config for the gateway child process.
/// Returns the `(tempfile, gateway_port)` — caller must keep `tempfile` alive
/// until the child is done.
fn write_temp_config(
    mock_addr: std::net::SocketAddr,
    gw_port: u16,
    pii_enabled: bool,
) -> (tempfile::NamedTempFile, u16) {
    use std::io::Write as _;

    let pii_flag = if pii_enabled { "true" } else { "false" };
    let toml = format!(
        r#"
[server]
bind_addr = "127.0.0.1:{gw_port}"

[[connections]]
name = "mock-upstream"
base_url = "http://{mock_addr}/v1"
api_key = "mock-key"
format = "open_ai"
models = ["gpt-4o-mini"]

[[virtual_keys]]
key = "{MEMORY_BENCH_KEY}"
connections = ["mock-upstream"]

[pii]
enabled_by_default = {pii_flag}
"#
    );

    let mut f = tempfile::Builder::new()
        .prefix("drgtw-bench-")
        .suffix(".toml")
        .tempfile_in("/tmp")
        .expect("create temp config");
    f.write_all(toml.as_bytes()).expect("write temp config");
    (f, gw_port)
}

/// Pick an ephemeral-ish fixed port for the gateway by binding then releasing.
/// Using port 0 would be ideal but we need to pass the port in a file before
/// the child starts; instead we bind, record, and immediately drop — the port
/// is very likely still free when the child starts (loopback, milliseconds).
fn reserve_port() -> u16 {
    let sock = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve port");
    sock.local_addr().expect("local addr").port()
    // sock drops here, releasing the port
}

// ─────────────────────────────────────────────────────────────────────────────
// Gateway child lifecycle
// ─────────────────────────────────────────────────────────────────────────────

/// Wait until the gateway port is accepting connections (up to 5 s).
async fn wait_for_gateway(port: u16) -> Result<(), String> {
    let addr = format!("127.0.0.1:{port}");
    for _ in 0..50 {
        if tokio::net::TcpStream::connect(&addr).await.is_ok() {
            return Ok(());
        }
        time::sleep(Duration::from_millis(100)).await;
    }
    Err(format!("gateway did not start on port {port} within 5 s"))
}

// ─────────────────────────────────────────────────────────────────────────────
// EMFILE detection
// ─────────────────────────────────────────────────────────────────────────────

/// True if the reqwest error looks like an EMFILE (too many open files).
fn is_emfile(e: &reqwest::Error) -> bool {
    let msg = e.to_string();
    msg.contains("Too many open files")
        || msg.contains("os error 24")
        || msg.contains("EMFILE")
}

// ─────────────────────────────────────────────────────────────────────────────
// Per-step measurement
// ─────────────────────────────────────────────────────────────────────────────

/// Request bodies.
const PASSTHROUGH_BODY: &str =
    r#"{"model":"gpt-4o-mini","messages":[{"role":"user","content":"ping"}]}"#;
const PII_BODY: &str = r#"{"model":"gpt-4o-mini","messages":[{"role":"user","content":"Contact alice@example.com or bob@corp.io, phone +49 89 12345678"}]}"#;

/// Fire `n` requests simultaneously; sample RSS every 100 ms while in-flight.
/// Returns `(peak_kb, settled_kb, error_count, emfile_count)`.
async fn run_step(
    client: &reqwest::Client,
    gw_url: &str,
    pid: u32,
    n: usize,
    delay_ms: u64,
    pii: bool,
    per_req_timeout: Duration,
) -> (u64, u64, u64, u64) {
    let body: &'static str = if pii { PII_BODY } else { PASSTHROUGH_BODY };
    let auth = format!("Bearer {MEMORY_BENCH_KEY}");
    let url = format!("{gw_url}/v1/chat/completions");

    // Launch all N requests into a JoinSet.
    let mut join_set: JoinSet<Result<(), String>> = JoinSet::new();
    for _ in 0..n {
        let client = client.clone();
        let url = url.clone();
        let auth = auth.clone();
        join_set.spawn(async move {
            let res = tokio::time::timeout(per_req_timeout, async {
                client
                    .post(&url)
                    .header("Authorization", auth)
                    .header("Content-Type", "application/json")
                    .body(body)
                    .send()
                    .await
            })
            .await;
            match res {
                Ok(Ok(resp)) => {
                    // drain body
                    let _ = resp.bytes().await;
                    Ok(())
                }
                Ok(Err(e)) => {
                    if is_emfile(&e) {
                        Err(format!("EMFILE: too many open files (try `ulimit -n 65536`): {e}"))
                    } else {
                        Err(e.to_string())
                    }
                }
                Err(_) => Err("timeout".to_string()),
            }
        });
    }

    // Sample RSS while requests are in-flight.
    // The mock upstream holds connections open for `delay_ms`, so all N should
    // be in-flight at the same time.
    let sample_interval = Duration::from_millis(100);
    // Give the requests a moment to hit the gateway before sampling.
    time::sleep(Duration::from_millis(50)).await;

    let mut peak_kb: u64 = 0;

    // Sample until all tasks complete; we interleave polling with sampling.
    // Since join_set.join_next() is async, we just drive a sampling loop
    // in parallel with draining the set.
    let sample_task = {
        tokio::spawn(async move {
            let mut samples = Vec::new();
            let mut peak = 0u64;
            // Sample for (delay_ms + 5000) ms max — requests timeout at delay + 30s
            let limit = delay_ms + 5000;
            let deadline = tokio::time::Instant::now() + Duration::from_millis(limit);
            while tokio::time::Instant::now() < deadline {
                if let Some(kb) = sample_rss_kb(pid) {
                    if kb > peak {
                        peak = kb;
                    }
                    samples.push(kb);
                }
                time::sleep(sample_interval).await;
            }
            (peak, samples)
        })
    };

    // Drain all requests.
    let mut errors: u64 = 0;
    let mut emfile_reported = false;
    while let Some(result) = join_set.join_next().await {
        match result {
            Ok(Ok(())) => {}
            Ok(Err(msg)) => {
                errors += 1;
                if msg.contains("EMFILE") && !emfile_reported {
                    eprintln!("[memory] WARNING: {msg}");
                    emfile_reported = true;
                } else {
                    debug!("[memory] request error: {msg}");
                }
            }
            Err(e) => {
                errors += 1;
                warn!("[memory] task join error: {e}");
            }
        }
    }

    // Collect RSS samples.
    let (peak_from_task, mut samples) = sample_task.await.expect("sample task");
    if peak_from_task > peak_kb {
        peak_kb = peak_from_task;
    }

    // Also do a final sample now that requests are done (settled).
    if let Some(kb) = sample_rss_kb(pid) {
        samples.push(kb);
    }

    let settled_kb = if samples.is_empty() {
        0
    } else {
        median_u64(&mut samples)
    };

    (peak_kb, settled_kb, errors, 0)
}

/// Median of a mutable slice (sorts in place).
fn median_u64(v: &mut [u64]) -> u64 {
    v.sort_unstable();
    let n = v.len();
    if n == 0 {
        return 0;
    }
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Table output
// ─────────────────────────────────────────────────────────────────────────────

fn print_memory_table(rows: &[MemoryRow]) {
    println!();
    println!(
        "{:<12} {:>12} {:>12} {:>12} {:>12} {:>8}",
        "concurrency", "peak_mb", "settled_mb", "idle_after_mb", "baseline_mb", "errors"
    );
    println!("{}", "-".repeat(72));
    for row in rows {
        println!(
            "{:<12} {:>12.1} {:>12.1} {:>12.1} {:>12.1} {:>8}",
            row.label,
            row.peak_mb,
            row.settled_mb,
            row.idle_after_mb,
            row.baseline_mb,
            row.errors,
        );
    }
    println!();
}

// ─────────────────────────────────────────────────────────────────────────────
// Entry point
// ─────────────────────────────────────────────────────────────────────────────

/// Run the memory benchmark.  Called from `main` when the `memory` subcommand
/// is selected.
pub async fn run(args: MemoryArgs) {
    // Parse concurrency steps.
    let steps: Vec<usize> = args
        .concurrency_steps
        .split(',')
        .map(|s| {
            s.trim()
                .parse::<usize>()
                .unwrap_or_else(|_| panic!("invalid concurrency step '{s}'"))
        })
        .collect();

    eprintln!(
        "[memory] gateway={} pii={} upstream_delay={}ms steps={:?}",
        args.gateway_bin.display(),
        args.pii.0,
        args.upstream_delay_ms,
        steps
    );

    // 1. Spawn mock upstream.
    let mock_addr = spawn_mock_upstream(args.upstream_delay_ms).await;
    eprintln!("[memory] mock upstream at http://{mock_addr}");

    // 2. Pick a port, write temp config, spawn gateway child.
    let gw_port = reserve_port();
    let (temp_config, _) = write_temp_config(mock_addr, gw_port, args.pii.0);
    let config_path = temp_config.path().to_path_buf();

    eprintln!("[memory] gateway config at {}", config_path.display());
    eprintln!("[memory] spawning gateway on port {gw_port}...");

    let child = std::process::Command::new(&args.gateway_bin)
        .arg("--config")
        .arg(&config_path)
        .stderr(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap_or_else(|e| {
            panic!(
                "failed to spawn gateway binary '{}': {e}",
                args.gateway_bin.display()
            )
        });

    let pid = child.id();
    let _guard = ChildGuard(child); // kills child on drop / panic

    // 3. Wait for the gateway to start listening.
    wait_for_gateway(gw_port)
        .await
        .expect("gateway failed to start");
    eprintln!("[memory] gateway ready (pid={pid})");

    let gw_url = format!("http://127.0.0.1:{gw_port}");

    // Per-request timeout: upstream delay + 30 s headroom.
    let per_req_timeout = Duration::from_millis(args.upstream_delay_ms) + Duration::from_secs(30);

    // HTTP client — large connection pool, no built-in timeout (we use per-req).
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(6000)
        .connection_verbose(false)
        .build()
        .expect("build reqwest client");

    let mut rows: Vec<MemoryRow> = Vec::new();

    // 4. Baseline (idle, pre-any-load).
    let baseline_kb = sample_rss_kb(pid).unwrap_or(0);
    let baseline_mb = baseline_kb as f64 / 1024.0;
    eprintln!("[memory] baseline RSS: {baseline_mb:.1} MB");

    rows.push(MemoryRow {
        label: "baseline".to_string(),
        baseline_mb,
        peak_mb: baseline_mb,
        settled_mb: baseline_mb,
        idle_after_mb: baseline_mb,
        errors: 0,
    });

    // 5. Per-step sweep.
    for &n in &steps {
        eprintln!("[memory] step concurrency={n}...");

        // Pre-step idle sample (becomes this step's baseline_mb).
        let pre_kb = sample_rss_kb(pid).unwrap_or(0);
        let pre_mb = pre_kb as f64 / 1024.0;

        let (peak_kb, settled_kb, errors, _) = run_step(
            &client,
            &gw_url,
            pid,
            n,
            args.upstream_delay_ms,
            args.pii.0,
            per_req_timeout,
        )
        .await;

        // 2-second cooldown then idle-after sample.
        time::sleep(Duration::from_secs(2)).await;
        let idle_after_kb = sample_rss_kb(pid).unwrap_or(0);

        let row = MemoryRow {
            label: n.to_string(),
            baseline_mb: pre_mb,
            peak_mb: peak_kb as f64 / 1024.0,
            settled_mb: settled_kb as f64 / 1024.0,
            idle_after_mb: idle_after_kb as f64 / 1024.0,
            errors,
        };

        eprintln!(
            "[memory]   peak={:.1} MB  settled={:.1} MB  idle_after={:.1} MB  errors={}",
            row.peak_mb, row.settled_mb, row.idle_after_mb, row.errors
        );

        rows.push(row);
    }

    print_memory_table(&rows);

    if let Some(json_path) = &args.json {
        let json = serde_json::to_string_pretty(&rows).expect("serialize");
        std::fs::write(json_path, json).expect("write JSON");
        eprintln!("[memory] JSON written to {}", json_path.display());
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Unit tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_vmrss_kb ────────────────────────────────────────────────────────

    #[test]
    fn vmrss_typical_line() {
        let status = "Name:\tdrgtw\nVmPeak:\t 20000 kB\nVmRSS:\t  12345 kB\nVmData:\t8000 kB\n";
        assert_eq!(parse_vmrss_kb(status), Some(12345));
    }

    #[test]
    fn vmrss_no_unit_field() {
        // Some kernels omit the "kB" suffix in proc status (unlikely but defensible).
        let status = "VmRSS:\t99\n";
        assert_eq!(parse_vmrss_kb(status), Some(99));
    }

    #[test]
    fn vmrss_absent() {
        let status = "Name:\tdrgtw\nVmPeak:\t20000 kB\n";
        assert_eq!(parse_vmrss_kb(status), None);
    }

    #[test]
    fn vmrss_empty_string() {
        assert_eq!(parse_vmrss_kb(""), None);
    }

    #[test]
    fn vmrss_large_value() {
        let status = "VmRSS:\t4294967295 kB\n"; // u32::MAX kB
        assert_eq!(parse_vmrss_kb(status), Some(4_294_967_295));
    }

    #[test]
    fn vmrss_leading_tabs_spaces() {
        // Exact kernel format: "VmRSS:\t    1234 kB"
        let status = "VmRSS:\t    1234 kB\n";
        assert_eq!(parse_vmrss_kb(status), Some(1234));
    }

    #[test]
    fn vmrss_multiline_fixture() {
        // Real /proc/self/status excerpt.
        let fixture = "\
Name:\tcargo
Umask:\t0022
State:\tS (sleeping)
Tgid:\t12345
VmPeak:\t 512000 kB
VmSize:\t 490000 kB
VmLck:\t       0 kB
VmRSS:\t  98304 kB
VmData:\t 200000 kB
VmStk:\t    132 kB
";
        assert_eq!(parse_vmrss_kb(fixture), Some(98304));
    }

    // ── median_u64 ────────────────────────────────────────────────────────────

    #[test]
    fn median_single() {
        let mut v = vec![42u64];
        assert_eq!(median_u64(&mut v), 42);
    }

    #[test]
    fn median_odd() {
        let mut v = vec![3u64, 1, 2];
        assert_eq!(median_u64(&mut v), 2);
    }

    #[test]
    fn median_even() {
        let mut v = vec![4u64, 1, 3, 2];
        // sorted: 1,2,3,4 → (2+3)/2 = 2
        assert_eq!(median_u64(&mut v), 2);
    }

    #[test]
    fn median_empty() {
        let mut v: Vec<u64> = vec![];
        assert_eq!(median_u64(&mut v), 0);
    }

    // ── parse_pii_flag ────────────────────────────────────────────────────────

    #[test]
    fn pii_flag_on_variants() {
        assert_eq!(parse_pii_flag("on"), Ok(true));
        assert_eq!(parse_pii_flag("true"), Ok(true));
        assert_eq!(parse_pii_flag("1"), Ok(true));
    }

    #[test]
    fn pii_flag_off_variants() {
        assert_eq!(parse_pii_flag("off"), Ok(false));
        assert_eq!(parse_pii_flag("false"), Ok(false));
        assert_eq!(parse_pii_flag("0"), Ok(false));
    }

    #[test]
    fn pii_flag_invalid() {
        assert!(parse_pii_flag("yes").is_err());
        assert!(parse_pii_flag("").is_err());
    }

    // ── concurrency step parsing ───────────────────────────────────────────────

    #[test]
    fn parse_steps_default() {
        let s = "10,100,500,1000,5000";
        let steps: Vec<usize> = s
            .split(',')
            .map(|x| x.trim().parse::<usize>().unwrap())
            .collect();
        assert_eq!(steps, vec![10, 100, 500, 1000, 5000]);
    }

    #[test]
    fn parse_steps_with_spaces() {
        let s = "10, 100, 1000";
        let steps: Vec<usize> = s
            .split(',')
            .map(|x| x.trim().parse::<usize>().unwrap())
            .collect();
        assert_eq!(steps, vec![10, 100, 1000]);
    }
}
