//! Flagship 3.1 — `kimetsu brain ask` CLI façade.
//!
//! Delegates to the shared composer in `kimetsu_chat::ask` so 3.1 (CLI) and
//! 3.2 (MCP tool) share identical retrieval + composition logic.
//!
//! Re-exports the public API so `main.rs` can import from `ask::` directly.

pub use kimetsu_chat::ask::{compose_answer, record_helpful_mark};

// Unit tests for CLI-only concerns (command fast-path detection, delegation
// round-trips). The heavy brain-integration tests live in kimetsu-chat/src/ask.rs.

#[cfg(test)]
mod tests {
    use super::*;

    // 3.4: command fast-path detection (delegates to kimetsu_chat::ask::is_command_query)
    #[test]
    fn is_command_query_delegated_correctly() {
        assert!(kimetsu_chat::ask::is_command_query(
            "how do I run the tests?"
        ));
        assert!(kimetsu_chat::ask::is_command_query("how to install deps"));
        assert!(!kimetsu_chat::ask::is_command_query(
            "explain the broker design"
        ));
    }

    // Verbatim path available via compose_answer (brain unavailable → graceful).
    #[test]
    fn compose_answer_graceful_for_missing_workspace() {
        let tmp = std::env::temp_dir().join(format!(
            "kimetsu_cli_ask_missing_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let ans = compose_answer(&tmp, "how do I run tests?");
        assert!(
            ans.answer.contains("Brain unavailable")
                || ans.answer.contains("Nothing in project memory"),
            "unexpected: {}",
            ans.answer
        );
    }
}
