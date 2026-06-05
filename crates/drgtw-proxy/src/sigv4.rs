//! SigV4 request signing for Bedrock (WP per md/bedrock-converse-design.md).
//!
//! A thin, **synchronous, I/O-free** wrapper over the standalone `aws-sigv4`
//! crate (the same signer the AWS SDK uses). We deliberately do NOT pull in the
//! full SDK / `aws-config` / IMDS: signing here is a pure transform over
//! `(method, url, headers, body, creds, time)` producing the set of header
//! name/value pairs the caller must add to the outgoing request. Keeping the
//! function free of `reqwest` types makes it trivially unit-testable with a
//! known-answer vector.

use std::time::SystemTime;

use aws_credential_types::Credentials;
use aws_sigv4::http_request::{
    SignableBody, SignableRequest, SigningSettings, sign as sigv4_sign,
};
use aws_sigv4::sign::v4;

/// AWS service name signed for. Bedrock Converse and ConverseStream both sign
/// under `"bedrock"`.
const SERVICE_NAME: &str = "bedrock";

/// `provider_name` recorded on the synthetic `Credentials`. Never sent on the
/// wire; purely diagnostic inside `aws-sigv4`.
const PROVIDER_NAME: &str = "drgtw";

/// Resolved per-connection SigV4 material (built once from a `Connection`).
///
/// `Debug` is implemented manually and REDACTS every credential field — this
/// struct must never print secrets into panics, error chains, or logs.
#[derive(Clone)]
pub struct SigV4Creds {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
    pub region: String,
}

impl std::fmt::Debug for SigV4Creds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SigV4Creds")
            .field("access_key_id", &"[redacted]")
            .field("secret_access_key", &"[redacted]")
            .field("session_token", &self.session_token.as_ref().map(|_| "[redacted]"))
            .field("region", &self.region)
            .finish()
    }
}

/// Signing failure. Wraps the underlying `aws-sigv4` error text (which never
/// contains secret material) so the caller can surface a gateway 502-class
/// error without an upstream call.
#[derive(Debug, thiserror::Error)]
pub enum SignError {
    #[error("sigv4 signing failed: {0}")]
    Sign(String),
}

