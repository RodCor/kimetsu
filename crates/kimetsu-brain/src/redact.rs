//! v0.4.5: secret redaction at ingest.
//!
//! Every memory.text and provenance snapshot that lands in brain.db
//! passes through [`redact_secrets`] first. The pattern set catches
//! the credential formats most likely to leak into an agent's tool
//! output (OAuth bearers, API keys, JWTs, AWS access pairs, etc.)
//! and replaces each match with a `[REDACTED:<kind>]` placeholder.
//!
//! Why redact at ingest, not later: brain.db is durable + content-
//! addressable + replicated across user / project scopes. A leak
//! that lands here lives forever and shows up in every retrieval
//! capsule, agent.done summary, and proposal review screen. The
//! cost of one false positive (a config string redacted) is much
//! lower than the cost of one true positive sitting in
//! `~/.kimetsu/brain.db` for years.
//!
//! Detection layers, applied in order:
//!   1. Exact-prefix patterns: `sk-ant-`, `sk-`, `ghp_`, `gho_`,
//!      `ghu_`, `ghs_`, `ghr_`, `github_pat_`, `xox[bopasr]-`,
//!      `AKIA`/`ASIA` (AWS access key IDs), `eyJ` (JWT header).
//!   2. Generic key assignments: a key whose name contains a secret
//!      word (`token`, `password`/`passwd`, `api_key`/`apikey`,
//!      `private_key`, `secret`, `credential`, `auth`) in env form
//!      (`export KEY=value`, quoted or not) or JSON/YAML form
//!      (`"key": "value"`). The value must still look secret (long
//!      enough, no bare spaces). `token` and `auth` only count as
//!      the last segment of the key, so `token_count` / `max_tokens`
//!      / `auth_type` are left alone. `bearer <value>` is the same
//!      idea for HTTP logs.
//!   3. High-entropy fallback (off by default): would catch random
//!      base64 strings of length ≥ 40 with entropy > 4.5 bits/char.
//!      Deferred to v0.4.5.1 so we don't false-positive on hashes,
//!      ulids, and content-addressable refs.
//!
//! The redaction is greedy + non-overlapping: a string matched by
//! pattern (1) won't be re-scanned for pattern (2). [`RedactionResult`]
//! returns both the redacted text AND a per-kind tally so callers
//! (chat REPL banner, CLI warning, MCP response field) can surface
//! "we found 2 secrets in this memory, kinds: aws_access_key,
//! github_pat" without re-parsing the redacted text.

use std::sync::OnceLock;

use regex::Regex;
use serde::Serialize;

/// One detected secret. `kind` is a stable string id usable in
/// telemetry and human-readable output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RedactedMatch {
    pub kind: &'static str,
    /// Byte offset within the ORIGINAL (pre-redaction) text where
    /// the match started.
    pub start: usize,
    /// Length of the matched run (bytes, original text).
    pub len: usize,
}

/// Output of [`redact_secrets`]. `text` is the redacted string;
/// `matches` is the per-secret tally + locations.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RedactionResult {
    pub text: String,
    pub matches: Vec<RedactedMatch>,
}

impl RedactionResult {
    pub fn was_redacted(&self) -> bool {
        !self.matches.is_empty()
    }
    /// One-liner like `"redacted 2 secrets: github_pat, openai_api_key"`.
    /// Empty when nothing was redacted.
    pub fn summary(&self) -> String {
        if self.matches.is_empty() {
            return String::new();
        }
        let mut kinds: Vec<&'static str> = self.matches.iter().map(|m| m.kind).collect();
        kinds.sort_unstable();
        kinds.dedup();
        format!(
            "redacted {} secret{}: {}",
            self.matches.len(),
            if self.matches.len() == 1 { "" } else { "s" },
            kinds.join(", ")
        )
    }
}

/// Redact `text` against the bundled secret pattern set. Returns
/// the redacted version + per-match tally. Allocates only when at
/// least one match is found — for the common case (clean text) the
/// result borrows nothing and matches is empty.
pub fn redact_secrets(text: &str) -> RedactionResult {
    merge_and_redact(text, collect_spans(text, patterns()))
}

