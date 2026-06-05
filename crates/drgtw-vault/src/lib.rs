//! DRGTW persistent encrypted entity vault (WP 9.1 / Phase 9).
//!
//! The vault provides stable, restorable entity→placeholder mappings backed by
//! a SQLite database. It is the persistence layer that lets embeddings/RAG
//! produce placeholders that are STABLE ACROSS REQUESTS and recoverable months
//! later.
//!
//! Security model:
//! - Original values are encrypted at rest with AES-256-GCM (random 12-byte
//!   nonce per value, nonce prepended to ciphertext).
//! - Lookups by value use a keyed blind index: `HMAC-SHA256(idx_subkey, kind ||
//!   0x00 || value)`. The plaintext value is NEVER stored in the database.
//! - The master key (32 bytes) is split into two independent subkeys via
//!   HMAC-SHA256 domain separation: one for value encryption, one for the blind
//!   index. The master key itself is never used directly as a cipher key.
//! - A key-check row (an AES-GCM-encrypted constant) is verified on open, so
//!   opening an existing DB with the WRONG key fails loudly with
//!   [`VaultError::BadKey`] instead of silently corrupting data.
//!
//! Concurrency: the database is opened in WAL mode with a busy timeout, and the
//! connection lives behind a `Mutex`. Callers are expected to run on blocking
//! threads. `get_or_assign` is atomic via a SQLite transaction, so concurrent
//! assignment of the same new value yields a single placeholder.

use std::path::Path;
use std::sync::Mutex;

use aes_gcm::aead::rand_core::RngCore;
use aes_gcm::aead::{Aead, OsRng};
use aes_gcm::{AeadCore, Aes256Gcm, Key, KeyInit, Nonce};
use hmac::{Hmac, Mac};
use rusqlite::{Connection, OptionalExtension};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Current on-disk schema version. Bump when the schema changes.
const SCHEMA_VERSION: i64 = 1;

/// Domain-separation labels for subkey derivation. Changing these invalidates
/// existing databases (encryption/index keys would differ), so they are frozen.
const ENC_SUBKEY_LABEL: &[u8] = b"drgtw-vault-enc-v1";
const IDX_SUBKEY_LABEL: &[u8] = b"drgtw-vault-idx-v1";

/// Constant plaintext encrypted under the derived encryption key and stored in
/// `meta`. On open we decrypt it and verify it matches; a mismatch (or AEAD
/// auth failure) means the supplied key is wrong.
const KEY_CHECK_PLAINTEXT: &[u8] = b"drgtw-vault-key-check-v1";

/// Errors returned by [`Vault`] operations.
#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    /// Filesystem-level error opening/creating the database.
    #[error("vault io error: {0}")]
    Io(#[source] std::io::Error),
    /// SQLite-level error.
    #[error("vault sqlite error: {0}")]
    Sqlite(#[source] rusqlite::Error),
    /// Encryption or decryption failure (AEAD auth failure, malformed
    /// ciphertext, etc.). The underlying message never includes key material.
    #[error("vault crypto error: {0}")]
    Crypto(String),
    /// The supplied key does not match the key the database was created with.
    /// The database is left untouched and can still be opened with the correct
    /// key.
    #[error("vault bad key: the supplied key does not match this database")]
    BadKey,
}

impl From<rusqlite::Error> for VaultError {
    fn from(e: rusqlite::Error) -> Self {
        VaultError::Sqlite(e)
    }
}

/// A persistent encrypted entity vault.
///
/// Thread-safe: the SQLite connection is held behind a `Mutex`. Clone is not
/// provided; share via `Arc<Vault>`.
pub struct Vault {
    conn: Mutex<Connection>,
    /// Derived AES-256-GCM key for value encryption.
    enc_key: Key<Aes256Gcm>,
    /// Derived HMAC-SHA256 key for the blind index.
    idx_key: [u8; 32],
}

impl std::fmt::Debug for Vault {
    /// Never emits key material.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vault")
            .field("enc_key", &"<redacted>")
            .field("idx_key", &"<redacted>")
            .finish()
    }
}

