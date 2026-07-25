//! v3.0: how injected memory is framed to the reader.
//!
//! MemSyco-Bench (arXiv 2607.01071) reports the uncomfortable result that most
//! memory systems score *worse* on their sycophancy track than using no memory
//! at all. The mechanism is not that the memories are wrong more often than
//! they are right — it is that a retrieved memory arrives looking like ground
//! truth, so the model defers to it over evidence directly in front of it. A
//! memory saying "the config lives at `config/app.toml`" beats the agent's own
//! `ls` showing it does not, because nothing in the injection said which of the
//! two wins.
//!
//! Kimetsu's own framing was the plain header `"Kimetsu brain relevant
//! knowledge for this task:"`. "Knowledge" is exactly the wrong word for it:
//! what the brain holds is *what was recorded*, at some past moment, by someone
//! or something that believed it then.
//!
//! ## The precision this needs
//!
//! Over-hedging is not the fix, and would cost more than it saves. An agent
//! told "here is something that might be wrong" ignores memory, which is the
//! whole product. The framing has to say which questions memory settles and
//! which it does not, because those are genuinely different:
//!
//! * Memory *is* authoritative about **what was decided, learned, or agreed** —
//!   a convention, a preference, a lesson from a real failure. There is no
//!   other source for those; the repository does not record why.
//! * Memory is *not* authoritative about **what the code currently is**. The
//!   working tree is, and it has moved since the memory was written. Every
//!   memory-versus-reality conflict is of this second kind.
//!
//! So the rule is one sentence and it is a rule about conflicts, not a
//! disclaimer: prefer what you can observe now over what was recorded then.
//! That is exactly the deference MemSyco measures, inverted.
//!
//! Model-free — this is a string.

/// Header for the per-turn context injection.
///
/// The date the memory was recorded is not in the header because it is not
/// known per-bundle; capsules carry their own dates when the question is about
/// time (see [`crate::ordering`]).
pub const CONTEXT_HEADER: &str = "Recorded in this project's Kimetsu brain. These are prior conclusions, not \
     observations of the current tree — where one conflicts with what you can \
     check now, what you can check now wins:";

/// Header for a proactive mid-work injection.
///
/// Shorter than [`CONTEXT_HEADER`] on purpose: the proactive hook interrupts
/// work that is already underway, on a budget of roughly one capsule, and a
/// three-line preamble around a one-line memory is the injection reading as
/// noise.
pub const PROACTIVE_SUFFIX: &str = " (recorded previously — verify before acting on it)";

/// Guidance handed to an MCP client alongside a context bundle.
///
/// The MCP surface has no fixed render, so the framing has to travel as an
/// instruction rather than as a header the client is guaranteed to print.
pub const MCP_HOW_TO_USE: &str = "Capsule summaries are prior conclusions recorded in this project's brain, not \
     observations of the current working tree. They are the authority on what was \
     decided, learned, or preferred — the repository does not record why. They are \
     not the authority on what the code is now: where a capsule conflicts with what \
     you can check in the tree, prefer what you can check. Memory capsules are \
     durable brain state; repo_file and repo_manifest capsules point to likely \
     relevant files and manifests.";

#[cfg(test)]
mod tests {
    use super::*;

    /// The framing is a rule about conflicts. A disclaimer that does not say
    /// which side wins leaves the reader exactly where MemSyco found it.
    #[test]
    fn every_framing_states_which_side_wins_a_conflict() {
        for text in [CONTEXT_HEADER, MCP_HOW_TO_USE] {
            let lower = text.to_ascii_lowercase();
            assert!(
                lower.contains("conflict"),
                "framing must name the conflict case: {text}"
            );
            assert!(
                lower.contains("wins") || lower.contains("prefer"),
                "framing must resolve the conflict, not just flag it: {text}"
            );
        }
    }

    /// Hedging costs the product its value, so the framing must not tell the
    /// reader the memory is probably wrong — only that the tree outranks it.
    #[test]
    fn the_framing_does_not_hedge_the_memory_itself() {
        for text in [CONTEXT_HEADER, PROACTIVE_SUFFIX, MCP_HOW_TO_USE] {
            let lower = text.to_ascii_lowercase();
            for hedge in [
                "may be wrong",
                "might be wrong",
                "unreliable",
                "do not trust",
            ] {
                assert!(
                    !lower.contains(hedge),
                    "framing must not hedge with {hedge:?}: {text}"
                );
            }
        }
    }

    /// The proactive hook interrupts work already underway on a one-capsule
    /// budget; a preamble longer than the memory reads as noise.
    #[test]
    fn the_proactive_framing_stays_short() {
        assert!(
            PROACTIVE_SUFFIX.len() < CONTEXT_HEADER.len() / 2,
            "proactive framing is {} chars",
            PROACTIVE_SUFFIX.len()
        );
    }
}
