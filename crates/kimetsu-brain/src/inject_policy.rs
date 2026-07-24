//! v3.0: deciding whether a proactive recall is worth interrupting for.
//!
//! Kimetsu's proactive hooks surface a memory mid-task — before a command that
//! matches a known failure, or after one that just failed. Whether to speak is
//! the whole ballgame: a memory system that interrupts too often gets muted,
//! and one that never speaks is just a database.
//!
//! Through v2.5 the rule was a fixed score threshold: inject when the top
//! capsule scores ≥ 0.45, or ≥ 0.35 when the agent is visibly looping. Those
//! numbers were picked by hand, apply identically to every brain and every
//! user, and never move no matter how the injections land.
//!
//! *Remember When It Matters* (arXiv 2607.08716) measured what this is worth:
//! a memory agent that decides per turn whether to inject a reminder or stay
//! silent is worth +8.3 pp on Terminal-Bench 2.0 over the same agent without
//! one. The decision is the mechanism, not the retrieval.
//!
//! ## The model
//!
//! A logistic regression over [`Features`] — score, kind, novelty, how often
//! this exact command has already failed, how much has already been injected
//! this session, how long since the last injection, and how strong the
//! evidence for the failure was.
//!
//! Small, linear, and inspectable on purpose. `kimetsu brain policy --status`
//! prints the weights; a user can see why the brain is talking more or less
//! than it used to, which is not true of anything larger.
//!
//! ## The prior is today's behaviour
//!
//! [`Policy::prior`] is fitted by hand so that its decision boundary is
//! *exactly* the old fixed threshold: p = 0.5 at score 0.45, and at 0.35 in
//! loop mode. Every other feature starts at weight zero.
//!
//! That matters more than the model does. A brain with no injection history
//! behaves precisely as it did before this module existed, so nothing changes
//! on upgrade; training can only move the boundary once there is evidence
//! about how injections actually landed. There is no cold-start regression to
//! trade against the eventual gain.
//!
//! ## Where the labels come from
//!
//! Each proactive injection writes a `proactive.injected` event carrying its
//! feature vector and the memory id. An injection is **positive** when that
//! memory was cited in the same session, and **negative** otherwise: the agent
//! was handed the memory and did not use it, which is the definition of an
//! interruption that was not worth it.
//!
//! This is Free-tier: gradient descent over a handful of floats, no model call.

use kimetsu_core::KimetsuResult;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

/// Number of features. Fixed so a persisted weight vector can be validated on
/// load — a policy file from a future version with more features is rejected
/// rather than silently misread.
pub const FEATURE_COUNT: usize = 7;

/// The old fixed score threshold, and the loop-mode threshold. The prior's
/// decision boundary is pinned to these so an untrained brain is unchanged.
pub const LEGACY_MIN_SCORE: f32 = 0.45;
pub const LEGACY_LOOP_MIN_SCORE: f32 = 0.35;

/// The abstain floor retrieval uses on the proactive path, below which a
/// candidate is not even offered to the policy.
///
/// Deliberately well under both legacy thresholds: the policy is supposed to
/// make the call, and a hard floor at the old threshold would make a trained
/// policy unable to ever speak sooner than the rule it replaced. It exists only
/// so obvious noise never reaches the decision.
pub const POLICY_RECALL_FLOOR: f32 = 0.20;

/// Minimum labelled examples before a fitted policy is allowed to replace the
/// prior. Below this, the fit is noise: a handful of injections cannot say
/// anything about a decision boundary, and a policy fitted on five examples
/// would swing wildly with each new one.
pub const MIN_TRAINING_EXAMPLES: usize = 40;

/// What the hook knows at the moment it decides whether to speak.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Features {
    /// Composite broker score of the best candidate capsule, in `[0, 1]`.
    pub score: f32,
    /// 1.0 when the agent is visibly repeating a failing command. A stuck
    /// agent should be interrupted sooner — this is the signal the old rule
    /// expressed by dropping the threshold.
    pub loop_mode: f32,
    /// 1.0 when the capsule is a `failure_pattern` — the kind most likely to
    /// prevent a wasted attempt rather than merely inform one.
    pub is_failure_pattern: f32,
    /// 1.0 when nothing has been injected yet this session, falling towards 0
    /// as the session accumulates injections. Encodes "the tenth interruption
    /// is worth less than the first".
    pub novelty: f32,
    /// How many times this exact command has already been observed failing,
    /// normalized: `min(count, 5) / 5`.
    pub repeat_count: f32,
    /// Time since the last injection, normalized against the refractory
    /// window: 0 immediately after one, 1 once well clear of it.
    pub recovery: f32,
    /// How strong the evidence was that something failed: 0 for a substring
    /// guess, 0.5 for a toolchain summary line, 1 for a real exit code.
    /// A guess should have to clear a higher bar than a fact.
    pub evidence: f32,
}

