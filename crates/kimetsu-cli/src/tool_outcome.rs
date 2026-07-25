//! v2.6: deciding whether a tool call actually failed.
//!
//! The proactive PostToolUse hook only earns its keep if "did that fail?" is
//! answered accurately. Through v2.5 the answer was a case-insensitive
//! substring scan for ten words — `error`, `failed`, `fatal`, `panic`,
//! `exception`, `traceback`, `denied`, `not found`, `cannot`, `no such`.
//!
//! That misfires constantly on real developer output:
//!
//! ```text
//! test result: ok. 517 passed; 0 failed        <- "failed"
//! Compiling error-chain v0.12.4                <- "error"
//! running 3 tests ... test error_handling ok   <- "error"
//! grep -rn "cannot" src/                       <- "cannot"
//! ```
//!
//! Every one of those is a *successful* command, and every one triggered a
//! proactive interruption. A memory system that cries wolf gets muted.
//!
//! This module answers the question with the strongest evidence available,
//! in order:
//!
//! 1. **Exit code** — authoritative when the harness gives us one. Nothing a
//!    substring can say outweighs `exit 0`.
//! 2. **Toolchain parsers** — cargo, rustc, tsc, pytest, jest/vitest, go test,
//!    make, npm. These read the summary line the tool prints precisely so
//!    humans can tell, which also makes for a far better error signature than
//!    "first line containing the word error".
//! 3. **Negative markers** — explicit statements of success (`0 failed`,
//!    `test result: ok`, `0 errors`) veto the substring heuristic. This is what
//!    kills the whole "passing test suite looks like a failure" class.
//! 4. **The substring list** — retained as a last resort, because plenty of
//!    tools have no parser and no exit code reaches us.
//!
//! The verdict carries its own [`Evidence`], so callers can require stronger
//! proof for more disruptive actions, and its own signature — the toolchain
//! parsers extract the actual diagnostic, which makes a far better retrieval
//! query than "first line containing the word error".

/// How the verdict was reached. Ordered weakest to strongest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Evidence {
    /// Nothing but the legacy substring scan. Noisy; treat with suspicion.
    Heuristic,
    /// A toolchain-specific summary line (cargo, pytest, jest, …).
    Toolchain,
    /// A real exit code from the harness.
    ExitCode,
}

/// What a tool call did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutcome {
    /// True when this looks like a failure.
    pub failed: bool,
    /// How the verdict was reached.
    pub evidence: Evidence,
    /// A short, human-meaningful description of the failure, when one could be
    /// extracted. `None` on success or when nothing useful was found.
    pub signature: Option<String>,
    /// Which parser recognised the output, when one did.
    pub toolchain: Option<&'static str>,
}

impl ToolOutcome {
    fn success(evidence: Evidence) -> Self {
        Self {
            failed: false,
            evidence,
            signature: None,
            toolchain: None,
        }
    }

    fn failure(
        evidence: Evidence,
        signature: Option<String>,
        toolchain: Option<&'static str>,
    ) -> Self {
        Self {
            failed: true,
            evidence,
            signature,
            toolchain,
        }
    }
}

/// Max length of an extracted error signature. Long enough to be identifying,
/// short enough to key a per-session dedupe map on.
const SIGNATURE_MAX_CHARS: usize = 120;

/// Legacy substring markers. Kept as the last-resort tier.
pub const FAILURE_MARKERS: &[&str] = &[
    "error",
    "failed",
    "fatal",
    "panic",
    "exception",
    "traceback",
    "denied",
    "not found",
    "cannot",
    "no such",
];

/// Phrases that assert success. Any of these vetoes the substring tier — they
/// are exactly the lines that contain a failure keyword *because* the tool is
/// reporting that none occurred.
const SUCCESS_MARKERS: &[&str] = &[
    "test result: ok",
    "0 failed",
    "0 errors",
    "0 error",
    "no errors",
    "failures: 0",
    "failed: 0",
    "errors: 0",
    "build succeeded",
    "all tests passed",
    "tests passed",
];

