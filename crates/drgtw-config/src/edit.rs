//! Safe UI-editing API for drgtw-config TOML files.
//!
//! Provides document-level read/write helpers that preserve comments and
//! `${ENV_VAR}` literals, plus a UI-mode validator and an atomic write with
//! backup.  The existing `load()` / `validate()` path is untouched.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use toml_edit::{DocumentMut, Item, Table, Value};

use crate::{Config, ConfigError};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A validation error for a specific config field, suitable for inline UI
/// display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldError {
    /// Dotted TOML path, e.g. `"server.bind_addr"` or `"connections[0].name"`.
    pub path: String,
    /// Human-readable description of what is wrong.
    pub message: String,
}

impl FieldError {
    fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for FieldError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

// ---------------------------------------------------------------------------
// read_document
// ---------------------------------------------------------------------------

/// Read a TOML file as an editable [`DocumentMut`].
///
/// Preserves all comments, whitespace, and `${ENV_VAR}` literals verbatim —
/// no env-var resolution is performed.  Returns [`ConfigError::Io`] if the
/// file cannot be read, or [`ConfigError::Invalid`] if the file is not valid
/// TOML (the parse error text is included in the message).
pub fn read_document(path: &Path) -> Result<DocumentMut, ConfigError> {
    let path_str = path.display().to_string();
    let raw = std::fs::read_to_string(path).map_err(|e| ConfigError::Io {
        path: path_str.clone(),
        source: e,
    })?;
    raw.parse::<DocumentMut>().map_err(|e| ConfigError::Invalid(format!(
        "cannot parse `{path_str}` as TOML: {e}"
    )))
}

// ---------------------------------------------------------------------------
// set_value
// ---------------------------------------------------------------------------

/// Set a scalar at a dotted path inside a [`DocumentMut`].
///
/// # Path syntax
///
/// - Simple dotted path: `"server.bind_addr"`, `"pii.enabled_by_default"`.
/// - Array-of-tables index: `"connections.0.base_url"` addresses the first
///   element of the `[[connections]]` array.
///
/// The value type (bool / integer / string) is inferred from the **existing**
/// item at the target.  When no existing item is present the value is stored
/// as a string.  Supported coercions:
/// - Existing bool → parse `"true"` / `"false"` (case-insensitive).
/// - Existing integer → parse with `str::parse::<i64>`.
/// - Anything else → store as a string.
///
/// # Errors
///
/// Returns [`ConfigError::Invalid`] when:
/// - The path is empty.
/// - An intermediate segment that should be a table is not one.
/// - An array index is out of bounds or the target is not an array of tables.
/// - The target field's type is bool but the value cannot be parsed as a bool.
/// - The target field's type is integer but the value cannot be parsed as an
///   integer.
pub fn set_value(
    doc: &mut DocumentMut,
    dotted_path: &str,
    value: &str,
) -> Result<(), ConfigError> {
    if dotted_path.is_empty() {
        return Err(ConfigError::Invalid("dotted_path must not be empty".to_owned()));
    }

    let segments: Vec<&str> = dotted_path.split('.').collect();

    // Walk / build the table hierarchy down to the parent of the target key,
    // then set the last segment.
    set_value_in_table(doc.as_table_mut(), &segments, value)
}

fn set_value_in_table(
    table: &mut Table,
    segments: &[&str],
    value: &str,
) -> Result<(), ConfigError> {
    debug_assert!(!segments.is_empty());

    let key = segments[0];

    if segments.len() == 1 {
        // Leaf: coerce to the existing item's type when present, otherwise
        // infer the most specific scalar type the string represents so a
        // freshly-added numeric/bool key isn't written as a quoted string.
        let new_item = if let Some(existing) = table.get(key) {
            coerce_value(existing, value, key)?
        } else {
            infer_value(value)
        };
        table.insert(key, new_item);
        return Ok(());
    }

    // Intermediate segment — could be a table or an array-of-tables.
    let rest = &segments[1..];

    // If the next segment after `key` is a decimal integer, treat `key` as an
    // array-of-tables and the integer as a 0-based index.
    if let (Some(&idx_str), more) = (rest.first(), &rest[1..]) {
        if let Ok(idx) = idx_str.parse::<usize>() {
            // Array-of-tables path: key.N.rest…
            let array_item = table.entry(key).or_insert_with(|| {
                Item::ArrayOfTables(toml_edit::ArrayOfTables::new())
            });
            match array_item {
                Item::ArrayOfTables(arr) => {
                    let len = arr.len();
                    let entry = arr.get_mut(idx).ok_or_else(|| {
                        ConfigError::Invalid(format!(
                            "array `{key}` index {idx} is out of bounds (len {len})"
                        ))
                    })?;
                    return set_value_in_table(entry, more, value);
                }
                _ => {
                    return Err(ConfigError::Invalid(format!(
                        "`{key}` is not an array of tables; cannot index with `{idx_str}`"
                    )));
                }
            }
        }
    }

    // Regular table traversal.
    let child = table.entry(key).or_insert_with(|| {
        Item::Table(Table::new())
    });
    match child {
        Item::Table(t) => set_value_in_table(t, rest, value),
        _ => Err(ConfigError::Invalid(format!(
            "`{key}` exists but is not a table; cannot traverse into it"
        ))),
    }
}

/// Infer the most specific TOML scalar a string represents, for keys that do
/// not yet exist in the document (so there is no existing type to coerce to).
/// Order: bool → integer → float → string. `${ENV_VAR}` placeholders and
/// anything non-numeric fall through to a string.
fn infer_value(raw: &str) -> Item {
    match raw.to_ascii_lowercase().as_str() {
        "true" => return Item::Value(Value::from(true)),
        "false" => return Item::Value(Value::from(false)),
        _ => {}
    }
    if let Ok(i) = raw.parse::<i64>() {
        return Item::Value(Value::from(i));
    }
    if let Ok(f) = raw.parse::<f64>() {
        return Item::Value(Value::from(f));
    }
    Item::Value(Value::from(raw))
}

/// Produce a new [`Item`] by coercing `raw` to the type implied by `existing`.
fn coerce_value(existing: &Item, raw: &str, key: &str) -> Result<Item, ConfigError> {
    match existing {
        Item::Value(Value::Boolean(_)) => {
            let b = match raw.to_ascii_lowercase().as_str() {
                "true" => true,
                "false" => false,
                other => {
                    return Err(ConfigError::Invalid(format!(
                        "field `{key}` is a bool; expected `true` or `false`, got `{other}`"
                    )));
                }
            };
            Ok(Item::Value(Value::Boolean(toml_edit::Formatted::new(b))))
        }
        Item::Value(Value::Integer(_)) => {
            let n: i64 = raw.parse().map_err(|_| {
                ConfigError::Invalid(format!(
                    "field `{key}` is an integer; `{raw}` is not a valid integer"
                ))
            })?;
            Ok(Item::Value(Value::Integer(toml_edit::Formatted::new(n))))
        }
        // Float, string, datetime — store as string (or pass through float
        // conversion) to keep it simple.  The validator will catch type errors.
        _ => Ok(Item::Value(Value::from(raw))),
    }
}

// ---------------------------------------------------------------------------
// validate_str  (UI mode)
// ---------------------------------------------------------------------------

/// Parse candidate TOML text and validate it in UI mode.
///
/// Unlike `load()`:
/// - No env-var resolution is performed; `${VAR}` placeholders are left as-is.
/// - Checks that require a resolved secret are skipped when the field still
///   holds a `${…}` placeholder (currently: `pii.vault.key` 64-hex check).
///
/// Returns `Ok(Config)` on success, or a `Vec<FieldError>` so the UI can show
/// inline errors next to each offending field.
pub fn validate_str(toml_text: &str) -> Result<Config, Vec<FieldError>> {
    let config: Config = toml::from_str(toml_text).map_err(|e| {
        vec![FieldError::new("(document)", format!("TOML parse error: {e}"))]
    })?;

    validate_inner(&config, true).map_err(|e| match e {
        ConfigError::Invalid(msg) => vec![field_error_from_message(msg)],
        ConfigError::MissingEnvVar { var, field } => vec![FieldError::new(
            field,
            format!("environment variable `{var}` is not set"),
        )],
        other => vec![FieldError::new("(document)", other.to_string())],
    })?;

    Ok(config)
}

/// Extract a best-effort dotted path from a validation error message.
///
/// Validation messages embed field names in a human-readable form.  This
/// function peels off a leading path token so the UI can highlight the right
/// field.  When no path can be inferred the path is `"(document)"`.
fn field_error_from_message(msg: String) -> FieldError {
    // Convention: most messages start with the dotted field name before a
    // space or backtick.  We grab everything up to the first backtick or space
    // as the path hint.
    let path = msg
        .split_once(' ')
        .map(|(p, _)| p.trim_matches('`'))
        .unwrap_or("(document)")
        .to_owned();
    FieldError::new(path, msg)
}

// ---------------------------------------------------------------------------
// write_safe
// ---------------------------------------------------------------------------

/// Write a new TOML config atomically, with validation and a timestamped
/// backup.
///
/// Steps:
/// 1. `validate_str(new_toml_text)` — reject on validation errors (original
///    file is never touched).
/// 2. Copy the current file to `<path>.bak.<unix_seconds>`.
/// 3. Write `new_toml_text` to a temp file in the **same directory** as
///    `path`.
/// 4. `fsync` the temp file.
/// 5. Atomically `rename` the temp file over `path`.
///
/// On any failure after step 2 the backup survives; the original is
/// untouched if the failure is in steps 3-5 (rename has not happened yet).
pub fn write_safe(path: &Path, new_toml_text: &str) -> Result<(), ConfigError> {
    // 1. Validate first — bail before touching the filesystem.
    validate_str(new_toml_text).map_err(|errors| {
        let summary = errors
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        ConfigError::Invalid(format!("validation failed: {summary}"))
    })?;

    // 2. Determine backup path.
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let bak_path = path.with_extension(format!("bak.{secs}"));

    // 2b. Copy current file to backup (best-effort; skip if file does not yet
    //     exist — first write).
    if path.exists() {
        std::fs::copy(path, &bak_path).map_err(|e| ConfigError::Io {
            path: bak_path.display().to_string(),
            source: e,
        })?;
    }

    // 3. Write to a temp file in the SAME directory so rename stays on the
    //    same filesystem (cross-device rename would fail).
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::Builder::new()
        .prefix(".drgtw-config-tmp-")
        .suffix(".toml")
        .tempfile_in(dir)
        .map_err(|e| ConfigError::Io {
            path: dir.display().to_string(),
            source: e,
        })?;

    use std::io::Write as _;
    tmp.write_all(new_toml_text.as_bytes()).map_err(|e| ConfigError::Io {
        path: dir.display().to_string(),
        source: e,
    })?;

    // 4. fsync.
    tmp.as_file().sync_all().map_err(|e| ConfigError::Io {
        path: dir.display().to_string(),
        source: e,
    })?;

    // 5. Atomic rename.
    let (_, tmp_path) = tmp.keep().map_err(|e| ConfigError::Io {
        path: dir.display().to_string(),
        source: e.into(),
    })?;
    std::fs::rename(&tmp_path, path).map_err(|e| {
        // Best-effort cleanup of the temp file.
        let _ = std::fs::remove_file(&tmp_path);
        ConfigError::Io {
            path: path.display().to_string(),
            source: e,
        }
    })?;

    Ok(())
}

// ---------------------------------------------------------------------------
// restart_required_changes
// ---------------------------------------------------------------------------

/// Return the names of config sections whose values changed between `old` and
/// `new` and that require a gateway restart to take effect.
///
/// Restart-required sections: `server`, `connections`, `virtual_keys`, `pii`,
/// `vault`, `mcp_servers`.
///
/// Hot-reloadable (no restart): `model_aliases`, `events`, `tracing`, `otel`.
pub fn restart_required_changes<'a>(old: &Config, new: &Config) -> Vec<&'static str> {
    let mut changed = Vec::new();

    if !configs_server_eq(old, new) {
        changed.push("server");
    }
    if !configs_connections_eq(old, new) {
        changed.push("connections");
    }
    if !configs_virtual_keys_eq(old, new) {
        changed.push("virtual_keys");
    }
    if !configs_pii_eq(old, new) {
        changed.push("pii");
    }
    if !configs_vault_eq(old, new) {
        changed.push("vault");
    }
    if !configs_mcp_servers_eq(old, new) {
        changed.push("mcp_servers");
    }

    changed
}

// --- comparison helpers (deliberately minimal — PartialEq not derived on Config)
//
// These compare only the fields that matter for restart detection.  They use
// string-serialised TOML via the `toml` crate for the complex nested types
// that do not implement `PartialEq`, but only on the types that DO implement
// `serde::Serialize`.  For types that only derive `Deserialize` we fall back
// to field-by-field comparison.

fn configs_server_eq(a: &Config, b: &Config) -> bool {
    a.server.bind_addr == b.server.bind_addr
        && a.server.max_body_bytes == b.server.max_body_bytes
}

fn configs_connections_eq(a: &Config, b: &Config) -> bool {
    // Connection does not derive PartialEq or Serialize; compare length then
    // field-by-field on the fields that are meaningful for restart detection.
    if a.connections.len() != b.connections.len() {
        return false;
    }
    a.connections.iter().zip(b.connections.iter()).all(|(ac, bc)| {
        ac.name == bc.name
            && ac.base_url == bc.base_url
            && ac.api_key == bc.api_key
            && ac.format == bc.format
            && ac.models == bc.models
            && ac.region == bc.region
            && ac.aws_access_key_id == bc.aws_access_key_id
            && ac.aws_secret_access_key == bc.aws_secret_access_key
            && ac.aws_session_token == bc.aws_session_token
            && model_costs_eq(&ac.model_costs, &bc.model_costs)
    })
}

fn model_costs_eq(
    a: &std::collections::HashMap<String, crate::ModelCost>,
    b: &std::collections::HashMap<String, crate::ModelCost>,
) -> bool {
    a.len() == b.len()
        && a.iter()
            .all(|(k, v)| b.get(k).map(|bv| bv == v).unwrap_or(false))
}

fn configs_virtual_keys_eq(a: &Config, b: &Config) -> bool {
    if a.virtual_keys.len() != b.virtual_keys.len() {
        return false;
    }
    a.virtual_keys.iter().zip(b.virtual_keys.iter()).all(|(av, bv)| {
        av.key == bv.key
            && av.connections == bv.connections
            && av.models == bv.models
            && av.rate_limit == bv.rate_limit
            && av.budget == bv.budget
    })
}

fn configs_pii_eq(a: &Config, b: &Config) -> bool {
    a.pii.enabled_by_default == b.pii.enabled_by_default
        && a.pii.disabled_recognizers == b.pii.disabled_recognizers
        && a.pii.embeddings_require_vault == b.pii.embeddings_require_vault
        && custom_recognizers_eq(&a.pii.custom_recognizers, &b.pii.custom_recognizers)
        && ner_config_eq(&a.pii.ner, &b.pii.ner)
}

fn custom_recognizers_eq(
    a: &[crate::CustomRecognizer],
    b: &[crate::CustomRecognizer],
) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b.iter())
            .all(|(ar, br)| ar.name == br.name && ar.pattern == br.pattern)
}

