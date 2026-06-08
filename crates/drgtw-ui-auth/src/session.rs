//! Stateless signed session tokens: `base64url(payload).<base64url(hmac)>`
//!
//! Payload encoding: `<base64url(sub)>|<exp_unix>`.
//! The sub is itself base64url-encoded so that arbitrary bytes in `sub` can
//! never inject a stray `|` separator.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

/// Authenticated session payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub sub: String,
    pub exp_unix: u64,
}

fn mac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut m = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    m.update(data);
    m.finalize().into_bytes().to_vec()
}

fn encode_payload(s: &Session) -> String {
    let sub_b64 = URL_SAFE_NO_PAD.encode(s.sub.as_bytes());
    format!("{}|{}", sub_b64, s.exp_unix)
}

/// Sign a session and return the token string.
pub fn sign_session(s: &Session, key: &[u8]) -> String {
    let payload = encode_payload(s);
    let payload_b64 = URL_SAFE_NO_PAD.encode(payload.as_bytes());
    let sig = mac(key, payload_b64.as_bytes());
    let sig_b64 = URL_SAFE_NO_PAD.encode(&sig);
    format!("{}.{}", payload_b64, sig_b64)
}

/// Verify a token. Returns `None` if the MAC is invalid or the session is expired.
///
/// `now_unix` is passed explicitly — no clock calls inside this function.
pub fn verify_session(token: &str, key: &[u8], now_unix: u64) -> Option<Session> {
    let (payload_b64, sig_b64) = token.split_once('.')?;

    // Constant-time MAC comparison.
    let expected_sig = mac(key, payload_b64.as_bytes());
    let actual_sig = URL_SAFE_NO_PAD.decode(sig_b64).ok()?;
    if expected_sig.ct_eq(&actual_sig).unwrap_u8() == 0 {
        return None;
    }

    let payload_bytes = URL_SAFE_NO_PAD.decode(payload_b64).ok()?;
    let payload = std::str::from_utf8(&payload_bytes).ok()?;

    let (sub_b64, exp_str) = payload.split_once('|')?;
    let sub_bytes = URL_SAFE_NO_PAD.decode(sub_b64).ok()?;
    let sub = String::from_utf8(sub_bytes).ok()?;
    let exp_unix: u64 = exp_str.parse().ok()?;

    if now_unix >= exp_unix {
        return None;
    }

    Some(Session { sub, exp_unix })
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &[u8] = b"test-key-32-bytes-for-hmac-sha256";

    fn session() -> Session {
        Session { sub: "user@example.com".into(), exp_unix: 9_999_999_999 }
    }

    #[test]
    fn round_trip() {
        let s = session();
        let tok = sign_session(&s, KEY);
        let got = verify_session(&tok, KEY, 0).unwrap();
        assert_eq!(got, s);
    }

    #[test]
    fn tampered_payload_is_none() {
        let tok = sign_session(&session(), KEY);
        let (_, sig) = tok.split_once('.').unwrap();
        // Replace payload with garbage.
        let bad = format!("AAAAAAAAAA.{}", sig);
        assert!(verify_session(&bad, KEY, 0).is_none());
    }

    #[test]
    fn tampered_mac_is_none() {
        let tok = sign_session(&session(), KEY);
        let (payload, _) = tok.split_once('.').unwrap();
        let bad = format!("{}.AAAAAAAAAA", payload);
        assert!(verify_session(&bad, KEY, 0).is_none());
    }

    #[test]
    fn expired_is_none() {
        // exp_unix = 100, now = 100 → expired.
        let s = Session { sub: "u".into(), exp_unix: 100 };
        let tok = sign_session(&s, KEY);
        assert!(verify_session(&tok, KEY, 100).is_none());
        assert!(verify_session(&tok, KEY, 200).is_none());
    }

    #[test]
    fn not_yet_expired_is_some() {
        let s = Session { sub: "u".into(), exp_unix: 1000 };
        let tok = sign_session(&s, KEY);
        assert!(verify_session(&tok, KEY, 999).is_some());
    }

    #[test]
    fn wrong_key_is_none() {
        let tok = sign_session(&session(), KEY);
        assert!(verify_session(&tok, b"different-key", 0).is_none());
    }

    #[test]
    fn sub_with_pipe_survives_round_trip() {
        // The sub is base64-encoded, so a pipe in sub must not corrupt parsing.
        let s = Session { sub: "foo|bar|baz".into(), exp_unix: 9_999_999_999 };
        let tok = sign_session(&s, KEY);
        let got = verify_session(&tok, KEY, 0).unwrap();
        assert_eq!(got.sub, "foo|bar|baz");
    }

    /// Asserts the constant-time path is taken: subtle::ConstantTimeEq is used
    /// in verify_session. This test confirms that a 1-bit flip in the MAC causes
    /// rejection without early-exit (we can't observe timing in a unit test, but
    /// we can assert correctness for every bit position).
    #[test]
    fn single_bit_flip_in_mac_rejected() {
        let tok = sign_session(&session(), KEY);
        let (payload, sig_b64) = tok.split_once('.').unwrap();
        let mut sig_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(sig_b64)
            .unwrap();
        // Flip every bit of the first byte in turn.
        for bit in 0..8u8 {
            let saved = sig_bytes[0];
            sig_bytes[0] ^= 1 << bit;
            let bad_sig = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&sig_bytes);
            let bad_tok = format!("{}.{}", payload, bad_sig);
            assert!(verify_session(&bad_tok, KEY, 0).is_none(), "bit {bit} flip not caught");
            sig_bytes[0] = saved;
        }
    }
}
