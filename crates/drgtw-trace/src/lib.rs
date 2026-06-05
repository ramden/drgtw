//! [`TraceWriter`] — non-blocking filesystem request tracer (logrotate-style).
//!
//! ## Design
//!
//! - A bounded [`tokio::sync::mpsc`] channel decouples the request hot path
//!   from disk I/O. [`TraceWriter::emit`] is `try_send`-only: if the buffer is
//!   full the entry is **dropped** (never blocks the caller) and a counter is
//!   incremented (readable via [`TraceWriter::dropped`]). This mirrors the
//!   `EventSink` idiom in `drgtw-events`.
//! - A single background Tokio task drains the channel and appends one JSON
//!   object per line to `<dir>/drgtw-trace.jsonl`, creating `dir` if missing.
//! - **Rotation**: after each write, if the active file is `>= rotate_max_bytes`
//!   it is renamed to `drgtw-trace-<UTC yyyymmdd-HHMMSS>.jsonl` (with a `-N`
//!   suffix on same-second collisions) and a fresh active file is opened.
//! - **Archive**: after a rotation, if the number of rotated `.jsonl` files is
//!   `>= archive_after_files` they are bundled into a
//!   `traces-<UTC yyyymmdd-HHMMSS>.tar.gz` and the originals are deleted.
//! - **Retention**: `.tar.gz` archives and stray rotated `.jsonl` files older
//!   than `retention_days` (by mtime) are pruned at startup, after each
//!   archive, and on an hourly tick.
//!
//! ## Privacy invariant
//!
//! The writer serialises whatever [`TraceEntry`] it receives. For the metadata
//! variants (`chat`/`messages`/`embeddings`/`models`) the caller must never
//! place request/response bodies in the entry. The `mcp` variant intentionally
//! carries tool `arguments`/`output`; both are truncated past 64 KiB.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::warn;

/// Active trace file name.
const ACTIVE_FILE: &str = "drgtw-trace.jsonl";
/// Prefix of rotated trace files.
const ROTATED_PREFIX: &str = "drgtw-trace-";
/// Prefix of archive bundles.
const ARCHIVE_PREFIX: &str = "traces-";
/// Maximum serialized size of `arguments`/`output` before truncation.
const FIELD_MAX_BYTES: usize = 64 * 1024;
/// Marker substituted for oversized `arguments`/`output` values.
const TRUNCATED_MARKER: &str = "…[truncated]";
/// Interval between repeated "buffer full / drop" warning logs (rate-limit).
const WARN_INTERVAL: Duration = Duration::from_secs(5);
/// Default channel buffer capacity.
const DEFAULT_BUFFER: usize = 1024;

// ── Config ──────────────────────────────────────────────────────────────────

/// Runtime options for a [`TraceWriter`].
///
/// This mirrors the `[tracing]` config section in `drgtw-config`
/// (`TracingConfig`) but lives here so the crate is config-independent.
#[derive(Debug, Clone)]
pub struct TraceOptions {
    /// Delete archives / rotated files older than this many days.
    pub retention_days: u64,
    /// The active file rotates once it reaches this size in bytes.
    pub rotate_max_bytes: u64,
    /// Rotated `.jsonl` files are bundled into a tar.gz once this many exist.
    pub archive_after_files: u64,
    /// Bounded channel capacity. Defaults to 1024 via [`Default`].
    pub buffer_size: usize,
}

impl Default for TraceOptions {
    fn default() -> Self {
        Self {
            retention_days: 90,
            rotate_max_bytes: 52_428_800,
            archive_after_files: 10,
            buffer_size: DEFAULT_BUFFER,
        }
    }
}

// ── TraceEntry ────────────────────────────────────────────────────────────────

/// One trace record, serialised as a single JSON line.
///
/// Common fields are flattened; the `kind`-specific fields live in the
/// [`TraceKind`] enum which is `#[serde(flatten)]`ed and internally tagged by
/// `kind`. `None` fields are skipped in the output so each line is compact.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TraceEntry {
    /// RFC3339 timestamp.
    pub ts: String,
    /// Correlation id for the request.
    pub request_id: String,
    /// Virtual key **name** only — never the secret value.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub virtual_key: Option<String>,
    /// HTTP status (or logical status) of the operation.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub status: Option<u16>,
    /// End-to-end latency in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub latency_ms: Option<u64>,
    /// Error message, if the operation failed.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub error: Option<String>,
    /// Kind-specific payload (also supplies the `kind` discriminator tag).
    #[serde(flatten)]
    pub detail: TraceKind,
}

/// Kind-specific trace payload. Tagged by `kind` in the JSON line.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TraceKind {
    /// Chat-completions request (metadata only — never bodies).
    Chat(LlmMeta),
    /// Anthropic messages request (metadata only — never bodies).
    Messages(LlmMeta),
    /// Embeddings request (metadata only — never bodies).
    Embeddings(LlmMeta),
    /// Model-listing request (metadata only — never bodies).
    Models(LlmMeta),
    /// MCP method call — includes tool arguments/output per requirement.
    Mcp(McpMeta),
}