impl Vault {
    /// Open or create the vault database at `path`.
    ///
    /// `key` is the 32-byte master key. Two subkeys are derived internally via
    /// HMAC-SHA256 domain separation: one for AES-256-GCM value encryption and
    /// one for the HMAC-SHA256 blind index. Schema migrations run on open.
    ///
    /// If `path` already exists, the supplied key is verified against the stored
    /// key-check row; a wrong key returns [`VaultError::BadKey`] and leaves the
    /// database intact.
    pub fn open(path: &Path, key: &[u8; 32]) -> Result<Self, VaultError> {
        // Derive the two independent subkeys via HMAC domain separation.
        let enc_bytes = derive_subkey(key, ENC_SUBKEY_LABEL);
        let idx_key = derive_subkey(key, IDX_SUBKEY_LABEL);
        let enc_key = *Key::<Aes256Gcm>::from_slice(&enc_bytes);

        let conn = Connection::open(path).map_err(VaultError::Sqlite)?;

        // Concurrency-friendly pragmas.
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(VaultError::Sqlite)?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(VaultError::Sqlite)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(VaultError::Sqlite)?;

        let vault = Vault {
            conn: Mutex::new(conn),
            enc_key,
            idx_key,
        };

        vault.migrate()?;
        vault.verify_or_init_key_check()?;

        Ok(vault)
    }

