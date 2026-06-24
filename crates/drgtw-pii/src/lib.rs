//! Reversible PII pseudonymization: detection, entity mapping, restore.
//!
//! Public API contract (Phase 3 / WP 3.1–3.3). Frozen — extend, don't break.
//! WP 9.2 adds [`store::EntityStore`] for cross-request placeholder stability.
//!
//! Pipeline: [`PiiEngine::scan`] finds [`Detection`]s → [`EntityMap`] assigns
//! stable placeholders and rewrites text → request leaves with `EMAIL_1` etc.
//! → response passes through [`EntityMap::restore`] (full body) or
//! [`StreamRestorer`] (SSE chunks) to put originals back.
//!
//! # Persistent vault integration (WP 9.2)
//!
//! Construct [`EntityMap::with_store`] to enable cross-request stability.
//! Use [`body::try_pseudonymize_body`] (already stores via the map) and
//! [`body::restore_body_with_store`] for full round-trip support including
//! placeholders from past requests.
//!
//! WP 9.3 adds [`vault_store::VaultStore`], the concrete [`EntityStore`] adapter
//! over the SQLite-backed `drgtw-vault` crate, so the proxy can plug a
//! persistent vault into the request pipeline.

use std::fmt;
use std::sync::Arc;

pub mod body;
pub mod engine;
pub mod entity_map;
pub mod ner_bridge;
pub mod recognizers;
pub mod store;
pub mod stream;
pub mod vault_store;

pub use body::{
    BodyFormat, collect_request_text, collect_response_text, pseudonymize_body, restore_body,
    restore_body_with_store, try_pseudonymize_body,
};
pub use engine::{EngineBuildError, EngineError, PiiEngine, build_engine_with_ner};
pub use entity_map::EntityMap;
pub use ner_bridge::NerRecognizer;
pub use store::{EntityStore, StoreError};
pub use stream::StreamRestorer;
pub use vault_store::VaultStore;

/// A detected PII span. Byte offsets into the scanned string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detection {
    pub start: usize,
    pub end: usize,
    pub kind: EntityKind,
}

/// Entity categories. Person/Org/Location come from NER (Phase 4);
/// IpAddress/DateTime are built-in regex recognizers (v0.0.8); NationalId/Nrp
/// have no built-in detector and are reachable only via `custom_recognizers`
/// or a future NER model.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum EntityKind {
    Email,
    Phone,
    Iban,
    CreditCard,
    Person,
    Org,
    Location,
    IpAddress,
    DateTime,
    NationalId,
    Nrp,
    /// From config `pii.custom_recognizers`; value = recognizer name.
    Custom(Arc<str>),
}

impl EntityKind {
    /// Placeholder prefix: `EMAIL`, `PHONE`, `IBAN`, `CARD`, `PERSON`, `ORG`,
    /// `LOCATION`, `IP`, `DATE`, `NID`, `NRP`; custom kinds use their uppercased
    /// name.
    pub fn placeholder_prefix(&self) -> String {
        match self {
            EntityKind::Email => "EMAIL".into(),
            EntityKind::Phone => "PHONE".into(),
            EntityKind::Iban => "IBAN".into(),
            EntityKind::CreditCard => "CARD".into(),
            EntityKind::Person => "PERSON".into(),
            EntityKind::Org => "ORG".into(),
            EntityKind::Location => "LOCATION".into(),
            EntityKind::IpAddress => "IP".into(),
            EntityKind::DateTime => "DATE".into(),
            EntityKind::NationalId => "NID".into(),
            EntityKind::Nrp => "NRP".into(),
            EntityKind::Custom(name) => name.to_uppercase(),
        }
    }

    /// Map a canonical config entity name (presidio-style, e.g. produced by
    /// [`drgtw_config::canonical_pii_entity_name`]) to its built-in
    /// [`EntityKind`]. Returns `None` for `NATIONAL_ID`/`NRP` (no built-in
    /// kind backs a bare name — those arrive via custom recognizers) and for
    /// unknown names.
    pub fn from_canonical_name(canon: &str) -> Option<EntityKind> {
        match canon {
            "EMAIL_ADDRESS" => Some(EntityKind::Email),
            "PHONE_NUMBER" => Some(EntityKind::Phone),
            "IBAN_CODE" => Some(EntityKind::Iban),
            "CREDIT_CARD" => Some(EntityKind::CreditCard),
            "PERSON" => Some(EntityKind::Person),
            "ORGANIZATION" => Some(EntityKind::Org),
            "LOCATION" => Some(EntityKind::Location),
            "IP_ADDRESS" => Some(EntityKind::IpAddress),
            "DATE_TIME" => Some(EntityKind::DateTime),
            "NATIONAL_ID" => Some(EntityKind::NationalId),
            "NRP" => Some(EntityKind::Nrp),
            _ => None,
        }
    }
}

/// Error returned by the fallible scan path and by NER inference failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectError(pub String);

impl fmt::Display for DetectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "detect error: {}", self.0)
    }
}

impl std::error::Error for DetectError {}

/// A PII detector. Implementations must be cheap to call per request and
/// thread-safe; compile regexes once at construction.
pub trait Recognizer: Send + Sync {
    /// Recognizer name (matches config `disabled_recognizers` entries).
    fn name(&self) -> &str;

    /// Return all detections in `text`. Overlap resolution happens in the
    /// engine — recognizers report everything they see.
    ///
    /// Infallible by design; recognizers that may fail (e.g. NER) should
    /// implement [`try_detect`] and fall back gracefully here.
    fn detect(&self, text: &str) -> Vec<Detection>;

    /// Fallible variant of [`detect`]. The default implementation wraps
    /// [`detect`] so that all existing recognizers remain backward-compatible.
    ///
    /// Recognizers that can fail (e.g. NER) override this method and implement
    /// [`detect`] as `self.try_detect(text).unwrap_or_default()`.
    fn try_detect(&self, text: &str) -> Result<Vec<Detection>, DetectError> {
        Ok(self.detect(text))
    }
}