/// Metadata for LLM-style requests. No request/response bodies — PII gateway.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct LlmMeta {
    /// Model name, if known.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub model: Option<String>,
    /// Upstream connection name, if known.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub connection: Option<String>,
    /// Prompt/input token count, if reported.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub input_tokens: Option<u64>,
    /// Completion/output token count, if reported.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub output_tokens: Option<u64>,
}

/// Metadata for MCP method calls. Carries tool args + output (truncated >64 KiB).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpMeta {
    /// JSON-RPC method (e.g. `tools/call`, `tools/list`).
    pub method: String,
    /// Tool name, for `tools/call`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tool: Option<String>,
    /// Upstream MCP server name, if known.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub server: Option<String>,
    /// Tool-call arguments. Truncated to a marker string if serialized > 64 KiB.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub arguments: Option<serde_json::Value>,
    /// Tool-call output. Truncated to a marker string if serialized > 64 KiB.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub output: Option<serde_json::Value>,
}

impl TraceEntry {
    /// Apply the 64 KiB truncation rule to `arguments`/`output` of an MCP entry.
    ///
    /// If the serialized JSON form of either value exceeds [`FIELD_MAX_BYTES`],
    /// the value is replaced with the string [`TRUNCATED_MARKER`]. Non-MCP
    /// entries are returned unchanged.
    fn truncate_oversized_fields(mut self) -> Self {
        if let TraceKind::Mcp(meta) = &mut self.detail {
            meta.arguments = truncate_value(meta.arguments.take());
            meta.output = truncate_value(meta.output.take());
        }
        self
    }
}

/// Replace a JSON value with the truncation marker if its serialized form
/// exceeds [`FIELD_MAX_BYTES`]. `None` passes through.
fn truncate_value(v: Option<serde_json::Value>) -> Option<serde_json::Value> {
    let v = v?;
    let len = serde_json::to_vec(&v).map(|b| b.len()).unwrap_or(usize::MAX);
    if len > FIELD_MAX_BYTES {
        Some(serde_json::Value::String(TRUNCATED_MARKER.to_string()))
    } else {
        Some(v)
    }
}

// ── TraceWriter ───────────────────────────────────────────────────────────────

/// Non-blocking filesystem tracer.
///
/// `TraceWriter` is `Clone`: cloning yields a second handle to the **same**
/// channel and dropped-counter. The background task ends when *all* senders
/// are dropped; [`TraceWriter::shutdown`] consumes the writer, drops its
/// sender, and awaits the task so tests can assert file contents.
#[derive(Clone)]
pub struct TraceWriter {
    tx: mpsc::Sender<TraceEntry>,
    dropped: Arc<AtomicU64>,
    handle: Arc<std::sync::Mutex<Option<JoinHandle<()>>>>,
}

impl TraceWriter {
    /// Create a new `TraceWriter` and spawn the background worker task.
    ///
    /// Trace files are written under `dir`, which is created if missing.
    /// The caller must be inside a Tokio runtime context.
    pub fn new(opts: TraceOptions, dir: PathBuf) -> Self {
        let (tx, rx) = mpsc::channel::<TraceEntry>(opts.buffer_size.max(1));
        let dropped = Arc::new(AtomicU64::new(0));
        let handle = tokio::spawn(worker(opts, dir, rx));
        Self {
            tx,
            dropped,
            handle: Arc::new(std::sync::Mutex::new(Some(handle))),
        }
    }

    /// Submit a trace entry for writing. Non-blocking; drops + counts if full.
    pub fn emit(&self, entry: TraceEntry) {
        if self.tx.try_send(entry).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            maybe_warn_drop();
        }
    }

    /// Total number of entries dropped since this writer was created.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Drop the sender and await the background task to flush all pending
    /// entries to disk. Intended for graceful shutdown and deterministic tests.
    ///
    /// If other clones still hold a sender the task will not stop until they
    /// are dropped too; this awaits the join handle which only completes once
    /// every sender is gone.
    pub async fn shutdown(self) {
        let TraceWriter {
            tx, handle, dropped: _,
        } = self;
        drop(tx);
        let h = handle.lock().ok().and_then(|mut g| g.take());
        if let Some(h) = h {
            let _ = h.await;
        }
    }
}

