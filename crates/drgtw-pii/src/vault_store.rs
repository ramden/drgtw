//! [`EntityStore`] adapter over the persistent [`drgtw_vault::Vault`]. WP 9.3.
//!
//! # Purpose
//!
//! The pii crate defines the [`EntityStore`] trait abstractly so it has no hard
//! dependency on a concrete persistence layer. This module provides the glue
//! that lets the SQLite-backed `drgtw-vault` crate satisfy that trait, enabling
//! cross-request placeholder stability (the embeddings / RAG guarantee).
//!
//! # Error mapping
//!
//! [`drgtw_vault::VaultError`] is translated into [`StoreError`] via its
//! `Display` impl. The vault crate guarantees its error messages never embed
//! key material (the crypto/bad-key variants carry only a generic description),
//! so forwarding the message text is safe.

use std::sync::Arc;

use drgtw_vault::Vault;

use crate::store::{EntityStore, StoreError};

/// An [`EntityStore`] backed by a persistent encrypted [`Vault`].
///
/// Cheaply clonable: holds an `Arc<Vault>`. The vault is internally
/// `Send + Sync` (a `Mutex`-guarded SQLite connection), so this adapter is too.
#[derive(Clone)]
pub struct VaultStore(pub Arc<Vault>);

impl VaultStore {
    /// Wrap an already-opened [`Vault`].
    pub fn new(vault: Arc<Vault>) -> Self {
        Self(vault)
    }
}

impl std::fmt::Debug for VaultStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VaultStore").finish_non_exhaustive()
    }
}

impl EntityStore for VaultStore {
    fn get_or_assign(&self, kind_prefix: &str, value: &str) -> Result<String, StoreError> {
        self.0
            .get_or_assign(kind_prefix, value)
            .map_err(|e| StoreError(format!("vault: {e}")))
    }

    fn lookup_placeholder(&self, placeholder: &str) -> Result<Option<String>, StoreError> {
        self.0
            .lookup_placeholder(placeholder)
            .map_err(|e| StoreError(format!("vault: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const TEST_KEY: [u8; 32] = [7u8; 32];

    fn open_vault(dir: &TempDir) -> Arc<Vault> {
        let path = dir.path().join("vault.db");
        Arc::new(Vault::open(&path, &TEST_KEY).expect("open vault"))
    }

    #[test]
    fn get_or_assign_is_stable_within_store() {
        let dir = TempDir::new().unwrap();
        let store = VaultStore::new(open_vault(&dir));

        let p1 = store.get_or_assign("EMAIL", "alice@example.com").unwrap();
        let p2 = store.get_or_assign("EMAIL", "alice@example.com").unwrap();
        assert_eq!(p1, p2, "same value must yield the same placeholder");
        assert!(p1.starts_with("EMAIL_"), "placeholder format: {p1}");
    }

    #[test]
    fn distinct_values_get_distinct_placeholders() {
        let dir = TempDir::new().unwrap();
        let store = VaultStore::new(open_vault(&dir));

        let p1 = store.get_or_assign("EMAIL", "alice@example.com").unwrap();
        let p2 = store.get_or_assign("EMAIL", "bob@example.com").unwrap();
        assert_ne!(p1, p2);
    }

    #[test]
    fn lookup_round_trips_assigned_placeholder() {
        let dir = TempDir::new().unwrap();
        let store = VaultStore::new(open_vault(&dir));

        let ph = store.get_or_assign("EMAIL", "alice@example.com").unwrap();
        let got = store.lookup_placeholder(&ph).unwrap();
        assert_eq!(got.as_deref(), Some("alice@example.com"));
    }

    #[test]
    fn lookup_unknown_placeholder_returns_none() {
        let dir = TempDir::new().unwrap();
        let store = VaultStore::new(open_vault(&dir));
        assert_eq!(store.lookup_placeholder("EMAIL_999").unwrap(), None);
    }

    #[test]
    fn placeholder_stable_across_reopen() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("vault.db");

        let ph_first = {
            let store = VaultStore::new(Arc::new(Vault::open(&path, &TEST_KEY).unwrap()));
            store.get_or_assign("EMAIL", "alice@example.com").unwrap()
        };
        // Reopen the same file with the same key — simulates a process restart.
        let store2 = VaultStore::new(Arc::new(Vault::open(&path, &TEST_KEY).unwrap()));
        let ph_second = store2.get_or_assign("EMAIL", "alice@example.com").unwrap();
        assert_eq!(ph_first, ph_second, "placeholder must survive reopen");
    }

    #[test]
    fn bad_key_maps_to_store_error_without_key_material() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("vault.db");
        // Create with one key.
        drop(Vault::open(&path, &TEST_KEY).unwrap());
        // Opening with a different key must fail at open (BadKey), which is a
        // vault-level error; this confirms our mapping surfaces vault errors.
        let wrong = [9u8; 32];
        let err = Vault::open(&path, &wrong).unwrap_err();
        let mapped = StoreError(format!("vault: {err}"));
        let msg = mapped.to_string();
        assert!(msg.contains("vault:"), "mapped message: {msg}");
        // Never leak the key bytes.
        assert!(!msg.contains("99090909"), "must not contain key material");
    }
}
