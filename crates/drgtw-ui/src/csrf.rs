//! Shared CSRF helper for the admin UI mutation forms.
//!
//! Wraps `drgtw_ui_auth::csrf::{csrf_token, verify_csrf}` keyed by
//! `ui.auth.session_key` + the logged-in username.  When `[ui.auth]` is
//! absent (open mode), CSRF is a no-op pass — same posture as the auth
//! middleware.
//!
//! Every POST form embeds `csrf_field(..)` and its handler calls
//! `csrf_ok(..)` before mutating.  The other UI builder reuses these two
//! functions — keep their signatures stable.

use drgtw_ui_auth::csrf::{csrf_token, verify_csrf};
use maud::{Markup, html};

use crate::UiState;

/// Hidden `<input name="csrf_token">` for a mutation form.
///
/// Open mode (no `[ui.auth]`) returns an empty fragment — no token needed.
#[must_use]
pub fn csrf_field(state: &UiState, user: &str) -> Markup {
    let Some(auth_cfg) = &state.config.ui.auth else {
        return html! {};
    };
    let token = csrf_token(auth_cfg.session_key.as_bytes(), user);
    html! { input type="hidden" name="csrf_token" value=(token); }
}

/// Verify a submitted CSRF token.
///
/// Open mode (no `[ui.auth]`) always returns `true`.
#[must_use]
pub fn csrf_ok(state: &UiState, user: &str, token: &str) -> bool {
    let Some(auth_cfg) = &state.config.ui.auth else {
        return true;
    };
    verify_csrf(token, auth_cfg.session_key.as_bytes(), user)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Instant;

    use drgtw_config::{Config, UiAuthConfig, UiConfig};

    use crate::{PgGate, UiState};

    fn state_open() -> UiState {
        let config = Arc::new(Config::default());
        UiState::new(Instant::now(), config, PathBuf::new(), PgGate::NotConfigured)
    }

    fn state_with_auth() -> UiState {
        let auth = UiAuthConfig {
            username: "admin".into(),
            password_hash: "$argon2id$v=19$m=65536,t=3,p=4$abc$xyz".into(),
            session_key: "test-session-key-32-bytes-padding!".into(),
            session_ttl_hours: 8,
        };
        let mut config = Config::default();
        config.ui = UiConfig { auth: Some(auth), ..UiConfig::default() };
        UiState::new(Instant::now(), Arc::new(config), PathBuf::new(), PgGate::NotConfigured)
    }

    #[test]
    fn open_mode_always_passes() {
        let state = state_open();
        assert!(super::csrf_ok(&state, "anyone", "bad-token"));
        assert!(super::csrf_ok(&state, "anyone", ""));
    }

    #[test]
    fn open_mode_field_is_empty() {
        let state = state_open();
        let markup = super::csrf_field(&state, "anyone").into_string();
        assert!(markup.is_empty(), "open mode must emit nothing, got: {markup:?}");
    }

    #[test]
    fn auth_mode_round_trip() {
        let state = state_with_auth();
        let markup = super::csrf_field(&state, "admin").into_string();
        // Extract the token value from the hidden input.
        let start = markup.find("value=\"").expect("has value attr") + 7;
        let end = markup[start..].find('"').expect("closing quote") + start;
        let token = &markup[start..end];
        assert!(super::csrf_ok(&state, "admin", token), "round-trip must verify");
    }

    #[test]
    fn auth_mode_bad_token_rejected() {
        let state = state_with_auth();
        assert!(!super::csrf_ok(&state, "admin", "not-a-valid-token"));
    }

    #[test]
    fn auth_mode_wrong_user_rejected() {
        let state = state_with_auth();
        // Mint a token for "admin", then verify against "other" — must fail.
        let markup = super::csrf_field(&state, "admin").into_string();
        let start = markup.find("value=\"").expect("has value attr") + 7;
        let end = markup[start..].find('"').expect("closing quote") + start;
        let token = &markup[start..end];
        assert!(!super::csrf_ok(&state, "other", token));
    }
}
