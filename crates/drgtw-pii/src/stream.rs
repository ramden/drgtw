//! Streaming restore with chunk-boundary holdback. WP 3.3.
//!
//! SSE response bytes flow through [`StreamRestorer::feed`]; placeholders are
//! replaced with their originals even when a placeholder is split across
//! chunk boundaries (`PERSO` + `N_1`). Replacement values are JSON-string
//! escaped because placeholders only ever occur inside JSON string values of
//! SSE `data:` payloads.
//!
//! # Algorithm
//!
//! The restorer maintains a `buf: String` (the holdback buffer).
//!
//! On each `feed(chunk)`:
//!
//! 1. Append `chunk` to `buf`.
//! 2. Scan `buf[scan_from..]` for complete placeholder occurrences that are
//!    **not** followed by a decimal digit (digit-boundary rule: `EMAIL_1` must
//!    not match inside `EMAIL_12`). Entries are sorted longest-first, so the
//!    greedy match always wins. If a complete placeholder sits at the very end
//!    of `buf`, it is **held** (not replaced) until more input arrives —
//!    the next chunk might start with a digit making it an unknown-longer
//!    placeholder.
//! 3. After replacements, find the longest suffix of `buf` that is a prefix
//!    (strict or complete) of any known placeholder. That suffix is held;
//!    everything before it is released.
//! 4. Holdback ≤ `max_placeholder_len - 1` bytes for non-matching text
//!    because the holdback contains at most the start of a placeholder.
//!
//! On `finish()`: same scan but the "complete placeholder at end of buffer"
//! hold is lifted — we know the stream is over.
//!
//! ## Re-scan strategy
//!
//! The holdback buf is bounded to at most `max_placeholder_len - 1` bytes at
//! all times. On every `feed()` call the full holdback is re-scanned from
//! byte 0. Because the holdback is tiny (≤ max placeholder length), this is
//! O(max_ph_len) per call — effectively O(1). No stale-pointer tracking needed.
//! A future optimisation could track a safe re-scan offset, but correctness
//! comes first and the current approach is already fast for realistic maps.

use std::sync::Arc;

use crate::EntityMap;

// ---------------------------------------------------------------------------
// Internal trait — decouples StreamRestorer from the concrete EntityMap so
// tests can inject a fake without touching the frozen entity_map.rs.
// ---------------------------------------------------------------------------

/// Internal view of the placeholder map needed by the streaming restorer.
#[doc(hidden)]
pub trait MapView: Send + Sync {
    /// Iterate `(placeholder, original)` pairs.
    fn pairs(&self) -> Vec<(String, String)>;
    /// Length in bytes of the longest placeholder (0 when empty).
    fn max_ph_len(&self) -> usize;
}

/// Adapter that delegates to the real `EntityMap` public API.
struct RealMapView(Arc<EntityMap>);

impl MapView for RealMapView {
    fn pairs(&self) -> Vec<(String, String)> {
        self.0
            .iter()
            .map(|(p, o)| (p.to_owned(), o.to_owned()))
            .collect()
    }
    fn max_ph_len(&self) -> usize {
        self.0.max_placeholder_len()
    }
}

// ---------------------------------------------------------------------------
// Pre-computed per-placeholder data
// ---------------------------------------------------------------------------

struct PlaceholderEntry {
    placeholder: String,
    /// JSON-string-escaped original, cached once at construction time.
    escaped_original: String,
}

// ---------------------------------------------------------------------------
// StreamRestorer
// ---------------------------------------------------------------------------

/// Stateful per-response restorer.
///
/// Algorithm contract (WP 3.3):
/// - `feed(chunk)` returns all input so far that is SAFE to release: output
///   ends before any suffix that is a strict prefix of any known placeholder.
/// - holdback never exceeds `map.max_placeholder_len() - 1` bytes
/// - non-matching text is never withheld beyond that bound
/// - `finish()` flushes any held bytes unmodified (a partial match at stream
///   end is by definition not a placeholder)
/// - replacement values are JSON-escaped (`"` → `\"`, `\` → `\\`, control
///   chars → `\uXXXX`) — placeholders appear only inside JSON strings
/// - placeholders unknown to the map (e.g. model hallucinating `EMAIL_99`)
///   pass through unchanged
pub struct StreamRestorer {
    // kept only for the public API shape; real work done via `entries`
    _map: Arc<EntityMap>,
    /// Pre-computed entries sorted descending by placeholder length so
    /// longest-match always wins (EMAIL_12 before EMAIL_1).
    entries: Vec<PlaceholderEntry>,
    /// Length of the longest known placeholder in bytes (0 when map empty).
    max_ph_len: usize,
    /// Holdback buffer: bytes received but not yet released.
    /// Invariant: `buf.len() <= max_ph_len - 1` after every `feed()`.
    buf: String,
}

