//! Conservative admission for unsolicited memory. Ranking scores are relative;
//! topical support must be checked against the original turn independently.
//! These are inspectable lexical heuristics, not calibrated probabilities.

use crate::context::{ContextBundle, ContextCapsule, content_tokens};

pub const CANDIDATE_POOL: usize = 6;
const MIN_CAPSULE_COVERAGE: f64 = 0.5;
const MIN_TURN_COVERAGE: f64 = 0.6;
const MAX_UNCERTAINTY: f64 = 0.35;

#[derive(Debug, PartialEq, Eq)]
pub enum AbstainReason {
    NoEvidence,
    NoIntent,
    InvalidScores,
    Mismatch,
    Diffuse,
}

#[derive(Debug)]
pub struct Assessment {
    pub reason: Option<AbstainReason>,
    pub eligible: Vec<usize>,
    pub coverage: f64,
    pub entropy: f64,
    pub uncertainty: f64,
}

impl Assessment {
    pub fn allows_injection(&self) -> bool {
        self.reason.is_none()
    }
}

/// Assess the bounded pool before the output cap. Entropy alone cannot tell
/// equally relevant memories from equally poor ones: weight it by unsupported
/// query content. A singleton still has to pass the independent mismatch gate.
pub fn assess(query: &str, capsules: &[ContextCapsule]) -> Assessment {
    let mut result = Assessment {
        reason: None,
        eligible: Vec::new(),
        coverage: 0.0,
        entropy: 0.0,
        uncertainty: 0.0,
    };
    let terms = content_tokens(query);
    if terms.is_empty() {
        result.reason = Some(AbstainReason::NoIntent);
        return result;
    }
    if capsules.is_empty() {
        result.reason = Some(AbstainReason::NoEvidence);
        return result;
    }
    if capsules
        .iter()
        .any(|c| !c.score.is_finite() || c.score < 0.0)
    {
        result.reason = Some(AbstainReason::InvalidScores);
        return result;
    }

    let mut covered = vec![false; terms.len()];
    let mut supported = Vec::new();
    for (idx, capsule) in capsules.iter().enumerate() {
        let mut text = capsule.summary.as_str();
        if let Some((prefix, body)) = text.split_once(" - ")
            && prefix.contains(':')
            && !prefix.contains(char::is_whitespace)
        {
            text = body;
        }
        // Dates/tags describe provenance; they must not supply topical support.
        while text.starts_with('[') {
            let Some((_, rest)) = text.split_once(']') else {
                break;
            };
            text = rest.trim_start();
        }
        let words = content_tokens(text);
        let matches: Vec<bool> = terms
            .iter()
            .map(|term| {
                words.iter().any(|word| {
                    word == term
                        // Light stemming leaves an optional trailing e in
                        // pairs such as migrate/migrated and cache/caches.
                        // Arbitrary prefixes (port/portal) are not evidence.
                        || word.strip_suffix('e') == Some(term.as_str())
                        || term.strip_suffix('e') == Some(word.as_str())
                })
            })
            .collect();
        let coverage = matches.iter().filter(|hit| **hit).count() as f64 / terms.len() as f64;
        if coverage >= MIN_CAPSULE_COVERAGE && capsule.score > 0.0 {
            result.eligible.push(idx);
            supported.push((capsule.score as f64, coverage));
            for (union, hit) in covered.iter_mut().zip(matches) {
                *union |= hit;
            }
        }
    }
    result.coverage = covered.iter().filter(|hit| **hit).count() as f64 / terms.len() as f64;
    if result.eligible.is_empty() {
        result.reason = Some(AbstainReason::Mismatch);
        return result;
    }
    let total: f64 = supported.iter().map(|(score, _)| score).sum();
    let mut mean_support = 0.0;
    for (score, coverage) in &supported {
        let probability = score / total;
        result.entropy -= probability * probability.ln();
        mean_support += probability * coverage;
    }
    if supported.len() > 1 {
        result.entropy = (result.entropy / (supported.len() as f64).ln()).clamp(0.0, 1.0);
    }
    result.uncertainty = result.entropy * (1.0 - mean_support);
    result.reason = if result.uncertainty > MAX_UNCERTAINTY {
        Some(AbstainReason::Diffuse)
    } else if result.coverage < MIN_TURN_COVERAGE {
        Some(AbstainReason::Mismatch)
    } else {
        None
    };
    result
}

/// Preserve rejected candidates for diagnostics, never as model-facing output.
pub fn filter_bundle(query: &str, bundle: &mut ContextBundle) -> Assessment {
    let assessment = assess(query, &bundle.capsules);
    for (idx, capsule) in std::mem::take(&mut bundle.capsules).into_iter().enumerate() {
        if !bundle.skipped && assessment.allows_injection() && assessment.eligible.contains(&idx) {
            bundle.capsules.push(capsule);
        } else {
            bundle.excluded.push(capsule);
        }
    }
    bundle.skipped |= bundle.capsules.is_empty();
    bundle.used_tokens = bundle.capsules.iter().map(|c| c.token_estimate).sum();
    assessment
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capsule(text: &str, score: f32) -> ContextCapsule {
        ContextCapsule::wire_minimal(text.into(), "memory".into(), score)
    }

    #[test]
    fn a_normalized_top_score_cannot_rescue_a_topic_mismatch() {
        let out = assess(
            "kubernetes pod eviction cargo",
            &[capsule("cargo fmt before committing", 1.0)],
        );
        assert_eq!(out.reason, Some(AbstainReason::Mismatch));
    }

    #[test]
    fn flat_partial_results_have_high_uncertainty() {
        let out = assess(
            "cache timeout retry queue",
            &[
                capsule("cache timeout settings", 0.9),
                capsule("retry queue settings", 0.9),
            ],
        );
        assert_eq!(out.reason, Some(AbstainReason::Diffuse));
        assert!(out.entropy > 0.99 && out.coverage > 0.99);
    }

    #[test]
    fn flat_aligned_results_are_not_ambiguous() {
        let out = assess(
            "cache timeout",
            &[
                capsule("cache timeout uses retries", 0.9),
                capsule("cache timeout uses circuit breakers", 0.9),
            ],
        );
        assert!(out.allows_injection());
        assert!(out.entropy > 0.99 && out.uncertainty < 0.01);
    }

    #[test]
    fn a_clear_match_survives_an_unrelated_candidate() {
        let out = assess(
            "cache timeout",
            &[
                capsule("cargo release procedure", 1.0),
                capsule("cache timeout uses retries", 0.8),
            ],
        );
        assert!(out.allows_injection());
        assert_eq!(out.eligible, vec![1]);
    }

    #[test]
    fn no_intent_or_evidence_or_invalid_scores_abstains() {
        assert_eq!(
            assess("please do that for me", &[capsule("anything", 1.0)]).reason,
            Some(AbstainReason::NoIntent)
        );
        assert_eq!(
            assess("cache timeout", &[]).reason,
            Some(AbstainReason::NoEvidence)
        );
        for score in [f32::NAN, f32::INFINITY, -0.5, 0.0] {
            assert!(
                !assess("cache timeout", &[capsule("cache timeout", score)]).allows_injection()
            );
        }
    }

    #[test]
    fn metadata_and_embedded_substrings_do_not_supply_support() {
        assert!(
            !assess("port", &[capsule("The portal theme uses dark colors", 1.0)])
                .allows_injection()
        );
        for text in [
            "project:convention - [2026-01-01] [tags: cache timeout] Cargo release procedure",
            "a showcase of timeouts",
        ] {
            assert!(!assess("cache timeout", &[capsule(text, 1.0)]).allows_injection());
        }
    }
}