fn ner_config_eq(a: &Option<crate::NerConfig>, b: &Option<crate::NerConfig>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(an), Some(bn)) => {
            an.model_dir == bn.model_dir
                && an.score_threshold == bn.score_threshold
                && an.fail_mode == bn.fail_mode
                && an.timeout_ms == bn.timeout_ms
                && an.workers == bn.workers
                && an.queue_capacity == bn.queue_capacity
        }
        _ => false,
    }
}

fn configs_vault_eq(a: &Config, b: &Config) -> bool {
    match (&a.pii.vault, &b.pii.vault) {
        (None, None) => true,
        (Some(av), Some(bv)) => av.path == bv.path && av.key == bv.key,
        _ => false,
    }
}

fn configs_mcp_servers_eq(a: &Config, b: &Config) -> bool {
    if a.mcp_servers.len() != b.mcp_servers.len() {
        return false;
    }
    a.mcp_servers.iter().all(|(name, as_)| {
        b.mcp_servers.get(name).map(|bs| {
            as_.url == bs.url
                && as_.auth_type == bs.auth_type
                && as_.auth_value == bs.auth_value
                && as_.extra_headers == bs.extra_headers
                && as_.description == bs.description
        }).unwrap_or(false)
    })
}

// ---------------------------------------------------------------------------
// validate_inner — shared validation logic
// ---------------------------------------------------------------------------
//
// `ui_mode = true`  → skip checks that require a resolved secret value
//                     (e.g. pii.vault.key 64-hex when the value is `${...}`).
// `ui_mode = false` → full validation, identical to the original `validate()`.
//
// The public `validate()` function calls this with `ui_mode = false`.