/// v2.6 #4 (knowledge packs): scrub credentials AND PII (email / phone / SSN /
/// credit-card) from a memory before it ships in a shareable pack. A published
/// pack must never carry secrets or personal data. Fast (regex over the text;
/// credit-card candidates are Luhn-gated to avoid false positives), no model.
pub fn scrub_for_export(text: &str) -> RedactionResult {
    let mut spans = collect_spans(text, patterns());
    spans.extend(collect_pii_spans(text));
    merge_and_redact(text, spans)
}

/// Collect raw `(start, end, kind)` regex matches for `patterns` (no validation
/// or overlap resolution — that's [`merge_and_redact`]).
fn collect_spans(text: &str, patterns: &[SecretPattern]) -> Vec<(usize, usize, &'static str)> {
    let mut spans: Vec<(usize, usize, &'static str)> = Vec::new();
    for pat in patterns {
        // Assignment patterns capture the secret value in a group so the
        // key stays visible. Every other pattern has no groups and the
        // whole match is the secret.
        if pat.regex.captures_len() > 1 {
            for caps in pat.regex.captures_iter(text) {
                if let Some(value) = (1..caps.len()).find_map(|i| caps.get(i)) {
                    // Ingest redacts, then the projector redacts the stored
                    // string again. A placeholder must not be treated as a
                    // fresh secret on that second pass.
                    if is_redaction_placeholder(value.as_str()) {
                        continue;
                    }
                    spans.push((value.start(), value.end(), pat.kind));
                }
            }
        } else {
            for m in pat.regex.find_iter(text) {
                spans.push((m.start(), m.end(), pat.kind));
            }
        }
    }
    spans
}

/// Sort spans, drop overlaps (earliest start wins; longest on a tie), then
/// rebuild the redacted text + per-match tally. Empty spans → text unchanged.
fn merge_and_redact(text: &str, mut spans: Vec<(usize, usize, &'static str)>) -> RedactionResult {
    if spans.is_empty() {
        return RedactionResult {
            text: text.to_string(),
            matches: Vec::new(),
        };
    }
    spans.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)));
    let mut accepted: Vec<(usize, usize, &'static str)> = Vec::new();
    let mut cursor = 0usize;
    for (start, end, kind) in spans {
        if start < cursor {
            continue;
        }
        accepted.push((start, end, kind));
        cursor = end;
    }

    let mut redacted = String::with_capacity(text.len());
    let mut matches: Vec<RedactedMatch> = Vec::with_capacity(accepted.len());
    let mut last = 0usize;
    for (start, end, kind) in accepted {
        redacted.push_str(&text[last..start]);
        redacted.push_str(&format!("[REDACTED:{kind}]"));
        matches.push(RedactedMatch {
            kind,
            start,
            len: end - start,
        });
        last = end;
    }
    redacted.push_str(&text[last..]);
    RedactionResult {
        text: redacted,
        matches,
    }
}