    /// Run schema migrations. Idempotent.
    fn migrate(&self) -> Result<(), VaultError> {
        let conn = self.conn.lock().expect("vault mutex poisoned");
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS entities (
                kind         TEXT    NOT NULL,
                value_hmac   BLOB    NOT NULL,
                placeholder  TEXT    NOT NULL UNIQUE,
                value_enc    BLOB    NOT NULL,
                created_unix INTEGER NOT NULL,
                PRIMARY KEY (kind, value_hmac)
            );
            -- Legacy table from the original sequential-counter scheme. No
            -- longer written or read (placeholders are now random, non-
            -- sequential), but kept so existing databases migrate cleanly
            -- without a schema-version bump.
            CREATE TABLE IF NOT EXISTS counters (
                kind TEXT    PRIMARY KEY,
                next INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS meta (
                k TEXT PRIMARY KEY,
                v BLOB NOT NULL
            );
            ",
        )
        .map_err(VaultError::Sqlite)?;

        // Record/verify schema version.
        let existing: Option<i64> = conn
            .query_row(
                "SELECT CAST(v AS INTEGER) FROM meta WHERE k = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .optional()
            .map_err(VaultError::Sqlite)?;
        if existing.is_none() {
            conn.execute(
                "INSERT INTO meta (k, v) VALUES ('schema_version', ?1)",
                [SCHEMA_VERSION.to_string()],
            )
            .map_err(VaultError::Sqlite)?;
        }
        Ok(())
    }

    /// On a fresh DB, store the encrypted key-check constant. On an existing DB,
    /// decrypt it and verify it matches — a mismatch means a wrong key.
    fn verify_or_init_key_check(&self) -> Result<(), VaultError> {
        let conn = self.conn.lock().expect("vault mutex poisoned");
        let stored: Option<Vec<u8>> = conn
            .query_row("SELECT v FROM meta WHERE k = 'key_check'", [], |r| r.get(0))
            .optional()
            .map_err(VaultError::Sqlite)?;

        match stored {
            None => {
                // Fresh DB: encrypt and store the key-check constant.
                let blob = self.encrypt(KEY_CHECK_PLAINTEXT)?;
                conn.execute(
                    "INSERT INTO meta (k, v) VALUES ('key_check', ?1)",
                    rusqlite::params![blob],
                )
                .map_err(VaultError::Sqlite)?;
                Ok(())
            }
            Some(blob) => {
                // Existing DB: decrypt and compare. A wrong key fails the AEAD
                // auth tag (Crypto error) or yields different bytes — both map
                // to BadKey here.
                match self.decrypt(&blob) {
                    Ok(pt) if pt == KEY_CHECK_PLAINTEXT => Ok(()),
                    _ => Err(VaultError::BadKey),
                }
            }
        }
    }

    /// Return the stable placeholder for `(kind_prefix, value)`.
    ///
    /// If the entity already exists, its existing placeholder is returned. If it
    /// is new, a placeholder `"{PREFIX}_{n}"` is assigned where `n` is a
    /// **cryptographically random**, non-sequential integer, atomically within
    /// a SQLite transaction. Concurrent assignment of the same new value yields
    /// the same placeholder.
    ///
    /// # Why random, not a counter (security)
    ///
    /// A running `PERSON_1`, `PERSON_2`, … counter is enumerable: the upstream
    /// provider learns how many distinct entities exist and their first-seen
    /// order, and (critically) the tokens are trivially guessable. Because
    /// [`crate::Vault::lookup_placeholder`] resolves *any* vault-known
    /// placeholder that appears in a model response (the RAG restore path), a
    /// guessable token lets a hallucinated or prompt-injected `PERSON_3` in one
    /// caller's response pull a *different* request's real value into it —
    /// cross-request data exfiltration. A ~64-bit random suffix makes valid
    /// tokens infeasible to guess or enumerate.
    ///
    /// # Why digits-only
    ///
    /// The response-restore pass in `drgtw-pii` recognises placeholder tokens
    /// with the regex `\b([A-Z][A-Z0-9_]*_[0-9]+)\b` — the suffix after the
    /// final `_` must be **all decimal digits** or the token is never restored
    /// (the placeholder would leak to the client instead of the real value).
    /// The random suffix is therefore a base-10 integer, never hex.
    pub fn get_or_assign(&self, kind_prefix: &str, value: &str) -> Result<String, VaultError> {
        let hmac = self.blind_index(kind_prefix, value);

        let mut conn = self.conn.lock().expect("vault mutex poisoned");
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(VaultError::Sqlite)?;

        // Existing entity?
        let existing: Option<String> = tx
            .query_row(
                "SELECT placeholder FROM entities WHERE kind = ?1 AND value_hmac = ?2",
                rusqlite::params![kind_prefix, hmac],
                |r| r.get(0),
            )
            .optional()
            .map_err(VaultError::Sqlite)?;
        if let Some(placeholder) = existing {
            // No write needed; commit (no-op) to release the lock cleanly.
            tx.commit().map_err(VaultError::Sqlite)?;
            return Ok(placeholder);
        }

        // New entity: assign a high-entropy, digits-only random placeholder.
        // Retry on the (astronomically rare) UNIQUE collision with another
        // placeholder already in the table.
        let value_enc = self.encrypt(value.as_bytes())?;
        let created = now_unix();

        // ~64 bits of entropy per attempt; 16 attempts is overkill given a near
        // empty namespace per draw, but bounds the loop deterministically.
        let mut placeholder = String::new();
        for _ in 0..16 {
            let candidate = format!("{kind_prefix}_{}", random_suffix());
            let taken: bool = tx
                .query_row(
                    "SELECT 1 FROM entities WHERE placeholder = ?1",
                    rusqlite::params![candidate],
                    |_| Ok(()),
                )
                .optional()
                .map_err(VaultError::Sqlite)?
                .is_some();
            if !taken {
                placeholder = candidate;
                break;
            }
        }
        if placeholder.is_empty() {
            return Err(VaultError::Crypto(
                "could not assign a collision-free placeholder".to_owned(),
            ));
        }

        tx.execute(
            "INSERT INTO entities (kind, value_hmac, placeholder, value_enc, created_unix)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![kind_prefix, hmac, placeholder, value_enc, created],
        )
        .map_err(VaultError::Sqlite)?;

        tx.commit().map_err(VaultError::Sqlite)?;
        Ok(placeholder)
    }

    /// Reverse lookup: return the decrypted original value for `placeholder`, or
    /// `None` if the placeholder is unknown.
    pub fn lookup_placeholder(&self, placeholder: &str) -> Result<Option<String>, VaultError> {
        let conn = self.conn.lock().expect("vault mutex poisoned");
        let blob: Option<Vec<u8>> = conn
            .query_row(
                "SELECT value_enc FROM entities WHERE placeholder = ?1",
                rusqlite::params![placeholder],
                |r| r.get(0),
            )
            .optional()
            .map_err(VaultError::Sqlite)?;
        drop(conn);

        match blob {
            None => Ok(None),
            Some(blob) => {
                let pt = self.decrypt(&blob)?;
                let s = String::from_utf8(pt)
                    .map_err(|e| VaultError::Crypto(format!("invalid utf-8 in value: {e}")))?;
                Ok(Some(s))
            }
        }
    }

    /// Number of distinct entities stored.
    pub fn len(&self) -> Result<u64, VaultError> {
        let conn = self.conn.lock().expect("vault mutex poisoned");
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM entities", [], |r| r.get(0))
            .map_err(VaultError::Sqlite)?;
        Ok(n as u64)
    }

    /// Returns `true` if the vault holds no entities.
    pub fn is_empty(&self) -> Result<bool, VaultError> {
        Ok(self.len()? == 0)
    }

    // --- crypto helpers ---

    /// Compute the blind index: `HMAC-SHA256(idx_key, kind || 0x00 || value)`.
    fn blind_index(&self, kind: &str, value: &str) -> Vec<u8> {
        let mut mac = <HmacSha256 as Mac>::new_from_slice(&self.idx_key)
            .expect("HMAC accepts any key length");
        mac.update(kind.as_bytes());
        mac.update(&[0x00]);
        mac.update(value.as_bytes());
        mac.finalize().into_bytes().to_vec()
    }

    /// Encrypt `plaintext` with AES-256-GCM. Output = `nonce(12) || ciphertext`.
    fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>, VaultError> {
        let cipher = Aes256Gcm::new(&self.enc_key);
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ct = cipher
            .encrypt(&nonce, plaintext)
            .map_err(|_| VaultError::Crypto("encryption failed".to_owned()))?;
        let mut out = Vec::with_capacity(nonce.len() + ct.len());
        out.extend_from_slice(nonce.as_slice());
        out.extend_from_slice(&ct);
        Ok(out)
    }

    /// Decrypt a `nonce(12) || ciphertext` blob produced by [`Self::encrypt`].
    fn decrypt(&self, blob: &[u8]) -> Result<Vec<u8>, VaultError> {
        if blob.len() < 12 {
            return Err(VaultError::Crypto("ciphertext too short".to_owned()));
        }
        let (nonce_bytes, ct) = blob.split_at(12);
        let nonce = Nonce::from_slice(nonce_bytes);
        let cipher = Aes256Gcm::new(&self.enc_key);
        cipher
            .decrypt(nonce, ct)
            .map_err(|_| VaultError::Crypto("decryption failed".to_owned()))
    }
}

/// Derive a 32-byte subkey from the master key via HMAC-SHA256 domain
/// separation: `HMAC-SHA256(master, label)`.
fn derive_subkey(master: &[u8; 32], label: &[u8]) -> [u8; 32] {
    let mut mac = <HmacSha256 as Mac>::new_from_slice(master).expect("HMAC accepts any key length");
    mac.update(label);
    let out = mac.finalize().into_bytes();
    let mut key = [0u8; 32];
    key.copy_from_slice(&out);
    key
}

/// Generate a high-entropy, non-sequential decimal suffix for a placeholder.
///
/// Drawn from the OS CSPRNG ([`OsRng`]). The value is a full `u64` (~64 bits),
/// rendered in base 10 so the resulting `{PREFIX}_{n}` token is recognised by
/// the digits-only restore regex in `drgtw-pii`. `OsRng` is forced to the high
/// half of the range so the suffix is always at least 19 digits — this removes
/// any small/guessable values and keeps the on-the-wire token length uniform.
fn random_suffix() -> u64 {
    // Set the top bit so the value is always >= 2^63 (>= 19 decimal digits),
    // eliminating short, low-magnitude suffixes while preserving 63 bits of
    // entropy below it.
    OsRng.next_u64() | (1u64 << 63)
}

/// Current Unix time in seconds.
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn test_key() -> [u8; 32] {
        // Deterministic, distinct bytes.
        let mut k = [0u8; 32];
        for (i, b) in k.iter_mut().enumerate() {
            *b = i as u8;
        }
        k
    }

