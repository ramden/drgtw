//! Virtual key authentication and key→connection resolution.
//!
//! Public API contract (Phase 1 / WP 1.1 — extended in Phase 2 / WP 2.2 + 2.3,
//! Phase 8 / WP 8.1).
//! Frozen interface — extend, don't break.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use drgtw_config::{Config, Connection};
use http::HeaderMap;
use subtle::ConstantTimeEq;

// ---------------------------------------------------------------------------
// Internal types
// ---------------------------------------------------------------------------

/// A virtual-key secret wrapped so its `Debug` impl never emits the raw bytes.
///
/// Constant-time comparison: we compare every stored key against the candidate
/// on every authenticate() call (no early exit when a match is found). To
/// avoid length-based side channels we pad both sides to the length of the
/// longest key before the byte-wise `ct_eq` comparison, then OR the match
/// results together so a match on any key returns success only once all keys
/// have been checked.
struct SecretKey(Vec<u8>);

impl std::fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

// ---------------------------------------------------------------------------
// Wildcard helpers
// ---------------------------------------------------------------------------

/// Returns true if `pattern` matches `candidate`.
///
/// - Exact match: `pattern` has no `*` → string equality.
/// - Prefix match: `pattern` ends with `*` → candidate starts with the prefix.
#[inline]
fn pattern_matches(pattern: &str, candidate: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => candidate.starts_with(prefix),
        None => pattern == candidate,
    }
}

/// Returns `true` if `pattern` is a wildcard (ends with `*`).
#[inline]
fn is_wildcard(pattern: &str) -> bool {
    pattern.ends_with('*')
}

// ---------------------------------------------------------------------------
// Public types — RateLimiter
// ---------------------------------------------------------------------------

/// Decision returned by [`RateLimiter::check`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RateDecision {
    /// No rate limit is configured for this key (or the key_id is unknown).
    Unlimited,
    /// Request allowed; `remaining` tokens left in the current window.
    Allowed { remaining: u32, limit: u32 },
    /// Rate limit exceeded; caller should retry after `retry_after_secs`.
    Limited { retry_after_secs: u64, limit: u32 },
}

/// Token-bucket state for a single virtual key.
struct Bucket {
    /// Capacity = the configured `requests` limit.
    capacity: u32,
    /// Refill rate: one token every `refill_interval`.
    refill_interval: Duration,
    /// Current token count (may be fractional internally, stored as f64).
    tokens: f64,
    /// Last time we refilled (used for continuous refill calculation).
    last_refill: Instant,
}

impl Bucket {
    fn new(capacity: u32, per_seconds: u32, now: Instant) -> Self {
        Bucket {
            capacity,
            refill_interval: Duration::from_secs_f64(per_seconds as f64 / capacity as f64),
            tokens: capacity as f64,
            last_refill: now,
        }
    }

    /// Refill tokens based on elapsed time, then try to consume one token.
    /// Returns `(allowed, remaining_after, retry_after_secs)`.
    fn check(&mut self, now: Instant) -> RateDecision {
        // Continuous refill: add tokens proportional to elapsed time.
        let elapsed = now.saturating_duration_since(self.last_refill);
        let refill_rate = 1.0 / self.refill_interval.as_secs_f64(); // tokens/sec
        let new_tokens = elapsed.as_secs_f64() * refill_rate;
        self.tokens = (self.tokens + new_tokens).min(self.capacity as f64);
        self.last_refill = now;

        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            let remaining = self.tokens.floor() as u32;
            RateDecision::Allowed {
                remaining,
                limit: self.capacity,
            }
        } else {
            // How long until we have 1 token? (1 - current_tokens) * refill_interval
            let secs_to_refill = (1.0 - self.tokens) * self.refill_interval.as_secs_f64();
            let retry_after_secs = secs_to_refill.ceil() as u64;
            RateDecision::Limited {
                retry_after_secs,
                limit: self.capacity,
            }
        }
    }
}

/// Per-key rate limiter. Thread-safe. Built once from config at startup.
///
/// Indexed by key index: key_id `"vk-{i}"` maps to bucket at index `i`.
/// Keys without a `rate_limit` in config have no bucket (→ `Unlimited`).
pub struct RateLimiter {
    /// `buckets[i]` corresponds to virtual_key[i]. `None` = no limit.
    buckets: Vec<Option<Mutex<Bucket>>>,
    /// Injectable clock function for deterministic tests.
    now_fn: fn() -> Instant,
}

impl RateLimiter {
    /// Build from config using the real system clock.
    pub fn new(config: &Config) -> Self {
        Self::new_with_clock(config, Instant::now)
    }

    /// Build from config with an injectable clock (for testing).
    pub fn new_with_clock(config: &Config, now_fn: fn() -> Instant) -> Self {
        let now = now_fn();
        let buckets = config
            .virtual_keys
            .iter()
            .map(|vk| {
                vk.rate_limit
                    .as_ref()
                    .map(|rl| Mutex::new(Bucket::new(rl.requests, rl.per_seconds, now)))
            })
            .collect();
        RateLimiter { buckets, now_fn }
    }

    /// Check (and consume one token from) the rate limit for `key_id`.
    ///
    /// `key_id` must be in `"vk-{index}"` format as produced by [`KeyStore`].
    /// Returns [`RateDecision::Unlimited`] for unknown key_ids or keys without limits.
    pub fn check(&self, key_id: &str) -> RateDecision {
        let idx = match parse_key_index(key_id) {
            Some(i) => i,
            None => return RateDecision::Unlimited,
        };
        match self.buckets.get(idx) {
            Some(Some(mutex)) => {
                let now = (self.now_fn)();
                let mut bucket = mutex.lock().expect("bucket mutex poisoned");
                bucket.check(now)
            }
            _ => RateDecision::Unlimited,
        }
    }
}

/// A point-in-time snapshot of a single key's rate-limit state, for UI display.
#[derive(Debug, Clone)]
pub struct RateLimiterSnapshot {
    /// Tokens remaining in the current window (floor of internal f64 counter).
    pub remaining: u32,
    /// Maximum tokens (bucket capacity).
    pub capacity: u32,
    /// Seconds until the next token is available (0 when tokens > 0).
    pub secs_to_next_token: u64,
}

/// A point-in-time snapshot of a single key's budget state, for UI display.
#[derive(Debug, Clone)]
pub struct BudgetSnapshot {
    /// Accumulated spend in the current window, in USD.
    pub spent_usd: f64,
    /// Maximum spend for the window, in USD.
    pub max_usd: f64,
    /// Seconds until the window resets (0 when no spend has occurred yet).
    pub secs_to_reset: u64,
}

impl RateLimiter {
    /// Build a new limiter from `new_config`, copying live bucket state for keys
    /// whose **secret** is unchanged between configs.
    ///
    /// Match is by secret value (constant-time not required here — both configs
    /// are trusted operator config, no user input). Keys not present in the old
    /// config, or keys whose secret changed, start with a full bucket.
    pub fn rebuild_from(&self, old_config: &drgtw_config::Config, new_config: &drgtw_config::Config) -> Self {
        let now = (self.now_fn)();
        let buckets = new_config
            .virtual_keys
            .iter()
            .map(|new_vk| {
                let Some(rl) = &new_vk.rate_limit else {
                    return None;
                };
                // Find a matching old bucket by secret identity.
                let old_bucket_state = old_config
                    .virtual_keys
                    .iter()
                    .enumerate()
                    .find(|(_, old_vk)| old_vk.key == new_vk.key)
                    .and_then(|(old_idx, _)| self.buckets.get(old_idx))
                    .and_then(|opt| opt.as_ref())
                    .map(|mutex| {
                        let bucket = mutex.lock().expect("bucket mutex poisoned");
                        (bucket.tokens, bucket.last_refill)
                    });

                let mut new_bucket = Bucket::new(rl.requests, rl.per_seconds, now);
                if let Some((tokens, last_refill)) = old_bucket_state {
                    // Only carry over state when capacity matches.
                    if new_bucket.capacity == rl.requests {
                        new_bucket.tokens = tokens.min(new_bucket.capacity as f64);
                        new_bucket.last_refill = last_refill;
                    }
                }
                Some(Mutex::new(new_bucket))
            })
            .collect();
        RateLimiter { buckets, now_fn: self.now_fn }
    }

    /// Return a point-in-time snapshot of the rate-limit state for `key_id`.
    ///
    /// Returns `None` when the key has no rate limit configured or the key_id
    /// is unknown. Does NOT consume a token — read-only.
    pub fn snapshot(&self, key_id: &str) -> Option<RateLimiterSnapshot> {
        let idx = parse_key_index(key_id)?;
        let mutex = self.buckets.get(idx)?.as_ref()?;
        let now = (self.now_fn)();
        let bucket = mutex.lock().expect("bucket mutex poisoned");
        let elapsed = now.saturating_duration_since(bucket.last_refill);
        let refill_rate = 1.0 / bucket.refill_interval.as_secs_f64();
        let current_tokens = (bucket.tokens + elapsed.as_secs_f64() * refill_rate)
            .min(bucket.capacity as f64);
        let secs_to_next_token = if current_tokens >= 1.0 {
            0
        } else {
            let secs = (1.0 - current_tokens) * bucket.refill_interval.as_secs_f64();
            secs.ceil() as u64
        };
        Some(RateLimiterSnapshot {
            remaining: current_tokens.floor() as u32,
            capacity: bucket.capacity,
            secs_to_next_token,
        })
    }
}

/// Parse `"vk-{n}"` → `n`. Returns `None` on malformed input.
fn parse_key_index(key_id: &str) -> Option<usize> {
    key_id.strip_prefix("vk-")?.parse().ok()
}

// ---------------------------------------------------------------------------
// Public types — BudgetTracker
// ---------------------------------------------------------------------------

/// Decision returned by [`BudgetTracker::check`].
///
/// `check` does **not** consume budget — call [`BudgetTracker::record`] after
/// a successful upstream response to add the actual spend.
#[derive(Debug, Clone, PartialEq)]
pub enum BudgetDecision {
    /// No budget is configured for this key (or the key_id is unknown).
    Unlimited,
    /// Budget not exhausted; `spent_usd` is the current window's cumulative spend.
    Allowed { spent_usd: f64, max_usd: f64 },
    /// Budget exhausted for this window; retry after `retry_after_secs` seconds.
    Exhausted { max_usd: f64, retry_after_secs: u64 },
}