pub(crate) fn validate_inner(config: &Config, ui_mode: bool) -> Result<(), ConfigError> {
    use std::collections::HashSet;

    // --- Server ---
    if config.server.max_body_bytes == 0 {
        return Err(ConfigError::Invalid(
            "server.max_body_bytes must be > 0".to_owned(),
        ));
    }

    // --- Connections ---
    let mut conn_names: HashSet<&str> = HashSet::new();
    for conn in &config.connections {
        if conn.name.is_empty() {
            return Err(ConfigError::Invalid(
                "connection name must not be empty".to_owned(),
            ));
        }
        if !conn_names.insert(conn.name.as_str()) {
            return Err(ConfigError::Invalid(format!(
                "duplicate connection name `{}`",
                conn.name
            )));
        }
        validate_base_url_inner(&conn.base_url, &conn.name)?;

        let akid = conn.aws_access_key_id.as_deref().filter(|s| !s.is_empty());
        let secret = conn
            .aws_secret_access_key
            .as_deref()
            .filter(|s| !s.is_empty());
        let token = conn.aws_session_token.as_deref().filter(|s| !s.is_empty());
        let has_region = conn.region.as_deref().is_some_and(|s| !s.is_empty());
        let has_sigv4 = akid.is_some() && secret.is_some();

        if akid.is_some() != secret.is_some() {
            return Err(ConfigError::Invalid(format!(
                "connections[{}]: aws_access_key_id and aws_secret_access_key must be set together",
                conn.name
            )));
        }
        if token.is_some() && !has_sigv4 {
            return Err(ConfigError::Invalid(format!(
                "connections[{}]: aws_session_token requires aws_access_key_id and aws_secret_access_key",
                conn.name
            )));
        }
        if has_sigv4 && !has_region {
            return Err(ConfigError::Invalid(format!(
                "connections[{}]: region is required for SigV4 Bedrock signing",
                conn.name
            )));
        }

        if conn.format == crate::ApiFormat::BedrockConverse {
            if !has_sigv4 && conn.api_key.is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "connections[{}]: bedrock_converse requires either aws_access_key_id+aws_secret_access_key or api_key",
                    conn.name
                )));
            }
        } else if conn.api_key.is_empty() {
            return Err(ConfigError::Invalid(format!(
                "connections[{}].api_key must not be empty",
                conn.name
            )));
        }

        let mut model_names: HashSet<&str> = HashSet::new();
        for model in &conn.models {
            let ctx = format!("connections[{}].models", conn.name);
            validate_model_pattern_inner(model, &ctx)?;
            if !model_names.insert(model.as_str()) {
                return Err(ConfigError::Invalid(format!(
                    "connections[{}].models contains duplicate `{}`",
                    conn.name, model
                )));
            }
        }

        for (key, cost) in &conn.model_costs {
            let ctx = format!("connections[{}].model_costs", conn.name);
            if key.is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "{ctx}: model cost key must not be empty"
                )));
            }
            validate_model_pattern_inner(key, &ctx)?;
            if !cost.input_per_1m.is_finite() || cost.input_per_1m < 0.0 {
                return Err(ConfigError::Invalid(format!(
                    "{ctx}[\"{key}\"].input_per_1m must be a finite value >= 0"
                )));
            }
            if !cost.output_per_1m.is_finite() || cost.output_per_1m < 0.0 {
                return Err(ConfigError::Invalid(format!(
                    "{ctx}[\"{key}\"].output_per_1m must be a finite value >= 0"
                )));
            }
        }
    }

    // --- Virtual keys ---
    let mut vk_keys: HashSet<&str> = HashSet::new();
    for vk in &config.virtual_keys {
        if !vk.key.starts_with(crate::VIRTUAL_KEY_PREFIX)
            || vk.key.len() <= crate::VIRTUAL_KEY_PREFIX.len()
        {
            return Err(ConfigError::Invalid(format!(
                "virtual key `{}` must start with `{}` and have additional characters",
                vk.key,
                crate::VIRTUAL_KEY_PREFIX
            )));
        }
        if !vk_keys.insert(vk.key.as_str()) {
            return Err(ConfigError::Invalid(format!(
                "duplicate virtual key `{}`",
                vk.key
            )));
        }
        if vk.connections.is_empty() {
            return Err(ConfigError::Invalid(format!(
                "virtual key `{}` has an empty connections list",
                vk.key
            )));
        }
        for conn_name in &vk.connections {
            if !conn_names.contains(conn_name.as_str()) {
                return Err(ConfigError::Invalid(format!(
                    "virtual key `{}` references unknown connection `{}`",
                    vk.key, conn_name
                )));
            }
        }
        if let Some(models) = &vk.models {
            if models.is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "virtual key `{}` has an empty models allowlist; omit the field to allow all",
                    vk.key
                )));
            }
            let ctx = format!("virtual key `{}`  models allowlist", vk.key);
            for pattern in models {
                validate_model_pattern_inner(pattern, &ctx)?;
            }
        }
        if let Some(rl) = &vk.rate_limit {
            if rl.requests == 0 {
                return Err(ConfigError::Invalid(format!(
                    "virtual key `{}` rate_limit.requests must be > 0",
                    vk.key
                )));
            }
            if rl.per_seconds == 0 {
                return Err(ConfigError::Invalid(format!(
                    "virtual key `{}` rate_limit.per_seconds must be > 0",
                    vk.key
                )));
            }
        }
        if let Some(budget) = &vk.budget {
            if !budget.max_usd.is_finite() || budget.max_usd <= 0.0 {
                return Err(ConfigError::Invalid(format!(
                    "virtual key `{}` budget.max_usd must be a finite value > 0",
                    vk.key
                )));
            }
            if budget.per_seconds == 0 {
                return Err(ConfigError::Invalid(format!(
                    "virtual key `{}` budget.per_seconds must be > 0",
                    vk.key
                )));
            }
        }
        if let Some(mcp_list) = &vk.mcp_servers {
            if mcp_list.is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "virtual key `{}` mcp_servers allowlist is empty; omit the field to allow all",
                    vk.key
                )));
            }
            for server_name in mcp_list {
                if !config.mcp_servers.contains_key(server_name.as_str()) {
                    return Err(ConfigError::Invalid(format!(
                        "virtual key `{}` mcp_servers references unknown server `{}`",
                        vk.key, server_name
                    )));
                }
            }
        }
    }

    // --- PII custom recognizers ---
    let mut rec_names: HashSet<&str> = HashSet::new();
    for rec in &config.pii.custom_recognizers {
        if rec.name.is_empty()
            || !rec
                .name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err(ConfigError::Invalid(format!(
                "pii.custom_recognizers name `{}` must be non-empty ascii alphanumeric/underscore",
                rec.name
            )));
        }
        if !rec_names.insert(rec.name.as_str()) {
            return Err(ConfigError::Invalid(format!(
                "duplicate pii.custom_recognizers name `{}`",
                rec.name
            )));
        }
        if rec.pattern.is_empty() {
            return Err(ConfigError::Invalid(format!(
                "pii.custom_recognizers `{}` has an empty pattern",
                rec.name
            )));
        }
    }

    // --- PII NER ---
    if let Some(ner) = &config.pii.ner {
        if ner.model_dir.is_empty() {
            return Err(ConfigError::Invalid(
                "pii.ner.model_dir must not be empty".to_owned(),
            ));
        }
        if !(0.0..=1.0).contains(&ner.score_threshold) {
            return Err(ConfigError::Invalid(format!(
                "pii.ner.score_threshold `{}` must be in the range 0.0..=1.0",
                ner.score_threshold
            )));
        }
        if ner.timeout_ms == 0 {
            return Err(ConfigError::Invalid(
                "pii.ner.timeout_ms must be > 0".to_owned(),
            ));
        }
        if ner.workers == 0 {
            return Err(ConfigError::Invalid(
                "pii.ner.workers must be > 0".to_owned(),
            ));
        }
        if ner.queue_capacity == 0 {
            return Err(ConfigError::Invalid(
                "pii.ner.queue_capacity must be > 0".to_owned(),
            ));
        }
    }

    // --- PII vault ---
    if let Some(vault) = &config.pii.vault {
        if vault.path.is_empty() {
            return Err(ConfigError::Invalid(
                "pii.vault.path must not be empty".to_owned(),
            ));
        }
        // In UI mode, skip the 64-hex check when the key is a `${...}`
        // placeholder (the secret has not been resolved).
        let is_placeholder = ui_mode && is_env_placeholder(&vault.key);
        if !is_placeholder {
            let key_ok =
                vault.key.len() == 64 && vault.key.chars().all(|c| c.is_ascii_hexdigit());
            if !key_ok {
                return Err(ConfigError::Invalid(
                    "pii.vault.key must be 64 hex characters".to_owned(),
                ));
            }
        }
    }

    // --- Events ---
    if let Some(events) = &config.events {
        validate_absolute_http_url_inner(&events.url, "events.url")?;
        if events.buffer_size == 0 {
            return Err(ConfigError::Invalid(
                "events.buffer_size must be > 0".to_owned(),
            ));
        }
        if events.timeout_ms == 0 {
            return Err(ConfigError::Invalid(
                "events.timeout_ms must be > 0".to_owned(),
            ));
        }
    }

    // --- MCP servers ---
    for (name, server) in &config.mcp_servers {
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(ConfigError::Invalid(format!(
                "mcp_servers name `{name}` must be non-empty ascii alphanumeric, `_`, or `-`"
            )));
        }
        validate_absolute_http_url_inner(&server.url, &format!("mcp_servers[{name}].url"))?;
        match server.auth_type {
            crate::McpAuthType::None => {
                if server.auth_value.is_some() {
                    return Err(ConfigError::Invalid(format!(
                        "mcp_servers[{name}].auth_value must be absent when auth_type is none"
                    )));
                }
            }
            _ => {
                let ok = server
                    .auth_value
                    .as_deref()
                    .map(|v| !v.is_empty())
                    .unwrap_or(false);
                if !ok {
                    return Err(ConfigError::Invalid(format!(
                        "mcp_servers[{name}].auth_value must be present and non-empty when auth_type is not none"
                    )));
                }
            }
        }
        for header in server.extra_headers.keys() {
            if header.is_empty()
                || !header
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-')
            {
                return Err(ConfigError::Invalid(format!(
                    "mcp_servers[{name}].extra_headers header name `{header}` must be non-empty ascii alphanumeric or `-`"
                )));
            }
        }
        if let Some(auth_value) = &server.auth_value
            && auth_value.chars().any(|c| c.is_ascii_control())
        {
            return Err(ConfigError::Invalid(format!(
                "mcp_servers[{name}].auth_value must not contain control characters"
            )));
        }
        for (header, value) in &server.extra_headers {
            if value.chars().any(|c| c.is_ascii_control()) {
                return Err(ConfigError::Invalid(format!(
                    "mcp_servers[{name}].extra_headers[{header}] must not contain control characters"
                )));
            }
        }
    }

    // --- OTel ---
    if config.otel.enabled {
        if config.otel.endpoint.is_empty() {
            return Err(ConfigError::Invalid(
                "otel.endpoint must not be empty when otel.enabled is true".to_owned(),
            ));
        }
        validate_absolute_http_url_inner(&config.otel.endpoint, "otel.endpoint")?;
        if !(0.0..=1.0).contains(&config.otel.sample_ratio) {
            return Err(ConfigError::Invalid(format!(
                "otel.sample_ratio must be in 0.0..=1.0, got {}",
                config.otel.sample_ratio
            )));
        }
        if config.otel.export_interval_ms == 0 {
            return Err(ConfigError::Invalid(
                "otel.export_interval_ms must be > 0".to_owned(),
            ));
        }
        if config.otel.export_timeout_ms == 0 {
            return Err(ConfigError::Invalid(
                "otel.export_timeout_ms must be > 0".to_owned(),
            ));
        }
    }

    // --- UI ---
    if let Some(history) = &config.ui.history
        && history.postgres_url.is_empty()
    {
        return Err(ConfigError::Invalid(
            "ui.history.postgres_url must not be empty".to_owned(),
        ));
    }

    if let Some(auth) = &config.ui.auth {
        if auth.username.is_empty() {
            return Err(ConfigError::Invalid(
                "ui.auth.username must not be empty".to_owned(),
            ));
        }
        if !auth.password_hash.starts_with("$argon2") {
            return Err(ConfigError::Invalid(
                "ui.auth.password_hash must be an argon2 PHC string (starts with $argon2)".to_owned(),
            ));
        }
        // In UI mode, skip the non-empty check when the key is a `${...}` placeholder.
        let key_is_placeholder = ui_mode && is_env_placeholder(&auth.session_key);
        if !key_is_placeholder && auth.session_key.is_empty() {
            return Err(ConfigError::Invalid(
                "ui.auth.session_key must not be empty".to_owned(),
            ));
        }
    }

    Ok(())
}

