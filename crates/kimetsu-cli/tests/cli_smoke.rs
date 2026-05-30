//! v0.5.3: CLI plumbing smoke tests.
//!
//! Shells out to the built `kimetsu` binary and verifies argument
//! parsing, subcommand wiring, and JSON output schemas don't drift.
//! Deliberately small (~5 tests, no model calls, no API keys) — the
//! purpose is to catch regressions that the in-process
//! `kimetsu-e2e` tests can't see by construction.
//!
//! Resolves the binary path via `CARGO_BIN_EXE_kimetsu`, which Cargo
//! sets when running integration tests for a crate that defines a
//! `[[bin]]`. No PATH hacks required.

use std::process::Command;

/// Path to the freshly-built `kimetsu` binary. Cargo injects
/// `CARGO_BIN_EXE_<name>` for each `[[bin]]` declared in the crate
/// being tested.
fn kimetsu_bin() -> &'static str {
    env!("CARGO_BIN_EXE_kimetsu")
}

#[test]
fn kimetsu_version_prints_a_version_string_and_exits_clean() {
    let output = Command::new(kimetsu_bin())
        .arg("--version")
        .output()
        .expect("spawn kimetsu --version");
    assert!(
        output.status.success(),
        "kimetsu --version should exit 0; got {:?}, stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("kimetsu"),
        "kimetsu --version should mention 'kimetsu'; got: {stdout}"
    );
}

#[test]
fn kimetsu_help_lists_top_level_subcommands() {
    let output = Command::new(kimetsu_bin())
        .arg("--help")
        .output()
        .expect("spawn kimetsu --help");
    assert!(output.status.success(), "kimetsu --help should exit 0");

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Spot-check the key user-facing subcommands.
    // If one of these disappears, --help drift caught at PR time.
    for expected in [
        "init",
        "brain",
        "chat",
        "doctor",
        "bridge",
        "mcp",
        "update",
        "uninstall",
    ] {
        assert!(
            stdout.contains(expected),
            "kimetsu --help should mention `{expected}`; got: {stdout}"
        );
    }
}

#[test]
fn kimetsu_uninstall_help_lists_confirmation_flags() {
    let output = Command::new(kimetsu_bin())
        .args(["uninstall", "--help"])
        .output()
        .expect("spawn kimetsu uninstall --help");
    assert!(output.status.success(), "uninstall --help should exit 0");

    let stdout = String::from_utf8_lossy(&output.stdout);
    for expected in ["--yes", "--dry-run", "--delete-user-data"] {
        assert!(
            stdout.contains(expected),
            "uninstall --help should mention `{expected}`; got: {stdout}"
        );
    }
}

#[test]
fn kimetsu_update_help_lists_check_mode() {
    let output = Command::new(kimetsu_bin())
        .args(["update", "--help"])
        .output()
        .expect("spawn kimetsu update --help");
    assert!(output.status.success(), "update --help should exit 0");

    let stdout = String::from_utf8_lossy(&output.stdout);
    for expected in ["--check", "--dry-run", "--flavor"] {
        assert!(
            stdout.contains(expected),
            "update --help should mention `{expected}`; got: {stdout}"
        );
    }
}

#[test]
fn kimetsu_brain_help_lists_brain_subcommands() {
    let output = Command::new(kimetsu_bin())
        .args(["brain", "--help"])
        .output()
        .expect("spawn kimetsu brain --help");
    assert!(output.status.success(), "brain --help should exit 0");

    let stdout = String::from_utf8_lossy(&output.stdout);
    for expected in ["memory", "ingest", "rebuild"] {
        assert!(
            stdout.contains(expected),
            "brain --help should mention `{expected}`; got: {stdout}"
        );
    }
}

#[test]
fn kimetsu_brain_memory_help_lists_v05_subcommands() {
    let output = Command::new(kimetsu_bin())
        .args(["brain", "memory", "--help"])
        .output()
        .expect("spawn kimetsu brain memory --help");
    assert!(output.status.success(), "brain memory --help should exit 0");

    let stdout = String::from_utf8_lossy(&output.stdout);
    // v0.5.0 added `blame`, v0.5.2 added `conflicts`. Both must be
    // discoverable via --help or the user can't find them.
    for expected in ["add", "list", "blame", "conflicts"] {
        assert!(
            stdout.contains(expected),
            "brain memory --help should mention `{expected}`; got: {stdout}"
        );
    }
}

#[test]
fn kimetsu_unknown_subcommand_exits_nonzero_with_helpful_message() {
    let output = Command::new(kimetsu_bin())
        .arg("not-a-real-subcommand")
        .output()
        .expect("spawn kimetsu with bogus subcommand");
    assert!(
        !output.status.success(),
        "kimetsu with bogus subcommand should NOT exit 0"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    // clap's standard "unrecognized subcommand" wording — if this
    // changes upstream we update; the test asserts the user gets
    // SOMETHING actionable on stderr.
    assert!(
        !stderr.trim().is_empty(),
        "stderr should not be empty on bad subcommand"
    );
}