    fn other_key() -> [u8; 32] {
        [0xABu8; 32]
    }

    /// The placeholder format contract: `{PREFIX}_{digits}` where the suffix is
    /// all ASCII decimal digits. This is the exact shape the response-restore
    /// regex in `drgtw-pii` (`\b([A-Z][A-Z0-9_]*_[0-9]+)\b`) matches; a hex or
    /// otherwise non-digit suffix would never be restored and the placeholder
    /// would leak to the client. Panics on violation.
    fn assert_placeholder_shape(prefix: &str, ph: &str) {
        let suffix = ph
            .strip_prefix(&format!("{prefix}_"))
            .unwrap_or_else(|| panic!("placeholder {ph:?} must start with {prefix:?}_"));
        assert!(!suffix.is_empty(), "placeholder {ph:?} has empty suffix");
        assert!(
            suffix.bytes().all(|b| b.is_ascii_digit()),
            "placeholder suffix must be ALL decimal digits (restore-regex contract): {ph:?}"
        );
    }

    #[test]
    fn test_assign_new_has_valid_digit_shape() {
        let dir = TempDir::new().unwrap();
        let v = Vault::open(&dir.path().join("v.db"), &test_key()).unwrap();
        let p = v.get_or_assign("PERSON", "Alice").unwrap();
        // Digits-only suffix (restore contract), and NOT the sequential counter.
        assert_placeholder_shape("PERSON", &p);
        assert_ne!(p, "PERSON_1", "must not be a sequential counter");
    }