/// Decide what happened, given whatever the harness handed us.
///
/// `exit_code` is `None` when the harness does not report one — which is the
/// common case for Claude Code's Bash `tool_response`, hence the fallbacks.
pub fn classify(output: &str, exit_code: Option<i64>) -> ToolOutcome {
    // 1. An exit code settles it. A tool that exits 0 did not fail, no matter
    //    how many times the word "error" appears in its output; a tool that
    //    exits non-zero did, even if it printed nothing recognisable.
    if let Some(code) = exit_code {
        if code == 0 {
            return ToolOutcome::success(Evidence::ExitCode);
        }
        let parsed = parse_toolchain(output);
        return ToolOutcome::failure(
            Evidence::ExitCode,
            parsed
                .as_ref()
                .and_then(|p| p.signature.clone())
                .or_else(|| first_failure_line(output)),
            parsed.and_then(|p| p.toolchain),
        );
    }

    // 2. A toolchain summary line is the next-best authority: these are the
    //    lines the tool prints specifically so a human can tell.
    if let Some(parsed) = parse_toolchain(output) {
        return if parsed.failed {
            ToolOutcome::failure(Evidence::Toolchain, parsed.signature, parsed.toolchain)
        } else {
            ToolOutcome::success(Evidence::Toolchain)
        };
    }

    // 3. No structure to read. Fall back to the substring scan — but let an
    //    explicit statement of success veto it.
    let lower = output.to_ascii_lowercase();
    if SUCCESS_MARKERS.iter().any(|m| lower.contains(m)) {
        return ToolOutcome::success(Evidence::Heuristic);
    }
    if FAILURE_MARKERS.iter().any(|m| lower.contains(m)) {
        return ToolOutcome::failure(Evidence::Heuristic, first_failure_line(output), None);
    }
    ToolOutcome::success(Evidence::Heuristic)
}

// ── Toolchain parsers ────────────────────────────────────────────────────────

struct Parsed {
    failed: bool,
    signature: Option<String>,
    toolchain: Option<&'static str>,
}

/// Try each toolchain parser. The first that recognises the output wins.
///
/// Order matters where outputs overlap: `cargo test` prints both cargo's
/// compile diagnostics and libtest's summary, and the compile error is the
/// more actionable of the two, so cargo goes first.
fn parse_toolchain(output: &str) -> Option<Parsed> {
    parse_cargo(output)
        .or_else(|| parse_rust_test(output))
        .or_else(|| parse_pytest(output))
        .or_else(|| parse_jest(output))
        .or_else(|| parse_go_test(output))
        .or_else(|| parse_tsc(output))
        .or_else(|| parse_npm(output))
        .or_else(|| parse_make(output))
}

fn truncate(s: &str) -> String {
    s.trim().chars().take(SIGNATURE_MAX_CHARS).collect()
}

/// cargo / rustc: `error[E0433]: …`, `error: could not compile …`,
/// `warning: N warnings emitted`. Note that `error:` must be at the start of a
/// line — "Compiling error-chain v0.12" is a package name, not a diagnostic.
fn parse_cargo(output: &str) -> Option<Parsed> {
    let mut saw_cargo = false;
    let mut first_error: Option<String> = None;
    for raw in output.lines() {
        let line = raw.trim_start();
        if line.starts_with("Compiling ")
            || line.starts_with("Finished ")
            || line.starts_with("Checking ")
            || line.starts_with("Running ")
            || line.starts_with("Fresh ")
        {
            saw_cargo = true;
        }
        if line.starts_with("error:") || line.starts_with("error[") {
            saw_cargo = true;
            if first_error.is_none() {
                first_error = Some(truncate(line));
            }
        }
    }
    if !saw_cargo {
        return None;
    }
    Some(Parsed {
        failed: first_error.is_some(),
        signature: first_error,
        toolchain: Some("cargo"),
    })
}

/// libtest: `test result: ok. 517 passed; 0 failed; …` or
/// `test result: FAILED. 4 passed; 1 failed; …`
fn parse_rust_test(output: &str) -> Option<Parsed> {
    let line = output
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with("test result:"))?;
    let failed = line.contains("FAILED");
    Some(Parsed {
        failed,
        signature: failed.then(|| truncate(line)),
        toolchain: Some("cargo-test"),
    })
}

/// pytest: `=== 3 failed, 12 passed in 1.20s ===` / `=== 12 passed in 0.9s ===`
/// / `=== 1 error in 0.3s ===`
fn parse_pytest(output: &str) -> Option<Parsed> {
    let line = output.lines().rev().find(|l| {
        let t = l.trim();
        t.starts_with("===")
            && (t.contains(" passed") || t.contains(" failed") || t.contains(" error"))
    })?;
    let failed = line.contains(" failed") || line.contains(" error");
    Some(Parsed {
        failed,
        signature: failed.then(|| truncate(line)),
        toolchain: Some("pytest"),
    })
}

/// jest / vitest: `Tests:  1 failed, 12 passed, 13 total`
fn parse_jest(output: &str) -> Option<Parsed> {
    let line = output
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with("Tests:"))?;
    let failed = line.contains("failed");
    Some(Parsed {
        failed,
        signature: failed.then(|| truncate(line)),
        toolchain: Some("jest"),
    })
}