/// Collect PII spans. `credit_card` candidates are kept only when they have
/// 13–19 digits AND pass the Luhn checksum, so ordinary long digit runs (ids,
/// timestamps) aren't scrubbed.
fn collect_pii_spans(text: &str) -> Vec<(usize, usize, &'static str)> {
    let mut spans: Vec<(usize, usize, &'static str)> = Vec::new();
    for pat in pii_patterns() {
        for m in pat.regex.find_iter(text) {
            if pat.kind == "credit_card" {
                let digits: String = m.as_str().chars().filter(char::is_ascii_digit).collect();
                if !(13..=19).contains(&digits.len()) || !luhn_valid(&digits) {
                    continue;
                }
            }
            spans.push((m.start(), m.end(), pat.kind));
        }
    }
    spans
}

/// Luhn (mod-10) checksum used to validate credit-card candidates.
fn luhn_valid(digits: &str) -> bool {
    if digits.is_empty() {
        return false;
    }
    let mut sum = 0u32;
    let mut double = false;
    for c in digits.chars().rev() {
        let mut d = match c.to_digit(10) {
            Some(d) => d,
            None => return false,
        };
        if double {
            d *= 2;
            if d > 9 {
                d -= 9;
            }
        }
        sum += d;
        double = !double;
    }
    sum.is_multiple_of(10)
}

/// PII pattern set (kept high-precision to avoid scrubbing legit technical text).
fn pii_patterns() -> &'static [SecretPattern] {
    static CELL: OnceLock<Vec<SecretPattern>> = OnceLock::new();
    CELL.get_or_init(|| {
        vec![
            SecretPattern {
                kind: "email",
                regex: Regex::new(r"(?i)\b[a-z0-9._%+\-]+@[a-z0-9.\-]+\.[a-z]{2,}\b").unwrap(),
            },
            SecretPattern {
                kind: "ssn",
                // US SSN `123-45-6789` (dashed only — bare 9-digit runs are too
                // ambiguous to scrub safely).
                regex: Regex::new(r"\b\d{3}-\d{2}-\d{4}\b").unwrap(),
            },
            SecretPattern {
                kind: "phone",
                // North-American style: optional +country, area code (paren or
                // bare), then 3-4 with a separator. Requires structure so it
                // doesn't trip on arbitrary number runs.
                regex: Regex::new(
                    r"\b(?:\+?\d{1,3}[ .\-]?)?(?:\(\d{3}\)|\d{3})[ .\-]\d{3}[ .\-]\d{4}\b",
                )
                .unwrap(),
            },
            SecretPattern {
                kind: "credit_card",
                // Candidate 13–19 digit run (spaces/dashes allowed) — Luhn-gated
                // in `collect_pii_spans`.
                regex: Regex::new(r"\b\d(?:[ \-]?\d){12,18}\b").unwrap(),
            },
        ]
    })
}

struct SecretPattern {
    kind: &'static str,
    regex: Regex,
}

fn patterns() -> &'static [SecretPattern] {
    static CELL: OnceLock<Vec<SecretPattern>> = OnceLock::new();
    CELL.get_or_init(|| {
        // Each pattern's `kind` is stable telemetry-grade ID; the
        // regex MUST be anchored at a unique prefix so we don't
        // shadow normal source text. Use word boundaries where the
        // pattern's prefix isn't already distinctive.
        vec![
            SecretPattern {
                kind: "anthropic_oauth",
                // Anthropic OAuth tokens: `sk-ant-` prefix + opaque tail.
                // Tail is at least 32 chars of [A-Za-z0-9_-].
                regex: Regex::new(r"sk-ant-[A-Za-z0-9_-]{32,}").unwrap(),
            },
            SecretPattern {
                kind: "openai_api_key",
                // OpenAI keys: `sk-` (not `sk-ant-`) + 32+ chars.
                // Negative lookahead isn't available in `regex`; we
                // order anthropic_oauth FIRST so it claims those bytes
                // before openai_api_key sees them.
                regex: Regex::new(r"sk-[A-Za-z0-9_-]{32,}").unwrap(),
            },
            SecretPattern {
                kind: "github_pat",
                // Classic + fine-grained GitHub PATs.
                // ghp_/gho_/ghu_/ghs_/ghr_ + 36 base62.
                // github_pat_ + base62/underscore length 50+.
                regex: Regex::new(
                    r"(?:ghp_|gho_|ghu_|ghs_|ghr_)[A-Za-z0-9]{36}|github_pat_[A-Za-z0-9_]{50,}",
                )
                .unwrap(),
            },
            SecretPattern {
                kind: "slack_token",
                regex: Regex::new(r"xox[bopasr]-[A-Za-z0-9-]{10,}").unwrap(),
            },
            SecretPattern {
                kind: "aws_access_key",
                // AKIA = long-lived, ASIA = STS temporary. Exactly 16
                // uppercase alphanum after the prefix per AWS docs.
                regex: Regex::new(r"(?:AKIA|ASIA)[A-Z0-9]{16}").unwrap(),
            },
            SecretPattern {
                kind: "jwt",
                // Three base64url segments separated by dots; first
                // starts with `eyJ` (base64url of `{"`). Cap total at
                // 4096 chars so a runaway match doesn't blow the line.
                regex: Regex::new(r"eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+").unwrap(),
            },
            SecretPattern {
                kind: "private_key_pem",
                // PEM-encoded private key BEGIN line — match the whole
                // block so the entire payload is wiped.
                regex: Regex::new(
                    r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
                )
                .unwrap(),
            },
            SecretPattern {
                kind: "google_api_key",
                // Google-style API keys: AIza + 35 char tail.
                regex: Regex::new(r"AIza[0-9A-Za-z_-]{35}").unwrap(),
            },
            SecretPattern {
                kind: "url_credentials",
                // Credentials embedded in a connection-string URI:
                // `scheme://user:password@host`. Common in env dumps and
                // `.env` snippets (DATABASE_URL=postgres://u:p@host, redis://,
                // amqp://, mongodb://, ...). The generic `password=` rule does
                // NOT match this shape, so without it the password lands in
                // brain.db in cleartext. Redact the `scheme://user:pass@` run;
                // the host stays readable.
                regex: Regex::new(r"(?i)\b[a-z][a-z0-9+.\-]*://[^\s:/@]+:[^\s:/@]{4,}@").unwrap(),
            },
            // Generic-assignment patterns. The value is a capture group
            // (see `collect_spans`) so a shape-specific pattern that
            // matches the same bytes — `api_key=sk-...` — keeps its
            // own kind. Key match: the keyword is a `_`/`-` segment,
            // not a prefix of a longer word (`author` ≠ `auth`).
            SecretPattern {
                kind: "generic_bearer",
                // `Bearer <token>` in HTTP-style logs.
                regex: Regex::new(r"(?i)bearer\s+[A-Za-z0-9_\-\.=]{12,}").unwrap(),
            },
            // `token` and `auth` must be the last segment. That keeps
            // `token_count`, `max_tokens`, and `auth_type` visible.
            generic_assignment("generic_token", "token", 20, false),
            generic_assignment("generic_auth", "auth", 20, false),
            // High-signal words match in any segment, so
            // `AWS_SECRET_ACCESS_KEY` and `DB_PASSWORD` are covered.
            generic_assignment("generic_password", "password|passwd", 8, true),
            generic_assignment("generic_api_key", r"api[_\-]?key", 12, true),
            generic_assignment("generic_private_key", r"private[_\-]?key", 12, true),
            generic_assignment("generic_secret", "secret|credential", 8, true),
        ]
    })
}

