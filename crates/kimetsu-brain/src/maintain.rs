//! v2.6: the brain's background upkeep.
//!
//! Kimetsu accumulated a shelf of maintenance passes — consolidation, query
//! routing, pruning, digest refresh, self-tuning, skill graduation — and every
//! one of them was a CLI command a human had to remember to run. In practice
//! nobody did, so a long-lived brain drifted: near-duplicates piled up, the
//! query-routing index went stale, skills never graduated, and the tune
//! triggers fired into a terminal nobody was reading.
//!
//! ## Why this is not a scheduler
//!
//! The obvious fix is a resident daemon with a timer. Kimetsu already has a
//! daemon — the embedder — and deliberately keeps it a dumb model cache with a
//! 300 ms client budget, because anything it does slowly is something the hook
//! waits for.
//!
//! So upkeep works the way the rest of the system already does: hooks are
//! stateless and cheap, and anything expensive is a detached spawn. Each pass
//! records when it last ran; [`due_passes`] answers "what is overdue"; the
//! session hooks fire a detached `kimetsu brain maintain` when anything is, and
//! return immediately. No resident process, no timer thread, nothing new to
//! supervise, and no way for upkeep to sit in front of a prompt.
//!
//! ## Free tier
//!
//! Every pass here is deterministic or statistical: co-citation stapling,
//! query-route rebuilding, usefulness pruning, digest assembly, skill-candidate
//! detection. No model call, so upkeep runs on the Free tier. Reflection (Deep)
//! is deliberately absent — it is a model call, and a background model call is
//! exactly the kind of surprise bill this project exists to avoid.

use std::collections::BTreeMap;
use std::path::Path;

use kimetsu_core::KimetsuResult;
use serde::{Deserialize, Serialize};

/// A unit of upkeep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Pass {
    /// Co-citation stapling + query-route rebuilding (`brain reinforce`).
    Reinforce,
    /// Rebuild the repo digest so the next warm start is current.
    Digest,
    /// Report memories that have earned pruning. Never destructive here — it
    /// surfaces candidates; retiring one stays a human decision.
    Prune,
    /// Detect memories that have been cited enough to become skills.
    Skills,
}

impl Pass {
    pub const ALL: [Pass; 4] = [Pass::Reinforce, Pass::Digest, Pass::Prune, Pass::Skills];

    pub fn as_str(self) -> &'static str {
        match self {
            Pass::Reinforce => "reinforce",
            Pass::Digest => "digest",
            Pass::Prune => "prune",
            Pass::Skills => "skills",
        }
    }

    /// How long before this pass is worth running again.
    ///
    /// These are deliberately long. Upkeep that runs constantly is a
    /// background CPU cost the user did not ask for; the passes here converge
    /// on evidence that accumulates over days, not minutes.
    pub fn interval_secs(self) -> u64 {
        match self {
            // Consolidation needs citations to accumulate before it has
            // anything new to staple.
            Pass::Reinforce => 24 * 60 * 60,
            // The digest also self-refreshes on staleness at warm start; this
            // is the backstop for a repo nobody has opened in a while.
            Pass::Digest => 12 * 60 * 60,
            Pass::Prune => 7 * 24 * 60 * 60,
            Pass::Skills => 24 * 60 * 60,
        }
    }
}

impl std::str::FromStr for Pass {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "reinforce" => Ok(Pass::Reinforce),
            "digest" => Ok(Pass::Digest),
            "prune" => Ok(Pass::Prune),
            "skills" => Ok(Pass::Skills),
            other => Err(format!("unknown maintenance pass `{other}`")),
        }
    }
}

/// When each pass last ran, persisted beside the digest in `.kimetsu/`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MaintenanceState {
    /// Pass name → unix seconds of its last run.
    #[serde(default)]
    pub last_run: BTreeMap<String, u64>,
}

impl MaintenanceState {
    pub fn last_run(&self, pass: Pass) -> Option<u64> {
        self.last_run.get(pass.as_str()).copied()
    }

    pub fn mark_ran(&mut self, pass: Pass, now: u64) {
        self.last_run.insert(pass.as_str().to_string(), now);
    }
}

pub fn state_path(kimetsu_dir: &Path) -> std::path::PathBuf {
    kimetsu_dir.join("maintenance.json")
}

