//! Build script: bakes build-provenance metadata into the binary so the
//! `GET /info` status endpoint can report it without a runtime git dependency.
//!
//! Two compile-time env vars are emitted:
//! - `DRGTW_GIT_SHA`  — short commit hash the binary was built from.
//! - `DRGTW_BUILT_AT` — RFC3339 UTC build timestamp.
//!
//! Resolution for the sha (first hit wins): the `DRGTW_GIT_SHA` env (explicit
//! override for CI / Docker `--build-arg`), then `git rev-parse --short=12 HEAD`
//! (works for local + CI cargo builds), then `"unknown"` (e.g. a from-source
//! Docker build with no `.git` context).
//!
//! The timestamp honours an explicit `DRGTW_BUILT_AT` override (reproducible
//! builds), else stamps the current build time.

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    let git_sha = env_nonempty("DRGTW_GIT_SHA").or_else(git_short_sha).unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=DRGTW_GIT_SHA={git_sha}");

    let built_at = env_nonempty("DRGTW_BUILT_AT").unwrap_or_else(|| {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        epoch_to_rfc3339(secs)
    });
    println!("cargo:rustc-env=DRGTW_BUILT_AT={built_at}");

    // Re-run when the override vars or the checked-out commit change.
    println!("cargo:rerun-if-env-changed=DRGTW_GIT_SHA");
    println!("cargo:rerun-if-env-changed=DRGTW_BUILT_AT");
    for head in ["../../.git/HEAD", "../../.git/packed-refs"] {
        if std::path::Path::new(head).exists() {
            println!("cargo:rerun-if-changed={head}");
        }
    }
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn git_short_sha() -> Option<String> {
    let out = Command::new("git").args(["rev-parse", "--short=12", "HEAD"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if sha.is_empty() { None } else { Some(sha) }
}

/// Format seconds-since-unix-epoch as RFC3339 UTC (`YYYY-MM-DDTHH:MM:SSZ`).
/// Civil-date conversion after Howard Hinnant's `civil_from_days`.
fn epoch_to_rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = (secs % 86_400) as i64;
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}