    /// SECURITY REGRESSION: placeholders must NOT be the sequential
    /// `PREFIX_1`, `PREFIX_2`, … scheme. Sequential tokens leak the entity
    /// count and first-seen order, and are trivially guessable — which lets a
    /// hallucinated/prompt-injected token (e.g. `PERSON_3`) in one caller's
    /// response exfiltrate another request's real value through the store-backed
    /// restore pass.
    #[test]
    fn test_placeholders_are_not_sequential() {
        let dir = TempDir::new().unwrap();
        let v = Vault::open(&dir.path().join("v.db"), &test_key()).unwrap();
        let p1 = v.get_or_assign("PERSON", "Alice").unwrap();
        let p2 = v.get_or_assign("PERSON", "Bob").unwrap();
        let p3 = v.get_or_assign("PERSON", "Carol").unwrap();

        for p in [&p1, &p2, &p3] {
            for n in 1..=5 {
                assert_ne!(
                    p,
                    &format!("PERSON_{n}"),
                    "must not be a small guessable sequential token: {p}"
                );
            }
        }
        assert_ne!(p1, p2);
        assert_ne!(p2, p3);
        assert_ne!(p1, p3);
    }

    /// SECURITY: the numeric suffix must carry enough entropy that it cannot be
    /// enumerated. A counter would yield 1,2,3…; we assert no small values and a
    /// wide spread across a batch of assignments.
    #[test]
    fn test_placeholder_suffix_is_high_entropy() {
        let dir = TempDir::new().unwrap();
        let v = Vault::open(&dir.path().join("v.db"), &test_key()).unwrap();

        let mut suffixes = Vec::new();
        for i in 0..32 {
            let ph = v.get_or_assign("PERSON", &format!("person-{i}")).unwrap();
            assert_placeholder_shape("PERSON", &ph);
            let suffix: u128 = ph
                .strip_prefix("PERSON_")
                .unwrap()
                .parse()
                .expect("suffix parses as a decimal integer");
            suffixes.push(suffix);
        }

        // No small/guessable values: a counter would produce 1..=32.
        assert_eq!(
            suffixes.iter().filter(|&&s| s <= 1_000_000).count(),
            0,
            "no placeholder may use a small/guessable suffix: {suffixes:?}"
        );

        // All distinct.
        let mut sorted = suffixes.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), suffixes.len(), "suffixes must be unique");

        // Wide spread — not a clustered, incrementing counter.
        let min = *suffixes.iter().min().unwrap();
        let max = *suffixes.iter().max().unwrap();
        assert!(
            max - min > 1_000_000_000,
            "suffixes must span a large range (random, not sequential): {min}..{max}"
        );
    }

    #[test]
    fn test_same_value_same_placeholder_within_instance() {
        let dir = TempDir::new().unwrap();
        let v = Vault::open(&dir.path().join("v.db"), &test_key()).unwrap();
        let p1 = v.get_or_assign("PERSON", "Alice").unwrap();
        let p2 = v.get_or_assign("PERSON", "Alice").unwrap();
        assert_eq!(p1, p2);
        assert_eq!(v.len().unwrap(), 1);
    }

    #[test]
    fn test_same_value_same_placeholder_across_reopen() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("v.db");
        let first = {
            let v = Vault::open(&path, &test_key()).unwrap();
            v.get_or_assign("PERSON", "Alice").unwrap()
        };
        // Reopen with same key — placeholder must be stable.
        let v2 = Vault::open(&path, &test_key()).unwrap();
        let second = v2.get_or_assign("PERSON", "Alice").unwrap();
        assert_eq!(first, second);
        assert_eq!(v2.len().unwrap(), 1);
    }

    #[test]
    fn test_different_values_get_distinct_placeholders() {
        let dir = TempDir::new().unwrap();
        let v = Vault::open(&dir.path().join("v.db"), &test_key()).unwrap();
        let a = v.get_or_assign("PERSON", "Alice").unwrap();
        let b = v.get_or_assign("PERSON", "Bob").unwrap();
        let c = v.get_or_assign("PERSON", "Carol").unwrap();
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
        assert_eq!(v.len().unwrap(), 3);
    }

    /// Stability is per-vault (provided by the `entities` row), NOT a
    /// deterministic function of the value. Two independent vaults — even with
    /// the same key — assign INDEPENDENT random placeholders to the same value.
    ///
    /// This is a deliberate privacy property: making the token a pure function
    /// of `(kind, value, key)` would let anyone holding the key (or observing
    /// tokens across vaults) link the same value across otherwise-separate
    /// vaults. Random per-assignment placeholders remove that linkage.
    #[test]
    fn test_placeholder_is_random_not_deterministic_across_vaults() {
        let dir = TempDir::new().unwrap();
        let v = Vault::open(&dir.path().join("v.db"), &test_key()).unwrap();
        let person = v.get_or_assign("PERSON", "Alice").unwrap();
        let email = v.get_or_assign("EMAIL", "a@x.com").unwrap();
        assert_placeholder_shape("PERSON", &person);
        assert_placeholder_shape("EMAIL", &email);

        let dir2 = TempDir::new().unwrap();
        let v2 = Vault::open(&dir2.path().join("v.db"), &test_key()).unwrap();
        let person2 = v2.get_or_assign("PERSON", "Alice").unwrap();
        let email2 = v2.get_or_assign("EMAIL", "a@x.com").unwrap();

        // Same value, same key, different vault → independent random tokens.
        assert_ne!(
            person, person2,
            "independent vaults must not produce linkable placeholders"
        );
        assert_ne!(
            email, email2,
            "independent vaults must not produce linkable placeholders"
        );
    }

    #[test]
    fn test_lookup_roundtrip() {
        let dir = TempDir::new().unwrap();
        let v = Vault::open(&dir.path().join("v.db"), &test_key()).unwrap();
        let p = v.get_or_assign("PERSON", "Alice").unwrap();
        assert_eq!(v.lookup_placeholder(&p).unwrap().as_deref(), Some("Alice"));
    }

    #[test]
    fn test_lookup_roundtrip_unicode() {
        let dir = TempDir::new().unwrap();
        let v = Vault::open(&dir.path().join("v.db"), &test_key()).unwrap();
        let value = "Zoë Müller — 北京 🦀";
        let p = v.get_or_assign("PERSON", value).unwrap();
        assert_eq!(v.lookup_placeholder(&p).unwrap().as_deref(), Some(value));
    }

    #[test]
    fn test_lookup_unknown_placeholder_is_none() {
        let dir = TempDir::new().unwrap();
        let v = Vault::open(&dir.path().join("v.db"), &test_key()).unwrap();
        assert!(v.lookup_placeholder("PERSON_999").unwrap().is_none());
    }

    #[test]
    fn test_wrong_key_on_reopen_is_bad_key_and_db_undamaged() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("v.db");
        let placeholder = {
            let v = Vault::open(&path, &test_key()).unwrap();
            v.get_or_assign("PERSON", "Alice").unwrap()
        };

        // Wrong key → BadKey.
        let err = Vault::open(&path, &other_key()).unwrap_err();
        assert!(
            matches!(err, VaultError::BadKey),
            "expected BadKey, got {err:?}"
        );

        // DB undamaged: correct key still works and data is intact.
        let v = Vault::open(&path, &test_key()).unwrap();
        assert_eq!(
            v.lookup_placeholder(&placeholder).unwrap().as_deref(),
            Some("Alice")
        );
        assert_eq!(v.len().unwrap(), 1);
    }

    #[test]
    fn test_concurrent_get_or_assign_same_new_value() {
        let dir = TempDir::new().unwrap();
        let v = Arc::new(Vault::open(&dir.path().join("v.db"), &test_key()).unwrap());

        let mut handles = Vec::new();
        for _ in 0..8 {
            let v = Arc::clone(&v);
            handles.push(std::thread::spawn(move || {
                v.get_or_assign("PERSON", "Alice").unwrap()
            }));
        }
        let results: Vec<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        // All threads see the SAME placeholder.
        let first = &results[0];
        assert!(
            results.iter().all(|r| r == first),
            "all placeholders must match: {results:?}"
        );
        // Assigned exactly once → exactly one entity.
        assert_eq!(v.len().unwrap(), 1);
        assert!(first.starts_with("PERSON_"));
    }

    #[test]
    fn test_plaintext_absent_from_db_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("v.db");
        let secret = "SuperSecretPlaintextValue12345";
        {
            let v = Vault::open(&path, &test_key()).unwrap();
            v.get_or_assign("PERSON", secret).unwrap();
        }
        // Read raw DB bytes (and WAL, if present) and assert the plaintext is
        // not present anywhere on disk.
        let mut bytes = std::fs::read(&path).unwrap();
        let wal = path.with_extension("db-wal");
        if wal.exists() {
            bytes.extend_from_slice(&std::fs::read(&wal).unwrap());
        }
        let needle = secret.as_bytes();
        let found = bytes.windows(needle.len()).any(|w| w == needle);
        assert!(!found, "plaintext value must not appear in DB file bytes");
    }

    #[test]
    fn test_len_and_is_empty() {
        let dir = TempDir::new().unwrap();
        let v = Vault::open(&dir.path().join("v.db"), &test_key()).unwrap();
        assert_eq!(v.len().unwrap(), 0);
        assert!(v.is_empty().unwrap());
        v.get_or_assign("PERSON", "Alice").unwrap();
        assert_eq!(v.len().unwrap(), 1);
        assert!(!v.is_empty().unwrap());
    }
}