/// Best-effort load — a missing or corrupt file means "nothing has ever run",
/// which makes every pass due. That is the safe direction: the worst case is
/// one extra upkeep run.
pub fn load_state(kimetsu_dir: &Path) -> MaintenanceState {
    std::fs::read_to_string(state_path(kimetsu_dir))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

pub fn save_state(kimetsu_dir: &Path, state: &MaintenanceState) -> KimetsuResult<()> {
    let path = state_path(kimetsu_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(state)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Which passes are overdue at `now`. Pure, so the schedule is unit-testable
/// without touching a clock or a disk.
pub fn due_passes(state: &MaintenanceState, now: u64) -> Vec<Pass> {
    Pass::ALL
        .into_iter()
        .filter(|pass| match state.last_run(*pass) {
            None => true,
            Some(last) => now.saturating_sub(last) >= pass.interval_secs(),
        })
        .collect()
}

/// What one pass did.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PassOutcome {
    pub pass: String,
    /// One line of human-readable detail.
    pub detail: String,
    /// False when the pass errored. Upkeep is best-effort: one failing pass
    /// must not stop the others, and must not fail the command.
    pub ok: bool,
}

/// Run `passes` against the brain at `start`.
///
/// Best-effort throughout: a pass that errors is reported and skipped, because
/// this runs detached where nobody is watching for a non-zero exit.
pub fn run_passes(start: &Path, passes: &[Pass]) -> Vec<PassOutcome> {
    passes.iter().map(|pass| run_pass(start, *pass)).collect()
}

fn run_pass(start: &Path, pass: Pass) -> PassOutcome {
    let detail = match pass {
        Pass::Reinforce => crate::reinforce::reinforce(start, true, true).map(|summary| {
            format!(
                "{} staple(s), {} route(s)",
                summary.staples_created, summary.routes_built
            )
        }),
        Pass::Digest => Ok(match crate::digest::build_or_load_digest(start, true) {
            Some(digest) => format!("rebuilt ({} chars)", digest.len()),
            None => "nothing to digest yet".to_string(),
        }),
        Pass::Prune => crate::maintenance::prune_low_usefulness(
            start,
            crate::maintenance::PruneOptions {
                // Report only. Retiring a memory is a decision with a blast
                // radius, and background code has no business making it.
                apply: false,
                ..Default::default()
            },
        )
        .map(|summary| format!("{} prune candidate(s)", summary.candidates.len())),
        Pass::Skills => crate::project::load_project_readonly(start).and_then(|(_p, _c, conn)| {
            crate::skill_synthesis::find_synthesis_candidates(&conn)
                .map(|candidates| format!("{} skill candidate(s)", candidates.len()))
        }),
    };

    match detail {
        Ok(detail) => PassOutcome {
            pass: pass.as_str().to_string(),
            detail,
            ok: true,
        },
        Err(err) => PassOutcome {
            pass: pass.as_str().to_string(),
            detail: err.to_string(),
            ok: false,
        },
    }
}

/// Unix seconds now.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn everything_is_due_on_a_brain_that_has_never_run_upkeep() {
        let state = MaintenanceState::default();
        assert_eq!(due_passes(&state, 1_000_000), Pass::ALL.to_vec());
    }

    #[test]
    fn a_pass_that_just_ran_is_not_due() {
        let now = 1_000_000;
        let mut state = MaintenanceState::default();
        state.mark_ran(Pass::Reinforce, now);
        let due = due_passes(&state, now + 60);
        assert!(!due.contains(&Pass::Reinforce), "got: {due:?}");
        assert!(
            due.contains(&Pass::Digest),
            "the others are untouched: {due:?}"
        );
    }

    #[test]
    fn a_pass_becomes_due_again_after_its_interval() {
        let now = 1_000_000;
        let mut state = MaintenanceState::default();
        state.mark_ran(Pass::Reinforce, now);
        assert!(
            !due_passes(&state, now + Pass::Reinforce.interval_secs() - 1)
                .contains(&Pass::Reinforce)
        );
        assert!(
            due_passes(&state, now + Pass::Reinforce.interval_secs()).contains(&Pass::Reinforce)
        );
    }

    /// A clock that jumped backwards (NTP correction, a restored backup) must
    /// not make a pass permanently un-due through an underflow.
    #[test]
    fn a_backwards_clock_does_not_wedge_the_schedule() {
        let mut state = MaintenanceState::default();
        state.mark_ran(Pass::Digest, 2_000_000);
        let due = due_passes(&state, 1_000_000);
        assert!(
            !due.contains(&Pass::Digest),
            "saturating_sub yields 0, so it is simply not yet due: {due:?}"
        );
    }

    #[test]
    fn state_round_trips_and_a_corrupt_file_makes_everything_due() {
        let dir = std::env::temp_dir().join(format!("kimetsu-maintain-{}", now_unix()));
        std::fs::create_dir_all(&dir).expect("mkdir");

        let mut state = MaintenanceState::default();
        state.mark_ran(Pass::Skills, 42);
        save_state(&dir, &state).expect("save");
        assert_eq!(load_state(&dir).last_run(Pass::Skills), Some(42));

        std::fs::write(state_path(&dir), "{ not json").expect("corrupt");
        assert_eq!(
            due_passes(&load_state(&dir), 1_000_000),
            Pass::ALL.to_vec(),
            "a corrupt file must fail towards running upkeep, not away from it"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pass_names_round_trip() {
        for pass in Pass::ALL {
            assert_eq!(pass.as_str().parse::<Pass>(), Ok(pass));
        }
        assert!("reflect".parse::<Pass>().is_err(), "Deep-tier, not upkeep");
    }

    /// Upkeep on a directory that is not a brain must report failures rather
    /// than panicking or erroring out — it runs detached, where an early exit
    /// would silently skip the remaining passes.
    #[test]
    fn passes_are_best_effort_against_a_missing_brain() {
        let dir = std::env::temp_dir().join(format!("kimetsu-maintain-nobrain-{}", now_unix()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let outcomes = run_passes(&dir, &Pass::ALL);
        assert_eq!(outcomes.len(), Pass::ALL.len(), "every pass is attempted");
        std::fs::remove_dir_all(&dir).ok();
    }
}
