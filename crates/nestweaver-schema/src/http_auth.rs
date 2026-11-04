//! Shared parsing of the HTTP `Authorization` header.

/// Extract the token from an `Authorization: Bearer <token>` header value.
///
/// RFC 9110 §11.1 makes the auth-scheme case-insensitive, so `bearer` and
/// `BEARER` are accepted as well as `Bearer` (a strict `strip_prefix("Bearer ")`
/// rejected conforming clients with a 401). One or more spaces may separate the
/// scheme from the token. Returns `None` for any other scheme, a missing
/// separator, or an empty token. The token itself is returned verbatim (no
/// trailing trim) so it still compares byte-for-byte against the configured
/// secret.
pub fn bearer_token(value: &str) -> Option<&str> {
    let (scheme, rest) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = rest.trim_start_matches(' ');
    (!token.is_empty()).then_some(token)
}

#[cfg(test)]
mod tests {
    use super::bearer_token;

    #[test]
    fn scheme_is_case_insensitive() {
        for scheme in ["Bearer", "bearer", "BEARER", "bEaReR"] {
            assert_eq!(bearer_token(&format!("{scheme} tok-123")), Some("tok-123"));
        }
    }

    #[test]
    fn tolerates_extra_spaces_between_scheme_and_token() {
        assert_eq!(bearer_token("Bearer    tok"), Some("tok"));
    }

    #[test]
    fn token_is_returned_verbatim() {
        assert_eq!(bearer_token("Bearer a b"), Some("a b"));
        assert_eq!(bearer_token("Bearer tok "), Some("tok "));
    }

    #[test]
    fn rejects_other_schemes_and_malformed_values() {
        for value in [
            "",
            "Bearer",
            "Bearer ",
            "Bearer   ",
            "Basic dXNlcjpwdw==",
            "Bearertok",
            "Token Bearer x",
            " Bearer tok",
        ] {
            assert_eq!(bearer_token(value), None, "{value:?}");
        }
    }
}
