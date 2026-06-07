//! Integration tests for StreamRestorer (WP 3.3).
//!
//! Tests are structured in three groups:
//!   1. Unit-level behavioural tests (all chunking scenarios, edge cases)
//!   2. Property tests via proptest (arbitrary chunking must equal naive replace)
//!   3. SSE realism tests (synthetic OpenAI SSE stream)

use drgtw_pii::stream::testutil::{FakeMap, restorer_from_fake};

// ---------------------------------------------------------------------------
// Reference implementation used by property tests
// ---------------------------------------------------------------------------

/// Naive full-text restore: longest-match, no-digit-after-match, pass-through
/// unknown placeholders. This is the spec; StreamRestorer must match it.
fn naive_restore(text: &str, pairs: &[(String, String)]) -> String {
    if pairs.is_empty() {
        return text.to_owned();
    }
    // Sort longest-first for greedy match.
    let mut sorted = pairs.to_vec();
    sorted.sort_by_key(|pair| std::cmp::Reverse(pair.0.len()));

    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len);
    // `run_start` tracks the start of the verbatim run, emitted as a slice
    // to preserve multibyte UTF-8 correctly.
    let mut run_start = 0usize;
    let mut i = 0usize;

    'outer: while i < len {
        for (ph, orig) in &sorted {
            let ph_bytes = ph.as_bytes();
            let ph_len = ph_bytes.len();
            if i + ph_len > len {
                continue;
            }
            if &bytes[i..i + ph_len] != ph_bytes {
                continue;
            }
            // Check digit-boundary rule.
            let after = i + ph_len;
            if bytes
                .get(after)
                .map(|b| b.is_ascii_digit())
                .unwrap_or(false)
            {
                // Digit follows — unknown longer shape, pass through verbatim.
                i += ph_len;
                continue 'outer;
            }
            // Flush verbatim run, then emit replacement.
            out.push_str(&text[run_start..i]);
            out.push_str(&drgtw_pii::stream::json_escape(orig));
            i += ph_len;
            run_start = i;
            continue 'outer;
        }
        // No match at i — advance one byte (ASCII safe since placeholders are ASCII).
        i += 1;
    }
    // Flush remaining verbatim run.
    out.push_str(&text[run_start..]);
    out
}

// ---------------------------------------------------------------------------
// Helper: run restorer over a slice of chunks, concatenate all output.
// ---------------------------------------------------------------------------
fn run_restorer(fake: &FakeMap, chunks: &[&str]) -> String {
    let mut r = restorer_from_fake(fake);
    let mut out = String::new();
    for c in chunks {
        out.push_str(&r.feed(c));
    }
    out.push_str(&r.finish());
    out
}

// ===========================================================================
// GROUP 1 — Behavioural / unit tests
// ===========================================================================

// ---------------------------------------------------------------------------
// 1a. Placeholder fully inside one chunk
// ---------------------------------------------------------------------------

#[test]
fn test_single_chunk_replacement() {
    let mut fake = FakeMap::new();
    fake.insert("EMAIL_1", "alice@example.com");
    assert_eq!(
        run_restorer(&fake, &["Hello EMAIL_1 world"]),
        "Hello alice@example.com world"
    );
}

// ---------------------------------------------------------------------------
// 1b. Split at every byte boundary of placeholder
// ---------------------------------------------------------------------------

#[test]
fn test_every_split_point_of_email_1() {
    let placeholder = "EMAIL_1";
    let original = "alice@example.com";
    let text = format!("prefix {} suffix", placeholder);
    let expected = format!("prefix {} suffix", original);

    for split in 0..=text.len() {
        let c1 = &text[..split];
        let c2 = &text[split..];

        let mut fake = FakeMap::new();
        fake.insert(placeholder, original);
        let out = run_restorer(&fake, &[c1, c2]);
        assert_eq!(out, expected, "split={split}");
    }
}

// ---------------------------------------------------------------------------
// 1c. Split across 3+ chunks
// ---------------------------------------------------------------------------

#[test]
fn test_three_chunk_split() {
    let mut fake = FakeMap::new();
    fake.insert("EMAIL_1", "user@example.com");
    let out = run_restorer(&fake, &["EM", "AIL", "_1!"]);
    assert_eq!(out, "user@example.com!");
}

