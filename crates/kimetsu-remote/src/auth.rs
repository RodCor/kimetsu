//! Bearer-token auth. A global token is valid for every repo; a per-repo token
//! is valid only for its repo. Comparison is constant-time and never logs the
//! token.

use std::collections::HashMap;

use subtle::ConstantTimeEq;

#[derive(Debug, Default, Clone)]
pub struct AuthConfig {
    /// Tokens valid for ALL repos.
    pub global: Vec<String>,
    /// repo-id → tokens valid only for that repo.
    pub per_repo: HashMap<String, Vec<String>>,
}

impl AuthConfig {
    /// True when no token is configured anywhere — the server must refuse to
    /// start in that case rather than run wide open.
    pub fn is_empty(&self) -> bool {
        self.global.is_empty() && self.per_repo.values().all(|v| v.is_empty())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum AuthOutcome {
    Ok,
    /// No/blank/unknown token → 401.
    Unauthorized,
    /// Token is known but not granted for this repo → 403.
    Forbidden,
}

/// Constant-time string compare. Unequal lengths are not equal; the byte
/// compare itself does not short-circuit.
fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.ct_eq(b).into()
}

/// True if `tok` matches any candidate, without an early return (so the number
/// of comparisons doesn't depend on which candidate matched).
fn any_match(candidates: &[String], tok: &str) -> bool {
    let mut hit = false;
    for c in candidates {
        hit |= ct_eq(c, tok);
    }
    hit
}

fn known_anywhere(auth: &AuthConfig, tok: &str) -> bool {
    let mut hit = any_match(&auth.global, tok);
    for toks in auth.per_repo.values() {
        hit |= any_match(toks, tok);
    }
    hit
}

/// Decide access for `repo` given the presented bearer token (already stripped
/// of the `Bearer ` prefix).
pub fn check(auth: &AuthConfig, repo: &str, bearer: Option<&str>) -> AuthOutcome {
    let Some(tok) = bearer.map(str::trim).filter(|t| !t.is_empty()) else {
        return AuthOutcome::Unauthorized;
    };
    if any_match(&auth.global, tok) {
        return AuthOutcome::Ok;
    }
    if let Some(toks) = auth.per_repo.get(repo)
        && any_match(toks, tok)
    {
        return AuthOutcome::Ok;
    }
    // A token valid for some OTHER repo is "known but not for this one" (403);
    // a token unknown everywhere is unauthorized (401).
    if known_anywhere(auth, tok) {
        AuthOutcome::Forbidden
    } else {
        AuthOutcome::Unauthorized
    }
}

/// True when the presented bearer token is one of the global/server-admin
/// tokens. Use this for operations that affect shared org/user memory rather
/// than a single repo brain.
pub fn is_global_token(auth: &AuthConfig, bearer: Option<&str>) -> bool {
    let Some(tok) = bearer.map(str::trim).filter(|t| !t.is_empty()) else {
        return false;
    };
    any_match(&auth.global, tok)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> AuthConfig {
        let mut per_repo = HashMap::new();
        per_repo.insert("acme-api".to_string(), vec!["tok_api".to_string()]);
        per_repo.insert("acme-web".to_string(), vec!["tok_web".to_string()]);
        AuthConfig {
            global: vec!["tok_admin".to_string()],
            per_repo,
        }
    }

    #[test]
    fn global_token_works_for_any_repo() {
        assert_eq!(
            check(&cfg(), "acme-api", Some("tok_admin")),
            AuthOutcome::Ok
        );
        assert_eq!(
            check(&cfg(), "whatever", Some("tok_admin")),
            AuthOutcome::Ok
        );
    }

    #[test]
    fn per_repo_token_scoped() {
        assert_eq!(check(&cfg(), "acme-api", Some("tok_api")), AuthOutcome::Ok);
        // valid token, wrong repo → forbidden
        assert_eq!(
            check(&cfg(), "acme-web", Some("tok_api")),
            AuthOutcome::Forbidden
        );
    }

    #[test]
    fn missing_or_unknown_is_unauthorized() {
        assert_eq!(check(&cfg(), "acme-api", None), AuthOutcome::Unauthorized);
        assert_eq!(
            check(&cfg(), "acme-api", Some("")),
            AuthOutcome::Unauthorized
        );
        assert_eq!(
            check(&cfg(), "acme-api", Some("nope")),
            AuthOutcome::Unauthorized
        );
    }

    #[test]
    fn global_token_detection_distinguishes_per_repo_tokens() {
        assert!(is_global_token(&cfg(), Some("tok_admin")));
        assert!(!is_global_token(&cfg(), Some("tok_api")));
        assert!(!is_global_token(&cfg(), Some("nope")));
    }
}
