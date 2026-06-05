//! Integration tests for [`drgtw_trace::TraceWriter`]: JSONL round-trip,
//! rotation, archive bundling, and retention pruning. All tests use a
//! `tempfile` directory and the synchronous `shutdown()` flush path so file
//! contents are deterministic.

use std::fs;
use std::io::Read;
use std::path::Path;
use std::time::{Duration, SystemTime};

use drgtw_trace::{LlmMeta, McpMeta, TraceEntry, TraceKind, TraceOptions, TraceWriter};

fn chat_entry(id: &str) -> TraceEntry {
    TraceEntry {
        ts: "2025-01-01T00:00:00Z".into(),
        request_id: id.into(),
        virtual_key: Some("vk-prod".into()),
        status: Some(200),
        latency_ms: Some(42),
        error: None,
        detail: TraceKind::Chat(LlmMeta {
            model: Some("gpt-4o".into()),
            connection: Some("openai".into()),
            input_tokens: Some(10),
            output_tokens: Some(20),
        }),
    }
}

fn mcp_entry(id: &str, args: serde_json::Value) -> TraceEntry {
    TraceEntry {
        ts: "2025-01-01T00:00:00Z".into(),
        request_id: id.into(),
        virtual_key: None,
        status: Some(200),
        latency_ms: Some(5),
        error: None,
        detail: TraceKind::Mcp(McpMeta {
            method: "tools/call".into(),
            tool: Some("search".into()),
            server: Some("srv".into()),
            arguments: Some(args),
            output: Some(serde_json::json!({"ok": true})),
        }),
    }
}

fn read_lines(p: &Path) -> Vec<String> {
    fs::read_to_string(p)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

#[tokio::test]
async fn entries_written_as_parseable_jsonl_lines() {
    let dir = tempfile::tempdir().unwrap();
    let opts = TraceOptions {
        rotate_max_bytes: 10_000_000, // no rotation
        ..Default::default()
    };
    let writer = TraceWriter::new(opts, dir.path().to_path_buf());

    writer.emit(chat_entry("a"));
    writer.emit(chat_entry("b"));
    writer.emit(mcp_entry("c", serde_json::json!({"q": "hi"})));
    writer.shutdown().await;

    let active = dir.path().join("drgtw-trace.jsonl");
    let lines = read_lines(&active);
    assert_eq!(lines.len(), 3, "expected 3 JSONL lines");

    let parsed: Vec<TraceEntry> = lines
        .iter()
        .map(|l| serde_json::from_str(l).expect("each line parses back to TraceEntry"))
        .collect();
    assert_eq!(parsed[0].request_id, "a");
    assert_eq!(parsed[2], mcp_entry("c", serde_json::json!({"q": "hi"})));
}

#[tokio::test]
async fn truncates_oversized_mcp_arguments_in_file() {
    let dir = tempfile::tempdir().unwrap();
    let writer = TraceWriter::new(TraceOptions::default(), dir.path().to_path_buf());

    let big = serde_json::Value::String("z".repeat(70 * 1024));
    writer.emit(mcp_entry("big", big));
    writer.shutdown().await;

    let lines = read_lines(&dir.path().join("drgtw-trace.jsonl"));
    assert_eq!(lines.len(), 1);
    let v: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
    assert_eq!(v["arguments"], serde_json::Value::String("…[truncated]".into()));
    // small output untouched
    assert_eq!(v["output"]["ok"], serde_json::Value::Bool(true));
}

#[tokio::test]
async fn rotation_triggers_at_small_threshold() {
    let dir = tempfile::tempdir().unwrap();
    let opts = TraceOptions {
        rotate_max_bytes: 200, // tiny → rotates after a couple of lines
        archive_after_files: 1000, // don't archive in this test
        ..Default::default()
    };
    let writer = TraceWriter::new(opts, dir.path().to_path_buf());
    for i in 0..40 {
        writer.emit(chat_entry(&format!("r{i}")));
    }
    writer.shutdown().await;

    let rotated: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| {
            let n = e.file_name();
            let n = n.to_string_lossy();
            n.starts_with("drgtw-trace-") && n.ends_with(".jsonl")
        })
        .collect();
    assert!(
        !rotated.is_empty(),
        "expected at least one rotated file, got none"
    );
}

#[tokio::test]
async fn archive_bundles_rotated_files_into_valid_targz() {
    let dir = tempfile::tempdir().unwrap();
    let opts = TraceOptions {
        rotate_max_bytes: 150,    // rotate quickly
        archive_after_files: 2,   // archive once 2 rotated files exist
        ..Default::default()
    };
    let writer = TraceWriter::new(opts, dir.path().to_path_buf());
    for i in 0..60 {
        writer.emit(chat_entry(&format!("e{i}")));
    }
    writer.shutdown().await;

    let entries: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();

    let archives: Vec<_> = entries
        .iter()
        .filter(|n| n.starts_with("traces-") && n.ends_with(".tar.gz"))
        .collect();
    assert!(!archives.is_empty(), "expected at least one tar.gz archive");

    // Open the first archive and confirm it contains rotated .jsonl members.
    let arc_path = dir.path().join(archives[0]);
    let f = fs::File::open(&arc_path).unwrap();
    let gz = flate2::read::GzDecoder::new(f);
    let mut tar = tar::Archive::new(gz);
    let mut member_names = Vec::new();
    let mut total_bytes = 0usize;
    for entry in tar.entries().unwrap() {
        let mut entry = entry.unwrap();
        let name = entry.path().unwrap().to_string_lossy().into_owned();
        let mut buf = Vec::new();
        entry.read_to_end(&mut buf).unwrap();
        total_bytes += buf.len();
        member_names.push(name);
    }
    assert!(
        member_names.iter().all(|n| n.starts_with("drgtw-trace-") && n.ends_with(".jsonl")),
        "archive members should be rotated jsonl files, got {member_names:?}"
    );
    assert!(total_bytes > 0, "archived members should be non-empty");
}

#[tokio::test]
async fn retention_deletes_old_archives() {
    let dir = tempfile::tempdir().unwrap();

    // Plant an "old" archive and an old rotated file, backdate their mtimes.
    let old_archive = dir.path().join("traces-20000101-000000.tar.gz");
    let old_rotated = dir.path().join("drgtw-trace-20000101-000000.jsonl");
    let fresh_archive = dir.path().join("traces-20990101-000000.tar.gz");
    fs::write(&old_archive, b"old").unwrap();
    fs::write(&old_rotated, b"old").unwrap();
    fs::write(&fresh_archive, b"new").unwrap();

    let ancient = SystemTime::now() - Duration::from_secs(200 * 86_400);
    let ft = filetime::FileTime::from_system_time(ancient);
    filetime::set_file_mtime(&old_archive, ft).unwrap();
    filetime::set_file_mtime(&old_rotated, ft).unwrap();

    // retention_days = 90 → both backdated files (200 days old) get pruned at
    // startup; the fresh archive survives.
    let opts = TraceOptions {
        retention_days: 90,
        ..Default::default()
    };
    let writer = TraceWriter::new(opts, dir.path().to_path_buf());
    // give the startup sweep a moment, then a no-op emit + shutdown.
    writer.emit(chat_entry("keepalive"));
    writer.shutdown().await;

    assert!(!old_archive.exists(), "old archive should be pruned");
    assert!(!old_rotated.exists(), "old rotated file should be pruned");
    assert!(fresh_archive.exists(), "fresh archive should survive retention");
}