/// Emit a rate-limited `WARN` about dropped trace entries.
fn maybe_warn_drop() {
    static LAST_WARN_MS: AtomicU64 = AtomicU64::new(0);
    let now_ms = now_unix_ms();
    let last = LAST_WARN_MS.load(Ordering::Relaxed);
    if now_ms.saturating_sub(last) >= WARN_INTERVAL.as_millis() as u64
        && LAST_WARN_MS
            .compare_exchange(last, now_ms, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        warn!("TraceWriter buffer full — trace entry dropped (further drops rate-limited)");
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ── Background worker ─────────────────────────────────────────────────────────

async fn worker(opts: TraceOptions, dir: PathBuf, mut rx: mpsc::Receiver<TraceEntry>) {
    if let Err(e) = std::fs::create_dir_all(&dir) {
        warn!("TraceWriter: cannot create trace dir {}: {e}", dir.display());
        // Still drain so senders don't block forever; writes will just fail.
    }

    // Startup retention sweep.
    prune_retention(&dir, opts.retention_days);

    let active = dir.join(ACTIVE_FILE);
    let mut tick = tokio::time::interval(Duration::from_secs(3600));
    // The immediate first tick fires right away; skip acting on it specially —
    // pruning at startup already happened above.
    tick.tick().await;

    loop {
        tokio::select! {
            maybe = rx.recv() => {
                match maybe {
                    Some(entry) => {
                        write_entry(&active, &entry);
                        if file_len(&active) >= opts.rotate_max_bytes
                            && rotate(&dir, &active).is_some()
                            && rotated_count(&dir) >= opts.archive_after_files
                        {
                            archive_rotated(&dir);
                            prune_retention(&dir, opts.retention_days);
                        }
                    }
                    None => break, // all senders dropped → flush done, exit.
                }
            }
            _ = tick.tick() => {
                prune_retention(&dir, opts.retention_days);
            }
        }
    }
}

/// Append one JSON line for `entry` (after applying field truncation).
fn write_entry(active: &Path, entry: &TraceEntry) {
    let entry = entry.clone().truncate_oversized_fields();
    let mut line = match serde_json::to_string(&entry) {
        Ok(s) => s,
        Err(e) => {
            warn!("TraceWriter: failed to serialise entry: {e}");
            return;
        }
    };
    line.push('\n');

    use std::io::Write as _;
    match std::fs::OpenOptions::new().create(true).append(true).open(active) {
        Ok(mut f) => {
            if let Err(e) = f.write_all(line.as_bytes()) {
                warn!("TraceWriter: write failed: {e}");
            }
        }
        Err(e) => warn!("TraceWriter: cannot open {}: {e}", active.display()),
    }
}

fn file_len(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

/// Rename the active file to a timestamped rotated name. Returns the new path.
fn rotate(dir: &Path, active: &Path) -> Option<PathBuf> {
    if !active.exists() {
        return None;
    }
    let stamp = utc_stamp(SystemTime::now());
    let mut target = dir.join(format!("{ROTATED_PREFIX}{stamp}.jsonl"));
    let mut n = 1;
    while target.exists() {
        target = dir.join(format!("{ROTATED_PREFIX}{stamp}-{n}.jsonl"));
        n += 1;
    }
    match std::fs::rename(active, &target) {
        Ok(()) => Some(target),
        Err(e) => {
            warn!("TraceWriter: rotation rename failed: {e}");
            None
        }
    }
}

/// Paths of all rotated `.jsonl` files currently in `dir`.
fn rotated_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for ent in rd.flatten() {
            let name = ent.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(ROTATED_PREFIX) && name.ends_with(".jsonl") {
                out.push(ent.path());
            }
        }
    }
    out.sort();
    out
}

fn rotated_count(dir: &Path) -> u64 {
    rotated_files(dir).len() as u64
}

/// Bundle all rotated `.jsonl` files into a `traces-<stamp>.tar.gz`, then
/// delete the originals.
fn archive_rotated(dir: &Path) {
    let files = rotated_files(dir);
    if files.is_empty() {
        return;
    }
    let stamp = utc_stamp(SystemTime::now());
    let mut archive = dir.join(format!("{ARCHIVE_PREFIX}{stamp}.tar.gz"));
    let mut n = 1;
    while archive.exists() {
        archive = dir.join(format!("{ARCHIVE_PREFIX}{stamp}-{n}.tar.gz"));
        n += 1;
    }

    let tar_gz = match std::fs::File::create(&archive) {
        Ok(f) => f,
        Err(e) => {
            warn!("TraceWriter: cannot create archive {}: {e}", archive.display());
            return;
        }
    };
    let enc = flate2::write::GzEncoder::new(tar_gz, flate2::Compression::default());
    let mut builder = tar::Builder::new(enc);

    for f in &files {
        let name = match f.file_name() {
            Some(n) => n,
            None => continue,
        };
        if let Err(e) = builder.append_path_with_name(f, name) {
            warn!("TraceWriter: failed to add {} to archive: {e}", f.display());
            return; // leave originals in place on failure
        }
    }

    match builder.into_inner().and_then(|enc| enc.finish()) {
        Ok(_) => {
            for f in &files {
                if let Err(e) = std::fs::remove_file(f) {
                    warn!("TraceWriter: cannot delete archived file {}: {e}", f.display());
                }
            }
        }
        Err(e) => warn!("TraceWriter: failed to finalise archive: {e}"),
    }
}

/// Delete `.tar.gz` archives and stray rotated `.jsonl` files older than
/// `retention_days` (by mtime).
fn prune_retention(dir: &Path, retention_days: u64) {
    let cutoff = match SystemTime::now().checked_sub(Duration::from_secs(retention_days * 86_400)) {
        Some(c) => c,
        None => return,
    };
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return,
    };
    for ent in rd.flatten() {
        let name = ent.file_name();
        let name = name.to_string_lossy();
        let is_archive = name.starts_with(ARCHIVE_PREFIX) && name.ends_with(".tar.gz");
        let is_rotated = name.starts_with(ROTATED_PREFIX) && name.ends_with(".jsonl");
        if !(is_archive || is_rotated) {
            continue;
        }
        let mtime = ent.metadata().and_then(|m| m.modified()).ok();
        if let Some(mtime) = mtime
            && mtime < cutoff
            && let Err(e) = std::fs::remove_file(ent.path())
        {
            warn!("TraceWriter: retention delete failed for {}: {e}", ent.path().display());
        }
    }
}

