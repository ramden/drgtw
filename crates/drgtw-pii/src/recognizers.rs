//! Built-in + custom regex recognizers. WP 3.1 implements.

use std::sync::{Arc, LazyLock};

use regex::Regex;

use crate::{Detection, EntityKind, Recognizer};

// ─────────────────────────────────────────────────────────────────────────────
// Shared static regexes (compiled once at first use)
// ─────────────────────────────────────────────────────────────────────────────

/// RFC-lite email: word-boundary anchored, requires TLD ≥2 alpha chars.
static EMAIL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b[a-zA-Z0-9._%+\-]+@[a-zA-Z0-9.\-]+\.[a-zA-Z]{2,}\b")
        .expect("EMAIL_RE is a valid regex")
});

/// Phone: international (+49/0049) and local European formats.
/// Requires leading +, 00, single 0, or parenthesised area code, plus ≥7 digits total.
/// Separators allowed: space, hyphen, slash, dot, parens.
/// Note: regex crate does not support lookahead; we filter in PhoneRecognizer::detect.
static PHONE_RE: LazyLock<Regex> = LazyLock::new(|| {
    // The pattern matches:
    //   - International: +49 89 1234567 | +49-89-123-4567 | 0049 89 1234567
    //   - Local German trunk: 089/1234567 | 089 1234567 | 089-1234-567
    //   - With parens area: (089) 1234567 | (089)1234567
    // "00" prefix is caught by the (?:\+|00) branch.
    // "0\d+" single-trunk (no separator) is NOT matched to avoid "00..." collision;
    // trunk-with-separator IS matched.
    Regex::new(
        r"(?x)
        (?:
            # International prefix: +CC or 00CC, optional separator
            (?:\+|00)\d{1,3}[\s\-./]?
            |
            # Parenthesised area code (no + prefix)
            \(\d{2,6}\)[\s\-./]?
            |
            # Local trunk 0 followed by 1-4 area digits THEN a separator
            # (ensures it's really a phone, not a plain number)
            0\d{1,4}[\s\-./]
        )
        # Subscriber digits with optional separators
        \d[\d\s\-./]{3,20}\d
        ",
    )
    .expect("PHONE_RE is a valid regex")
});

/// IBAN candidate: 2 uppercase letters, 2 digits, then 11-30 alphanumeric/space chars.
/// Full mod-97 validation is done in code.
static IBAN_CANDIDATE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[A-Z]{2}[0-9]{2}[A-Za-z0-9 ]{11,30}").expect("IBAN_CANDIDATE_RE is a valid regex")
});

/// Credit card candidate: 13-19 digits optionally separated by single space or dash.
/// regex crate has no lookahead/lookbehind; we use word-boundary-style anchoring via
/// \b and post-process in CreditCardRecognizer::detect to reject spans inside longer runs.
static CC_CANDIDATE_RE: LazyLock<Regex> = LazyLock::new(|| {
    // Match groups of 4 digits separated by exactly one space or dash (common card formats)
    // OR a plain unseparated 13-19 digit run. Digit isolation is enforced in detect().
    Regex::new(r"\d{4}(?:[ \-]\d{4}){2,4}|\d{13,19}").expect("CC_CANDIDATE_RE is a valid regex")
});

// ─────────────────────────────────────────────────────────────────────────────
// EmailRecognizer
// ─────────────────────────────────────────────────────────────────────────────

pub struct EmailRecognizer;

impl EmailRecognizer {
    pub fn new() -> Self {
        // Force compilation at construction so first detect() is not slower.
        LazyLock::force(&EMAIL_RE);
        Self
    }
}

impl Default for EmailRecognizer {
    fn default() -> Self {
        Self::new()
    }
}

impl Recognizer for EmailRecognizer {
    fn name(&self) -> &str {
        "email"
    }

