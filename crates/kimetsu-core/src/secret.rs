//! v0.4.9: `SecretString` — a wrapper that hides its inner value
//! from `Debug` / `Display` / `serde` so accidental
//! `{:?}`/panic-backtrace/tracing dumps don't leak credentials.
//!
//! Why this exists:
//!   `ClaudeCodeProvider` and `AnthropicProvider` previously held
//!   `api_key: String` and derived `#[derive(Debug)]`. Any
//!   `{:?}` print of the provider, any `tracing::debug!(?provider)`
//!   call, any panic with the struct in the backtrace, would emit
//!   the raw token. We never logged the providers in production
//!   paths, but the latent risk was real — a `dbg!` left in a
//!   debug session would have been enough to leak via stderr.
//!
//! Contract:
//!   * `Debug` and `Display` always emit `"[REDACTED]"`. There is
//!     no way to derive the value from format output.
//!   * `serde::Serialize` emits the same redaction string. Brain
//!     traces, MCP responses, and run JSONL files cannot persist
//!     the inner value by accident.
//!   * The value is only reachable via `expose_secret()`. Each
//!     call site that legitimately needs the cleartext (subprocess
//!     env, HTTP header) must explicitly opt in — and those
//!     accesses are easy to grep for in code review.
//!
//! What this is NOT:
//!   * Not memory-safe / not zeroizing — the underlying String
//!     stays in heap allocations like any other String. For that
//!     we'd reach for the `secrecy` / `zeroize` crates; v0.4.9
//!     keeps the dependency footprint tight and addresses only
//!     the actual incident vector (Debug + Display + serde).

use std::fmt;

/// Holds a credential and refuses to print it. Construct via
/// `SecretString::new(value)` or `String::into`. Read with
/// `expose_secret()` at the exact call site that needs the
/// cleartext — those callers should be enumerable on `git grep`.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretString(String);

impl SecretString {
    /// Wrap a plaintext credential. The inner String is moved in
    /// and never reborrowed by reference outside this type.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrow the cleartext. Use sparingly — every call site is a
    /// place where the secret could leak into logs.
    pub fn expose_secret(&self) -> &str {
        &self.0
    }

    /// True when the inner value is empty. Useful for "did the env
    /// var resolve?" checks without exposing the value.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Length of the inner value. Used by diagnostic logs to
    /// confirm "secret is present" without disclosing it.
    pub fn len(&self) -> usize {
        self.0.len()
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Length included so panic backtraces still say something
        // useful ("a 64-char token is present") without leaking
        // the value.
        write!(f, "SecretString([REDACTED; len={}])", self.0.len())
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl From<String> for SecretString {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for SecretString {
    fn from(value: &str) -> Self {
        Self::new(value.to_string())
    }
}

/// `Serialize` always emits the redaction marker so brain traces +
/// MCP responses + run JSONL files cannot persist the cleartext.
impl serde::Serialize for SecretString {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str("[REDACTED]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_format_never_includes_inner_value() {
        let s = SecretString::new("sk-ant-api03-very-real-looking-but-not");
        let dbg = format!("{:?}", s);
        assert!(
            !dbg.contains("sk-ant-api03"),
            "Debug must NOT include the inner value: {dbg}"
        );
        assert!(dbg.contains("REDACTED"));
        assert!(dbg.contains("len=38"));
    }

    #[test]
    fn display_emits_redaction_marker() {
        let s = SecretString::new("hunter2");
        assert_eq!(format!("{}", s), "[REDACTED]");
        assert!(!format!("{}", s).contains("hunter2"));
    }

    #[test]
    fn serialize_emits_redaction_marker() {
        let s = SecretString::new("sk-ant-api03-leaky-1234567890");
        let json = serde_json::to_string(&s).expect("serialize");
        assert_eq!(json, "\"[REDACTED]\"");
        assert!(!json.contains("sk-ant"));
    }

    #[test]
    fn expose_secret_returns_cleartext() {
        let s = SecretString::new("real-value");
        assert_eq!(s.expose_secret(), "real-value");
    }

    #[test]
    fn empty_and_len_helpers() {
        assert!(SecretString::new("").is_empty());
        let s = SecretString::new("abcd");
        assert!(!s.is_empty());
        assert_eq!(s.len(), 4);
    }

    /// Regression guard: when a struct holding a SecretString is
    /// Debug-printed, the secret stays redacted EVEN INSIDE the
    /// parent struct's derived Debug output.
    #[test]
    fn parent_struct_derive_debug_does_not_leak() {
        #[derive(Debug)]
        #[allow(dead_code)] // fields only written, not read; struct exists to test Debug redaction
        struct Provider {
            api_key: SecretString,
            model: String,
        }
        let p = Provider {
            api_key: SecretString::new("sk-ant-api03-DEFINITELY-LEAKED-IF-BROKEN"),
            model: "claude-opus".into(),
        };
        let dbg = format!("{:?}", p);
        assert!(
            !dbg.contains("DEFINITELY-LEAKED-IF-BROKEN"),
            "parent struct's derived Debug must NOT leak nested SecretString: {dbg}"
        );
        assert!(dbg.contains("REDACTED"));
        assert!(
            dbg.contains("claude-opus"),
            "non-secret fields should print"
        );
    }
}