#[test]
fn test_many_chunk_split() {
    let mut fake = FakeMap::new();
    fake.insert("PERSON_1", "Bob Jones");
    // Split "PERSON_1" one byte at a time
    let text = "PERSON_1 said hello";
    let chunks: Vec<&str> = text.split("").filter(|s| !s.is_empty()).collect();
    let out = run_restorer(&fake, &chunks);
    assert_eq!(out, "Bob Jones said hello");
}

// ---------------------------------------------------------------------------
// 1d. EMAIL_1 / EMAIL_12 prefix overlap — digit continuation
// ---------------------------------------------------------------------------

#[test]
fn test_overlap_longer_wins_in_one_chunk() {
    let mut fake = FakeMap::new();
    fake.insert("EMAIL_1", "alice@example.com");
    fake.insert("EMAIL_12", "bob@example.com");
    let out = run_restorer(&fake, &["start EMAIL_12 end"]);
    assert_eq!(out, "start bob@example.com end");
}

#[test]
fn test_overlap_split_between_email1_and_2() {
    // "EMAIL_1" in chunk1, "2 end" in chunk2 — EMAIL_12 must win.
    let mut fake = FakeMap::new();
    fake.insert("EMAIL_1", "alice@example.com");
    fake.insert("EMAIL_12", "bob@example.com");
    let out = run_restorer(&fake, &["EMAIL_1", "2 end"]);
    assert_eq!(out, "bob@example.com end");
}

#[test]
fn test_overlap_non_digit_releases_shorter() {
    let mut fake = FakeMap::new();
    fake.insert("EMAIL_1", "alice@example.com");
    fake.insert("EMAIL_12", "bob@example.com");
    // 'x' follows EMAIL_1 → EMAIL_1 fires.
    let out = run_restorer(&fake, &["EMAIL_1", "x"]);
    assert_eq!(out, "alice@example.comx");
}

#[test]
fn test_overlap_digit_unknown_passes_through() {
    // Only EMAIL_1 in map; "EMAIL_12" is unknown.
    let mut fake = FakeMap::new();
    fake.insert("EMAIL_1", "alice@example.com");
    let out = run_restorer(&fake, &["EMAIL_12 here"]);
    assert_eq!(out, "EMAIL_12 here");
}

#[test]
fn test_overlap_digit_split_not_in_map() {
    // EMAIL_1 held in chunk1, chunk2 starts with '2' → EMAIL_12 unknown.
    let mut fake = FakeMap::new();
    fake.insert("EMAIL_1", "alice@example.com");
    let out = run_restorer(&fake, &["EMAIL_1", "2x"]);
    assert_eq!(out, "EMAIL_12x");
}

// ---------------------------------------------------------------------------
// 1e. Trailing placeholder resolved by finish()
// ---------------------------------------------------------------------------

#[test]
fn test_trailing_placeholder_finish() {
    let mut fake = FakeMap::new();
    fake.insert("EMAIL_1", "user@example.com");
    let out = run_restorer(&fake, &["see EMAIL_1"]);
    assert_eq!(out, "see user@example.com");
}

#[test]
fn test_trailing_partial_placeholder_finish_emits_verbatim() {
    // Buffer ends with "EMA" — a partial prefix of EMAIL_1; finish() must
    // emit it as-is since it cannot complete into any placeholder.
    let mut fake = FakeMap::new();
    fake.insert("EMAIL_1", "user@example.com");
    let out = run_restorer(&fake, &["test EMA"]);
    assert_eq!(out, "test EMA");
}

// ---------------------------------------------------------------------------
// 1f. Unknown placeholder passthrough
// ---------------------------------------------------------------------------

#[test]
fn test_unknown_placeholder_passthrough() {
    let mut fake = FakeMap::new();
    fake.insert("EMAIL_1", "user@example.com");
    let out = run_restorer(&fake, &["EMAIL_99 stays"]);
    assert_eq!(out, "EMAIL_99 stays");
}

#[test]
fn test_unknown_placeholder_mixed_with_known() {
    let mut fake = FakeMap::new();
    fake.insert("EMAIL_1", "user@example.com");
    let out = run_restorer(&fake, &["EMAIL_99 and EMAIL_1 done"]);
    assert_eq!(out, "EMAIL_99 and user@example.com done");
}

// ---------------------------------------------------------------------------
// 1g. JSON escaping
// ---------------------------------------------------------------------------