impl StreamRestorer {
    /// Construct from a real `EntityMap`.
    pub fn new(map: Arc<EntityMap>) -> Self {
        let view = RealMapView(Arc::clone(&map));
        Self::from_view(map, &view)
    }

    /// Length of the current holdback buffer in bytes. Exposed for tests.
    #[doc(hidden)]
    pub fn holdback_len(&self) -> usize {
        self.buf.len()
    }

    /// Internal constructor shared with tests — accepts any `MapView`.
    #[doc(hidden)]
    pub fn from_view(map: Arc<EntityMap>, view: &dyn MapView) -> Self {
        let mut entries: Vec<PlaceholderEntry> = view
            .pairs()
            .into_iter()
            .map(|(placeholder, original)| PlaceholderEntry {
                escaped_original: json_escape(&original),
                placeholder,
            })
            .collect();
        // Longest first → greedy longest-match replacement.
        entries.sort_by_key(|e| std::cmp::Reverse(e.placeholder.len()));

        let max_ph_len = view.max_ph_len();

        Self {
            _map: map,
            entries,
            max_ph_len,
            buf: String::new(),
        }
    }

    // -----------------------------------------------------------------------
    // Public API
    // -----------------------------------------------------------------------

    /// Feed a chunk; get back restored text safe to forward now.
    pub fn feed(&mut self, chunk: &str) -> String {
        self.buf.push_str(chunk);
        self.process(false)
    }

    /// End of stream: release whatever is held back.
    pub fn finish(&mut self) -> String {
        self.process(true)
    }

    // -----------------------------------------------------------------------
    // Core processing
    // -----------------------------------------------------------------------

    fn process(&mut self, is_finish: bool) -> String {
        if self.entries.is_empty() || self.max_ph_len == 0 {
            // No placeholders known — passthrough everything immediately.
            return std::mem::take(&mut self.buf);
        }

        // --- Step 1: apply replacements in buf --------------------------------
        // Replace all complete placeholders that are safe to replace now.
        // Placeholders at the exact end of buf are kept verbatim (handled by
        // holdback logic below) unless is_finish.
        let transformed = self.apply_replacements(is_finish);
        self.buf = transformed;

        // --- Step 2: find longest suffix that must be held -------------------
        // Hold back any suffix of buf that is a prefix (partial or complete)
        // of any known placeholder. A complete placeholder at end is held
        // because the next chunk might start with a digit.
        let holdback_len = if is_finish {
            0
        } else {
            longest_prefix_suffix(&self.buf, &self.entries)
        };

        let release_up_to = self.buf.len().saturating_sub(holdback_len);

        // --- Step 3: split and release ----------------------------------------
        let released = self.buf[..release_up_to].to_owned();
        self.buf = self.buf[release_up_to..].to_owned();

        released
    }

    /// Scan `buf[scan_from..]`, replace complete placeholders (with digit-
    /// boundary checking), and return the transformed replacement string for
    /// that slice.
    ///
    /// A placeholder at the exact end of the scan region is treated as
    /// follows when `!is_finish`:
    /// - It is emitted verbatim (no replacement), so it stays in the buffer.
    /// - `longest_prefix_suffix` in `process()` will then hold it back,
    ///   waiting for the next chunk which might bring a digit.
    ///
    /// This approach keeps `apply_replacements` free of early-exit side-effects:
    /// it always returns a complete replacement for the entire `buf[scan_from..]`.
    fn apply_replacements(&self, is_finish: bool) -> String {
        let src = self.buf.as_str();
        if src.is_empty() {
            return String::new();
        }

        let src_bytes = src.as_bytes();
        let src_len = src_bytes.len();
        let mut out = String::with_capacity(src_len);
        // `run_start` tracks the start of the current verbatim (non-placeholder) run.
        // We accumulate verbatim runs as string slices rather than byte-by-byte,
        // preserving multibyte UTF-8 correctly.
        let mut run_start = 0usize;
        let mut i = 0usize;

        'outer: while i < src_len {
            // Try placeholders longest-first.
            for entry in &self.entries {
                let ph = entry.placeholder.as_bytes();
                let ph_len = ph.len();

                if i + ph_len > src_len {
                    continue; // doesn't fit
                }
                if &src_bytes[i..i + ph_len] != ph {
                    continue; // no match
                }

                // Matched! Check the character immediately after the placeholder.
                let after = i + ph_len;
                let next_byte = src_bytes.get(after).copied();

                let should_replace = match next_byte {
                    Some(b) if b.is_ascii_digit() => {
                        // Digit follows in-buffer: longer unknown placeholder.
                        // Pass through verbatim (digit is present so no hold
                        // needed — we know it's not in our map).
                        false
                    }
                    None if !is_finish => {
                        // Placeholder at exact end; next chunk unknown.
                        // Leave verbatim so longest_prefix_suffix holds it.
                        false
                    }
                    _ => true, // non-digit follows, or true end of stream
                };

                if should_replace {
                    // Flush accumulated verbatim run.
                    out.push_str(&src[run_start..i]);
                    out.push_str(&entry.escaped_original);
                    i += ph_len;
                    run_start = i;
                    continue 'outer;
                } else {
                    // Emit placeholder verbatim — advance past it.
                    // (verbatim run will capture it when flushed)
                    i += ph_len;
                    continue 'outer;
                }
            }

            // No placeholder matched at position i.
            // Advance by one byte; the verbatim run will capture this byte.
            i += 1;
        }

