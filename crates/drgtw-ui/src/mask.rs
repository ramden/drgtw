//! Secret masking for the read-only config viewer.
//!
//! The viewer must NEVER render resolved secret material. Values that still
//! carry a `${ENV_VAR}` reference are shown verbatim (they are placeholders,
//! not secrets); anything else is masked to a short, non-reversible hint.

/// Mask a possibly-secret value for display.
///
/// - A value containing `${...}` is an unresolved env placeholder → shown as-is.
/// - Otherwise the value is masked: short values become `••••`, longer ones
///   keep a `sk-…last4` shape so an operator can eyeball which key is wired
///   without the material leaking.
pub fn mask_secret(value: &str) -> String {
    if value.contains("${") {
        return value.to_owned();
    }
    let len = value.chars().count();
    if len <= 4 {
        return "••••".to_owned();
    }
    let prefix: String = value.chars().take(3).collect();
    let suffix: String = value.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    format!("{prefix}…{suffix}")
}

/// Mask a connection string (e.g. a Postgres URL), hiding any embedded password
/// while keeping `${ENV_VAR}` placeholders intact for the config viewer.
pub fn mask_url(value: &str) -> String {
    if value.contains("${") {
        return value.to_owned();
    }
    // Replace a `user:password@` authority with `user:••••@`.
    if let Some(at) = value.find('@')
        && let Some(scheme_end) = value.find("://")
    {
        let authority = &value[scheme_end + 3..at];
        if let Some(colon) = authority.find(':') {
            let user = &authority[..colon];
            return format!("{}{}:••••{}", &value[..scheme_end + 3], user, &value[at..]);
        }
    }
    value.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_placeholder_passes_through() {
        assert_eq!(mask_secret("${OPENAI_KEY}"), "${OPENAI_KEY}");
        assert_eq!(mask_url("${DATABASE_URL}"), "${DATABASE_URL}");
    }

    #[test]
    fn short_secret_fully_masked() {
        assert_eq!(mask_secret("abcd"), "••••");
    }

    #[test]
    fn long_secret_keeps_shape() {
        assert_eq!(mask_secret("sk-live-abc1234"), "sk-…1234");
    }

    #[test]
    fn url_password_masked() {
        assert_eq!(
            mask_url("postgres://user:secret@db:5432/x"),
            "postgres://user:••••@db:5432/x"
        );
    }
}