/// Per-key budget window state.
struct BudgetWindow {
    /// Max spend for the window.
    max_usd: f64,
    /// Window length.
    window: Duration,
    /// Accumulated spend in the current window.
    spent_usd: f64,
    /// When the current window started (None = no spend yet).
    window_start: Option<Instant>,
}

impl BudgetWindow {
    fn new(max_usd: f64, per_seconds: u32) -> Self {
        BudgetWindow {
            max_usd,
            window: Duration::from_secs(per_seconds as u64),
            spent_usd: 0.0,
            window_start: None,
        }
    }

    /// Expire the window if it has elapsed, resetting spend.
    fn maybe_reset(&mut self, now: Instant) {
        if let Some(start) = self.window_start
            && now.saturating_duration_since(start) >= self.window
        {
            self.spent_usd = 0.0;
            self.window_start = None;
        }
    }

    fn check(&mut self, now: Instant) -> BudgetDecision {
        self.maybe_reset(now);
        if self.spent_usd < self.max_usd {
            BudgetDecision::Allowed {
                spent_usd: self.spent_usd,
                max_usd: self.max_usd,
            }
        } else {
            let retry_after_secs = match self.window_start {
                None => 0,
                Some(start) => {
                    let elapsed = now.saturating_duration_since(start);
                    let remaining = self.window.saturating_sub(elapsed);
                    remaining.as_secs() + if remaining.subsec_nanos() > 0 { 1 } else { 0 }
                }
            };
            BudgetDecision::Exhausted {
                max_usd: self.max_usd,
                retry_after_secs,
            }
        }
    }

    fn record(&mut self, cost_usd: f64, now: Instant) {
        self.maybe_reset(now);
        if self.window_start.is_none() && cost_usd > 0.0 {
            self.window_start = Some(now);
        }
        self.spent_usd += cost_usd;
    }
}

/// Per-key budget tracker. Thread-safe. Built once from config at startup.
///
/// Budget windows are **fixed windows** anchored to the first spend in the
/// window. When the window elapses the counter resets automatically on the
/// next `check` or `record` call.
///
/// State is **in-memory only** — restarting the gateway resets all counters.
///
/// Indexed by key index: key_id `"vk-{i}"` maps to window at index `i`.
/// Keys without a `budget` in config always return [`BudgetDecision::Unlimited`].
pub struct BudgetTracker {
    /// `windows[i]` corresponds to virtual_key[i]. `None` = no budget configured.
    windows: Vec<Option<Mutex<BudgetWindow>>>,
    /// Injectable clock function for deterministic tests.
    now_fn: fn() -> Instant,
}

impl BudgetTracker {
    /// Build from config using the real system clock.
    pub fn new(config: &drgtw_config::Config) -> Self {
        Self::new_with_clock(config, Instant::now)
    }

    /// Build from config with an injectable clock (for testing).
    pub fn new_with_clock(config: &drgtw_config::Config, now_fn: fn() -> Instant) -> Self {
        let windows = config
            .virtual_keys
            .iter()
            .map(|vk| {
                vk.budget
                    .as_ref()
                    .map(|b| Mutex::new(BudgetWindow::new(b.max_usd, b.per_seconds)))
            })
            .collect();
        BudgetTracker { windows, now_fn }
    }

    /// Check whether `key_id` is within budget. Does **not** consume budget.
    ///
    /// Returns [`BudgetDecision::Unlimited`] for unknown key_ids or keys without budgets.
    pub fn check(&self, key_id: &str) -> BudgetDecision {
        let idx = match parse_key_index(key_id) {
            Some(i) => i,
            None => return BudgetDecision::Unlimited,
        };
        match self.windows.get(idx) {
            Some(Some(mutex)) => {
                let now = (self.now_fn)();
                let mut window = mutex.lock().expect("budget window mutex poisoned");
                window.check(now)
            }
            _ => BudgetDecision::Unlimited,
        }
    }

    /// Record `cost_usd` spend for `key_id` in the current window.
    ///
    /// If `key_id` is unknown or has no budget configured, this is a no-op.
    /// If this is the first spend in a new window, the window clock starts now.
    pub fn record(&self, key_id: &str, cost_usd: f64) {
        let idx = match parse_key_index(key_id) {
            Some(i) => i,
            None => return,
        };
        if let Some(Some(mutex)) = self.windows.get(idx) {
            let now = (self.now_fn)();
            let mut window = mutex.lock().expect("budget window mutex poisoned");
            window.record(cost_usd, now);
        }
    }

    /// Build a new tracker from `new_config`, carrying over live window state for
    /// keys whose **secret** is unchanged between configs.
    ///
    /// A key whose secret (the `vk.key` string) is unchanged keeps its
    /// `spent_usd` and `window_start`. Keys not found in the old config, or
    /// keys whose secret changed, start with a fresh empty window.
    pub fn rebuild_from(&self, old_config: &drgtw_config::Config, new_config: &drgtw_config::Config) -> Self {
        let now = (self.now_fn)();
        let windows = new_config
            .virtual_keys
            .iter()
            .map(|new_vk| {
                let Some(budget) = &new_vk.budget else {
                    return None;
                };
                // Find the old window state for the same secret.
                let old_window_state = old_config
                    .virtual_keys
                    .iter()
                    .enumerate()
                    .find(|(_, old_vk)| old_vk.key == new_vk.key)
                    .and_then(|(old_idx, _)| self.windows.get(old_idx))
                    .and_then(|opt| opt.as_ref())
                    .map(|mutex| {
                        let w = mutex.lock().expect("budget window mutex poisoned");
                        (w.spent_usd, w.window_start)
                    });

                let mut new_window = BudgetWindow::new(budget.max_usd, budget.per_seconds);
                if let Some((spent_usd, window_start)) = old_window_state {
                    // Only carry over spend if the max_usd matches — a budget
                    // increase/decrease resets the window so operators see clean state.
                    if (new_window.max_usd - budget.max_usd).abs() < f64::EPSILON {
                        new_window.spent_usd = spent_usd;
                        new_window.window_start = window_start;
                        // Expire the window if it has already elapsed.
                        new_window.maybe_reset(now);
                    }
                }
                Some(Mutex::new(new_window))
            })
            .collect();
        BudgetTracker { windows, now_fn: self.now_fn }
    }

    /// Return a point-in-time snapshot of the budget state for `key_id`.
    ///
    /// Returns `None` when the key has no budget configured or the key_id is
    /// unknown. Does NOT record any spend — read-only.
    pub fn snapshot(&self, key_id: &str) -> Option<BudgetSnapshot> {
        let idx = parse_key_index(key_id)?;
        let mutex = self.windows.get(idx)?.as_ref()?;
        let now = (self.now_fn)();
        let mut window = mutex.lock().expect("budget window mutex poisoned");
        window.maybe_reset(now);
        let secs_to_reset = match window.window_start {
            None => 0,
            Some(start) => {
                let elapsed = now.saturating_duration_since(start);
                window.window.saturating_sub(elapsed).as_secs()
            }
        };
        Some(BudgetSnapshot {
            spent_usd: window.spent_usd,
            max_usd: window.max_usd,
            secs_to_reset,
        })
    }
}

// ---------------------------------------------------------------------------
// Public types — KeyStore
// ---------------------------------------------------------------------------

/// Immutable store of virtual keys, built once from config at startup.
pub struct KeyStore {
    /// `entries[i]` corresponds to `config.virtual_keys[i]`.
    entries: Vec<KeyEntry>,
    /// Maximum key length (bytes), used for constant-time padding.
    max_key_len: usize,
}

/// Internal per-key record, not exposed outside this crate.
#[derive(Debug)]
struct KeyEntry {
    key_id: String,
    secret: SecretKey,
    connections: Vec<Arc<Connection>>,
    model_allowlist: Option<Vec<String>>,
    /// Resolved MCP server allowlist: empty = all servers allowed.
    mcp_servers: Vec<String>,
    /// Whether this key may bypass PII scanning via the `x-drgtw-pii: off` header.
    allow_pii_bypass: bool,
}