/// go test: lines beginning `FAIL` / `ok  `.
fn parse_go_test(output: &str) -> Option<Parsed> {
    let mut saw = false;
    let mut fail_line: Option<String> = None;
    for raw in output.lines() {
        let line = raw.trim_end();
        if line.starts_with("ok  \t") || line.starts_with("ok\t") {
            saw = true;
        }
        if line.starts_with("FAIL\t") || line == "FAIL" {
            saw = true;
            if fail_line.is_none() {
                fail_line = Some(truncate(line));
            }
        }
    }
    if !saw {
        return None;
    }
    Some(Parsed {
        failed: fail_line.is_some(),
        signature: fail_line,
        toolchain: Some("go-test"),
    })
}

/// tsc: `src/a.ts(3,7): error TS2322: …` and
/// `Found 3 errors in 2 files.` / `Found 0 errors.`
fn parse_tsc(output: &str) -> Option<Parsed> {
    let found = output
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with("Found ") && l.contains("error"));
    if let Some(line) = found {
        let failed = !line.contains("Found 0 errors");
        return Some(Parsed {
            failed,
            signature: failed.then(|| truncate(line)),
            toolchain: Some("tsc"),
        });
    }
    let diagnostic = output
        .lines()
        .find(|l| l.contains("): error TS") || l.trim_start().starts_with("error TS"))?;
    Some(Parsed {
        failed: true,
        signature: Some(truncate(diagnostic)),
        toolchain: Some("tsc"),
    })
}

/// npm: `npm error …` (npm 10+) / `npm ERR! …` (npm 9 and earlier).
fn parse_npm(output: &str) -> Option<Parsed> {
    let line = output.lines().find(|l| {
        let t = l.trim_start();
        t.starts_with("npm ERR!") || t.starts_with("npm error")
    })?;
    Some(Parsed {
        failed: true,
        signature: Some(truncate(line)),
        toolchain: Some("npm"),
    })
}

/// make: `make: *** [target] Error 2`
fn parse_make(output: &str) -> Option<Parsed> {
    let line = output
        .lines()
        .find(|l| l.contains("make") && l.contains("***"))?;
    Some(Parsed {
        failed: true,
        signature: Some(truncate(line)),
        toolchain: Some("make"),
    })
}