        // Flush remaining verbatim run.
        out.push_str(&src[run_start..]);
        out
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Return the length (bytes) of the longest suffix of `s` that is a prefix
/// (strict or complete) of any placeholder in `entries`. Returns 0 if none.
///
/// A complete placeholder at the end of `s` counts: its trailing character
/// could be followed by a digit in the next chunk, so it must be held.
fn longest_prefix_suffix(s: &str, entries: &[PlaceholderEntry]) -> usize {
    if s.is_empty() || entries.is_empty() {
        return 0;
    }
    let s_bytes = s.as_bytes();
    let s_len = s_bytes.len();
    let mut best = 0usize;

    for entry in entries {
        let ph = entry.placeholder.as_bytes();
        let ph_len = ph.len();
        let max_check = s_len.min(ph_len);
        // Try the longest suffix first; stop at first hit for this placeholder.
        for suffix_len in (1..=max_check).rev() {
            let suffix_start = s_len - suffix_len;
            if s_bytes[suffix_start..] == ph[..suffix_len] {
                if suffix_len > best {
                    best = suffix_len;
                }
                break;
            }
        }
    }

    best
}

/// JSON-string-escape a string value. Used once per entity at construction.
///
/// Rules: `"` → `\"`, `\` → `\\`, control chars (U+0000–U+001F) → `\uXXXX`
/// (with canonical single-char escapes for common controls).
#[doc(hidden)]
pub fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\x08' => out.push_str("\\b"),
            '\x09' => out.push_str("\\t"),
            '\x0A' => out.push_str("\\n"),
            '\x0C' => out.push_str("\\f"),
            '\x0D' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04X}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Test helpers — a fake MapView usable without real EntityMap.
// `pub` so integration tests (outside the crate) can import them.
// ---------------------------------------------------------------------------

/// Test helpers: fake `MapView` and `restorer_from_fake` constructor.
/// Exposed so integration tests can build a `StreamRestorer` without a
/// real (WP 3.2) `EntityMap`.
#[doc(hidden)]
pub mod testutil {
    use super::*;

    /// Minimal fake `MapView` for unit/property tests.
    pub struct FakeMap {
        pub pairs: Vec<(String, String)>,
    }

    impl FakeMap {
        pub fn new() -> Self {
            Self { pairs: Vec::new() }
        }

        pub fn insert(&mut self, placeholder: impl Into<String>, original: impl Into<String>) {
            self.pairs.push((placeholder.into(), original.into()));
        }
    }

    impl Default for FakeMap {
        fn default() -> Self {
            Self::new()
        }
    }

    impl MapView for FakeMap {
        fn pairs(&self) -> Vec<(String, String)> {
            self.pairs.clone()
        }
        fn max_ph_len(&self) -> usize {
            self.pairs.iter().map(|(p, _)| p.len()).max().unwrap_or(0)
        }
    }

    /// Build a `StreamRestorer` from a `FakeMap`, bypassing the real EntityMap.
    /// `EntityMap::new()` is safe to call (only its methods panic).
    pub fn restorer_from_fake(fake: &FakeMap) -> StreamRestorer {
        let dummy = Arc::new(EntityMap::new());
        StreamRestorer::from_view(dummy, fake)
    }
}