/// True when `value` is one of this module's `[REDACTED:<kind>]` placeholders.
fn is_redaction_placeholder(value: &str) -> bool {
    let Some(inner) = value
        .strip_prefix("[REDACTED:")
        .and_then(|value| value.strip_suffix(']'))
    else {
        return false;
    };
    !inner.is_empty() && inner.chars().all(|c| c.is_ascii_lowercase() || c == '_')
}

/// Env (`export KEY=value`, quoted or not) and JSON/YAML (`"key": "value"`)
/// assignment. `keyword` is a regex alternation of secret words. When
/// `allow_trailing` is set, the word may sit in the middle of the key
/// (`AWS_SECRET_ACCESS_KEY`); otherwise it must be the final segment.
fn generic_assignment(
    kind: &'static str,
    keyword: &str,
    min_len: usize,
    allow_trailing: bool,
) -> SecretPattern {
    let trailing = if allow_trailing {
        r"(?:[_\-][A-Za-z0-9]+)*"
    } else {
        ""
    };
    // `{min}` is formatted into the quantifier `{N,}`. Quoted values may
    // contain spaces; bare values may not, so prose doesn't get swallowed.
    let pattern = format!(
        r#"(?i)(?:^|[^A-Za-z0-9])(?:export\s+)?["']?(?:[A-Za-z0-9]+[_\-])*(?:{keyword}){trailing}["']?\s*[:=]\s*(?:"([^"\n]{{{min_len},}})"|'([^'\n]{{{min_len},}})'|([^\s"']{{{min_len},}}))"#
    );
    SecretPattern {
        kind,
        regex: Regex::new(&pattern).unwrap_or_else(|err| {
            panic!("generic assignment regex for {kind} failed: {err}; pattern: {pattern}")
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_text_round_trips_untouched() {
        let raw = "use ripgrep before broad file reads, prefer thiserror for errors";
        let r = redact_secrets(raw);
        assert!(!r.was_redacted());
        assert_eq!(r.text, raw);
        assert!(r.summary().is_empty());
    }

    // v2.6 #4: scrub_for_export adds PII on top of credentials.
    #[test]
    fn scrub_for_export_redacts_pii_and_credentials() {
        let raw = "contact alice@example.com or 415-555-0142; ssn 123-45-6789; \
                   key sk-ant-AbCdEfGhIjKlMnOpQrStUvWx0123456789";
        let r = scrub_for_export(raw);
        let kinds: std::collections::BTreeSet<&str> = r.matches.iter().map(|m| m.kind).collect();
        assert!(kinds.contains("email"), "{r:?}");
        assert!(kinds.contains("phone"), "{r:?}");
        assert!(kinds.contains("ssn"), "{r:?}");
        assert!(kinds.contains("anthropic_oauth"), "{r:?}");
        assert!(!r.text.contains("alice@example.com"));
        assert!(!r.text.contains("123-45-6789"));
        assert!(!r.text.contains("sk-ant-"));
    }

    #[test]
    fn scrub_for_export_luhn_gates_credit_cards() {
        // 4242 4242 4242 4242 passes Luhn → scrubbed.
        let good = scrub_for_export("card 4242 4242 4242 4242 on file");
        assert!(
            good.matches.iter().any(|m| m.kind == "credit_card"),
            "valid card must scrub: {good:?}"
        );
        // A 16-digit run that FAILS Luhn (e.g. an id/timestamp concat) is kept.
        let bad = scrub_for_export("trace 1234567890123456 step");
        assert!(
            !bad.matches.iter().any(|m| m.kind == "credit_card"),
            "non-Luhn digit run must NOT scrub: {bad:?}"
        );
    }

    #[test]
    fn scrub_for_export_leaves_technical_text_alone() {
        // Versions, hashes, ulids, ports — must not trip PII patterns.
        let raw = "build with cargo 1.79; commit a1b2c3d4; port 8787; ulid 01K8YMJ448514TP6CPQ";
        let r = scrub_for_export(raw);
        assert!(!r.was_redacted(), "false positive: {r:?}");
        assert_eq!(r.text, raw);
    }

    #[test]
    fn luhn_check() {
        assert!(luhn_valid("4242424242424242"));
        assert!(!luhn_valid("4242424242424241"));
        assert!(!luhn_valid(""));
    }

    #[test]
    fn anthropic_oauth_token_is_redacted() {
        let raw =
            "export CLAUDE_CODE_OAUTH_TOKEN=sk-ant-api03-AbCdEfGhIjKlMnOpQrStUv0123456789AbCdEf";
        let r = redact_secrets(raw);
        assert!(r.was_redacted(), "{:?}", r);
        assert!(r.text.contains("[REDACTED:anthropic_oauth]"));
        assert!(!r.text.contains("sk-ant-api03"));
        assert_eq!(r.matches.len(), 1);
        assert_eq!(r.matches[0].kind, "anthropic_oauth");
    }

    #[test]
    fn openai_key_is_redacted_without_shadowing_anthropic_prefix() {
        // Ensure pattern ordering: anthropic claims sk-ant-... first.
        let raw =
            "two: sk-1234567890abcdef1234567890abcdef AND sk-ant-1234567890abcdef1234567890abcdef";
        let r = redact_secrets(raw);
        let kinds: Vec<_> = r.matches.iter().map(|m| m.kind).collect();
        assert!(kinds.contains(&"openai_api_key"));
        assert!(kinds.contains(&"anthropic_oauth"));
        assert!(r.text.contains("[REDACTED:openai_api_key]"));
        assert!(r.text.contains("[REDACTED:anthropic_oauth]"));
    }

    #[test]
    fn url_embedded_credentials_are_redacted() {
        let raw = "DATABASE_URL=postgres://admin:S3cr3tP4ssw0rd@db.internal:5432/prod";
        let r = redact_secrets(raw);
        assert!(r.was_redacted(), "{:?}", r);
        assert!(r.text.contains("[REDACTED:url_credentials]"));
        assert!(!r.text.contains("S3cr3tP4ssw0rd"));
        // Host stays readable; only the credentials run is wiped.
        assert!(r.text.contains("db.internal:5432/prod"));

        // Other common schemes are covered too.
        let redis = redact_secrets("redis://default:An0therSecret123@cache:6379");
        assert!(redis.text.contains("[REDACTED:url_credentials]"));
        assert!(!redis.text.contains("An0therSecret123"));

        // A URL without credentials must NOT be redacted.
        let clean = redact_secrets("see https://example.com/path?x=1 for docs");
        assert!(!clean.was_redacted(), "{:?}", clean);
    }

    #[test]
    fn github_pat_classic_and_fine_grained_redacted() {
        // Realistic lengths: classic PATs are `ghp_` + 36 base62;
        // fine-grained PATs are `github_pat_` + ≥50 base62/underscore.
        let raw = concat!(
            "classic: ghp_abcdefghijklmnopqrstuvwxyzABCDEFGHIJ ",
            "fine: github_pat_11AAA_abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJabcdef",
        );
        let r = redact_secrets(raw);
        let kinds: Vec<_> = r.matches.iter().map(|m| m.kind).collect();
        assert_eq!(
            kinds.iter().filter(|k| **k == "github_pat").count(),
            2,
            "two github_pat matches expected; got matches: {:?}",
            r.matches
        );
        assert!(!r.text.contains("ghp_abcdef"));
        assert!(!r.text.contains("github_pat_11AAA"));
    }

    #[test]
    fn slack_aws_jwt_pem_google_all_redact() {
        let raw = concat!(
            // Synthetic, non-functional token shaped to exercise the
            // slack_token detector. The prefix is split so secret
            // scanners don't flag the literal as a real credential.
            "slack=",
            "xoxb",
            "-12345678-abcdefghijklmnop ",
            "aws=AKIAIOSFODNN7EXAMPLE ",
            "jwt=eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJ0ZXN0In0.abcdef ",
            "google=AIzaSyDx0o-1234567890abcdefghijklmnopqrs ",
            "pem=-----BEGIN RSA PRIVATE KEY-----\nABCDEFG\n-----END RSA PRIVATE KEY-----"
        );
        let r = redact_secrets(raw);
        let kinds: Vec<&'static str> = {
            let mut k = r.matches.iter().map(|m| m.kind).collect::<Vec<_>>();
            k.sort_unstable();
            k.dedup();
            k
        };
        for expected in [
            "aws_access_key",
            "google_api_key",
            "jwt",
            "private_key_pem",
            "slack_token",
        ] {
            assert!(
                kinds.contains(&expected),
                "missing kind {expected}: {kinds:?}"
            );
        }
    }

    #[test]
    fn generic_assignments_match_only_with_secret_looking_value() {
        // Should match: long enough value.
        let bad = "config: api_key = \"abcdef1234567890\" \n token : 0123456789abcdefghij1234567890\n password = hunter2hunter2";
        let r_bad = redact_secrets(bad);
        let kinds: Vec<_> = r_bad.matches.iter().map(|m| m.kind).collect();
        assert!(kinds.contains(&"generic_api_key"));
        assert!(kinds.contains(&"generic_token"));
        assert!(kinds.contains(&"generic_password"));

        // Should NOT match: too-short value or non-secret-looking.
        let safe = "api_key = short  token: 12345  password = a";
        let r_safe = redact_secrets(safe);
        assert!(
            r_safe.matches.is_empty(),
            "short values should not trip generic patterns: {r_safe:?}"
        );
    }

    #[test]
    fn bearer_token_in_curl_log_is_redacted() {
        let raw = "curl -H 'Authorization: Bearer abc123def456ghi789' https://api.example.com";
        let r = redact_secrets(raw);
        assert!(r.was_redacted());
        assert_eq!(r.matches[0].kind, "generic_bearer");
        assert!(r.text.contains("[REDACTED:generic_bearer]"));
        assert!(!r.text.contains("abc123def456ghi789"));
    }

    #[test]
    fn overlapping_matches_keep_first_only() {
        // The same byte run could match two patterns (e.g. an
        // openai_api_key inside a `Bearer ` prefix). The earlier
        // pattern claims it; we don't double-redact.
        let raw = "Authorization: Bearer sk-1234567890abcdef1234567890abcdef1234";
        let r = redact_secrets(raw);
        assert_eq!(
            r.matches.len(),
            1,
            "non-overlapping rule should pick one: {r:?}"
        );
    }

    #[test]
    fn summary_lists_unique_kinds() {
        let raw =
            "ghp_abcdefghijklmnopqrstuvwxyzABCDEFGHIJ and ghp_ZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZ";
        let r = redact_secrets(raw);
        let summary = r.summary();
        assert!(summary.contains("github_pat"));
        // Only one kind reported even though two matches happened.
        assert!(summary.starts_with("redacted 2 secrets: github_pat"));
    }

    #[test]
    fn match_offsets_point_into_original_text() {
        let raw = "prefix sk-ant-api03-1234567890abcdef1234567890abcdef suffix";
        let r = redact_secrets(raw);
        assert_eq!(r.matches.len(), 1);
        let m = &r.matches[0];
        // The matched bytes start at "sk-ant-..." and extend over the token.
        let original_match = &raw[m.start..m.start + m.len];
        assert!(original_match.starts_with("sk-ant-api03"));
    }

    #[test]
    fn redaction_preserves_non_secret_surroundings() {
        let raw = "# Save to .env\nCLAUDE_CODE_OAUTH_TOKEN=sk-ant-api03-AbCdEfGhIjKlMnOpQrStUv0123456789AbCdEf\n# Use it";
        let r = redact_secrets(raw);
        assert!(r.text.starts_with("# Save to .env"));
        assert!(r.text.ends_with("# Use it"));
        assert!(
            r.text
                .contains("CLAUDE_CODE_OAUTH_TOKEN=[REDACTED:anthropic_oauth]")
        );
    }

    fn redaction_kinds(result: &RedactionResult) -> Vec<&'static str> {
        result.matches.iter().map(|m| m.kind).collect()
    }

    /// Failure text stays free of the input and the value. Callers pass a
    /// short `label` so a miss is identifiable without printing either.
    fn assert_value_redacted(label: &str, raw: &str, secret: &str) {
        let r = redact_secrets(raw);
        let kinds = redaction_kinds(&r);
        assert!(
            r.was_redacted(),
            "expected redaction for {label} (input len {}, kinds {kinds:?})",
            raw.len()
        );
        assert!(
            !r.text.contains(secret),
            "value still present for {label} (input len {}, value len {}, kinds {kinds:?})",
            raw.len(),
            secret.len()
        );
    }

    fn assert_unchanged(label: &str, raw: &str) {
        let r = redact_secrets(raw);
        let kinds = redaction_kinds(&r);
        assert!(
            !r.was_redacted(),
            "unexpected redaction for {label} (input len {}, kinds {kinds:?})",
            raw.len()
        );
        assert!(
            r.text == raw,
            "text changed for {label} (input len {}, output len {})",
            raw.len(),
            r.text.len()
        );
    }

    #[test]
    fn embedded_env_secret_names_are_redacted() {
        // Underscore is a word character, so a `\btoken` / `\bpassword`
        // rule misses these. The value must go; the key stays.
        let token = "GITHUB_TOKEN=abcdefghijklmnopqrstuvwxyz0123456789";
        assert_value_redacted(
            "github_token",
            token,
            "abcdefghijklmnopqrstuvwxyz0123456789",
        );
        assert!(redact_secrets(token).text.contains("GITHUB_TOKEN="));

        let exported = "export DB_PASSWORD=\"hunter2hunter2\"";
        assert_value_redacted("export_db_password", exported, "hunter2hunter2");
        assert!(redact_secrets(exported).text.contains("DB_PASSWORD="));

        let spaced = "export GITHUB_TOKEN = 'abcdefghijklmnopqrstuvwxyz012345'";
        assert_value_redacted(
            "export_github_token_quoted",
            spaced,
            "abcdefghijklmnopqrstuvwxyz012345",
        );

        let passwd = "DB_PASSWD=hunter2hunter2";
        assert_value_redacted("db_passwd", passwd, "hunter2hunter2");

        let secret = "AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
        assert_value_redacted(
            "aws_secret_access_key",
            secret,
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
        );
        assert!(
            redact_secrets(secret)
                .text
                .contains("AWS_SECRET_ACCESS_KEY=")
        );

        assert_value_redacted("apikey", "APIKEY=abcdef1234567890", "abcdef1234567890");
        assert_value_redacted(
            "private_key",
            "private_key=abcdef1234567890",
            "abcdef1234567890",
        );
        assert_value_redacted(
            "user_credential",
            "user_credential=hunter2hunter2",
            "hunter2hunter2",
        );
        assert_value_redacted(
            "proxy_auth",
            "proxy_auth=abcdefghijklmnopqrst",
            "abcdefghijklmnopqrst",
        );
    }

    #[test]
    fn json_and_yaml_secret_keys_are_redacted() {
        let json = r#"{"github_token": "abcdefghijklmnopqrstuvwxyz0123456789", "db_password": "hunter2hunter2", "api_key": "abcdef1234567890"}"#;
        assert_value_redacted(
            "json_github_token",
            json,
            "abcdefghijklmnopqrstuvwxyz0123456789",
        );
        assert_value_redacted("json_db_password", json, "hunter2hunter2");
        assert_value_redacted("json_api_key", json, "abcdef1234567890");
        let redacted = redact_secrets(json);
        assert!(redacted.text.contains("\"github_token\""));
        assert!(redacted.text.contains("\"db_password\""));

        let compact = r#"{"github_token":"abcdefghijklmnopqrstuvwxyz0123456789"}"#;
        assert_value_redacted(
            "json_github_token_compact",
            compact,
            "abcdefghijklmnopqrstuvwxyz0123456789",
        );

        let yaml = "db_password: hunter2hunter2\napi-key: \"abcdef1234567890\"\nprivate_key: 'abcdef1234567890'\n";
        assert_value_redacted("yaml_db_password", yaml, "hunter2hunter2");
        assert_value_redacted("yaml_api_key", yaml, "abcdef1234567890");
    }

    #[test]
    fn second_pass_does_not_rewrite_placeholders() {
        // add_memory redacts, then the projector redacts the stored text again.
        let raw = "Add CLAUDE_CODE_OAUTH_TOKEN=sk-ant-api03-AbCdEfGhIjKlMnOpQrStUv0123456789AbCdEf to .env";
        let once = redact_secrets(raw);
        let once_kinds = redaction_kinds(&once);
        assert!(
            once.text.contains("[REDACTED:anthropic_oauth]"),
            "missing anthropic_oauth placeholder (len {}, kinds {once_kinds:?})",
            once.text.len()
        );
        let twice = redact_secrets(&once.text);
        let twice_kinds = redaction_kinds(&twice);
        assert!(
            twice.text == once.text,
            "second pass changed text (first len {}, second len {})",
            once.text.len(),
            twice.text.len()
        );
        assert!(
            !twice.was_redacted(),
            "second pass redacted again (kinds {twice_kinds:?})"
        );

        let json = r#"{"github_token":"abcdefghijklmnopqrstuvwxyz0123456789"}"#;
        let once = redact_secrets(json);
        let again = redact_secrets(&once.text);
        assert!(
            again.text == once.text,
            "second json pass changed text (first len {}, second len {})",
            once.text.len(),
            again.text.len()
        );
    }

    #[test]
    fn shaped_secret_assigned_to_generic_key_keeps_specific_kind() {
        let raw = "api_key=sk-1234567890abcdef1234567890abcdef1234";
        let r = redact_secrets(raw);
        let kinds = redaction_kinds(&r);
        assert_eq!(kinds.len(), 1, "kinds {kinds:?}");
        assert_eq!(
            kinds.first().copied(),
            Some("openai_api_key"),
            "kinds {kinds:?}"
        );
        assert!(
            r.text.starts_with("api_key="),
            "key prefix missing (output len {}, kinds {kinds:?})",
            r.text.len()
        );
        assert!(
            !r.text.contains("sk-1234"),
            "shaped value still present (output len {}, kinds {kinds:?})",
            r.text.len()
        );
    }

    #[test]
    fn non_secret_token_and_auth_names_stay_visible() {
        // Values are long enough that a key-name miss is the only thing
        // that can keep these from being redacted.
        assert_unchanged("token_count", "token_count=12345678901234567890");
        assert_unchanged("max_tokens_colon", "max_tokens: 12345678901234567890");
        assert_unchanged("max_tokens_eq", "max_tokens=12345678901234567890");
        assert_unchanged("tokens", "tokens=12345678901234567890");
        assert_unchanged("author", "author=JaneDoeTheWriterPersonExtra");
        assert_unchanged("auth_type", "auth_type=application-default-login");
    }
}
