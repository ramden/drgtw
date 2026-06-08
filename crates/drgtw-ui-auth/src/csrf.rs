//! Stateless CSRF tokens: HMAC-SHA256(key, session_id), base64url-encoded.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

/// Produce a CSRF token bound to `session_id`.
pub fn csrf_token(key: &[u8], session_id: &str) -> String {
    let mut m = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    m.update(session_id.as_bytes());
    let tag = m.finalize().into_bytes();
    URL_SAFE_NO_PAD.encode(tag)
}

/// Verify a CSRF token in constant time.
pub fn verify_csrf(token: &str, key: &[u8], session_id: &str) -> bool {
    let expected = csrf_token(key, session_id);
    // Decode both to bytes before comparing so length differences don't leak.
    let Ok(tok_bytes) = URL_SAFE_NO_PAD.decode(token) else {
        return false;
    };
    let exp_bytes = URL_SAFE_NO_PAD.decode(&expected).expect("our own encoding is valid");
    if tok_bytes.len() != exp_bytes.len() {
        return false;
    }
    tok_bytes.ct_eq(&exp_bytes).unwrap_u8() == 1
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &[u8] = b"csrf-key-for-tests";

    #[test]
    fn matching_token_is_true() {
        let tok = csrf_token(KEY, "session-abc");
        assert!(verify_csrf(&tok, KEY, "session-abc"));
    }

    #[test]
    fn tampered_token_is_false() {
        let tok = csrf_token(KEY, "session-abc");
        // Flip the first character.
        let mut bad: Vec<char> = tok.chars().collect();
        bad[0] = if bad[0] == 'A' { 'B' } else { 'A' };
        let bad: String = bad.into_iter().collect();
        assert!(!verify_csrf(&bad, KEY, "session-abc"));
    }

    #[test]
    fn wrong_session_id_is_false() {
        let tok = csrf_token(KEY, "session-abc");
        assert!(!verify_csrf(&tok, KEY, "session-xyz"));
    }

    #[test]
    fn empty_token_is_false() {
        assert!(!verify_csrf("", KEY, "session-abc"));
    }

    /// Constant-time path: ConstantTimeEq is exercised — verify single-bit flips
    /// in the decoded MAC bytes are all caught.
    #[test]
    fn single_bit_flip_rejected() {
        let tok = csrf_token(KEY, "sid");
        let mut bytes = URL_SAFE_NO_PAD.decode(&tok).unwrap();
        for i in 0..bytes.len() {
            for bit in 0..8u8 {
                let saved = bytes[i];
                bytes[i] ^= 1 << bit;
                let bad = URL_SAFE_NO_PAD.encode(&bytes);
                assert!(!verify_csrf(&bad, KEY, "sid"), "byte {i} bit {bit} not caught");
                bytes[i] = saved;
            }
        }
    }
}