impl Features {
    /// Feature vector in weight order. Order is part of the persisted format.
    pub fn to_vec(self) -> [f32; FEATURE_COUNT] {
        [
            self.score,
            self.loop_mode,
            self.is_failure_pattern,
            self.novelty,
            self.repeat_count,
            self.recovery,
            self.evidence,
        ]
    }

    /// Human-readable names, aligned with [`Self::to_vec`]. Used by
    /// `brain policy --status` so the weights mean something on screen.
    pub const NAMES: [&'static str; FEATURE_COUNT] = [
        "score",
        "loop_mode",
        "is_failure_pattern",
        "novelty",
        "repeat_count",
        "recovery",
        "evidence",
    ];

    /// Reconstruct from a persisted vector (event payloads, training data).
    pub fn from_slice(v: &[f32]) -> Option<Self> {
        if v.len() != FEATURE_COUNT {
            return None;
        }
        Some(Self {
            score: v[0],
            loop_mode: v[1],
            is_failure_pattern: v[2],
            novelty: v[3],
            repeat_count: v[4],
            recovery: v[5],
            evidence: v[6],
        })
    }
}

/// A fitted (or prior) injection policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Policy {
    pub weights: Vec<f32>,
    pub bias: f32,
    /// How many labelled examples produced these weights. 0 = the hand-set
    /// prior, which is the legacy threshold rule.
    pub trained_on: usize,
    /// RFC 3339 timestamp of the fit, or `None` for the prior.
    pub trained_at: Option<String>,
}

impl Policy {
    /// The hand-set prior: exactly the pre-v3.0 fixed-threshold rule.
    ///
    /// `z = W_SCORE·score + W_LOOP·loop_mode + BIAS`, solved so that `z = 0`
    /// (p = 0.5) at score 0.45 normally and at 0.35 in loop mode. Every other
    /// weight is zero, so no other feature can move the decision until the
    /// policy has been trained on real outcomes.
    pub fn prior() -> Self {
        // W_SCORE·0.45 + BIAS = 0  and  W_SCORE·0.35 + W_LOOP + BIAS = 0
        //   => BIAS = -0.45·W_SCORE,  W_LOOP = 0.10·W_SCORE
        const W_SCORE: f32 = 20.0;
        let mut weights = vec![0.0; FEATURE_COUNT];
        weights[0] = W_SCORE;
        weights[1] = 0.10 * W_SCORE;
        Self {
            weights,
            bias: -LEGACY_MIN_SCORE * W_SCORE,
            trained_on: 0,
            trained_at: None,
        }
    }

    /// True when this is the untrained prior.
    pub fn is_prior(&self) -> bool {
        self.trained_on == 0
    }

    /// Reject a policy whose shape does not match this build — a file written
    /// by a version with a different feature set would otherwise be read as
    /// nonsense weights.
    pub fn is_valid(&self) -> bool {
        self.weights.len() == FEATURE_COUNT
            && self.bias.is_finite()
            && self.weights.iter().all(|w| w.is_finite())
    }

    /// Probability that injecting here is worth it.
    pub fn probability(&self, features: &Features) -> f32 {
        let z: f32 = self
            .weights
            .iter()
            .zip(features.to_vec())
            .map(|(w, x)| w * x)
            .sum::<f32>()
            + self.bias;
        sigmoid(z)
    }

    /// The decision. `p >= 0.5` speaks.
    pub fn should_inject(&self, features: &Features) -> bool {
        self.probability(features) >= 0.5
    }
}

impl Default for Policy {
    fn default() -> Self {
        Self::prior()
    }
}

fn sigmoid(z: f32) -> f32 {
    if z >= 0.0 {
        1.0 / (1.0 + (-z).exp())
    } else {
        // Numerically stable for very negative z: exp(z) rather than exp(-z).
        let e = z.exp();
        e / (1.0 + e)
    }
}

/// One labelled injection: what the hook saw, and whether it paid off.
#[derive(Debug, Clone, Copy)]
pub struct Example {
    pub features: Features,
    /// True when the injected memory was cited in the same session.
    pub useful: bool,
}