#[test]
fn test_original_with_quote() {
    let mut fake = FakeMap::new();
    fake.insert("EMAIL_1", r#"a"b"#);
    let out = run_restorer(&fake, &["EMAIL_1"]);
    assert_eq!(out, r#"a\"b"#);
}

#[test]
fn test_original_with_backslash() {
    let mut fake = FakeMap::new();
    fake.insert("EMAIL_1", "a\\b");
    let out = run_restorer(&fake, &["EMAIL_1"]);
    assert_eq!(out, "a\\\\b");
}

#[test]
fn test_original_with_newline() {
    let mut fake = FakeMap::new();
    fake.insert("EMAIL_1", "x\ny");
    let out = run_restorer(&fake, &["[EMAIL_1]"]);
    assert_eq!(out, "[x\\ny]");
}

#[test]
fn test_original_with_all_escapes() {
    let mut fake = FakeMap::new();
    fake.insert("EMAIL_1", "\"\\\n\r\t\x01");
    let out = run_restorer(&fake, &["EMAIL_1"]);
    assert_eq!(out, "\\\"\\\\\\n\\r\\t\\u0001");
}

// ---------------------------------------------------------------------------
// 1h. Empty chunks
// ---------------------------------------------------------------------------

#[test]
fn test_empty_chunks_ignored() {
    let mut fake = FakeMap::new();
    fake.insert("EMAIL_1", "user@example.com");
    let out = run_restorer(&fake, &["", "EMAIL_1", "", " done", ""]);
    assert_eq!(out, "user@example.com done");
}

// ---------------------------------------------------------------------------
// 1i. Multibyte text around placeholders
// ---------------------------------------------------------------------------

#[test]
fn test_multibyte_context() {
    let mut fake = FakeMap::new();
    fake.insert("EMAIL_1", "ü@example.com");
    let out = run_restorer(&fake, &["Héllo EMAIL_1 Wörld"]);
    assert_eq!(out, "Héllo ü@example.com Wörld");
}

#[test]
fn test_multibyte_split_at_placeholder_boundary() {
    let mut fake = FakeMap::new();
    fake.insert("EMAIL_1", "ü@x.com");
    let text = "日本語 EMAIL_1 テスト";
    for split in 0..=text.len() {
        if !text.is_char_boundary(split) {
            continue;
        }
        let out = run_restorer(&fake, &[&text[..split], &text[split..]]);
        assert_eq!(out, "日本語 ü@x.com テスト", "split={split}");
    }
}

// ---------------------------------------------------------------------------
// 1j. Non-matching text is never withheld longer than max_placeholder_len-1
// ---------------------------------------------------------------------------

#[test]
fn test_holdback_bounded_by_max_ph_len() {
    let mut fake = FakeMap::new();
    fake.insert("EMAIL_1", "user@example.com"); // len=7, max hold=6
    let max_allowed = "EMAIL_1".len() - 1;

    let mut r = restorer_from_fake(&fake);
    for chunk in &["aaaa", "bbbb", "cccccccc", "x x x x x x x x x"] {
        r.feed(chunk);
        let held = r.holdback_len();
        assert!(
            held <= max_allowed,
            "holdback={} > max_allowed={} after chunk {:?}",
            held,
            max_allowed,
            chunk
        );
    }
    r.finish();
}

// ---------------------------------------------------------------------------
// 1k. Multiple placeholders, repeated occurrences
// ---------------------------------------------------------------------------

#[test]
fn test_multiple_and_repeated_placeholders() {
    let mut fake = FakeMap::new();
    fake.insert("EMAIL_1", "alice@example.com");
    fake.insert("EMAIL_2", "bob@example.com");
    let out = run_restorer(&fake, &["From EMAIL_1 to EMAIL_2, CC EMAIL_1 again"]);
    assert_eq!(
        out,
        "From alice@example.com to bob@example.com, CC alice@example.com again"
    );
}

// ===========================================================================
// GROUP 2 — Property tests
// ===========================================================================

use proptest::prelude::*;

/// Strategy: generate a list of (placeholder, original) pairs.
fn placeholder_pairs_strategy() -> impl Strategy<Value = Vec<(String, String)>> {
    // Fixed set of placeholder prefixes and indices.
    let prefixes = vec!["EMAIL", "PERSON", "PHONE", "IBAN", "CARD"];
    let indices: Vec<u32> = vec![1, 2, 3, 12, 99];

    // Generate 1–4 pairs from the fixed set.
    prop::collection::vec(
        (
            prop::sample::select(prefixes),
            prop::sample::select(indices.clone()),
            // Originals: printable ASCII + some unicode, including chars that
            // need JSON-escaping.
            r#"[a-zA-Z0-9@.\-_+ü"\\/ ]{1,20}"#,
        ),
        1..5,
    )
    .prop_map(|v| {
        v.into_iter()
            .map(|(prefix, idx, orig)| (format!("{}_{}", prefix, idx), orig))
            .collect::<Vec<_>>()
    })
}

/// Strategy: chunk a string at random byte positions.
#[allow(dead_code)]
fn chunk_strategy(text: String) -> impl Strategy<Value = Vec<String>> {
    let len = text.len();
    if len == 0 {
        return Just(vec!["".to_owned()]).boxed();
    }
    // Generate 0–5 split points within the string (char boundaries only).
    prop::collection::vec(0usize..len, 0..6)
        .prop_map(move |splits| {
            // Keep only char-boundary positions.
            let mut valid: Vec<usize> = splits
                .into_iter()
                .filter(|&i| text.is_char_boundary(i))
                .collect();
            valid.sort();
            valid.dedup();

            let mut chunks = Vec::new();
            let mut prev = 0usize;
            for &s in &valid {
                if s > prev {
                    chunks.push(text[prev..s].to_owned());
                    prev = s;
                }
            }
            chunks.push(text[prev..].to_owned());
            chunks
        })
        .boxed()
}

/// Build a text that includes placeholders mixed with noise.
fn text_with_placeholders_strategy(pairs: &[(String, String)]) -> impl Strategy<Value = String> {
    let phs: Vec<String> = pairs.iter().map(|(p, _)| p.clone()).collect();
    // Noise fragments including tricky near-miss strings.
    let noise: Vec<&'static str> = vec![
        "hello ",
        " world ",
        "test ",
        ", ",
        " and ",
        "EMAIL_",
        "EMAIL_1x",
        "PERSON_",
        "unrelated text 123 ",
        "!@# ",
        "unicode: ñ ü 日 ",
    ];

    prop::collection::vec(
        prop_oneof![
            prop::sample::select(phs).prop_map(|s| s),
            prop::sample::select(noise).prop_map(|s| s.to_owned()),
        ],
        1..10,
    )
    .prop_map(|parts| parts.concat())
}

proptest! {
    #![proptest_config(proptest::test_runner::Config {
        cases: 256,
        ..Default::default()
    })]

    /// Core property: any chunking of any text containing known placeholders
    /// must produce the same output as the naive full-text reference impl.
    #[test]
    fn prop_any_chunking_equals_naive_restore(
        _pairs in placeholder_pairs_strategy(),
        text in text_with_placeholders_strategy(&[
            ("EMAIL_1".to_owned(), "a".to_owned()),
            ("EMAIL_12".to_owned(), "b".to_owned()),
            ("PERSON_1".to_owned(), "c".to_owned()),
        ]),
        split_positions in prop::collection::vec(0usize..100usize, 0..8),
    ) {
        // Build a fake map with fixed pairs for reproducible expected output.
        let fixed_pairs = vec![
            ("EMAIL_1".to_owned(), "alice@example.com".to_owned()),
            ("EMAIL_12".to_owned(), "bob@example.com".to_owned()),
            ("PERSON_1".to_owned(), "Charlie".to_owned()),
            ("PHONE_1".to_owned(), "+49123456".to_owned()),
        ];

        let expected = naive_restore(&text, &fixed_pairs);

        let mut fake = FakeMap::new();
        for (ph, orig) in &fixed_pairs {
            fake.insert(ph.clone(), orig.clone());
        }

        // Build chunks from split positions (clamped to text.len(), char-boundary-safe).
        let mut valid_splits: Vec<usize> = split_positions
            .into_iter()
            .map(|s| s.min(text.len()))
            .filter(|&s| text.is_char_boundary(s))
            .collect();
        valid_splits.sort();
        valid_splits.dedup();

        let mut chunks: Vec<String> = Vec::new();
        let mut prev = 0usize;
        for s in &valid_splits {
            if *s > prev {
                chunks.push(text[prev..*s].to_owned());
                prev = *s;
            }
        }
        chunks.push(text[prev..].to_owned());

        let chunk_refs: Vec<&str> = chunks.iter().map(String::as_str).collect();
        let actual = run_restorer(&fake, &chunk_refs);

        prop_assert_eq!(actual, expected);
    }

    /// Property: output of streaming restore equals naive restore for random
    /// original values including JSON-special characters.
    #[test]
    fn prop_json_escape_correctness(
        original in r#"[a-zA-Z0-9 \t"\\]{0,30}"#,
        noise_before in "[a-z ]{0,10}",
        noise_after in "[a-z ]{0,10}",
        split in 0usize..50usize,
    ) {
        let ph = "EMAIL_1";
        let text = format!("{}{}{}", noise_before, ph, noise_after);
        let pairs = vec![(ph.to_owned(), original.clone())];
        let expected = naive_restore(&text, &pairs);

        let mut fake = FakeMap::new();
        fake.insert(ph, &original);

        let split = split.min(text.len());
        let split = (0..=split).rev().find(|&s| text.is_char_boundary(s)).unwrap_or(0);
        let out = run_restorer(&fake, &[&text[..split], &text[split..]]);
        prop_assert_eq!(out, expected);
    }

    /// Property: the buffer holdback after any feed() call never exceeds
    /// max_ph_len - 1 bytes of non-matching content.
    #[test]
    fn prop_holdback_bounded(
        chunks in prop::collection::vec("[a-z ]{0,20}", 1..10),
    ) {
        let mut fake = FakeMap::new();
        fake.insert("EMAIL_1", "user@example.com");
        let max_hold = "EMAIL_1".len().saturating_sub(1);

        let mut r = restorer_from_fake(&fake);
        for chunk in &chunks {
            r.feed(chunk.as_str());
            let held = r.holdback_len();
            prop_assert!(
                held <= max_hold,
                "holdback={} > {} after chunk {:?}", held, max_hold, chunk
            );
        }
    }

    /// Property: empty map is a pure passthrough with no buffering.
    #[test]
    fn prop_empty_map_passthrough(
        chunks in prop::collection::vec(".*", 0..10),
    ) {
        let fake = FakeMap::new();
        let text: String = chunks.iter().map(String::as_str).collect();
        let out = run_restorer(&fake, &chunks.iter().map(String::as_str).collect::<Vec<_>>());
        prop_assert_eq!(out, text);
    }
}