// ---------------------------------------------------------------------------
// Inline unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod unit_tests {
    use super::testutil::*;
    use super::*;

    // --- json_escape ---------------------------------------------------------

    #[test]
    fn json_escape_plain() {
        assert_eq!(json_escape("hello"), "hello");
    }

    #[test]
    fn json_escape_quote() {
        assert_eq!(json_escape(r#"say "hi""#), r#"say \"hi\""#);
    }

    #[test]
    fn json_escape_backslash() {
        assert_eq!(json_escape("a\\b"), "a\\\\b");
    }

    #[test]
    fn json_escape_newline() {
        assert_eq!(json_escape("line1\nline2"), "line1\\nline2");
    }

    #[test]
    fn json_escape_control() {
        assert_eq!(json_escape("\x01"), "\\u0001");
    }

    #[test]
    fn json_escape_tab() {
        assert_eq!(json_escape("\t"), "\\t");
    }

    // --- empty map / empty chunk --------------------------------------------

    #[test]
    fn empty_map_passthrough() {
        let mut r = restorer_from_fake(&FakeMap::new());
        assert_eq!(r.feed("hello"), "hello");
        assert_eq!(r.finish(), "");
    }

    #[test]
    fn empty_chunk_noop() {
        let mut fake = FakeMap::new();
        fake.insert("EMAIL_1", "user@example.com");
        let mut r = restorer_from_fake(&fake);
        assert_eq!(r.feed(""), "");
        // EMAIL_1 at end is held
        assert_eq!(r.feed("EMAIL_1"), "");
        assert_eq!(r.finish(), "user@example.com");
    }

    // --- placeholder fully inside one chunk ---------------------------------

    #[test]
    fn placeholder_single_chunk() {
        let mut fake = FakeMap::new();
        fake.insert("EMAIL_1", "user@example.com");
        let mut r = restorer_from_fake(&fake);
        let out = r.feed("Hello EMAIL_1 world");
        let rest = r.finish();
        assert_eq!(out + &rest, "Hello user@example.com world");
    }

    // --- split at every byte boundary of placeholder ------------------------

    #[test]
    fn split_at_every_byte_boundary() {
        let placeholder = "EMAIL_1";
        let original = "user@example.com";
        let text = format!("before {} after", placeholder);

        for split in 0..=text.len() {
            let chunk1 = &text[..split];
            let chunk2 = &text[split..];

            let mut fake = FakeMap::new();
            fake.insert(placeholder, original);
            let mut r = restorer_from_fake(&fake);

            let o1 = r.feed(chunk1);
            let o2 = r.feed(chunk2);
            let fin = r.finish();
            let combined = o1 + &o2 + &fin;
            assert_eq!(
                combined,
                format!("before {} after", original),
                "failed at split={split}"
            );
        }
    }

    // --- split across 3+ chunks ---------------------------------------------

    #[test]
    fn split_three_chunks() {
        let mut fake = FakeMap::new();
        fake.insert("EMAIL_1", "user@example.com");
        let mut r = restorer_from_fake(&fake);

        let o1 = r.feed("EM");
        let o2 = r.feed("AIL");
        let o3 = r.feed("_1 done");
        let fin = r.finish();
        assert_eq!(o1 + &o2 + &o3 + &fin, "user@example.com done");
    }

    // --- EMAIL_1 / EMAIL_12 prefix-overlap ----------------------------------

    #[test]
    fn prefix_overlap_longer_wins() {
        let mut fake = FakeMap::new();
        fake.insert("EMAIL_1", "alice@example.com");
        fake.insert("EMAIL_12", "bob@example.com");
        let mut r = restorer_from_fake(&fake);

        let out = r.feed("EMAIL_12 end");
        let fin = r.finish();
        assert_eq!(out + &fin, "bob@example.com end");
    }

    #[test]
    fn prefix_overlap_non_digit_releases_shorter() {
        // Feed "EMAIL_1" (held), then "x" — EMAIL_1 must fire.
        let mut fake = FakeMap::new();
        fake.insert("EMAIL_1", "alice@example.com");
        fake.insert("EMAIL_12", "bob@example.com");
        let mut r = restorer_from_fake(&fake);

        let o1 = r.feed("EMAIL_1"); // held: could be EMAIL_12...
        let o2 = r.feed("x"); // 'x' not digit → release EMAIL_1 replacement
        let fin = r.finish();
        assert_eq!(o1 + &o2 + &fin, "alice@example.comx");
    }

    #[test]
    fn prefix_overlap_digit_not_in_map_passes_through() {
        // Only EMAIL_1 in map; stream delivers "EMAIL_1" then "2x".
        // EMAIL_12 is unknown → pass entire "EMAIL_12" verbatim.
        let mut fake = FakeMap::new();
        fake.insert("EMAIL_1", "alice@example.com");
        let mut r = restorer_from_fake(&fake);

        let o1 = r.feed("EMAIL_1"); // held
        let o2 = r.feed("2x");
        let fin = r.finish();
        assert_eq!(o1 + &o2 + &fin, "EMAIL_12x");
    }

    // --- trailing placeholder resolved by finish() --------------------------

    #[test]
    fn trailing_placeholder_finish() {
        let mut fake = FakeMap::new();
        fake.insert("EMAIL_1", "user@example.com");
        let mut r = restorer_from_fake(&fake);

        let o = r.feed("see EMAIL_1");
        let fin = r.finish();
        assert_eq!(o + &fin, "see user@example.com");
    }

    // --- unknown placeholder passthrough ------------------------------------

    #[test]
    fn unknown_placeholder_passthrough() {
        let mut fake = FakeMap::new();
        fake.insert("EMAIL_1", "user@example.com");
        let mut r = restorer_from_fake(&fake);

        let out = r.feed("EMAIL_99 is unknown");
        let fin = r.finish();
        assert_eq!(out + &fin, "EMAIL_99 is unknown");
    }

    // --- JSON escaping of originals -----------------------------------------

    #[test]
    fn original_with_quote() {
        let mut fake = FakeMap::new();
        fake.insert("EMAIL_1", r#"say "hi" user@x.com"#);
        let mut r = restorer_from_fake(&fake);

        let out = r.feed("EMAIL_1");
        let fin = r.finish();
        assert_eq!(out + &fin, r#"say \"hi\" user@x.com"#);
    }

    #[test]
    fn original_with_backslash() {
        let mut fake = FakeMap::new();
        fake.insert("EMAIL_1", "path\\to\\file");
        let mut r = restorer_from_fake(&fake);

        let out = r.feed("EMAIL_1");
        let fin = r.finish();
        assert_eq!(out + &fin, "path\\\\to\\\\file");
    }

    #[test]
    fn original_with_newline() {
        let mut fake = FakeMap::new();
        fake.insert("EMAIL_1", "line1\nline2");
        let mut r = restorer_from_fake(&fake);

        let out = r.feed("val=EMAIL_1;");
        let fin = r.finish();
        assert_eq!(out + &fin, "val=line1\\nline2;");
    }

    // --- multibyte context --------------------------------------------------

    #[test]
    fn multibyte_context() {
        let mut fake = FakeMap::new();
        fake.insert("EMAIL_1", "ü@example.com");
        let mut r = restorer_from_fake(&fake);

        let out = r.feed("Héllo EMAIL_1 Wörld");
        let fin = r.finish();
        assert_eq!(out + &fin, "Héllo ü@example.com Wörld");
    }

    // --- holdback length invariant ------------------------------------------

    #[test]
    fn holdback_never_exceeds_max_ph_len_minus_one() {
        let mut fake = FakeMap::new();
        fake.insert("EMAIL_1", "user@example.com"); // placeholder len = 7
        let max_hold = "EMAIL_1".len() - 1; // 6

        let mut r = restorer_from_fake(&fake);
        let chunks = [
            "aaaa",
            "bbbb",
            "cccc",
            "dddd longish non-matching text here",
        ];
        for chunk in &chunks {
            r.feed(chunk);
            let held = r.buf.len();
            assert!(
                held <= max_hold,
                "held {held} > max allowed {max_hold} after chunk {chunk:?}"
            );
        }
        r.finish();
    }

    // --- multiple placeholders ----------------------------------------------

    #[test]
    fn multiple_placeholders() {
        let mut fake = FakeMap::new();
        fake.insert("EMAIL_1", "alice@example.com");
        fake.insert("EMAIL_2", "bob@example.com");
        let mut r = restorer_from_fake(&fake);

        let out = r.feed("from EMAIL_1 to EMAIL_2!");
        let fin = r.finish();
        assert_eq!(out + &fin, "from alice@example.com to bob@example.com!");
    }

    // --- SSE realism --------------------------------------------------------

    #[test]
    fn sse_realistic_chunked() {
        let mut fake = FakeMap::new();
        fake.insert("PERSON_1", "Alice Smith");
        fake.insert("EMAIL_1", "alice@company.com");

        let expected = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":",
            "\"Hello Alice Smith, your email alice@company.com is confirmed.\"}}]}\n\n"
        );

        let chunks = [
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hello PER",
            "SON_1, your email EMA",
            "IL_1 is confirmed.\"}}]}\n\n",
        ];

        let mut r = restorer_from_fake(&fake);
        let mut assembled = String::new();
        for chunk in &chunks {
            assembled.push_str(&r.feed(chunk));
        }
        assembled.push_str(&r.finish());

        assert_eq!(assembled, expected);
    }
}