/// Last-resort signature: the first line containing a failure marker, else the
/// first non-empty line.
fn first_failure_line(output: &str) -> Option<String> {
    let marked = output.lines().find(|l| {
        let lc = l.to_ascii_lowercase();
        FAILURE_MARKERS.iter().any(|m| lc.contains(m))
    });
    let line = marked.or_else(|| output.lines().find(|l| !l.trim().is_empty()))?;
    let sig = truncate(line);
    if sig.is_empty() { None } else { Some(sig) }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── The false positives that motivated this module ───────────────────

    /// The one that matters most: a fully passing Rust test run contains the
    /// word "failed" on its summary line, and used to read as a failure.
    #[test]
    fn a_passing_rust_test_run_is_not_a_failure() {
        let output = "running 517 tests\n\
                      test config::tests::tier_defaults_to_free ... ok\n\
                      \n\
                      test result: ok. 517 passed; 0 failed; 1 ignored; 0 measured\n";
        let outcome = classify(output, None);
        assert!(!outcome.failed, "got: {outcome:?}");
        assert_eq!(outcome.evidence, Evidence::Toolchain);
    }

    #[test]
    fn compiling_a_crate_named_error_chain_is_not_a_failure() {
        let output = "   Compiling error-chain v0.12.4\n\
                          Finished `dev` profile [unoptimized] target(s) in 3.21s\n";
        assert!(!classify(output, None).failed);
    }

    #[test]
    fn a_test_named_error_handling_passing_is_not_a_failure() {
        let output = "running 3 tests\n\
                      test error_handling_rejects_bad_input ... ok\n\
                      test cannot_open_missing_file ... ok\n\
                      \n\
                      test result: ok. 3 passed; 0 failed; 0 ignored\n";
        assert!(!classify(output, None).failed);
    }

    #[test]
    fn a_passing_pytest_run_is_not_a_failure() {
        let output = "collected 12 items\n\n=== 12 passed in 0.94s ===\n";
        let outcome = classify(output, None);
        assert!(!outcome.failed, "got: {outcome:?}");
        assert_eq!(outcome.toolchain, None, "success needs no signature");
    }

    #[test]
    fn tsc_reporting_zero_errors_is_not_a_failure() {
        assert!(!classify("Found 0 errors.\n", None).failed);
    }

    #[test]
    fn an_explicit_success_marker_vetoes_the_substring_scan() {
        // No parser recognises this, but it says out loud that nothing failed.
        let output = "custom-runner: suite complete, 0 failed, 40 ok\n";
        let outcome = classify(output, None);
        assert!(!outcome.failed, "got: {outcome:?}");
        assert_eq!(outcome.evidence, Evidence::Heuristic);
    }

    // ── Real failures still register ─────────────────────────────────────

    #[test]
    fn a_failing_rust_test_run_is_a_failure_with_a_signature() {
        let output = "running 5 tests\n\
                      test a ... FAILED\n\
                      \n\
                      test result: FAILED. 4 passed; 1 failed; 0 ignored\n";
        let outcome = classify(output, None);
        assert!(outcome.failed);
        assert_eq!(outcome.toolchain, Some("cargo-test"));
        assert!(
            outcome
                .signature
                .as_deref()
                .unwrap_or("")
                .contains("FAILED"),
            "got: {outcome:?}"
        );
    }

    #[test]
    fn a_cargo_compile_error_is_a_failure_and_reports_the_diagnostic() {
        let output = "   Compiling kimetsu-brain v2.5.3\n\
                      error[E0433]: failed to resolve: use of undeclared crate `foo`\n\
                      \u{20} --> src/lib.rs:3:5\n";
        let outcome = classify(output, None);
        assert!(outcome.failed);
        assert_eq!(outcome.toolchain, Some("cargo"));
        assert!(
            outcome
                .signature
                .as_deref()
                .unwrap_or("")
                .starts_with("error[E0433]"),
            "signature should be the diagnostic, not a random line: {outcome:?}"
        );
    }

    #[test]
    fn a_failing_pytest_run_is_a_failure() {
        let outcome = classify("=== 3 failed, 12 passed in 1.20s ===\n", None);
        assert!(outcome.failed);
        assert_eq!(outcome.toolchain, Some("pytest"));
    }

    #[test]
    fn a_failing_jest_run_is_a_failure() {
        let outcome = classify("Tests:       1 failed, 12 passed, 13 total\n", None);
        assert!(outcome.failed);
        assert_eq!(outcome.toolchain, Some("jest"));
    }

    #[test]
    fn a_passing_jest_run_is_not_a_failure() {
        assert!(!classify("Tests:       13 passed, 13 total\n", None).failed);
    }

    #[test]
    fn go_test_failures_and_successes_are_distinguished() {
        assert!(classify("FAIL\tgithub.com/a/b\t0.02s\n", None).failed);
        assert!(!classify("ok  \tgithub.com/a/b\t0.02s\n", None).failed);
    }

    #[test]
    fn npm_and_make_failures_are_recognised() {
        assert!(classify("npm error code ELIFECYCLE\n", None).failed);
        assert!(classify("npm ERR! code ELIFECYCLE\n", None).failed);
        assert!(classify("make: *** [Makefile:12: build] Error 2\n", None).failed);
    }

    #[test]
    fn tsc_diagnostics_are_recognised() {
        let outcome = classify(
            "src/a.ts(3,7): error TS2322: Type 'x' is not assignable.\n",
            None,
        );
        assert!(outcome.failed);
        assert_eq!(outcome.toolchain, Some("tsc"));
    }

    #[test]
    fn unstructured_failures_still_fall_through_to_the_substring_scan() {
        let outcome = classify("bash: frobnicate: command not found\n", None);
        assert!(outcome.failed);
        assert_eq!(outcome.evidence, Evidence::Heuristic);
        assert!(outcome.signature.is_some());
    }

    // ── Exit codes outrank everything ────────────────────────────────────

    #[test]
    fn exit_zero_beats_any_amount_of_scary_output() {
        let output = "error: this is fine\nfatal: also fine\npanic: still fine\n";
        let outcome = classify(output, Some(0));
        assert!(!outcome.failed, "got: {outcome:?}");
        assert_eq!(outcome.evidence, Evidence::ExitCode);
    }

    #[test]
    fn nonzero_exit_is_a_failure_even_with_silent_output() {
        let outcome = classify("", Some(1));
        assert!(outcome.failed);
        assert_eq!(outcome.evidence, Evidence::ExitCode);
    }

    #[test]
    fn a_nonzero_exit_still_borrows_the_toolchain_signature() {
        let output = "   Compiling foo v0.1.0\nerror[E0308]: mismatched types\n";
        let outcome = classify(output, Some(101));
        assert!(outcome.failed);
        assert_eq!(outcome.evidence, Evidence::ExitCode);
        assert_eq!(outcome.toolchain, Some("cargo"));
        assert!(
            outcome
                .signature
                .as_deref()
                .unwrap_or("")
                .starts_with("error[E0308]")
        );
    }

    // ── Evidence ordering ────────────────────────────────────────────────

    #[test]
    fn evidence_is_ordered_weakest_to_strongest() {
        assert!(Evidence::Heuristic < Evidence::Toolchain);
        assert!(Evidence::Toolchain < Evidence::ExitCode);
    }

    #[test]
    fn empty_output_without_an_exit_code_is_not_a_failure() {
        assert!(!classify("", None).failed);
        assert!(!classify("   \n\n", None).failed);
    }
}