// ===========================================================================
// GROUP 3 — SSE realism tests
// ===========================================================================

#[test]
fn sse_person_split_across_chunks() {
    let mut fake = FakeMap::new();
    fake.insert("PERSON_1", "Alice Smith");
    fake.insert("EMAIL_1", "alice@company.com");

    let expected = concat!(
        r#"data: {"choices":[{"delta":{"content":"Dear Alice Smith, please use alice@company.com."}}]}"#,
        "\n\n"
    );

    // Awkward splits: inside "PERSON_" and across ": {\"" boundary.
    let chunks = [
        r#"data: {"choices":[{"delta":{"content":"Dear PER"#,
        r#"SON_1, please use EMAIL"#,
        r#"_1."}}]}"#,
        "\n\n",
    ];

    let mut r = restorer_from_fake(&fake);
    let mut out = String::new();
    for c in &chunks {
        out.push_str(&r.feed(c));
    }
    out.push_str(&r.finish());
    assert_eq!(out, expected);
}

#[test]
fn sse_multiple_events() {
    let mut fake = FakeMap::new();
    fake.insert("EMAIL_1", "user@x.com");

    // Two separate SSE events in one stream.
    let chunks = [
        "data: {\"a\":\"EMAIL_1\"}\n\n",
        "data: {\"b\":\"EMAIL_1\"}\n\n",
    ];

    let out = run_restorer(&fake, &chunks);
    assert_eq!(
        out,
        "data: {\"a\":\"user@x.com\"}\n\ndata: {\"b\":\"user@x.com\"}\n\n"
    );
}

#[test]
fn sse_placeholder_split_at_colon_boundary() {
    // Split right before the digit in "EMAIL_1"
    let mut fake = FakeMap::new();
    fake.insert("EMAIL_1", "restored@example.com");

    let full = "data: {\"content\":\"EMAIL_1\"}\n\n";
    let split = full.find('1').unwrap(); // split at digit
    let c1 = &full[..split];
    let c2 = &full[split..];

    let out = run_restorer(&fake, &[c1, c2]);
    assert_eq!(out, "data: {\"content\":\"restored@example.com\"}\n\n");
}

#[test]
fn sse_done_signal_passes_through() {
    let mut fake = FakeMap::new();
    fake.insert("EMAIL_1", "user@x.com");

    // OpenAI DONE signal must pass through untouched.
    let chunks = ["data: [DONE]\n\n"];
    let out = run_restorer(&fake, &chunks);
    assert_eq!(out, "data: [DONE]\n\n");
}
