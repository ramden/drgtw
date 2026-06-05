//! WP 9.3 bin-level e2e: `--validate-config` opens the persistent vault.
//!
//! `main.rs`'s `--validate-config` path is exactly `server::router(...)` followed
//! by `process::exit(1)` on `Err`. These tests exercise `server::router` (the
//! fallible part) directly: a good vault config builds the router; a vault that
//! cannot be opened (wrong key, missing parent dir) returns `Err`, which is the
//! exit-1 path in the binary.

use std::io::Write as _;
use std::sync::Arc;

use drgtw_config::load;
use tempfile::TempDir;

const VAULT_KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// Write a temp TOML config with a `[pii.vault]` block and load it.
fn load_vault_config(dir: &TempDir, vault_path: &str, vault_key: &str) -> Arc<drgtw_config::Config> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);

    let toml_content = format!(
        r#"
[[connections]]
name = "mock-upstream"
base_url = "http://127.0.0.1:1/v1"
api_key = "upstream-secret"
format = "open_ai"
models = ["gpt-4o"]

[[virtual_keys]]
key = "sk-drgtw-vault-e2e01"
connections = ["mock-upstream"]
models = ["gpt-4o"]

[pii]
enabled_by_default = true

[pii.vault]
path = "{vault_path}"
key = "{vault_key}"
"#
    );

    let path = dir.path().join(format!("drgtw-vault-e2e-{n}.toml"));
    let mut f = std::fs::File::create(&path).expect("create temp config");
    f.write_all(toml_content.as_bytes()).expect("write temp config");
    let cfg = load(&path).expect("load temp config");
    Arc::new(cfg)
}

/// `--validate-config` builds the router, which opens the vault. A valid vault
/// config must succeed (this is the `config valid` exit-0 path).
#[test]
fn test_validate_config_opens_vault_ok() {
    let dir = TempDir::new().unwrap();
    let vault_path = dir.path().join("vault.db");
    let cfg = load_vault_config(&dir, &vault_path.to_string_lossy(), VAULT_KEY);

    // server::router == the fallible part of --validate-config.
    let result = drgtw::server::router(cfg, dir.path());
    assert!(result.is_ok(), "valid vault config must build the router");
    assert!(vault_path.exists(), "vault file must be created on open");
}

/// A wrong-but-valid-hex key against an existing vault → BadKey → router Err
/// → the binary's exit-1 path.
#[test]
fn test_validate_config_wrong_key_fails() {
    let dir = TempDir::new().unwrap();
    let vault_path = dir.path().join("vault.db");

    // Create the vault with the correct key first.
    let cfg_ok = load_vault_config(&dir, &vault_path.to_string_lossy(), VAULT_KEY);
    let _ = drgtw::server::router(cfg_ok, dir.path()).expect("first build creates vault");

    // Now a config with a different valid-hex key must fail to build.
    let wrong = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
    let cfg_bad = load_vault_config(&dir, &vault_path.to_string_lossy(), wrong);
    let err = drgtw::server::router(cfg_bad, dir.path()).expect_err("wrong key must fail");
    let msg = err.to_string();
    assert!(msg.to_lowercase().contains("vault"), "error mentions vault: {msg}");
    // Key material must never leak.
    assert!(!msg.contains(wrong), "key material must not leak: {msg}");
}

/// A vault path whose parent directory is missing → router Err (exit-1 path).
#[test]
fn test_validate_config_missing_parent_dir_fails() {
    let dir = TempDir::new().unwrap();
    let bogus = dir.path().join("nope").join("vault.db");
    let cfg = load_vault_config(&dir, &bogus.to_string_lossy(), VAULT_KEY);

    let err = drgtw::server::router(cfg, dir.path()).expect_err("missing parent dir must fail");
    let msg = err.to_string();
    assert!(msg.to_lowercase().contains("vault"), "error mentions vault: {msg}");
}