/// Fit a policy by gradient descent on the logistic loss.
///
/// Returns the prior unchanged when there is too little data
/// ([`MIN_TRAINING_EXAMPLES`]) or when every label is the same — a fit on
/// all-positive or all-negative examples has no boundary to find and would
/// drive the weights off to infinity.
///
/// Starts from the prior rather than from zero, so a small dataset nudges the
/// legacy rule rather than replacing it, and L2 pulls back towards the prior
/// for the same reason.
pub fn fit(examples: &[Example]) -> Policy {
    const EPOCHS: usize = 400;
    const LEARNING_RATE: f32 = 0.05;
    /// Pull back towards the prior, not towards zero: with little data the
    /// right answer is "close to what we already did".
    const L2: f32 = 0.01;

    if examples.len() < MIN_TRAINING_EXAMPLES {
        return Policy::prior();
    }
    let positives = examples.iter().filter(|e| e.useful).count();
    if positives == 0 || positives == examples.len() {
        return Policy::prior();
    }

    let prior = Policy::prior();
    let mut weights = prior.weights.clone();
    let mut bias = prior.bias;
    let n = examples.len() as f32;

    for _ in 0..EPOCHS {
        let mut grad_w = [0.0f32; FEATURE_COUNT];
        let mut grad_b = 0.0f32;
        for example in examples {
            let x = example.features.to_vec();
            let z: f32 = weights.iter().zip(x).map(|(w, xi)| w * xi).sum::<f32>() + bias;
            let error = sigmoid(z) - if example.useful { 1.0 } else { 0.0 };
            for (g, xi) in grad_w.iter_mut().zip(x) {
                *g += error * xi;
            }
            grad_b += error;
        }
        for (i, g) in grad_w.iter().enumerate() {
            // Regularise toward the prior weight, not toward zero.
            let pull = L2 * (weights[i] - prior.weights[i]);
            weights[i] -= LEARNING_RATE * (g / n + pull);
        }
        bias -= LEARNING_RATE * (grad_b / n + L2 * (bias - prior.bias));
    }

    let fitted = Policy {
        weights,
        bias,
        trained_on: examples.len(),
        trained_at: None,
    };
    // A fit that diverged (NaN from pathological data) is worse than no fit.
    if fitted.is_valid() { fitted } else { prior }
}

/// Fraction of `examples` the policy labels correctly. Reported by
/// `brain policy --status` so a user can see whether the fit beat the prior.
pub fn accuracy(policy: &Policy, examples: &[Example]) -> f32 {
    if examples.is_empty() {
        return 0.0;
    }
    let correct = examples
        .iter()
        .filter(|e| policy.should_inject(&e.features) == e.useful)
        .count();
    correct as f32 / examples.len() as f32
}

// ── Persistence + labelling ─────────────────────────────────────────────────

/// The event kind written on every proactive injection.
pub const INJECTED_EVENT: &str = "proactive.injected";

/// Where a fitted policy lives: beside `digest.md` in `.kimetsu/`, because it
/// is learned per-project state that should travel with the project and be
/// picked up by a backup of it.
pub fn policy_path(kimetsu_dir: &std::path::Path) -> std::path::PathBuf {
    kimetsu_dir.join("inject-policy.json")
}

/// Load the fitted policy, or the prior when there is none, the file is
/// unreadable, or it was written by a build with a different feature set.
///
/// Never fails: a broken policy file degrades to the legacy threshold rule,
/// which is the behaviour it replaced.
pub fn load(kimetsu_dir: &std::path::Path) -> Policy {
    std::fs::read_to_string(policy_path(kimetsu_dir))
        .ok()
        .and_then(|text| serde_json::from_str::<Policy>(&text).ok())
        .filter(Policy::is_valid)
        .unwrap_or_else(Policy::prior)
}

