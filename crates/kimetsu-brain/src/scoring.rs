//! How the brain keeps score — every learning-loop constant in one place.
//!
//! These numbers govern promotion, decay, penalties, and ranking bias. They
//! are deliberately centralized (v2.5.1) so tuning or auditing the loop is a
//! one-page read instead of a grep across `projector.rs`, `context.rs`, and
//! `project.rs`.
//!
//! | constant | value | applied in |
//! |---|---|---|
//! | [`CITED_DELTA`] | ±1.0 | projector, on run terminal events |
//! | [`PASSENGER_DELTA`] | ±0.1 | projector, on run terminal events |
//! | [`FAILURE_PENALTY_CITES_DIVISOR`] | 3.0 | projector, cited-in-failed-run scaling |
//! | [`CONF_ALPHA`] | 0.05 | projector, confidence calibration |
//! | [`USEFULNESS_BOOST_CAP`] | 0.10 | broker ranking (context) |
//! | multiplier envelope | [0.5, 1.5], full at 3 uses | broker ranking (context) |
//!
//! The signal flow: citations move `usefulness_score` by the deltas below;
//! retrieval turns the score into a bounded multiplier whose GAIN is capped
//! (a proven memory can win ties, never rescue irrelevance); decay pulls old
//! signal toward neutral; pruning uses the raw ratio with its own floor
//! (see `maintenance::PruneOptions`, default `min_uses = 3`, `ratio <= -0.2`).

/// Strong usefulness delta for memories the model explicitly cited:
/// `+1.0` when the run finishes, negative (scaled, see
/// [`FAILURE_PENALTY_CITES_DIVISOR`]) when it fails for a non-Gate reason.
pub(crate) const CITED_DELTA: f64 = 1.0;

/// Weak usefulness delta for silent passengers — retrieved into context but
/// never cited. Deliberately small: without citation evidence we should not
/// claim the memory helped (or hurt) much. A pure silent passenger's ratio
/// bottoms out at `-0.1`, which can never cross the prune floor of `-0.2` —
/// only repeated cited failures can make a memory prunable.
pub(crate) const PASSENGER_DELTA: f64 = 0.1;

/// v2.5.1: cited-in-failure penalty scaling. The effective penalty is
/// `-CITED_DELTA / (1 + prior_citations / DIVISOR)`, so a memory with 3
/// prior citations takes -0.5 and one with 9 takes -0.25. A proven memory
/// absorbs unlucky flaky runs; an unproven one takes the full hit.
pub(crate) const FAILURE_PENALTY_CITES_DIVISOR: f64 = 3.0;

/// Confidence-calibration smoothing (Bayesian-ish): each cited terminal
/// outcome moves confidence 5% toward its target (1.0 success, 0.0 failure),
/// clamped to [0.1, 0.99].
pub(crate) const CONF_ALPHA: f64 = 0.05;

/// v2.5.1: absolute cap on the usefulness boost's relevance GAIN.
///
/// The multiplier used to apply multiplicatively (`raw * 1.5` at max boost),
/// which let a weakly relevant but often-cited memory outrank a strongly
/// relevant uncited one — with hundreds of boosted memories, retrieval
/// flooded with cited-but-irrelevant capsules (measured in the LoCoMo k=5
/// learning runs). Capping the gain makes usefulness a bounded prior: it
/// reorders candidates within a relevance band but can never rescue an
/// irrelevant memory past a genuine match. Penalties (multiplier < 1.0)
/// stay multiplicative — suppressing net-negative memories below their raw
/// relevance is safe and desirable.
pub(crate) const USEFULNESS_BOOST_CAP: f32 = 0.10;

/// Usefulness-multiplier envelope: the ratio `usefulness_score / use_count`
/// maps onto `[MULTIPLIER_MIN, MULTIPLIER_MAX]`, blended linearly toward
/// full strength as `use_count` climbs to [`FULL_CONFIDENCE_USES`] (so a
/// memory with 1-2 uses gets partial credit instead of a hard cutoff).
pub(crate) const MULTIPLIER_MIN: f32 = 0.5;
pub(crate) const MULTIPLIER_MAX: f32 = 1.5;
pub(crate) const FULL_CONFIDENCE_USES: u32 = 3;

// ── Consolidation v1 (v2.5.2): co-citation stapling + query routing ────────
// Literature-hardened bounds; see docs/superpowers/consolidation-v1-prior-art
// (local). Design lineage: staple trigger from Small 1973 co-citation
// frequency + SOAR success-gated chunking; routing bounds from ACT-R's fixed
// source-activation budget + power-law decay; propensity from Joachims 2017.

/// A pair of memories must be cited TOGETHER at least this many times before
/// a staple is created (never staple on a single lucky co-cite).
pub(crate) const STAPLE_MIN_CO_CITES: u32 = 2;

/// A query->memory route must have at least this many successful citations
/// before it fires at retrieval time (min-support).
pub(crate) const ROUTE_MIN_CITES: u32 = 2;

/// Minimum cosine similarity between the incoming query and a stored
/// successful query for its routes to fire.
pub(crate) const ROUTE_QUERY_SIM_FLOOR: f32 = 0.80;

/// Absolute cap on the routing boost's relevance GAIN per candidate — the
/// same bounded-prior discipline as [`USEFULNESS_BOOST_CAP`]: routing may
/// reorder within a relevance band, never rescue an irrelevant memory.
pub(crate) const ROUTING_BOOST_CAP: f32 = 0.10;

/// Fixed total routing budget per retrieval (ACT-R source activation):
/// the sum of all routing gains applied to one query's candidates never
/// exceeds this, however many routes match.
pub(crate) const ROUTING_BUDGET: f32 = 0.25;

/// Power-law decay exponent on route strength by age of last citation
/// (ACT-R base-level d ~ 0.5): strength *= (1 + age_days)^-0.5.
pub(crate) const ROUTE_DECAY_EXPONENT: f32 = 0.5;
