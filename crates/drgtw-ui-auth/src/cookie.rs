//! Pure `Set-Cookie` string helpers — no axum, no HTTP types.

/// Build a `Set-Cookie` header value.
///
/// Always sets `HttpOnly; SameSite=Lax; Path=/; Max-Age=<max_age_secs>`.
/// Appends `; Secure` when `secure` is `true`.
pub fn session_cookie(name: &str, value: &str, max_age_secs: u64, secure: bool) -> String {
    let mut s = format!(
        "{}={}; HttpOnly; SameSite=Lax; Path=/; Max-Age={}",
        name, value, max_age_secs
    );
    if secure {
        s.push_str("; Secure");
    }
    s
}

/// Build a `Set-Cookie` header value that instructs the browser to delete the cookie.
pub fn clear_cookie(name: &str) -> String {
    format!("{}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0", name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contains_httponly_and_samesite_lax() {
        let c = session_cookie("sid", "tok", 3600, false);
        assert!(c.contains("HttpOnly"), "missing HttpOnly");
        assert!(c.contains("SameSite=Lax"), "missing SameSite=Lax");
        assert!(c.contains("Path=/"), "missing Path=/");
        assert!(c.contains("Max-Age=3600"), "missing Max-Age");
    }

    #[test]
    fn secure_flag_present_when_requested() {
        let c = session_cookie("sid", "tok", 3600, true);
        assert!(c.contains("; Secure"), "missing Secure");
    }

    #[test]
    fn no_secure_flag_when_false() {
        let c = session_cookie("sid", "tok", 3600, false);
        assert!(!c.contains("Secure"), "unexpected Secure");
    }

    #[test]
    fn clear_cookie_has_max_age_zero() {
        let c = clear_cookie("sid");
        assert!(c.contains("Max-Age=0"), "missing Max-Age=0");
        assert!(c.starts_with("sid="), "missing name");
    }

    #[test]
    fn clear_cookie_has_httponly_and_samesite() {
        let c = clear_cookie("sid");
        assert!(c.contains("HttpOnly"));
        assert!(c.contains("SameSite=Lax"));
    }

    #[test]
    fn name_and_value_are_in_output() {
        let c = session_cookie("my_session", "abc123", 7200, false);
        assert!(c.starts_with("my_session=abc123"));
    }
}