/// Format a [`SystemTime`] as UTC `yyyymmdd-HHMMSS` (no external date crate).
fn utc_stamp(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
    let (y, mo, d, h, mi, s) = civil_from_unix(secs);
    format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}")
}

/// Convert unix seconds to a UTC civil date-time tuple `(year, month, day,
/// hour, min, sec)`. Uses Howard Hinnant's days-from-civil inverse algorithm.
fn civil_from_unix(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let hour = (rem / 3600) as u32;
    let min = ((rem % 3600) / 60) as u32;
    let sec = (rem % 60) as u32;

    // days since 1970-01-01 → civil date (Hinnant).
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d, hour, min, sec)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_stamp_known_epoch() {
        // 2021-01-01T00:00:00Z = 1_609_459_200
        let t = UNIX_EPOCH + Duration::from_secs(1_609_459_200);
        assert_eq!(utc_stamp(t), "20210101-000000");
        // 1970-01-01T00:00:00Z
        assert_eq!(utc_stamp(UNIX_EPOCH), "19700101-000000");
        // 2009-02-13T23:31:30Z = 1_234_567_890
        let t2 = UNIX_EPOCH + Duration::from_secs(1_234_567_890);
        assert_eq!(utc_stamp(t2), "20090213-233130");
    }

    #[test]
    fn truncation_replaces_oversized_arguments_and_output() {
        let big = serde_json::Value::String("x".repeat(FIELD_MAX_BYTES + 100));
        let small = serde_json::json!({"a": 1});
        let entry = TraceEntry {
            ts: "2025-01-01T00:00:00Z".into(),
            request_id: "r1".into(),
            virtual_key: None,
            status: None,
            latency_ms: None,
            error: None,
            detail: TraceKind::Mcp(McpMeta {
                method: "tools/call".into(),
                tool: Some("search".into()),
                server: None,
                arguments: Some(big),
                output: Some(small.clone()),
            }),
        }
        .truncate_oversized_fields();

        if let TraceKind::Mcp(m) = entry.detail {
            assert_eq!(
                m.arguments,
                Some(serde_json::Value::String(TRUNCATED_MARKER.to_string()))
            );
            assert_eq!(m.output, Some(small)); // small value untouched
        } else {
            panic!("expected mcp");
        }
    }

    #[test]
    fn small_fields_not_truncated() {
        let v = serde_json::json!({"q": "hello"});
        assert_eq!(truncate_value(Some(v.clone())), Some(v));
        assert_eq!(truncate_value(None), None);
    }

    #[test]
    fn chat_entry_skips_none_fields_in_json() {
        let entry = TraceEntry {
            ts: "2025-01-01T00:00:00Z".into(),
            request_id: "r2".into(),
            virtual_key: Some("vk-name".into()),
            status: Some(200),
            latency_ms: Some(12),
            error: None,
            detail: TraceKind::Chat(LlmMeta {
                model: Some("gpt-4o".into()),
                connection: Some("openai".into()),
                input_tokens: Some(5),
                output_tokens: None,
            }),
        };
        let s = serde_json::to_string(&entry).unwrap();
        assert!(s.contains("\"kind\":\"chat\""));
        assert!(s.contains("\"virtual_key\":\"vk-name\""));
        assert!(!s.contains("error"));
        assert!(!s.contains("output_tokens"));
        // round-trips back to the same value.
        let back: TraceEntry = serde_json::from_str(&s).unwrap();
        assert_eq!(back, entry);
    }
}