impl std::fmt::Debug for KeyStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Show structure but never the key material.
        f.debug_struct("KeyStore")
            .field("entry_count", &self.entries.len())
            .field(
                "key_ids",
                &self.entries.iter().map(|e| &e.key_id).collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// A successfully authenticated virtual key with its resolved permissions.
#[derive(Debug, Clone)]
pub struct ResolvedKey {
    /// Stable identifier for logging — NEVER the secret. Format: `vk-{index}`.
    pub key_id: String,
    /// Connections this key may use (shared, resolved at store build time).
    pub connections: Vec<Arc<Connection>>,
    /// Optional model allowlist; `None` = all models of allowed connections.
    pub model_allowlist: Option<Vec<String>>,
    /// Resolved MCP server allowlist. Empty vec = all configured servers allowed.
    /// Populated from `VirtualKey.mcp_servers`; `None` in config maps to `vec![]`.
    pub mcp_servers: Vec<String>,
    /// Whether this key is authorized to bypass PII scanning per request via the
    /// `x-drgtw-pii: off` header. Unauthorized keys' bypass headers are ignored.
    pub allow_pii_bypass: bool,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AuthError {
    /// No credential found in any supported header. → 401
    #[error("missing API key")]
    MissingKey,
    /// Credential present but unknown. → 401
    #[error("invalid API key")]
    UnknownKey,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RouteError {
    /// Requested model not allowed for this key. → 403
    #[error("model `{0}` is not allowed for this key")]
    ModelNotAllowed(String),
    /// No connection of this key serves the requested model. → 404
    #[error("no configured connection serves model `{0}`")]
    UnknownModel(String),
}

// ---------------------------------------------------------------------------
// KeyStore implementation
// ---------------------------------------------------------------------------

impl KeyStore {
    /// Build the store from validated config.
    ///
    /// Each [`Connection`] is wrapped in [`Arc`] exactly once and shared across
    /// all virtual keys that reference it.
    pub fn new(config: &Config) -> Self {
        // Build a name → Arc<Connection> map so each connection object is
        // allocated once regardless of how many virtual keys reference it.
        let conn_map: HashMap<&str, Arc<Connection>> = config
            .connections
            .iter()
            .map(|c| (c.name.as_str(), Arc::new(c.clone())))
            .collect();

        let entries: Vec<KeyEntry> = config
            .virtual_keys
            .iter()
            .enumerate()
            .map(|(idx, vk)| {
                let connections: Vec<Arc<Connection>> = vk
                    .connections
                    .iter()
                    .map(|name| {
                        conn_map
                            .get(name.as_str())
                            .expect("config validated: connection must exist")
                            .clone()
                    })
                    .collect();

                KeyEntry {
                    key_id: format!("vk-{idx}"),
                    secret: SecretKey(vk.key.as_bytes().to_vec()),
                    connections,
                    model_allowlist: vk.models.clone(),
                    mcp_servers: vk.mcp_servers.clone().unwrap_or_default(),
                    allow_pii_bypass: vk.allow_pii_bypass,
                }
            })
            .collect();

        let max_key_len = entries.iter().map(|e| e.secret.0.len()).max().unwrap_or(0);

        KeyStore {
            entries,
            max_key_len,
        }
    }

    /// Authenticate a request.
    ///
    /// Credential lookup order (first present wins):
    /// 1. `Authorization: Bearer <key>` (OpenAI SDK style)
    /// 2. `x-api-key: <key>` (Anthropic SDK style)
    ///
    /// ## Constant-time comparison strategy
    ///
    /// To resist timing side channels we:
    /// - Extract the candidate credential once.
    /// - Pad both the candidate and every stored key to `max_key_len` bytes
    ///   with a fixed byte (`0xFF`) before comparing, so all comparisons
    ///   operate on equal-length buffers. This prevents length leakage.
    /// - Iterate over **all** entries without short-circuiting: a `u8` flag
    ///   `found` accumulates `ct_eq` results via bitwise OR. The actual
    ///   [`ResolvedKey`] is selected afterward with a second pass. This ensures
    ///   the comparison loop runs in constant time with respect to which key (if
    ///   any) matches.
    pub fn authenticate(&self, headers: &HeaderMap) -> Result<ResolvedKey, AuthError> {
        // --- credential extraction ---
        let candidate: &[u8] = {
            // Try Authorization: Bearer <key>
            if let Some(auth_val) = headers.get(http::header::AUTHORIZATION) {
                let s = auth_val.to_str().unwrap_or("");
                if let Some(token) = s.strip_prefix("Bearer ") {
                    token.as_bytes()
                } else {
                    // Malformed Authorization header — fall through to x-api-key.
                    headers
                        .get("x-api-key")
                        .and_then(|v| v.to_str().ok())
                        .map(|s| s.as_bytes())
                        .ok_or(AuthError::MissingKey)?
                }
            } else if let Some(api_key_val) = headers.get("x-api-key") {
                api_key_val.to_str().unwrap_or("").as_bytes()
            } else {
                return Err(AuthError::MissingKey);
            }
        };

        if candidate.is_empty() {
            return Err(AuthError::MissingKey);
        }

        // --- constant-time comparison against all stored keys ---
        //
        // We pad to max_key_len + 1 to guarantee padded_candidate and
        // padded_stored are the same length for every entry. The +1 ensures
        // a zero-length store still produces a valid (non-empty) comparison.
        let pad_len = self.max_key_len.max(candidate.len()) + 1;
        let pad_byte = 0xFF_u8;

        let mut padded_candidate = vec![pad_byte; pad_len];
        let write_len = candidate.len().min(pad_len);
        padded_candidate[..write_len].copy_from_slice(&candidate[..write_len]);

        // `matched_idx` is set to `i` when entry `i` matches, but we never
        // break early — the loop runs for all entries unconditionally.
        let mut found: u8 = 0;
        let mut matched_idx: usize = 0;

        for (i, entry) in self.entries.iter().enumerate() {
            let mut padded_stored = vec![pad_byte; pad_len];
            let slen = entry.secret.0.len().min(pad_len);
            padded_stored[..slen].copy_from_slice(&entry.secret.0[..slen]);

            // ct_eq returns 1 (as Choice) when equal, 0 otherwise.
            let eq: u8 = padded_candidate.ct_eq(&padded_stored).unwrap_u8();

            // Update matched_idx only when eq == 1 (branch-free via masking).
            // If eq == 1: matched_idx = i.  If eq == 0: no change.
            let mask = eq.wrapping_neg(); // 0x00 or 0xFF
            matched_idx = (i & (mask as usize)) | (matched_idx & !(mask as usize));
            found |= eq;
        }

        if found == 0 {
            return Err(AuthError::UnknownKey);
        }

        let entry = &self.entries[matched_idx];
        Ok(ResolvedKey {
            key_id: entry.key_id.clone(),
            connections: entry.connections.clone(),
            model_allowlist: entry.model_allowlist.clone(),
            mcp_servers: entry.mcp_servers.clone(),
            allow_pii_bypass: entry.allow_pii_bypass,
        })
    }
}

// ---------------------------------------------------------------------------
// ResolvedKey implementation
// ---------------------------------------------------------------------------

impl ResolvedKey {
    /// Resolve the upstream connection for a requested model.
    ///
    /// Rules (WP 2.2 — wildcard support):
    /// - if an allowlist exists and no pattern matches `model` → ModelNotAllowed
    /// - exact-match connections (config order) are tried before wildcard connections
    /// - among wildcards, the one with the longest prefix wins (tiebreak: config order)
    /// - otherwise → UnknownModel
    pub fn connection_for_model(&self, model: &str) -> Result<&Connection, RouteError> {
        self.connections_for_model(model).map(|v| v[0])
    }

    /// Resolve **all** candidate upstream connections for a requested model.
    ///
    /// Rules (WP 8.1 — multi-candidate for fallback):
    /// - if an allowlist exists and no pattern matches `model` → `ModelNotAllowed`
    /// - exact-match connections come first (config order among exact matches)
    /// - wildcard-matching connections follow, ordered longest-prefix-first
    ///   (tiebreak: config order among ties)
    /// - otherwise → `UnknownModel`
    ///
    /// The returned slice always has at least one element on success.
    /// The first element is identical to what [`connection_for_model`] returns.
    pub fn connections_for_model(&self, model: &str) -> Result<Vec<&Connection>, RouteError> {
        // Allowlist check first (supports wildcards).
        if let Some(allowlist) = &self.model_allowlist
            && !allowlist.iter().any(|p| pattern_matches(p, model))
        {
            return Err(RouteError::ModelNotAllowed(model.to_owned()));
        }

        let mut exact_matches: Vec<&Connection> = Vec::new();

        // Phase 1: exact match across all connections (config order).
        for conn in &self.connections {
            if conn.models.iter().any(|m| !is_wildcard(m) && m == model) {
                exact_matches.push(conn.as_ref());
            }
        }

        // Phase 2: wildcard matches — collect all wildcard-matching connections,
        // then sort longest-prefix-first (stable sort preserves config order as tiebreak).
        //
        // We use (prefix_len, config_index) to produce a deterministic ordering:
        // larger prefix_len comes first; among equal prefix_len, smaller config_index
        // (i.e. earlier in config) comes first.
        struct WildcardMatch<'a> {
            conn: &'a Connection,
            prefix_len: usize,
            config_idx: usize,
        }

        let mut wildcard_matches: Vec<WildcardMatch<'_>> = Vec::new();
        for (idx, conn) in self.connections.iter().enumerate() {
            let mut best_prefix_for_conn: isize = -1;
            for pat in &conn.models {
                if !is_wildcard(pat) {
                    continue;
                }
                let prefix = pat.strip_suffix('*').unwrap_or("");
                if model.starts_with(prefix) && (prefix.len() as isize) > best_prefix_for_conn {
                    best_prefix_for_conn = prefix.len() as isize;
                }
            }
            if best_prefix_for_conn >= 0 {
                wildcard_matches.push(WildcardMatch {
                    conn: conn.as_ref(),
                    prefix_len: best_prefix_for_conn as usize,
                    config_idx: idx,
                });
            }
        }
        // Sort: longest prefix first; config order as tiebreak.
        wildcard_matches.sort_by(|a, b| {
            b.prefix_len
                .cmp(&a.prefix_len)
                .then(a.config_idx.cmp(&b.config_idx))
        });

        let mut result = exact_matches;
        result.extend(wildcard_matches.into_iter().map(|wm| wm.conn));

        if result.is_empty() {
            return Err(RouteError::UnknownModel(model.to_owned()));
        }
        Ok(result)
    }

    /// All models this key may use (intersection of connection models and
    /// allowlist), deduplicated, for `/v1/models`.
    ///
    /// WP 2.2 rules:
    /// - Exact model names from connections come first (first-seen across
    ///   connections in config order), filtered through the allowlist.
    /// - Wildcard patterns from connections that pass the allowlist filter
    ///   follow, as-is (they cannot be enumerated), in first-seen order, deduped.
    /// - Wildcard patterns in the allowlist appear as-is after exact models,
    ///   deduped, since they cover models not in any connection's explicit list.
    pub fn allowed_models(&self) -> Vec<String> {
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut exact_result: Vec<String> = Vec::new();
        let mut wildcard_result: Vec<String> = Vec::new();

        for conn in &self.connections {
            for model_pattern in &conn.models {
                if is_wildcard(model_pattern) {
                    // Wildcard in connection model list: include if allowlist permits.
                    let permitted = match &self.model_allowlist {
                        None => true,
                        Some(allowlist) => allowlist.iter().any(|p| {
                            // An allowlist pattern "gpt-*" covers connection wildcard
                            // "gpt-*" if they are the same pattern, or the allowlist
                            // is a match-all "*".
                            p == model_pattern || p == "*"
                        }),
                    };
                    if permitted && seen.insert(model_pattern.clone()) {
                        wildcard_result.push(model_pattern.clone());
                    }
                } else {
                    // Exact model: include if allowlist permits (using pattern match).
                    let permitted = match &self.model_allowlist {
                        None => true,
                        Some(allowlist) => {
                            allowlist.iter().any(|p| pattern_matches(p, model_pattern))
                        }
                    };
                    if permitted && seen.insert(model_pattern.clone()) {
                        exact_result.push(model_pattern.clone());
                    }
                }
            }
        }

        // Wildcard entries from the allowlist itself (those not already covered
        // by connection wildcards above).
        if let Some(allowlist) = &self.model_allowlist {
            for pattern in allowlist {
                if is_wildcard(pattern) && seen.insert(pattern.clone()) {
                    wildcard_result.push(pattern.clone());
                }
            }
        }

        exact_result.extend(wildcard_result);
        exact_result
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use drgtw_config::{ApiFormat, Config, Connection, PiiConfig, ServerConfig, VirtualKey};
    use http::HeaderMap;
    use std::collections::HashMap;

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    fn make_connection(name: &str, models: &[&str]) -> Connection {
        Connection {
            name: name.to_owned(),
            base_url: "https://api.example.com".to_owned(),
            api_key: "upstream-secret".to_owned(),
            format: ApiFormat::OpenAi,
            models: models.iter().map(|s| s.to_string()).collect(),
            model_costs: HashMap::new(),
            region: None,
            aws_access_key_id: None,
            aws_secret_access_key: None,
            aws_session_token: None,
        }
    }

    fn make_virtual_key(key: &str, connections: &[&str], models: Option<&[&str]>) -> VirtualKey {
        VirtualKey {
            key: key.to_owned(),
            connections: connections.iter().map(|s| s.to_string()).collect(),
            models: models.map(|ms| ms.iter().map(|s| s.to_string()).collect()),
            rate_limit: None,
            budget: None,
            mcp_servers: None,
            allow_pii_bypass: false,
        }
    }

    fn make_virtual_key_with_rate_limit(
        key: &str,
        connections: &[&str],
        models: Option<&[&str]>,
        requests: u32,
        per_seconds: u32,
    ) -> VirtualKey {
        VirtualKey {
            key: key.to_owned(),
            connections: connections.iter().map(|s| s.to_string()).collect(),
            models: models.map(|ms| ms.iter().map(|s| s.to_string()).collect()),
            rate_limit: Some(drgtw_config::RateLimit {
                requests,
                per_seconds,
            }),
            budget: None,
            mcp_servers: None,
            allow_pii_bypass: false,
        }
    }

    fn make_virtual_key_with_budget(
        key: &str,
        connections: &[&str],
        max_usd: f64,
        per_seconds: u32,
    ) -> VirtualKey {
        VirtualKey {
            key: key.to_owned(),
            connections: connections.iter().map(|s| s.to_string()).collect(),
            models: None,
            rate_limit: None,
            budget: Some(drgtw_config::Budget {
                max_usd,
                per_seconds,
            }),
            mcp_servers: None,
            allow_pii_bypass: false,
        }
    }

    /// Build a Config with two connections and two virtual keys for common tests.
    fn two_key_config() -> Config {
        Config {
            server: ServerConfig::default(),
            connections: vec![
                make_connection("openai", &["gpt-4o", "gpt-4o-mini"]),
                make_connection("anthropic", &["claude-3-5-sonnet", "claude-3-haiku"]),
            ],
            virtual_keys: vec![
                // vk-0: allowlist restricts to gpt-4o only
                make_virtual_key("sk-drgtw-key0abc", &["openai"], Some(&["gpt-4o"])),
                // vk-1: no allowlist, both connections
                make_virtual_key("sk-drgtw-key1xyz", &["openai", "anthropic"], None),
            ],
            pii: PiiConfig::default(),
            ..Config::default()
        }
    }

    fn bearer(key: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            http::header::AUTHORIZATION,
            format!("Bearer {key}").parse().unwrap(),
        );
        h
    }

    fn x_api_key(key: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("x-api-key", key.parse().unwrap());
        h
    }

    fn both_headers(bearer_key: &str, api_key: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            http::header::AUTHORIZATION,
            format!("Bearer {bearer_key}").parse().unwrap(),
        );
        h.insert("x-api-key", api_key.parse().unwrap());
        h
    }

    // -----------------------------------------------------------------------
    // KeyStore::new
    // -----------------------------------------------------------------------

    #[test]
    fn test_store_builds_from_config() {
        let cfg = two_key_config();
        let store = KeyStore::new(&cfg);
        assert_eq!(store.entries.len(), 2);
        assert_eq!(store.entries[0].key_id, "vk-0");
        assert_eq!(store.entries[1].key_id, "vk-1");
    }

    #[test]
    fn test_store_empty_config() {
        let cfg = Config {
            server: ServerConfig::default(),
            connections: vec![],
            virtual_keys: vec![],
            pii: PiiConfig::default(),
            ..Config::default()
        };
        let store = KeyStore::new(&cfg);
        assert_eq!(store.entries.len(), 0);
    }

    #[test]
    fn test_connections_are_shared_arcs() {
        // A connection referenced by two virtual keys should be the same Arc.
        let cfg = Config {
            server: ServerConfig::default(),
            connections: vec![make_connection("shared", &["gpt-4o"])],
            virtual_keys: vec![
                make_virtual_key("sk-drgtw-keyA111", &["shared"], None),
                make_virtual_key("sk-drgtw-keyB222", &["shared"], None),
            ],
            pii: PiiConfig::default(),
            ..Config::default()
        };
        let store = KeyStore::new(&cfg);
        let ptr0 = Arc::as_ptr(&store.entries[0].connections[0]);
        let ptr1 = Arc::as_ptr(&store.entries[1].connections[0]);
        assert_eq!(ptr0, ptr1, "same Arc should be shared between entries");
    }

    // -----------------------------------------------------------------------
    // authenticate — happy paths
    // -----------------------------------------------------------------------

    #[test]
    fn test_authenticate_bearer_header() {
        let store = KeyStore::new(&two_key_config());
        let rk = store.authenticate(&bearer("sk-drgtw-key0abc")).unwrap();
        assert_eq!(rk.key_id, "vk-0");
    }

    #[test]
    fn test_authenticate_x_api_key_header() {
        let store = KeyStore::new(&two_key_config());
        let rk = store.authenticate(&x_api_key("sk-drgtw-key1xyz")).unwrap();
        assert_eq!(rk.key_id, "vk-1");
    }

    #[test]
    fn test_authenticate_second_key_by_bearer() {
        let store = KeyStore::new(&two_key_config());
        let rk = store.authenticate(&bearer("sk-drgtw-key1xyz")).unwrap();
        assert_eq!(rk.key_id, "vk-1");
    }

    // -----------------------------------------------------------------------
    // authenticate — header precedence
    // -----------------------------------------------------------------------

    #[test]
    fn test_authenticate_authorization_wins_over_x_api_key() {
        // Both headers present — Authorization should win.
        let store = KeyStore::new(&two_key_config());
        // bearer = key0, x-api-key = key1; should return vk-0
        let h = both_headers("sk-drgtw-key0abc", "sk-drgtw-key1xyz");
        let rk = store.authenticate(&h).unwrap();
        assert_eq!(rk.key_id, "vk-0");
    }

    #[test]
    fn test_authenticate_malformed_authorization_falls_through_to_x_api_key() {
        // Authorization present but without "Bearer " prefix — must fall through.
        let mut h = HeaderMap::new();
        h.insert(
            http::header::AUTHORIZATION,
            "Token sk-drgtw-key0abc".parse().unwrap(),
        );
        h.insert("x-api-key", "sk-drgtw-key1xyz".parse().unwrap());

        let store = KeyStore::new(&two_key_config());
        let rk = store.authenticate(&h).unwrap();
        assert_eq!(rk.key_id, "vk-1");
    }

    // -----------------------------------------------------------------------
    // authenticate — errors
    // -----------------------------------------------------------------------

    #[test]
    fn test_authenticate_missing_both_headers() {
        let store = KeyStore::new(&two_key_config());
        let err = store.authenticate(&HeaderMap::new()).unwrap_err();
        assert_eq!(err, AuthError::MissingKey);
    }

    #[test]
    fn test_authenticate_unknown_key() {
        let store = KeyStore::new(&two_key_config());
        let err = store
            .authenticate(&bearer("sk-drgtw-does-not-exist"))
            .unwrap_err();
        assert_eq!(err, AuthError::UnknownKey);
    }

    #[test]
    fn test_authenticate_unknown_key_via_x_api_key() {
        let store = KeyStore::new(&two_key_config());
        let err = store
            .authenticate(&x_api_key("sk-drgtw-wrong"))
            .unwrap_err();
        assert_eq!(err, AuthError::UnknownKey);
    }

    #[test]
    fn test_authenticate_missing_only_malformed_authorization() {
        // Authorization header present but no Bearer prefix AND no x-api-key.
        let mut h = HeaderMap::new();
        h.insert(
            http::header::AUTHORIZATION,
            "Token sk-drgtw-key0abc".parse().unwrap(),
        );
        let store = KeyStore::new(&two_key_config());
        let err = store.authenticate(&h).unwrap_err();
        assert_eq!(err, AuthError::MissingKey);
    }

    // -----------------------------------------------------------------------
    // connection_for_model
    // -----------------------------------------------------------------------

    #[test]
    fn test_connection_for_model_happy_path() {
        let store = KeyStore::new(&two_key_config());
        let rk = store.authenticate(&bearer("sk-drgtw-key1xyz")).unwrap();
        // vk-1 has no allowlist; openai is first, serves gpt-4o
        let conn = rk.connection_for_model("gpt-4o").unwrap();
        assert_eq!(conn.name, "openai");
    }

    #[test]
    fn test_connection_for_model_picks_second_connection() {
        let store = KeyStore::new(&two_key_config());
        let rk = store.authenticate(&bearer("sk-drgtw-key1xyz")).unwrap();
        // vk-1: claude-3-5-sonnet is only in anthropic
        let conn = rk.connection_for_model("claude-3-5-sonnet").unwrap();
        assert_eq!(conn.name, "anthropic");
    }

    #[test]
    fn test_connection_for_model_allowlist_deny() {
        let store = KeyStore::new(&two_key_config());
        // vk-0 allowlist = ["gpt-4o"]; gpt-4o-mini is not in it
        let rk = store.authenticate(&bearer("sk-drgtw-key0abc")).unwrap();
        let err = rk.connection_for_model("gpt-4o-mini").unwrap_err();
        assert_eq!(err, RouteError::ModelNotAllowed("gpt-4o-mini".to_owned()));
    }

    #[test]
    fn test_connection_for_model_allowlist_permits() {
        let store = KeyStore::new(&two_key_config());
        let rk = store.authenticate(&bearer("sk-drgtw-key0abc")).unwrap();
        let conn = rk.connection_for_model("gpt-4o").unwrap();
        assert_eq!(conn.name, "openai");
    }

    #[test]
    fn test_connection_for_model_unknown_model() {
        let store = KeyStore::new(&two_key_config());
        let rk = store.authenticate(&bearer("sk-drgtw-key1xyz")).unwrap();
        let err = rk.connection_for_model("llama-3").unwrap_err();
        assert_eq!(err, RouteError::UnknownModel("llama-3".to_owned()));
    }

    // -----------------------------------------------------------------------
    // allowed_models
    // -----------------------------------------------------------------------

    #[test]
    fn test_allowed_models_no_allowlist() {
        let store = KeyStore::new(&two_key_config());
        // vk-1: no allowlist, both connections
        let rk = store.authenticate(&bearer("sk-drgtw-key1xyz")).unwrap();
        let models = rk.allowed_models();
        // openai first: gpt-4o, gpt-4o-mini; then anthropic: claude-3-5-sonnet, claude-3-haiku
        assert_eq!(
            models,
            vec![
                "gpt-4o",
                "gpt-4o-mini",
                "claude-3-5-sonnet",
                "claude-3-haiku"
            ]
        );
    }

    #[test]
    fn test_allowed_models_with_allowlist() {
        let store = KeyStore::new(&two_key_config());
        // vk-0: allowlist = ["gpt-4o"], only openai
        let rk = store.authenticate(&bearer("sk-drgtw-key0abc")).unwrap();
        let models = rk.allowed_models();
        assert_eq!(models, vec!["gpt-4o"]);
    }

    #[test]
    fn test_allowed_models_deduped_stable_order() {
        // Two connections that both list the same model — should appear once,
        // in first-seen order.
        let cfg = Config {
            server: ServerConfig::default(),
            connections: vec![
                make_connection("conn-a", &["shared-model", "model-a"]),
                make_connection("conn-b", &["shared-model", "model-b"]),
            ],
            virtual_keys: vec![make_virtual_key(
                "sk-drgtw-dedup1",
                &["conn-a", "conn-b"],
                None,
            )],
            pii: PiiConfig::default(),
            ..Config::default()
        };
        let store = KeyStore::new(&cfg);
        let rk = store.authenticate(&bearer("sk-drgtw-dedup1")).unwrap();
        let models = rk.allowed_models();
        // shared-model first-seen from conn-a, then model-a, then model-b
        assert_eq!(models, vec!["shared-model", "model-a", "model-b"]);
    }

    #[test]
    fn test_allowed_models_empty_when_allowlist_has_no_overlap() {
        // Allowlist contains a model that no connection serves.
        let cfg = Config {
            server: ServerConfig::default(),
            connections: vec![make_connection("conn", &["gpt-4o"])],
            virtual_keys: vec![make_virtual_key(
                "sk-drgtw-nooverlap",
                &["conn"],
                Some(&["llama-3"]),
            )],
            pii: PiiConfig::default(),
            ..Config::default()
        };
        let store = KeyStore::new(&cfg);
        let rk = store.authenticate(&bearer("sk-drgtw-nooverlap")).unwrap();
        assert!(rk.allowed_models().is_empty());
    }

    // -----------------------------------------------------------------------
    // Debug output must not contain key material
    // -----------------------------------------------------------------------

    #[test]
    fn test_debug_store_no_key_material() {
        let store = KeyStore::new(&two_key_config());
        let debug_str = format!("{store:?}");
        assert!(
            !debug_str.contains("sk-drgtw-key0abc"),
            "Debug must not expose key: {debug_str}"
        );
        assert!(
            !debug_str.contains("sk-drgtw-key1xyz"),
            "Debug must not expose key: {debug_str}"
        );
    }

    #[test]
    fn test_debug_secret_key_is_redacted() {
        let sk = SecretKey(b"sk-drgtw-supersecret".to_vec());
        let debug_str = format!("{sk:?}");
        assert_eq!(debug_str, "<redacted>");
    }

    #[test]
    fn test_debug_resolved_key_no_secret() {
        let store = KeyStore::new(&two_key_config());
        let rk = store.authenticate(&bearer("sk-drgtw-key0abc")).unwrap();
        let debug_str = format!("{rk:?}");
        assert!(
            !debug_str.contains("sk-drgtw-key0abc"),
            "ResolvedKey Debug must not expose key: {debug_str}"
        );
    }

    // -----------------------------------------------------------------------
    // WP 2.2 — wildcard routing: connection_for_model
    // -----------------------------------------------------------------------

    fn wildcard_routing_config() -> Config {
        // openai: exact "gpt-4o", wildcard "gpt-4o-*"
        // azure:  wildcard "gpt-*" (less specific)
        // anthropic: exact "claude-3-5-sonnet"
        Config {
            server: ServerConfig::default(),
            connections: vec![
                make_connection("openai", &["gpt-4o", "gpt-4o-*"]),
                make_connection("azure", &["gpt-*"]),
                make_connection("anthropic", &["claude-3-5-sonnet"]),
            ],
            virtual_keys: vec![make_virtual_key(
                "sk-drgtw-wctest",
                &["openai", "azure", "anthropic"],
                None,
            )],
            pii: PiiConfig::default(),
            ..Config::default()
        }
    }

    fn get_rk(cfg: &Config, key: &str) -> ResolvedKey {
        KeyStore::new(cfg).authenticate(&bearer(key)).unwrap()
    }

    #[test]
    fn test_resolved_key_carries_allow_pii_bypass() {
        let mut vk_bypass = make_virtual_key("sk-drgtw-bypass", &["openai"], None);
        vk_bypass.allow_pii_bypass = true;
        let vk_normal = make_virtual_key("sk-drgtw-normal", &["openai"], None);
        let cfg = Config {
            connections: vec![make_connection("openai", &["gpt-4o"])],
            virtual_keys: vec![vk_bypass, vk_normal],
            pii: PiiConfig::default(),
            ..Config::default()
        };
        let store = KeyStore::new(&cfg);
        assert!(
            store.authenticate(&bearer("sk-drgtw-bypass")).unwrap().allow_pii_bypass,
            "key configured allow_pii_bypass=true must resolve to true"
        );
        assert!(
            !store.authenticate(&bearer("sk-drgtw-normal")).unwrap().allow_pii_bypass,
            "key without the flag must resolve to false (fail-closed)"
        );
    }

    #[test]
    fn test_wildcard_exact_beats_wildcard() {
        // "gpt-4o" is an exact entry in openai; azure has "gpt-*" wildcard.
        // Exact should win → openai.
        let cfg = wildcard_routing_config();
        let rk = get_rk(&cfg, "sk-drgtw-wctest");
        let conn = rk.connection_for_model("gpt-4o").unwrap();
        assert_eq!(conn.name, "openai");
    }

    #[test]
    fn test_wildcard_longer_prefix_wins() {
        // "gpt-4o-mini": openai has "gpt-4o-*" (prefix "gpt-4o-", 7 chars),
        // azure has "gpt-*" (prefix "gpt-", 4 chars). openai wins.
        let cfg = wildcard_routing_config();
        let rk = get_rk(&cfg, "sk-drgtw-wctest");
        let conn = rk.connection_for_model("gpt-4o-mini").unwrap();
        assert_eq!(conn.name, "openai");
    }

    #[test]
    fn test_wildcard_shorter_prefix_used_when_only_match() {
        // "gpt-3.5-turbo": openai "gpt-4o-*" does NOT match, azure "gpt-*" DOES.
        let cfg = wildcard_routing_config();
        let rk = get_rk(&cfg, "sk-drgtw-wctest");
        let conn = rk.connection_for_model("gpt-3.5-turbo").unwrap();
        assert_eq!(conn.name, "azure");
    }

    #[test]
    fn test_wildcard_match_all_star() {
        // Connection with "*" should match anything.
        let cfg = Config {
            server: ServerConfig::default(),
            connections: vec![make_connection("catch-all", &["*"])],
            virtual_keys: vec![make_virtual_key("sk-drgtw-catchall", &["catch-all"], None)],
            pii: PiiConfig::default(),
            ..Config::default()
        };
        let rk = get_rk(&cfg, "sk-drgtw-catchall");
        let conn = rk.connection_for_model("anything-at-all").unwrap();
        assert_eq!(conn.name, "catch-all");
    }

    #[test]
    fn test_wildcard_allowlist_permits() {
        // VK allowlist has "gpt-*"; gpt-4o-mini is permitted.
        let cfg = Config {
            server: ServerConfig::default(),
            connections: vec![make_connection("openai", &["gpt-4o", "gpt-4o-mini"])],
            virtual_keys: vec![make_virtual_key(
                "sk-drgtw-wkallowlist",
                &["openai"],
                Some(&["gpt-*"]),
            )],
            pii: PiiConfig::default(),
            ..Config::default()
        };
        let rk = get_rk(&cfg, "sk-drgtw-wkallowlist");
        let conn = rk.connection_for_model("gpt-4o-mini").unwrap();
        assert_eq!(conn.name, "openai");
    }

    #[test]
    fn test_wildcard_allowlist_denies_non_matching() {
        // VK allowlist has "gpt-*"; "claude-3-haiku" is denied.
        let cfg = Config {
            server: ServerConfig::default(),
            connections: vec![make_connection("multi", &["gpt-4o", "claude-3-haiku"])],
            virtual_keys: vec![make_virtual_key(
                "sk-drgtw-wkdeny",
                &["multi"],
                Some(&["gpt-*"]),
            )],
            pii: PiiConfig::default(),
            ..Config::default()
        };
        let rk = get_rk(&cfg, "sk-drgtw-wkdeny");
        let err = rk.connection_for_model("claude-3-haiku").unwrap_err();
        assert_eq!(
            err,
            RouteError::ModelNotAllowed("claude-3-haiku".to_owned())
        );
    }

    #[test]
    fn test_wildcard_unknown_model_not_matching_any_pattern() {
        // No connection has a pattern matching "llama-3".
        let cfg = wildcard_routing_config();
        let rk = get_rk(&cfg, "sk-drgtw-wctest");
        let err = rk.connection_for_model("llama-3").unwrap_err();
        assert_eq!(err, RouteError::UnknownModel("llama-3".to_owned()));
    }

    // -----------------------------------------------------------------------
    // WP 2.2 — allowed_models with wildcards
    // -----------------------------------------------------------------------

    #[test]
    fn test_allowed_models_wildcard_in_connection_no_allowlist() {
        // Connection has exact + wildcard; no allowlist → both appear.
        // Exact comes first, wildcard after.
        let cfg = Config {
            server: ServerConfig::default(),
            connections: vec![make_connection("openai", &["gpt-4o", "gpt-4o-*"])],
            virtual_keys: vec![make_virtual_key("sk-drgtw-amwild", &["openai"], None)],
            pii: PiiConfig::default(),
            ..Config::default()
        };
        let rk = get_rk(&cfg, "sk-drgtw-amwild");
        let models = rk.allowed_models();
        assert_eq!(models, vec!["gpt-4o", "gpt-4o-*"]);
    }

    #[test]
    fn test_allowed_models_wildcard_allowlist_yields_wildcard_entry() {
        // VK allowlist has "gpt-*"; connection has exact "gpt-4o".
        // Result: "gpt-4o" (exact, permitted by wildcard allowlist), then "gpt-*"
        // from the allowlist itself.
        let cfg = Config {
            server: ServerConfig::default(),
            connections: vec![make_connection("openai", &["gpt-4o"])],
            virtual_keys: vec![make_virtual_key(
                "sk-drgtw-amwkallow",
                &["openai"],
                Some(&["gpt-*"]),
            )],
            pii: PiiConfig::default(),
            ..Config::default()
        };
        let rk = get_rk(&cfg, "sk-drgtw-amwkallow");
        let models = rk.allowed_models();
        // gpt-4o (exact, matches allowlist "gpt-*"), then "gpt-*" from allowlist
        assert_eq!(models, vec!["gpt-4o", "gpt-*"]);
    }

    #[test]
    fn test_allowed_models_wildcard_deduped() {
        // Two connections both have "gpt-*"; should appear only once.
        let cfg = Config {
            server: ServerConfig::default(),
            connections: vec![
                make_connection("openai", &["gpt-*"]),
                make_connection("azure", &["gpt-*"]),
            ],
            virtual_keys: vec![make_virtual_key(
                "sk-drgtw-amwkdedup",
                &["openai", "azure"],
                None,
            )],
            pii: PiiConfig::default(),
            ..Config::default()
        };
        let rk = get_rk(&cfg, "sk-drgtw-amwkdedup");
        let models = rk.allowed_models();
        assert_eq!(models, vec!["gpt-*"]);
    }

    // -----------------------------------------------------------------------
    // WP 2.3 — RateLimiter
    // -----------------------------------------------------------------------

    fn make_rate_limit_config(requests: u32, per_seconds: u32) -> Config {
        Config {
            server: ServerConfig::default(),
            connections: vec![make_connection("conn", &["gpt-4o"])],
            virtual_keys: vec![make_virtual_key_with_rate_limit(
                "sk-drgtw-limited",
                &["conn"],
                None,
                requests,
                per_seconds,
            )],
            pii: PiiConfig::default(),
            ..Config::default()
        }
    }

    #[test]
    fn test_rate_limiter_unlimited_when_no_config() {
        // vk-0 has no rate_limit → always Unlimited.
        let cfg = Config {
            server: ServerConfig::default(),
            connections: vec![make_connection("conn", &["gpt-4o"])],
            virtual_keys: vec![make_virtual_key("sk-drgtw-nolimit2", &["conn"], None)],
            pii: PiiConfig::default(),
            ..Config::default()
        };
        let rl = RateLimiter::new(&cfg);
        assert_eq!(rl.check("vk-0"), RateDecision::Unlimited);
    }

    #[test]
    fn test_rate_limiter_unknown_key_id_unlimited() {
        let cfg = make_rate_limit_config(10, 60);
        let rl = RateLimiter::new(&cfg);
        assert_eq!(rl.check("vk-99"), RateDecision::Unlimited);
        assert_eq!(rl.check("not-a-key"), RateDecision::Unlimited);
    }

    #[test]
    fn test_rate_limiter_allowed_counts_down() {
        // capacity=3: first call → remaining=2, second → 1, third → 0.
        let cfg = make_rate_limit_config(3, 60);
        let rl = RateLimiter::new(&cfg);
        assert_eq!(
            rl.check("vk-0"),
            RateDecision::Allowed {
                remaining: 2,
                limit: 3
            }
        );
        assert_eq!(
            rl.check("vk-0"),
            RateDecision::Allowed {
                remaining: 1,
                limit: 3
            }
        );
        assert_eq!(
            rl.check("vk-0"),
            RateDecision::Allowed {
                remaining: 0,
                limit: 3
            }
        );
    }

    #[test]
    fn test_rate_limiter_exhaustion_gives_limited() {
        let cfg = make_rate_limit_config(2, 60);
        let rl = RateLimiter::new(&cfg);
        assert!(matches!(rl.check("vk-0"), RateDecision::Allowed { .. }));
        assert!(matches!(rl.check("vk-0"), RateDecision::Allowed { .. }));
        let decision = rl.check("vk-0");
        match decision {
            RateDecision::Limited {
                retry_after_secs,
                limit,
            } => {
                assert_eq!(limit, 2);
                assert!(retry_after_secs > 0, "retry_after_secs should be positive");
                assert!(
                    retry_after_secs <= 60,
                    "retry_after_secs={retry_after_secs} should be ≤ window"
                );
            }
            other => panic!("expected Limited, got {other:?}"),
        }
    }

    #[test]
    fn test_rate_limiter_independent_buckets() {
        // Two VKs: vk-0 limited, vk-1 unlimited.
        let cfg = Config {
            server: ServerConfig::default(),
            connections: vec![make_connection("conn", &["gpt-4o"])],
            virtual_keys: vec![
                make_virtual_key_with_rate_limit("sk-drgtw-vk0lim", &["conn"], None, 1, 60),
                make_virtual_key("sk-drgtw-vk1nolim", &["conn"], None),
            ],
            pii: PiiConfig::default(),
            ..Config::default()
        };
        let rl = RateLimiter::new(&cfg);

        // vk-0: consume the one token
        assert!(matches!(rl.check("vk-0"), RateDecision::Allowed { .. }));
        // vk-0: now limited
        assert!(matches!(rl.check("vk-0"), RateDecision::Limited { .. }));
        // vk-1: unlimited regardless
        assert_eq!(rl.check("vk-1"), RateDecision::Unlimited);
    }

    #[test]
    fn test_rate_limiter_refill_over_time() {
        // Use injectable clock: capacity=2, per_seconds=1.
        // Advance clock manually to simulate refill without sleeping.
        use std::cell::Cell;

        thread_local! {
            static FAKE_NOW: Cell<u64> = const { Cell::new(0) };
        }

        fn fake_clock() -> Instant {
            // We can't create arbitrary Instants, so we use a real Instant
            // offset by a Duration. Start from a fixed base.
            static BASE: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
            let base = *BASE.get_or_init(Instant::now);
            let nanos = FAKE_NOW.with(|c| c.get());
            base + Duration::from_millis(nanos)
        }

        // Reset clock to 0.
        FAKE_NOW.with(|c| c.set(0));

        let cfg = make_rate_limit_config(2, 1); // 2 requests per 1 second
        let rl = RateLimiter::new_with_clock(&cfg, fake_clock);

        // t=0: consume both tokens
        assert!(matches!(rl.check("vk-0"), RateDecision::Allowed { .. }));
        assert!(matches!(rl.check("vk-0"), RateDecision::Allowed { .. }));
        // t=0: now limited
        assert!(matches!(rl.check("vk-0"), RateDecision::Limited { .. }));

        // Advance time by 600ms — half window (2 req/s → one token every 500ms)
        FAKE_NOW.with(|c| c.set(600));
        // One token should have refilled by now (1 token per 500ms).
        assert!(
            matches!(rl.check("vk-0"), RateDecision::Allowed { .. }),
            "expected Allowed after 600ms (one refill interval of 500ms)"
        );

        // Advance time by 1100ms more (total 1700ms) — enough for 2 more refills.
        FAKE_NOW.with(|c| c.set(1700));
        assert!(matches!(rl.check("vk-0"), RateDecision::Allowed { .. }));
        assert!(matches!(rl.check("vk-0"), RateDecision::Allowed { .. }));
        // Should be limited again.
        assert!(matches!(rl.check("vk-0"), RateDecision::Limited { .. }));
    }

    // -----------------------------------------------------------------------
    // WP 8.1 — connections_for_model
    // -----------------------------------------------------------------------

    fn multi_conn_config() -> Config {
        // openai: exact "gpt-4o", wildcard "gpt-4o-*"
        // azure:  wildcard "gpt-*" (less specific)
        // anthropic: exact "claude-3-5-sonnet"
        Config {
            server: ServerConfig::default(),
            connections: vec![
                make_connection("openai", &["gpt-4o", "gpt-4o-*"]),
                make_connection("azure", &["gpt-*"]),
                make_connection("anthropic", &["claude-3-5-sonnet"]),
            ],
            virtual_keys: vec![make_virtual_key(
                "sk-drgtw-multi",
                &["openai", "azure", "anthropic"],
                None,
            )],
            pii: PiiConfig::default(),
            events: None,
            fallback: drgtw_config::FallbackConfig::default(),
            mcp_servers: Default::default(),
            tracing: Default::default(),
            model_aliases: Default::default(),
            otel: Default::default(),
            ui: Default::default(),
            guardrails: Default::default(),
        }
    }

    #[test]
    fn test_connections_for_model_exact_only() {
        // "claude-3-5-sonnet" is an exact match in anthropic only.
        let cfg = multi_conn_config();
        let rk = get_rk(&cfg, "sk-drgtw-multi");
        let conns = rk.connections_for_model("claude-3-5-sonnet").unwrap();
        assert_eq!(conns.len(), 1);
        assert_eq!(conns[0].name, "anthropic");
    }

    #[test]
    fn test_connections_for_model_exact_beats_wildcard_in_order() {
        // "gpt-4o" is exact in openai; azure has "gpt-*" wildcard.
        // Result: [openai (exact), azure (wildcard "gpt-*")].
        let cfg = multi_conn_config();
        let rk = get_rk(&cfg, "sk-drgtw-multi");
        let conns = rk.connections_for_model("gpt-4o").unwrap();
        assert_eq!(conns[0].name, "openai", "exact match first");
        // azure wildcard "gpt-*" also matches → should appear second
        assert!(
            conns.iter().any(|c| c.name == "azure"),
            "wildcard match included"
        );
    }

    #[test]
    fn test_connections_for_model_wildcard_longest_prefix_first() {
        // "gpt-4o-mini": openai "gpt-4o-*" (7-char prefix) and azure "gpt-*" (4-char prefix).
        // No exact match. Result: [openai, azure] (longest prefix first).
        let cfg = multi_conn_config();
        let rk = get_rk(&cfg, "sk-drgtw-multi");
        let conns = rk.connections_for_model("gpt-4o-mini").unwrap();
        assert_eq!(conns[0].name, "openai", "longer prefix (gpt-4o-) wins");
        assert_eq!(conns[1].name, "azure", "shorter prefix (gpt-) second");
    }

    #[test]
    fn test_connections_for_model_first_equals_connection_for_model() {
        // connections_for_model[0] must always equal connection_for_model result.
        let cfg = multi_conn_config();
        let rk = get_rk(&cfg, "sk-drgtw-multi");
        for model in &[
            "gpt-4o",
            "gpt-4o-mini",
            "gpt-3.5-turbo",
            "claude-3-5-sonnet",
        ] {
            let single = rk.connection_for_model(model).unwrap();
            let multi = rk.connections_for_model(model).unwrap();
            assert_eq!(
                single.name, multi[0].name,
                "connection_for_model and connections_for_model[0] must agree for model={model}"
            );
        }
    }

    #[test]
    fn test_connections_for_model_allowlist_deny() {
        // VK with allowlist ["gpt-*"]; "claude-3-5-sonnet" must be denied.
        let cfg = Config {
            server: ServerConfig::default(),
            connections: vec![
                make_connection("openai", &["gpt-4o"]),
                make_connection("anthropic", &["claude-3-5-sonnet"]),
            ],
            virtual_keys: vec![make_virtual_key(
                "sk-drgtw-restrict",
                &["openai", "anthropic"],
                Some(&["gpt-*"]),
            )],
            pii: PiiConfig::default(),
            events: None,
            fallback: drgtw_config::FallbackConfig::default(),
            mcp_servers: Default::default(),
            tracing: Default::default(),
            model_aliases: Default::default(),
            otel: Default::default(),
            ui: Default::default(),
            guardrails: Default::default(),
        };
        let rk = get_rk(&cfg, "sk-drgtw-restrict");
        let err = rk.connections_for_model("claude-3-5-sonnet").unwrap_err();
        assert_eq!(
            err,
            RouteError::ModelNotAllowed("claude-3-5-sonnet".to_owned())
        );
    }

    #[test]
    fn test_connections_for_model_unknown_model() {
        let cfg = multi_conn_config();
        let rk = get_rk(&cfg, "sk-drgtw-multi");
        let err = rk.connections_for_model("llama-3").unwrap_err();
        assert_eq!(err, RouteError::UnknownModel("llama-3".to_owned()));
    }

    #[test]
    fn test_connections_for_model_config_order_tiebreak() {
        // Two connections both have "gpt-*" (same prefix length 4).
        // Config order: openai first, azure second → result [openai, azure].
        let cfg = Config {
            server: ServerConfig::default(),
            connections: vec![
                make_connection("openai", &["gpt-*"]),
                make_connection("azure", &["gpt-*"]),
            ],
            virtual_keys: vec![make_virtual_key("sk-drgtw-tie", &["openai", "azure"], None)],
            pii: PiiConfig::default(),
            events: None,
            fallback: drgtw_config::FallbackConfig::default(),
            mcp_servers: Default::default(),
            tracing: Default::default(),
            model_aliases: Default::default(),
            otel: Default::default(),
            ui: Default::default(),
            guardrails: Default::default(),
        };
        let rk = get_rk(&cfg, "sk-drgtw-tie");
        let conns = rk.connections_for_model("gpt-4o").unwrap();
        assert_eq!(conns.len(), 2);
        assert_eq!(
            conns[0].name, "openai",
            "config order tiebreak: openai first"
        );
        assert_eq!(conns[1].name, "azure");
    }

    // -----------------------------------------------------------------------
    // WP 8.1 — BudgetTracker
    // -----------------------------------------------------------------------

    fn make_budget_config(max_usd: f64, per_seconds: u32) -> Config {
        Config {
            server: ServerConfig::default(),
            connections: vec![make_connection("conn", &["gpt-4o"])],
            virtual_keys: vec![make_virtual_key_with_budget(
                "sk-drgtw-budgeted",
                &["conn"],
                max_usd,
                per_seconds,
            )],
            pii: PiiConfig::default(),
            events: None,
            fallback: drgtw_config::FallbackConfig::default(),
            mcp_servers: Default::default(),
            tracing: Default::default(),
            model_aliases: Default::default(),
            otel: Default::default(),
            ui: Default::default(),
            guardrails: Default::default(),
        }
    }

    #[test]
    fn test_budget_tracker_unlimited_without_config() {
        // Key has no budget → always Unlimited.
        let cfg = Config {
            server: ServerConfig::default(),
            connections: vec![make_connection("conn", &["gpt-4o"])],
            virtual_keys: vec![make_virtual_key("sk-drgtw-nobudget2", &["conn"], None)],
            pii: PiiConfig::default(),
            events: None,
            fallback: drgtw_config::FallbackConfig::default(),
            mcp_servers: Default::default(),
            tracing: Default::default(),
            model_aliases: Default::default(),
            otel: Default::default(),
            ui: Default::default(),
            guardrails: Default::default(),
        };
        let bt = BudgetTracker::new(&cfg);
        assert_eq!(bt.check("vk-0"), BudgetDecision::Unlimited);
    }

    #[test]
    fn test_budget_tracker_unknown_key_id_unlimited() {
        let cfg = make_budget_config(10.0, 3600);
        let bt = BudgetTracker::new(&cfg);
        assert_eq!(bt.check("vk-99"), BudgetDecision::Unlimited);
        assert_eq!(bt.check("not-a-key"), BudgetDecision::Unlimited);
    }

    #[test]
    fn test_budget_tracker_spend_accumulates() {
        let cfg = make_budget_config(1.0, 3600);
        let bt = BudgetTracker::new(&cfg);

        // Before any spend: spent_usd = 0.
        assert_eq!(
            bt.check("vk-0"),
            BudgetDecision::Allowed {
                spent_usd: 0.0,
                max_usd: 1.0
            }
        );

        // Record 0.25.
        bt.record("vk-0", 0.25);
        match bt.check("vk-0") {
            BudgetDecision::Allowed { spent_usd, max_usd } => {
                assert!((spent_usd - 0.25).abs() < 1e-9, "spent_usd={spent_usd}");
                assert!((max_usd - 1.0).abs() < 1e-9);
            }
            other => panic!("expected Allowed, got {other:?}"),
        }

        // Record another 0.50 → total 0.75, still allowed.
        bt.record("vk-0", 0.50);
        assert!(matches!(bt.check("vk-0"), BudgetDecision::Allowed { .. }));
    }

    #[test]
    fn test_budget_tracker_exhaustion_at_max() {
        let cfg = make_budget_config(0.5, 3600);
        let bt = BudgetTracker::new(&cfg);

        // Record up to the limit.
        bt.record("vk-0", 0.5);

        // Now check → Exhausted.
        match bt.check("vk-0") {
            BudgetDecision::Exhausted {
                max_usd,
                retry_after_secs,
            } => {
                assert!((max_usd - 0.5).abs() < 1e-9);
                assert!(retry_after_secs > 0, "retry_after_secs should be positive");
                assert!(
                    retry_after_secs <= 3600,
                    "retry_after_secs={retry_after_secs}"
                );
            }
            other => panic!("expected Exhausted, got {other:?}"),
        }
    }

    #[test]
    fn test_budget_tracker_window_reset_via_fake_clock() {
        use std::cell::Cell;

        thread_local! {
            static BT_FAKE_MS: Cell<u64> = const { Cell::new(0) };
        }

        fn bt_clock() -> Instant {
            static BASE: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
            let base = *BASE.get_or_init(Instant::now);
            base + Duration::from_millis(BT_FAKE_MS.with(|c| c.get()))
        }

        BT_FAKE_MS.with(|c| c.set(0));

        // Budget: $0.50 per 60-second window.
        let cfg = make_budget_config(0.50, 60);
        let bt = BudgetTracker::new_with_clock(&cfg, bt_clock);

        // t=0: record $0.50 → exhausted.
        bt.record("vk-0", 0.50);
        assert!(matches!(bt.check("vk-0"), BudgetDecision::Exhausted { .. }));

        // Advance 61 seconds → window has elapsed → counter resets.
        BT_FAKE_MS.with(|c| c.set(61_000));
        assert_eq!(
            bt.check("vk-0"),
            BudgetDecision::Allowed {
                spent_usd: 0.0,
                max_usd: 0.50
            },
            "window should reset after 61s"
        );

        // Can spend again.
        bt.record("vk-0", 0.25);
        assert!(matches!(bt.check("vk-0"), BudgetDecision::Allowed { .. }));
    }

    #[test]
    fn test_budget_tracker_independent_keys() {
        // vk-0 has $1 budget; vk-1 has no budget.
        let cfg = Config {
            server: ServerConfig::default(),
            connections: vec![make_connection("conn", &["gpt-4o"])],
            virtual_keys: vec![
                make_virtual_key_with_budget("sk-drgtw-bvk0", &["conn"], 1.0, 3600),
                make_virtual_key("sk-drgtw-bvk1", &["conn"], None),
            ],
            pii: PiiConfig::default(),
            events: None,
            fallback: drgtw_config::FallbackConfig::default(),
            mcp_servers: Default::default(),
            tracing: Default::default(),
            model_aliases: Default::default(),
            otel: Default::default(),
            ui: Default::default(),
            guardrails: Default::default(),
        };
        let bt = BudgetTracker::new(&cfg);

        // Exhaust vk-0.
        bt.record("vk-0", 1.0);
        assert!(matches!(bt.check("vk-0"), BudgetDecision::Exhausted { .. }));

        // vk-1 is still unlimited.
        assert_eq!(bt.check("vk-1"), BudgetDecision::Unlimited);
    }

    #[test]
    fn test_budget_tracker_record_unknown_key_id_noop() {
        let cfg = make_budget_config(1.0, 3600);
        let bt = BudgetTracker::new(&cfg);

        // Recording on unknown keys must not panic and must be a no-op.
        bt.record("vk-99", 999.0);
        bt.record("not-a-key", 999.0);

        // vk-0's budget is unaffected.
        assert_eq!(
            bt.check("vk-0"),
            BudgetDecision::Allowed {
                spent_usd: 0.0,
                max_usd: 1.0
            }
        );
    }

    #[test]
    fn test_budget_tracker_retry_after_secs_value() {
        use std::cell::Cell;

        thread_local! {
            static RA_FAKE_MS: Cell<u64> = const { Cell::new(0) };
        }

        fn ra_clock() -> Instant {
            static BASE: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
            let base = *BASE.get_or_init(Instant::now);
            base + Duration::from_millis(RA_FAKE_MS.with(|c| c.get()))
        }

        RA_FAKE_MS.with(|c| c.set(0));

        // Budget: $0.10 per 100s window.
        let cfg = make_budget_config(0.10, 100);
        let bt = BudgetTracker::new_with_clock(&cfg, ra_clock);

        // Exhaust at t=0 (window starts at 0).
        bt.record("vk-0", 0.10);

        // At t=30s → 70s remain → retry_after_secs = 70.
        RA_FAKE_MS.with(|c| c.set(30_000));
        match bt.check("vk-0") {
            BudgetDecision::Exhausted {
                retry_after_secs, ..
            } => {
                assert_eq!(
                    retry_after_secs, 70,
                    "retry_after_secs should be 70 at t=30s"
                );
            }
            other => panic!("expected Exhausted, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // rebuild_from — RateLimiter
    // -----------------------------------------------------------------------

    #[test]
    fn test_rate_limiter_rebuild_surviving_key_keeps_tokens() {
        // Key "sk-drgtw-key0abc" has rate limit 5/60s.
        let cfg = make_rate_limit_config(5, 60);
        let rl = RateLimiter::new(&cfg);

        // Consume 3 tokens.
        for _ in 0..3 {
            rl.check("vk-0");
        }
        let snap_before = rl.snapshot("vk-0").expect("snapshot present");
        assert_eq!(snap_before.remaining, 2, "3 consumed → 2 remaining");

        // Rebuild with the same config (same secret).
        let rl2 = rl.rebuild_from(&cfg, &cfg);
        let snap_after = rl2.snapshot("vk-0").expect("snapshot present after rebuild");
        assert_eq!(snap_after.remaining, 2, "surviving key keeps its token count");
    }

    #[test]
    fn test_rate_limiter_rebuild_new_key_starts_full() {
        let old_cfg = make_rate_limit_config(4, 60);
        let rl = RateLimiter::new(&old_cfg);
        rl.check("vk-0"); // consume 1 token

        // New config has a different secret — bucket must start full.
        let new_cfg = Config {
            virtual_keys: vec![make_virtual_key_with_rate_limit(
                "sk-drgtw-newkey999",
                &["conn1"],
                None,
                4,
                60,
            )],
            ..old_cfg.clone()
        };
        let rl2 = rl.rebuild_from(&old_cfg, &new_cfg);
        let snap = rl2.snapshot("vk-0").expect("snapshot present");
        // Full bucket: capacity 4, all tokens available.
        assert_eq!(snap.capacity, 4);
        assert_eq!(snap.remaining, 4, "new secret → full bucket");
    }

    #[test]
    fn test_rate_limiter_rebuild_reordered_keys_keep_their_own_counters() {
        // Two keys, consume from vk-0 only.
        let cfg = Config {
            connections: vec![make_connection("conn1", &["gpt-4"])],
            virtual_keys: vec![
                make_virtual_key_with_rate_limit("sk-drgtw-keyA111", &["conn1"], None, 5, 60),
                make_virtual_key_with_rate_limit("sk-drgtw-keyB222", &["conn1"], None, 5, 60),
            ],
            ..Config::default()
        };
        let rl = RateLimiter::new(&cfg);
        rl.check("vk-0"); // consume from keyA
        rl.check("vk-0");

        // Rebuild with swapped order — each key must track its own counter.
        let cfg2 = Config {
            connections: vec![make_connection("conn1", &["gpt-4"])],
            virtual_keys: vec![
                make_virtual_key_with_rate_limit("sk-drgtw-keyB222", &["conn1"], None, 5, 60),
                make_virtual_key_with_rate_limit("sk-drgtw-keyA111", &["conn1"], None, 5, 60),
            ],
            ..Config::default()
        };
        let rl2 = rl.rebuild_from(&cfg, &cfg2);

        // vk-0 is now keyB (untouched → full).
        let snap_b = rl2.snapshot("vk-0").expect("vk-0 (keyB) snapshot");
        assert_eq!(snap_b.remaining, 5, "keyB had no consumption, must be full");

        // vk-1 is now keyA (2 consumed → 3 remaining).
        let snap_a = rl2.snapshot("vk-1").expect("vk-1 (keyA) snapshot");
        assert_eq!(snap_a.remaining, 3, "keyA had 2 consumed, must carry counter");
    }

    // -----------------------------------------------------------------------
    // rebuild_from — BudgetTracker
    // -----------------------------------------------------------------------

    #[test]
    fn test_budget_tracker_rebuild_surviving_key_keeps_spend() {
        let cfg = make_budget_config(1.0, 3600);
        let bt = BudgetTracker::new(&cfg);

        // Record $0.25 spend.
        bt.record("vk-0", 0.25);
        let snap_before = bt.snapshot("vk-0").expect("snapshot present");
        assert!((snap_before.spent_usd - 0.25).abs() < 1e-9, "spent_usd = 0.25");

        // Rebuild with same config (same secret).
        let bt2 = bt.rebuild_from(&cfg, &cfg);
        let snap_after = bt2.snapshot("vk-0").expect("snapshot after rebuild");
        assert!(
            (snap_after.spent_usd - 0.25).abs() < 1e-9,
            "surviving key keeps spend: got {}",
            snap_after.spent_usd
        );
    }

    #[test]
    fn test_budget_tracker_rebuild_new_key_starts_empty() {
        let old_cfg = make_budget_config(1.0, 3600);
        let bt = BudgetTracker::new(&old_cfg);
        bt.record("vk-0", 0.50);

        // Different secret → fresh window.
        let new_cfg = Config {
            virtual_keys: vec![make_virtual_key_with_budget(
                "sk-drgtw-newkey999",
                &["conn1"],
                1.0,
                3600,
            )],
            ..old_cfg.clone()
        };
        let bt2 = bt.rebuild_from(&old_cfg, &new_cfg);
        let snap = bt2.snapshot("vk-0").expect("snapshot present");
        assert!((snap.spent_usd - 0.0).abs() < 1e-9, "new secret → zero spend");
    }

    #[test]
    fn test_budget_tracker_rebuild_reordered_keys_keep_their_own_spend() {
        let cfg = Config {
            connections: vec![make_connection("conn1", &["gpt-4"])],
            virtual_keys: vec![
                make_virtual_key_with_budget("sk-drgtw-keyA111", &["conn1"], 1.0, 3600),
                make_virtual_key_with_budget("sk-drgtw-keyB222", &["conn1"], 1.0, 3600),
            ],
            ..Config::default()
        };
        let bt = BudgetTracker::new(&cfg);
        bt.record("vk-0", 0.30); // spend on keyA only

        // Swap order.
        let cfg2 = Config {
            connections: vec![make_connection("conn1", &["gpt-4"])],
            virtual_keys: vec![
                make_virtual_key_with_budget("sk-drgtw-keyB222", &["conn1"], 1.0, 3600),
                make_virtual_key_with_budget("sk-drgtw-keyA111", &["conn1"], 1.0, 3600),
            ],
            ..Config::default()
        };
        let bt2 = bt.rebuild_from(&cfg, &cfg2);

        // vk-0 is now keyB (no spend).
        let snap_b = bt2.snapshot("vk-0").expect("keyB snapshot");
        assert!((snap_b.spent_usd).abs() < 1e-9, "keyB had no spend");

        // vk-1 is now keyA ($0.30 spend carried).
        let snap_a = bt2.snapshot("vk-1").expect("keyA snapshot");
        assert!(
            (snap_a.spent_usd - 0.30).abs() < 1e-9,
            "keyA spend carried: got {}",
            snap_a.spent_usd
        );
    }

    // -----------------------------------------------------------------------
    // ResolvedKey mcp_servers field
    // -----------------------------------------------------------------------

    #[test]
    fn test_resolved_key_mcp_servers_none_maps_to_empty_vec() {
        let cfg = two_key_config();
        let store = KeyStore::new(&cfg);
        let rk = store.authenticate(&bearer("sk-drgtw-key0abc")).unwrap();
        assert!(
            rk.mcp_servers.is_empty(),
            "None mcp_servers in VirtualKey → empty vec in ResolvedKey"
        );
    }
}