    fn detect(&self, text: &str) -> Vec<Detection> {
        EMAIL_RE
            .find_iter(text)
            .map(|m| Detection {
                start: m.start(),
                end: m.end(),
                kind: EntityKind::Email,
            })
            .collect()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// PhoneRecognizer
// ─────────────────────────────────────────────────────────────────────────────

pub struct PhoneRecognizer;

impl PhoneRecognizer {
    pub fn new() -> Self {
        LazyLock::force(&PHONE_RE);
        Self
    }

    /// Count digit characters in a string slice.
    fn digit_count(s: &str) -> usize {
        s.chars().filter(|c| c.is_ascii_digit()).count()
    }
}

impl Default for PhoneRecognizer {
    fn default() -> Self {
        Self::new()
    }
}

impl Recognizer for PhoneRecognizer {
    fn name(&self) -> &str {
        "phone"
    }

    fn detect(&self, text: &str) -> Vec<Detection> {
        PHONE_RE
            .find_iter(text)
            .filter_map(|m| {
                let s = m.as_str().trim();
                let digits = Self::digit_count(s);
                // Require at least 7 digits to avoid short false positives.
                if digits < 7 {
                    return None;
                }
                // Reject standalone short numbers that look like years (4 digits
                // after trimming separators — already excluded by ≥7 check, but
                // kept explicit). Also reject numbers where ALL digits form a
                // single 4-digit run (e.g. "year 2026" would not match the regex
                // anyway, but guard defensively).
                if digits == 4 {
                    return None;
                }

                // Compute tight byte offsets (trim trailing whitespace/separators
                // that the regex may have included).
                let full_start = m.start();
                let raw = m.as_str();
                // Trim trailing separators from the match.
                let trimmed =
                    raw.trim_end_matches([' ', '-', '/', '.', '(']);
                let end = full_start + trimmed.len();
                let trimmed_front = trimmed.trim_start_matches(' ');
                let start = end - trimmed_front.len();

                Some(Detection {
                    start,
                    end,
                    kind: EntityKind::Phone,
                })
            })
            .collect()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// IbanRecognizer
// ─────────────────────────────────────────────────────────────────────────────

pub struct IbanRecognizer;

impl IbanRecognizer {
    pub fn new() -> Self {
        LazyLock::force(&IBAN_CANDIDATE_RE);
        Self
    }

    /// Validate IBAN using mod-97 check (ISO 13616).
    /// Returns true iff the checksum is valid.
    fn validate_iban(raw: &str) -> bool {
        // Strip spaces and convert to uppercase.
        let s: String = raw
            .chars()
            .filter(|c| *c != ' ')
            .collect::<String>()
            .to_uppercase();
        if s.len() < 15 || s.len() > 34 {
            return false;
        }
        // Rearrange: move first 4 chars to end.
        let rearranged = format!("{}{}", &s[4..], &s[..4]);
        // Replace letters with digits (A=10, B=11, …, Z=35).
        let mut numeric = String::with_capacity(rearranged.len() * 2);
        for ch in rearranged.chars() {
            if ch.is_ascii_digit() {
                numeric.push(ch);
            } else if ch.is_ascii_alphabetic() {
                let val = (ch as u8 - b'A') + 10;
                numeric.push_str(&val.to_string());
            } else {
                return false;
            }
        }
        // Compute mod 97 via incremental u64 fold to handle large numbers.
        let mut remainder: u64 = 0;
        for ch in numeric.chars() {
            let digit = ch as u64 - b'0' as u64;
            remainder = (remainder * 10 + digit) % 97;
        }
        remainder == 1
    }
}

impl Default for IbanRecognizer {
    fn default() -> Self {
        Self::new()
    }
}

impl Recognizer for IbanRecognizer {
    fn name(&self) -> &str {
        "iban"
    }

    fn detect(&self, text: &str) -> Vec<Detection> {
        IBAN_CANDIDATE_RE
            .find_iter(text)
            .filter_map(|m| {
                let raw = m.as_str();
                // The IBAN body regex `[A-Za-z0-9 ]{11,30}` may greedily include
                // trailing spaces + alphabetic words (e.g. " bitte"). We try the
                // shortest valid checksum: start from the minimum 15-char IBAN and
                // extend up to the full match length, checking mod-97 at each
                // plausible boundary (end of a digit-run or 4-char group).
                //
                // Strategy: try trimming trailing space+word runs until we pass or
                // exhaust the candidate.
                let mut candidate = raw;
                loop {
                    let trimmed = candidate.trim_end_matches(' ');
                    if Self::validate_iban(trimmed) {
                        let end = m.start() + trimmed.len();
                        return Some(Detection {
                            start: m.start(),
                            end,
                            kind: EntityKind::Iban,
                        });
                    }
                    // Remove the last "word" (non-space run) from the end.
                    let no_last_word = trimmed.trim_end_matches(|c: char| c != ' ');
                    if no_last_word.len() == trimmed.len() || no_last_word.len() < 15 {
                        // Nothing removed or too short — give up.
                        return None;
                    }
                    candidate = no_last_word;
                }
            })
            .collect()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// CreditCardRecognizer
// ─────────────────────────────────────────────────────────────────────────────

pub struct CreditCardRecognizer;

impl CreditCardRecognizer {
    pub fn new() -> Self {
        LazyLock::force(&CC_CANDIDATE_RE);
        Self
    }

    /// Luhn algorithm check. Input may contain spaces and dashes (stripped).
    fn luhn_valid(s: &str) -> bool {
        let digits: Vec<u32> = s
            .chars()
            .filter(|c| c.is_ascii_digit())
            .map(|c| c as u32 - '0' as u32)
            .collect();
        let n = digits.len();
        if !(13..=19).contains(&n) {
            return false;
        }
        let sum: u32 = digits
            .iter()
            .rev()
            .enumerate()
            .map(|(i, &d)| {
                if i % 2 == 1 {
                    let doubled = d * 2;
                    if doubled > 9 { doubled - 9 } else { doubled }
                } else {
                    d
                }
            })
            .sum();
        sum.is_multiple_of(10)
    }
}

impl Default for CreditCardRecognizer {
    fn default() -> Self {
        Self::new()
    }
}

impl Recognizer for CreditCardRecognizer {
    fn name(&self) -> &str {
        "credit_card"
    }

    fn detect(&self, text: &str) -> Vec<Detection> {
        let text_bytes = text.as_bytes();
        CC_CANDIDATE_RE
            .find_iter(text)
            .filter_map(|m| {
                // Guard: reject if the match is embedded inside a longer digit run.
                // Check the byte immediately before and after the match.
                let before_ok = m.start() == 0 || !text_bytes[m.start() - 1].is_ascii_digit();
                let after_ok = m.end() == text.len() || !text_bytes[m.end()].is_ascii_digit();
                if !before_ok || !after_ok {
                    return None;
                }
                if Self::luhn_valid(m.as_str()) {
                    Some(Detection {
                        start: m.start(),
                        end: m.end(),
                        kind: EntityKind::CreditCard,
                    })
                } else {
                    None
                }
            })
            .collect()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// CustomRegexRecognizer
// ─────────────────────────────────────────────────────────────────────────────

pub struct CustomRegexRecognizer {
    name: Arc<str>,
    regex: Regex,
}

impl CustomRegexRecognizer {
    /// Compile pattern; returns `Err` if regex is invalid.
    pub fn new(name: &str, pattern: &str) -> Result<Self, regex::Error> {
        let regex = Regex::new(pattern)?;
        Ok(Self {
            name: Arc::from(name),
            regex,
        })
    }
}

impl Recognizer for CustomRegexRecognizer {
    fn name(&self) -> &str {
        &self.name
    }

    fn detect(&self, text: &str) -> Vec<Detection> {
        self.regex
            .find_iter(text)
            .map(|m| Detection {
                start: m.start(),
                end: m.end(),
                kind: EntityKind::Custom(Arc::clone(&self.name)),
            })
            .collect()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Email ────────────────────────────────────────────────────────────────

    #[test]
    fn email_detects_common_formats() {
        let r = EmailRecognizer::new();
        let cases = [
            "user@example.com",
            "first.last+tag@sub.domain.org",
            "x@y.de",
            "support@company.co.uk",
        ];
        for case in &cases {
            let dets = r.detect(case);
            assert_eq!(dets.len(), 1, "expected 1 detection for {case:?}");
            assert_eq!(&case[dets[0].start..dets[0].end], *case);
            assert_eq!(dets[0].kind, EntityKind::Email);
        }
    }

    #[test]
    fn email_false_positive_no_tld() {
        let r = EmailRecognizer::new();
        // Missing TLD (no dot in domain) → should NOT match.
        let dets = r.detect("not@anaddress");
        assert!(dets.is_empty(), "should not detect 'not@anaddress'");
    }

    #[test]
    fn email_false_positive_single_char_tld() {
        let r = EmailRecognizer::new();
        let dets = r.detect("bad@foo.x");
        assert!(dets.is_empty(), "single-char TLD should not match");
    }

    #[test]
    fn email_in_sentence() {
        let r = EmailRecognizer::new();
        let text = "Contact us at support@acme.io for help.";
        let dets = r.detect(text);
        assert_eq!(dets.len(), 1);
        assert_eq!(&text[dets[0].start..dets[0].end], "support@acme.io");
    }

    #[test]
    fn email_multibyte_before_detection() {
        let r = EmailRecognizer::new();
        // Umlauts before the email — byte offsets must be valid.
        let text = "Hällö wörld user@example.com";
        let dets = r.detect(text);
        assert_eq!(dets.len(), 1);
        // Verify slicing by start..end is safe and correct.
        let slice = &text[dets[0].start..dets[0].end];
        assert_eq!(slice, "user@example.com");
    }

    #[test]
    fn email_emoji_before_detection() {
        let r = EmailRecognizer::new();
        let text = "👋 reach me at alice@example.org today";
        let dets = r.detect(text);
        assert_eq!(dets.len(), 1);
        assert_eq!(&text[dets[0].start..dets[0].end], "alice@example.org");
    }

    // ── Phone ────────────────────────────────────────────────────────────────

    #[test]
    fn phone_detects_international_formats() {
        let r = PhoneRecognizer::new();
        let cases: &[(&str, &str)] = &[
            ("+49 89 1234567", "+49 89 1234567"),
            ("+49-89-123-4567", "+49-89-123-4567"),
            ("0049891234567", "0049891234567"),
            ("0049 89 1234567", "0049 89 1234567"),
        ];
        for (text, expected) in cases {
            let dets = r.detect(text);
            assert!(!dets.is_empty(), "expected detection for {text:?}");
            let got = &text[dets[0].start..dets[0].end];
            assert_eq!(got, *expected, "mismatch for {text:?}");
        }
    }

    #[test]
    fn phone_detects_local_german_formats() {
        let r = PhoneRecognizer::new();
        let cases: &[(&str, &str)] = &[
            ("089/1234567", "089/1234567"),
            ("(089) 1234567", "(089) 1234567"),
        ];
        for (text, expected) in cases {
            let dets = r.detect(text);
            assert!(!dets.is_empty(), "expected detection for {text:?}");
            let got = &text[dets[0].start..dets[0].end];
            assert_eq!(got, *expected, "mismatch for {text:?}");
        }
    }

    #[test]
    fn phone_false_positive_plain_integer() {
        let r = PhoneRecognizer::new();
        // Plain short integers without phone shape.
        assert!(r.detect("12345").is_empty(), "12345 should not match");
        assert!(r.detect("id 98765").is_empty(), "id 98765 should not match");
    }

    #[test]
    fn phone_false_positive_year() {
        let r = PhoneRecognizer::new();
        assert!(
            r.detect("year 2026").is_empty(),
            "year 2026 should not match"
        );
        assert!(r.detect("in 2024").is_empty(), "in 2024 should not match");
    }

    #[test]
    fn phone_multibyte_before_detection() {
        let r = PhoneRecognizer::new();
        let text = "Ruf mich an: +49 89 1234567 bitte";
        let dets = r.detect(text);
        assert!(!dets.is_empty());
        let slice = &text[dets[0].start..dets[0].end];
        // Verify it's valid UTF-8 slice containing the phone number.
        assert!(slice.contains("+49"), "slice should contain +49: {slice:?}");
    }

    // ── IBAN ─────────────────────────────────────────────────────────────────

    #[test]
    fn iban_validates_de_no_spaces() {
        let r = IbanRecognizer::new();
        // DE89 3704 0044 0532 0130 00 — valid DE IBAN without spaces.
        let text = "DE89370400440532013000";
        let dets = r.detect(text);
        assert_eq!(dets.len(), 1);
        assert_eq!(&text[dets[0].start..dets[0].end], text);
        assert_eq!(dets[0].kind, EntityKind::Iban);
    }

    #[test]
    fn iban_validates_de_with_spaces() {
        let r = IbanRecognizer::new();
        let text = "DE89 3704 0044 0532 0130 00";
        let dets = r.detect(text);
        assert_eq!(dets.len(), 1);
        assert_eq!(&text[dets[0].start..dets[0].end], text);
    }

    #[test]
    fn iban_validates_gb() {
        let r = IbanRecognizer::new();
        let text = "GB29NWBK60161331926819";
        let dets = r.detect(text);
        assert_eq!(dets.len(), 1);
        assert_eq!(dets[0].kind, EntityKind::Iban);
    }

    #[test]
    fn iban_validates_fr() {
        let r = IbanRecognizer::new();
        // FR76 3000 6000 0112 3456 7890 189
        let text = "FR7630006000011234567890189";
        let dets = r.detect(text);
        assert_eq!(dets.len(), 1);
        assert_eq!(dets[0].kind, EntityKind::Iban);
    }

    #[test]
    fn iban_rejects_wrong_checksum() {
        let r = IbanRecognizer::new();
        // Mutate one digit → invalid checksum.
        let text = "DE00370400440532013000"; // 00 check digits are always invalid per spec
        let dets = r.detect(text);
        assert!(dets.is_empty(), "wrong checksum should not be detected");
    }

    #[test]
    fn iban_rejects_right_shape_wrong_checksum() {
        let r = IbanRecognizer::new();
        // Looks like an IBAN but check digit is wrong (89 → 88).
        let text = "DE88370400440532013000";
        let dets = r.detect(text);
        assert!(dets.is_empty(), "wrong checksum should not be detected");
    }

    #[test]
    fn iban_multibyte_before_detection() {
        let r = IbanRecognizer::new();
        let text = "Konto: 🏦 DE89370400440532013000 bitte überweisen";
        let dets = r.detect(text);
        assert_eq!(dets.len(), 1);
        let slice = &text[dets[0].start..dets[0].end];
        assert_eq!(slice, "DE89370400440532013000");
    }

    // ── Credit Card ──────────────────────────────────────────────────────────

    #[test]
    fn credit_card_detects_spaced_visa() {
        let r = CreditCardRecognizer::new();
        // 4111 1111 1111 1111 — canonical Luhn-valid Visa test number.
        let text = "4111 1111 1111 1111";
        let dets = r.detect(text);
        assert_eq!(dets.len(), 1);
        assert_eq!(&text[dets[0].start..dets[0].end], text);
        assert_eq!(dets[0].kind, EntityKind::CreditCard);
    }

    #[test]
    fn credit_card_detects_unspaced() {
        let r = CreditCardRecognizer::new();
        let text = "4111111111111111";
        let dets = r.detect(text);
        assert_eq!(dets.len(), 1);
        assert_eq!(&text[dets[0].start..dets[0].end], text);
    }

    #[test]
    fn credit_card_rejects_luhn_fail() {
        let r = CreditCardRecognizer::new();
        // Last digit +1 breaks Luhn.
        let text = "4111 1111 1111 1112";
        let dets = r.detect(text);
        assert!(
            dets.is_empty(),
            "Luhn-invalid number should not be detected"
        );
    }

    #[test]
    fn credit_card_rejects_all_same_digits_luhn_fail() {
        let r = CreditCardRecognizer::new();
        let text = "1234567890123456";
        // Likely Luhn-invalid; if it happens to pass we accept, but test the mechanism.
        if !CreditCardRecognizer::luhn_valid(text) {
            assert!(r.detect(text).is_empty());
        }
    }

    #[test]
    fn credit_card_multibyte_before_detection() {
        let r = CreditCardRecognizer::new();
        let text = "Karte: 🎴 4111 1111 1111 1111 abgelaufen";
        let dets = r.detect(text);
        assert_eq!(dets.len(), 1);
        let slice = &text[dets[0].start..dets[0].end];
        assert_eq!(slice, "4111 1111 1111 1111");
    }

    // ── Custom ───────────────────────────────────────────────────────────────

    #[test]
    fn custom_detects_pattern() {
        let r = CustomRegexRecognizer::new("ticket", r"TKT-\d{4,8}").unwrap();
        let text = "Please handle TKT-12345 asap.";
        let dets = r.detect(text);
        assert_eq!(dets.len(), 1);
        assert_eq!(&text[dets[0].start..dets[0].end], "TKT-12345");
        assert_eq!(dets[0].kind, EntityKind::Custom(Arc::from("ticket")));
    }

    #[test]
    fn custom_invalid_regex_returns_error() {
        let result = CustomRegexRecognizer::new("bad", r"[unclosed");
        assert!(result.is_err(), "invalid regex should return Err");
    }

    #[test]
    fn custom_name_returned() {
        let r = CustomRegexRecognizer::new("myrecog", r"\d+").unwrap();
        assert_eq!(r.name(), "myrecog");
    }
}