/// Persist a fitted policy (temp + rename, so a reader never sees a torn file).
pub fn save(kimetsu_dir: &std::path::Path, policy: &Policy) -> KimetsuResult<()> {
    let path = policy_path(kimetsu_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(policy)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Remove a fitted policy, returning the brain to the prior.
pub fn reset(kimetsu_dir: &std::path::Path) -> KimetsuResult<()> {
    let path = policy_path(kimetsu_dir);
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

/// Record a proactive injection decision, with the features that drove it.
///
/// The event *is* the training data — without it there is nothing to fit on.
/// Both outcomes are recorded: an injection that was suppressed is as
/// informative as one that fired, provided it is labelled as suppressed.
///
/// Written through the same telemetry path as `context.served`, so a
/// misconfigured `project.toml` cannot stop it, and best-effort by contract:
/// losing a training sample must never break the agent's turn.
pub fn record_injection(
    start: &std::path::Path,
    memory_id: &str,
    features: &Features,
    injected: bool,
) {
    let payload = serde_json::json!({
        "memory_id": memory_id,
        "features": features.to_vec().to_vec(),
        "injected": injected,
    });
    let _ = crate::feedback::log_telemetry_event(start, INJECTED_EVENT, payload);
}

/// Build the training set: every recorded injection, labelled by whether the
/// injected memory was cited after it was surfaced.
///
/// "Cited after" is the honest signal available: the agent was handed the
/// memory and then leaned on it. An injection that was never cited is one the
/// agent did not need — which is exactly the interruption worth learning not
/// to make.
pub fn collect_examples(conn: &Connection) -> KimetsuResult<Vec<Example>> {
    let mut stmt = conn.prepare(
        "SELECT e.payload_json, e.ts
         FROM events AS e
         WHERE e.kind = ?1
         ORDER BY e.ts",
    )?;
    let rows = stmt
        .query_map(rusqlite::params![INJECTED_EVENT], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut examples = Vec::new();
    for (payload_json, ts) in rows {
        let Ok(payload) = serde_json::from_str::<serde_json::Value>(&payload_json) else {
            continue;
        };
        // Only injections that actually happened are labelled: a suppressed
        // one has no outcome to observe.
        if payload.get("injected").and_then(serde_json::Value::as_bool) == Some(false) {
            continue;
        }
        let Some(memory_id) = payload.get("memory_id").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Some(values) = payload
            .get("features")
            .and_then(serde_json::Value::as_array)
        else {
            continue;
        };
        let floats: Vec<f32> = values
            .iter()
            .filter_map(|v| v.as_f64().map(|f| f as f32))
            .collect();
        let Some(features) = Features::from_slice(&floats) else {
            continue; // written by a build with a different feature set
        };

        let cited: bool = conn
            .query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM memory_citations
                     WHERE memory_id = ?1 AND cited_at >= ?2
                 )",
                rusqlite::params![memory_id, ts],
                |r| r.get(0),
            )
            .unwrap_or(false);
        examples.push(Example {
            features,
            useful: cited,
        });
    }
    Ok(examples)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn features(score: f32, loop_mode: bool) -> Features {
        Features {
            score,
            loop_mode: if loop_mode { 1.0 } else { 0.0 },
            is_failure_pattern: 0.0,
            novelty: 1.0,
            repeat_count: 0.0,
            recovery: 1.0,
            evidence: 0.5,
        }
    }

    // ── The prior must be the old rule, exactly ──────────────────────────

    /// The upgrade-safety property: a brain with no injection history must
    /// behave precisely as it did before this module existed.
    #[test]
    fn the_prior_reproduces_the_legacy_threshold() {
        let policy = Policy::prior();
        assert!(policy.is_prior());

        // Normal mode: the boundary is exactly LEGACY_MIN_SCORE.
        assert!(!policy.should_inject(&features(LEGACY_MIN_SCORE - 0.01, false)));
        assert!(policy.should_inject(&features(LEGACY_MIN_SCORE + 0.01, false)));
        assert!(
            (policy.probability(&features(LEGACY_MIN_SCORE, false)) - 0.5).abs() < 1e-4,
            "p must be exactly 0.5 at the legacy threshold"
        );

        // Loop mode: the boundary drops to LEGACY_LOOP_MIN_SCORE.
        assert!(!policy.should_inject(&features(LEGACY_LOOP_MIN_SCORE - 0.01, true)));
        assert!(policy.should_inject(&features(LEGACY_LOOP_MIN_SCORE + 0.01, true)));
        assert!(
            (policy.probability(&features(LEGACY_LOOP_MIN_SCORE, true)) - 0.5).abs() < 1e-4,
            "loop mode must cross at the legacy loop threshold"
        );
    }

    /// No feature other than score and loop mode may influence an untrained
    /// policy — otherwise the upgrade would silently change behaviour.
    #[test]
    fn the_prior_ignores_every_untrained_feature() {
        let policy = Policy::prior();
        let base = features(0.5, false);
        let loud = Features {
            is_failure_pattern: 1.0,
            novelty: 0.0,
            repeat_count: 1.0,
            recovery: 0.0,
            evidence: 1.0,
            ..base
        };
        assert!(
            (policy.probability(&base) - policy.probability(&loud)).abs() < 1e-6,
            "untrained features must have zero weight"
        );
    }

    // ── Training ─────────────────────────────────────────────────────────

    fn dataset(n: usize, boundary: f32) -> Vec<Example> {
        (0..n)
            .map(|i| {
                // Sweep score across [0, 1]; label by a boundary that differs
                // from the prior's, so a successful fit has to move.
                let score = i as f32 / n as f32;
                Example {
                    features: features(score, false),
                    useful: score >= boundary,
                }
            })
            .collect()
    }

    /// Too little data must leave the prior alone: a boundary fitted on a
    /// handful of injections would swing with every new one.
    #[test]
    fn a_small_dataset_does_not_move_the_policy() {
        let fitted = fit(&dataset(MIN_TRAINING_EXAMPLES - 1, 0.8));
        assert_eq!(fitted, Policy::prior());
        assert!(fitted.is_prior());
    }

    /// All-positive or all-negative data has no boundary to find; fitting it
    /// would drive the weights off to infinity.
    #[test]
    fn single_class_data_does_not_move_the_policy() {
        let all_useful: Vec<Example> = (0..100)
            .map(|_| Example {
                features: features(0.5, false),
                useful: true,
            })
            .collect();
        assert_eq!(fit(&all_useful), Policy::prior());

        let none_useful: Vec<Example> = all_useful
            .iter()
            .map(|e| Example {
                useful: false,
                ..*e
            })
            .collect();
        assert_eq!(fit(&none_useful), Policy::prior());
    }

    /// The point of the whole module: given evidence that injections below a
    /// higher bar were not used, the policy gets quieter.
    #[test]
    fn training_moves_the_boundary_towards_the_evidence() {
        // Injections only paid off above 0.8, well above the legacy 0.45.
        let examples = dataset(200, 0.8);
        let fitted = fit(&examples);

        assert!(
            !fitted.is_prior(),
            "a large clean dataset must produce a fit"
        );
        assert_eq!(fitted.trained_on, 200);
        assert!(fitted.is_valid());

        // The old rule would speak at 0.5; the trained policy should not.
        assert!(
            Policy::prior().should_inject(&features(0.5, false)),
            "sanity: the prior does speak here"
        );
        assert!(
            !fitted.should_inject(&features(0.5, false)),
            "the fit must learn to stay quiet where injections went unused"
        );
        assert!(
            fitted.should_inject(&features(0.95, false)),
            "and must still speak where they landed"
        );
        assert!(
            accuracy(&fitted, &examples) > accuracy(&Policy::prior(), &examples),
            "the fit must beat the prior on its own data"
        );
    }

    /// And the other direction: evidence that low-scoring injections were
    /// useful should make it talk more.
    #[test]
    fn training_can_also_make_the_policy_speak_sooner() {
        let examples = dataset(200, 0.2);
        let fitted = fit(&examples);
        assert!(!fitted.is_prior());
        assert!(
            fitted.should_inject(&features(0.3, false)),
            "injections that paid off below the legacy floor must raise the odds"
        );
    }

    // ── Persistence shape ────────────────────────────────────────────────

    #[test]
    fn a_policy_round_trips_through_json() {
        let fitted = fit(&dataset(200, 0.7));
        let json = serde_json::to_string(&fitted).expect("serialize");
        let back: Policy = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(fitted, back);
    }

    /// A policy file from a build with a different feature set must be
    /// rejected, not read as nonsense weights.
    #[test]
    fn a_wrong_shaped_policy_is_invalid() {
        let bad = Policy {
            weights: vec![1.0; FEATURE_COUNT + 2],
            bias: 0.0,
            trained_on: 100,
            trained_at: None,
        };
        assert!(!bad.is_valid());

        let nan = Policy {
            weights: vec![f32::NAN; FEATURE_COUNT],
            bias: 0.0,
            trained_on: 100,
            trained_at: None,
        };
        assert!(!nan.is_valid());
    }

    #[test]
    fn feature_names_line_up_with_the_vector() {
        assert_eq!(Features::NAMES.len(), FEATURE_COUNT);
        assert_eq!(features(0.5, false).to_vec().len(), FEATURE_COUNT);
        let round = Features::from_slice(&features(0.42, true).to_vec()).expect("round trip");
        assert_eq!(round, features(0.42, true));
        assert!(Features::from_slice(&[0.1, 0.2]).is_none());
    }

    #[test]
    fn sigmoid_is_stable_at_the_extremes() {
        assert!(sigmoid(0.0) == 0.5);
        assert!(sigmoid(200.0).is_finite() && sigmoid(200.0) > 0.999);
        assert!(sigmoid(-200.0).is_finite() && sigmoid(-200.0) < 0.001);
    }
}