/// Returns `true` when the entire value is a single `${VAR}` placeholder.
fn is_env_placeholder(s: &str) -> bool {
    s.starts_with("${") && s.ends_with('}') && s.len() > 3
}

// Copies of the private helpers from lib.rs, used by validate_inner.

fn validate_model_pattern_inner(pattern: &str, context: &str) -> Result<(), ConfigError> {
    if pattern.is_empty() {
        return Err(ConfigError::Invalid(format!(
            "{context} contains an empty string"
        )));
    }
    let star_count = pattern.chars().filter(|&c| c == '*').count();
    if star_count > 1 {
        return Err(ConfigError::Invalid(format!(
            "{context}: model pattern `{pattern}` contains more than one `*`"
        )));
    }
    if star_count == 1 && !pattern.ends_with('*') {
        return Err(ConfigError::Invalid(format!(
            "{context}: model pattern `{pattern}` has `*` in non-terminal position; `*` may only appear at the end"
        )));
    }
    Ok(())
}

fn validate_base_url_inner(url_str: &str, conn_name: &str) -> Result<(), ConfigError> {
    let field = format!("connections[{}].base_url", conn_name);
    validate_absolute_http_url_inner(url_str, &field)
}

fn validate_absolute_http_url_inner(url_str: &str, field: &str) -> Result<(), ConfigError> {
    use url::Url;
    let url = Url::parse(url_str).map_err(|_| {
        ConfigError::Invalid(format!("{field} `{url_str}` is not a valid URL"))
    })?;
    let scheme = url.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(ConfigError::Invalid(format!(
            "{field} `{url_str}` must use http or https scheme"
        )));
    }
    if !url.host_str().map(|h| !h.is_empty()).unwrap_or(false) {
        return Err(ConfigError::Invalid(format!(
            "{field} `{url_str}` must be an absolute URL with a host"
        )));
    }
    if url.query().is_some() {
        return Err(ConfigError::Invalid(format!(
            "{field} `{url_str}` must not contain a query string"
        )));
    }
    if url.fragment().is_some() {
        return Err(ConfigError::Invalid(format!(
            "{field} `{url_str}` must not contain a fragment"
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use tempfile::NamedTempFile;

    // Minimal valid TOML — used as a base for tests that just need a parseable
    // document without a full connection.
    const MINIMAL_TOML: &str = r#"
# gateway config
[server]
bind_addr = "127.0.0.1:8080"
max_body_bytes = 1048576
"#;

    // Full valid TOML with a connection, used for round-trip tests.
    const FULL_TOML: &str = r#"
# Example Corp gateway
[server]
bind_addr = "127.0.0.1:8080"
max_body_bytes = 1048576

[[connections]]
name = "primary"
base_url = "https://api.example.com/v1"
api_key = "${API_KEY}"
format = "open_ai"
"#;

    // -----------------------------------------------------------------------
    // read_document
    // -----------------------------------------------------------------------

    #[test]
    fn test_read_document_preserves_comments() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(FULL_TOML.as_bytes()).unwrap();
        let doc = read_document(f.path()).expect("should parse");
        assert!(doc.to_string().contains("# Example Corp gateway"));
    }

    #[test]
    fn test_read_document_missing_file() {
        let err = read_document(Path::new("/tmp/drgtw-edit-no-such-file-xyz.toml"))
            .expect_err("should fail");
        assert!(matches!(err, ConfigError::Io { .. }));
    }

    // -----------------------------------------------------------------------
    // set_value + round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn test_set_value_string_field() {
        let mut doc: DocumentMut = MINIMAL_TOML.parse().unwrap();
        set_value(&mut doc, "server.bind_addr", "0.0.0.0:9090").unwrap();
        let out = doc.to_string();
        assert!(out.contains("0.0.0.0:9090"), "updated value present: {out}");
        assert!(out.contains("# gateway config"), "comment preserved: {out}");
    }

    #[test]
    fn test_set_value_integer_field() {
        let mut doc: DocumentMut = MINIMAL_TOML.parse().unwrap();
        set_value(&mut doc, "server.max_body_bytes", "2097152").unwrap();
        let out = doc.to_string();
        assert!(out.contains("2097152"), "updated integer: {out}");
    }

    #[test]
    fn test_set_value_bool_field() {
        let toml = "[pii]\nenabled_by_default = true\n";
        let mut doc: DocumentMut = toml.parse().unwrap();
        set_value(&mut doc, "pii.enabled_by_default", "false").unwrap();
        let out = doc.to_string();
        assert!(out.contains("false"), "updated bool: {out}");
    }

    #[test]
    fn test_set_value_bool_invalid() {
        let toml = "[pii]\nenabled_by_default = true\n";
        let mut doc: DocumentMut = toml.parse().unwrap();
        let err = set_value(&mut doc, "pii.enabled_by_default", "yes").expect_err("bad bool");
        assert!(matches!(err, ConfigError::Invalid(_)));
    }

    #[test]
    fn test_set_value_integer_invalid() {
        let mut doc: DocumentMut = MINIMAL_TOML.parse().unwrap();
        let err =
            set_value(&mut doc, "server.max_body_bytes", "notanumber").expect_err("bad int");
        assert!(matches!(err, ConfigError::Invalid(_)));
    }

    #[test]
    fn test_set_value_new_key() {
        let mut doc: DocumentMut = MINIMAL_TOML.parse().unwrap();
        // Key not present → stored as string.
        set_value(&mut doc, "server.extra_field", "hello").unwrap();
        let out = doc.to_string();
        assert!(out.contains("hello"), "new key written: {out}");
    }

    #[test]
    fn test_set_value_empty_path_rejected() {
        let mut doc: DocumentMut = MINIMAL_TOML.parse().unwrap();
        let err = set_value(&mut doc, "", "x").expect_err("empty path");
        assert!(matches!(err, ConfigError::Invalid(_)));
    }

    #[test]
    fn test_set_value_array_of_tables_index() {
        let mut doc: DocumentMut = FULL_TOML.parse().unwrap();
        set_value(&mut doc, "connections.0.base_url", "https://api2.example.com/v1").unwrap();
        let out = doc.to_string();
        assert!(
            out.contains("api2.example.com"),
            "array-of-tables index updated: {out}"
        );
    }

    #[test]
    fn test_set_value_array_out_of_bounds() {
        let mut doc: DocumentMut = FULL_TOML.parse().unwrap();
        let err = set_value(&mut doc, "connections.5.base_url", "https://x.example.com")
            .expect_err("oob index");
        assert!(matches!(err, ConfigError::Invalid(_)));
    }

    #[test]
    fn test_round_trip_preserves_env_var_placeholder() {
        // Start with a document containing ${API_KEY}. Change bind_addr via
        // set_value. The placeholder must survive the round-trip unchanged.
        let mut doc: DocumentMut = FULL_TOML.parse().unwrap();
        set_value(&mut doc, "server.bind_addr", "0.0.0.0:9000").unwrap();
        let out = doc.to_string();
        assert!(out.contains("${API_KEY}"), "placeholder preserved: {out}");
        assert!(out.contains("0.0.0.0:9000"), "new value present: {out}");
        assert!(
            out.contains("# Example Corp gateway"),
            "comment preserved: {out}"
        );
    }

    // -----------------------------------------------------------------------
    // validate_str
    // -----------------------------------------------------------------------

    #[test]
    fn test_validate_str_accepts_minimal_toml() {
        validate_str(MINIMAL_TOML).expect("minimal config is valid");
    }

    #[test]
    fn test_validate_str_rejects_bad_bind_addr() {
        let toml = r#"
[server]
bind_addr = "not-an-addr"
"#;
        // serde will reject a bad SocketAddr during deserialisation.
        let errs = validate_str(toml).expect_err("bad bind_addr");
        assert!(!errs.is_empty());
    }

    #[test]
    fn test_validate_str_rejects_duplicate_connection_names() {
        let toml = r#"
[[connections]]
name = "dup"
base_url = "https://api.example.com/v1"
api_key = "key1"
format = "open_ai"

[[connections]]
name = "dup"
base_url = "https://api.example.com/v1"
api_key = "key2"
format = "open_ai"
"#;
        let errs = validate_str(toml).expect_err("duplicate names");
        assert!(
            errs.iter().any(|e| e.message.contains("duplicate")),
            "expected duplicate error in: {errs:?}"
        );
    }

    #[test]
    fn test_validate_str_accepts_vault_key_placeholder() {
        // In UI mode, ${VAULT_KEY} must not trigger the 64-hex check.
        let toml = r#"
[pii.vault]
path = "/var/db/vault.sqlite"
key = "${VAULT_KEY}"
"#;
        validate_str(toml).expect("placeholder vault key must be accepted in UI mode");
    }

    #[test]
    fn test_validate_str_rejects_vault_key_bad_literal() {
        // A literal value that is not 64 hex chars must still be rejected.
        let toml = r#"
[pii.vault]
path = "/var/db/vault.sqlite"
key = "tooshort"
"#;
        let errs = validate_str(toml).expect_err("bad vault key");
        assert!(
            errs.iter().any(|e| e.message.contains("64 hex")),
            "expected 64-hex error in: {errs:?}"
        );
    }

    #[test]
    fn test_validate_str_field_error_paths() {
        let toml = r#"
[[connections]]
name = "dup"
base_url = "https://api.example.com/v1"
api_key = "key"
format = "open_ai"

[[connections]]
name = "dup"
base_url = "https://api.example.com/v1"
api_key = "key"
format = "open_ai"
"#;
        let errs = validate_str(toml).expect_err("dup");
        // Each FieldError must have a non-empty path.
        for e in &errs {
            assert!(!e.path.is_empty(), "path must not be empty: {e:?}");
        }
    }

    // -----------------------------------------------------------------------
    // write_safe
    // -----------------------------------------------------------------------

    #[test]
    fn test_write_safe_writes_valid_toml_atomically() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(MINIMAL_TOML.as_bytes()).unwrap();
        let path = f.path().to_path_buf();

        let new_toml = r#"
[server]
bind_addr = "127.0.0.1:9191"
max_body_bytes = 1048576
"#;
        write_safe(&path, new_toml).expect("write_safe should succeed");

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("9191"), "new content present");
    }

    #[test]
    fn test_write_safe_creates_bak_file() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(MINIMAL_TOML.as_bytes()).unwrap();
        let path = f.path().to_path_buf();
        let dir = path.parent().unwrap().to_path_buf();

        let new_toml = "[server]\nbind_addr = \"127.0.0.1:8080\"\nmax_body_bytes = 1048576\n";
        write_safe(&path, new_toml).expect("write_safe should succeed");

        // A .bak.<n> file must exist in the same directory.
        let bak_exists = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .contains(".bak.")
            });
        assert!(bak_exists, "backup file must be created in {dir:?}");
    }

    #[test]
    fn test_write_safe_invalid_toml_leaves_original_unchanged() {
        let original = MINIMAL_TOML;
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(original.as_bytes()).unwrap();
        let path = f.path().to_path_buf();

        // Try to write TOML with a bad bind_addr (serde rejects it).
        let bad_toml = "[server]\nbind_addr = \"not-valid\"\n";
        let result = write_safe(&path, bad_toml);
        assert!(result.is_err(), "should reject invalid TOML");

        // Original bytes must be unchanged.
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after, original, "original file must not be modified on error");
    }

    // -----------------------------------------------------------------------
    // restart_required_changes
    // -----------------------------------------------------------------------

    fn load_cfg(toml: &str) -> Config {
        toml::from_str(toml).expect("test cfg")
    }

    #[test]
    fn test_restart_required_server_bind_addr() {
        let old = load_cfg("[server]\nbind_addr = \"127.0.0.1:8080\"\n");
        let new = load_cfg("[server]\nbind_addr = \"0.0.0.0:9090\"\n");
        let changed = restart_required_changes(&old, &new);
        assert!(changed.contains(&"server"), "server change requires restart");
    }

    #[test]
    fn test_restart_not_required_tracing_only() {
        let old = load_cfg("[tracing]\ndir = \"traces\"\n");
        let new = load_cfg("[tracing]\ndir = \"other-traces\"\n");
        let changed = restart_required_changes(&old, &new);
        assert!(
            !changed.contains(&"tracing"),
            "tracing is hot-reloadable, not in restart list"
        );
        assert!(changed.is_empty(), "no restart-required sections changed: {changed:?}");
    }

    #[test]
    fn test_restart_required_connections_change() {
        let old_toml = r#"
[[connections]]
name = "a"
base_url = "https://api.example.com/v1"
api_key = "key"
format = "open_ai"
"#;
        let new_toml = r#"
[[connections]]
name = "a"
base_url = "https://api2.example.com/v1"
api_key = "key"
format = "open_ai"
"#;
        let old = load_cfg(old_toml);
        let new = load_cfg(new_toml);
        let changed = restart_required_changes(&old, &new);
        assert!(
            changed.contains(&"connections"),
            "connection change requires restart"
        );
    }

    #[test]
    fn test_restart_not_required_otel_only() {
        let old = load_cfg("[otel]\nenabled = false\n");
        let new = load_cfg("[otel]\nenabled = true\nendpoint = \"http://localhost:4317\"\n");
        let changed = restart_required_changes(&old, &new);
        assert!(
            !changed.contains(&"otel"),
            "otel is hot-reloadable, not in restart list"
        );
    }

    #[test]
    fn test_no_changes_returns_empty() {
        let cfg = load_cfg(MINIMAL_TOML);
        let changed = restart_required_changes(&cfg, &cfg);
        assert!(changed.is_empty(), "identical configs: {changed:?}");
    }
}