/// Sign an already-built request and return the headers that must be added to
/// it. `body` is the exact bytes that will be sent (post-PII, post-translate);
/// the signature binds the payload hash, so callers MUST sign the final wire
/// body.
///
/// Returned pairs always include `authorization` and `x-amz-date`, and — when a
/// session token is present — `x-amz-security-token`. With the default
/// `SigningSettings` the payload is hashed into the signature but no
/// `x-amz-content-sha256` header is emitted (UNSIGNED-PAYLOAD is not used).
///
/// `now` is injectable so a known-answer test can pin the `x-amz-date`.
pub fn sign_bedrock_request(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    creds: &SigV4Creds,
    now: SystemTime,
) -> Result<Vec<(String, String)>, SignError> {
    let identity = Credentials::new(
        creds.access_key_id.clone(),
        creds.secret_access_key.clone(),
        creds.session_token.clone(),
        None,
        PROVIDER_NAME,
    )
    .into();

    let signing_params = v4::SigningParams::builder()
        .identity(&identity)
        .region(&creds.region)
        .name(SERVICE_NAME)
        .time(now)
        .settings(SigningSettings::default())
        .build()
        .map_err(|e| SignError::Sign(e.to_string()))?
        .into();

    let signable = SignableRequest::new(
        method,
        url,
        headers.iter().map(|(k, v)| (*k, *v)),
        SignableBody::Bytes(body),
    )
    .map_err(|e| SignError::Sign(e.to_string()))?;

    let (instructions, _signature) = sigv4_sign(signable, &signing_params)
        .map_err(|e| SignError::Sign(e.to_string()))?
        .into_parts();

    Ok(instructions
        .headers()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// AWS SigV4 published test-suite credentials.
    const TEST_AKID: &str = "AKIDEXAMPLE";
    const TEST_SECRET: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";

    /// 2015-08-30T12:36:00Z — the AWS sigv4 test-suite timestamp, as a
    /// `SystemTime` (Unix epoch seconds 1440938160).
    fn fixed_time() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_440_938_160)
    }

    fn creds(region: &str, session_token: Option<&str>) -> SigV4Creds {
        SigV4Creds {
            access_key_id: TEST_AKID.to_owned(),
            secret_access_key: TEST_SECRET.to_owned(),
            session_token: session_token.map(str::to_owned),
            region: region.to_owned(),
        }
    }

    fn find<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn authorization_header_shape_is_correct() {
        let headers = sign_bedrock_request(
            "POST",
            "https://bedrock-runtime.eu-central-1.amazonaws.com/model/eu.amazon.nova-pro-v1%3A0/converse",
            &[("content-type", "application/json")],
            br#"{"messages":[]}"#,
            &creds("eu-central-1", None),
            fixed_time(),
        )
        .expect("signing should succeed");

        let auth = find(&headers, "authorization").expect("authorization header present");
        assert!(
            auth.starts_with("AWS4-HMAC-SHA256 "),
            "algorithm prefix: {auth}"
        );
        // Credential scope: <date>/<region>/<service>/aws4_request
        assert!(
            auth.contains("/eu-central-1/bedrock/aws4_request"),
            "credential scope region/service/aws4_request: {auth}"
        );
        assert!(
            auth.contains("Credential=AKIDEXAMPLE/"),
            "credential access key id: {auth}"
        );
        assert!(auth.contains("SignedHeaders="), "signed headers: {auth}");
        assert!(auth.contains("Signature="), "signature: {auth}");
    }

    #[test]
    fn x_amz_date_is_deterministic_for_fixed_time() {
        let headers = sign_bedrock_request(
            "POST",
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/m/converse",
            &[("content-type", "application/json")],
            b"{}",
            &creds("us-east-1", None),
            fixed_time(),
        )
        .expect("signing should succeed");

        // 1440938160 == 2015-08-30T12:36:00Z
        assert_eq!(
            find(&headers, "x-amz-date"),
            Some("20150830T123600Z"),
            "x-amz-date pinned by injected time"
        );
    }

    #[test]
    fn signature_is_deterministic() {
        let sign_once = || {
            sign_bedrock_request(
                "POST",
                "https://bedrock-runtime.us-east-1.amazonaws.com/model/m/converse",
                &[("content-type", "application/json")],
                br#"{"a":1}"#,
                &creds("us-east-1", None),
                fixed_time(),
            )
            .expect("sign")
        };
        let a = sign_once();
        let b = sign_once();
        assert_eq!(
            find(&a, "authorization"),
            find(&b, "authorization"),
            "same inputs + fixed time => identical Authorization"
        );
    }

    #[test]
    fn session_token_header_present_when_set() {
        let headers = sign_bedrock_request(
            "POST",
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/m/converse",
            &[("content-type", "application/json")],
            b"{}",
            &creds("us-east-1", Some("SESSIONTOKENVALUE")),
            fixed_time(),
        )
        .expect("signing should succeed");

        assert_eq!(
            find(&headers, "x-amz-security-token"),
            Some("SESSIONTOKENVALUE"),
            "session token forwarded as x-amz-security-token"
        );
    }

    #[test]
    fn session_token_header_absent_when_unset() {
        let headers = sign_bedrock_request(
            "POST",
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/m/converse",
            &[("content-type", "application/json")],
            b"{}",
            &creds("us-east-1", None),
            fixed_time(),
        )
        .expect("signing should succeed");

        assert!(
            find(&headers, "x-amz-security-token").is_none(),
            "no session token => no x-amz-security-token header"
        );
    }

    #[test]
    fn distinct_bodies_produce_distinct_signatures() {
        let sign_body = |body: &[u8]| {
            let h = sign_bedrock_request(
                "POST",
                "https://bedrock-runtime.us-east-1.amazonaws.com/model/m/converse",
                &[("content-type", "application/json")],
                body,
                &creds("us-east-1", None),
                fixed_time(),
            )
            .expect("sign");
            find(&h, "authorization").unwrap().to_owned()
        };
        assert_ne!(
            sign_body(b"{}"),
            sign_body(br#"{"x":1}"#),
            "payload is bound into the signature"
        );
    }
}
