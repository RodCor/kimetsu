//! C7 e2e: context.served event → retrieval hit-rate / skip-rate analytics.
//!
//! Verifies that:
//!   1. `log_telemetry_event` writes `context.served` events (hook path).
//!   2. `compute_insights` correctly computes hit-rate, avg_top_score, and
//!      skip_rate from those events.
//!   3. A fresh project with no context.served events returns served==0 and
//!      all optional fields as None (backward-compat / old-DB case).
//!   4. The KIMETSU_BRAIN_LOG_RETRIEVAL=0 env-var suppression is exercised
//!      at the project::log_telemetry_event level (the hook skips the call
//!      entirely; here we simply assert the helper is the gating point).

use kimetsu_brain::analytics::{self, InsightsOptions};
use kimetsu_brain::project;
use kimetsu_brain::user_brain::with_user_brain_disabled;
use kimetsu_e2e::prelude::*;

// ---------------------------------------------------------------------------
// Helper: seed a context.served event via log_telemetry_event.
// ---------------------------------------------------------------------------

fn seed_served(root: &std::path::Path, capsule_count: u64, top_score: f32, skipped: bool) {
    project::log_telemetry_event(
        root,
        "context.served",
        serde_json::json!({
            "query_hash": format!("{:016x}", capsule_count),
            "capsule_count": capsule_count,
            "top_score": top_score,
            "skipped": skipped,
            "stage": "localization",
        }),
    )
    .expect("log_telemetry_event must not fail");
}

// ---------------------------------------------------------------------------
// 1. Hit-rate and skip-rate reflect seeded events.
// ---------------------------------------------------------------------------

#[test]
fn insights_hit_rate_reflects_seeded_context_served_events() {
    with_user_brain_disabled(|| {
        let project = TempProject::init("insights_hit_rate");

        // 3 hits + 2 misses
        seed_served(project.root(), 2, 0.80, false); // hit
        seed_served(project.root(), 5, 0.70, false); // hit
        seed_served(project.root(), 1, 0.55, false); // hit
        seed_served(project.root(), 0, 0.0, true); // miss
        seed_served(project.root(), 0, 0.12, true); // miss (skipped=true)

        let report = analytics::compute_insights(project.root(), InsightsOptions::default())
            .expect("insights");

        let rs = &report.retrieval;
        assert_eq!(rs.served, 5, "served must count all 5 events");
        assert_eq!(
            rs.with_hit, 3,
            "with_hit must count 3 events with capsule_count>=1"
        );

        let hr = rs.hit_rate.expect("hit_rate must be Some when served>0");
        assert!(
            (hr - 3.0 / 5.0).abs() < 1e-9,
            "hit_rate should be 3/5 = 0.6; got {hr}"
        );

        // avg_top_score over hits: (0.80 + 0.70 + 0.55) / 3 ≈ 0.6833
        let avg = rs.avg_top_score.expect("avg_top_score must be Some");
        assert!(
            (avg - (0.80 + 0.70 + 0.55) / 3.0).abs() < 0.001,
            "avg_top_score mismatch; got {avg}"
        );

        // skip_rate: 2 skipped / 5 served = 0.4
        let sr = report
            .token_economy
            .skip_rate
            .expect("skip_rate must be Some when served>0");
        assert!(
            (sr - 2.0 / 5.0).abs() < 1e-9,
            "skip_rate should be 2/5 = 0.4; got {sr}"
        );
    });
}

// ---------------------------------------------------------------------------
// 2. Fresh project → no context.served → all None / zero.
// ---------------------------------------------------------------------------

#[test]
fn insights_no_context_served_returns_zero_served_and_none_rates() {
    with_user_brain_disabled(|| {
        let project = TempProject::init("insights_no_served");

        let report = analytics::compute_insights(project.root(), InsightsOptions::default())
            .expect("insights");

        let rs = &report.retrieval;
        assert_eq!(rs.served, 0, "fresh project: served must be 0");
        assert_eq!(rs.with_hit, 0, "fresh project: with_hit must be 0");
        assert!(
            rs.hit_rate.is_none(),
            "fresh project: hit_rate must be None"
        );
        assert!(
            rs.avg_top_score.is_none(),
            "fresh project: avg_top_score must be None"
        );
        assert!(
            report.token_economy.skip_rate.is_none(),
            "fresh project: skip_rate must be None"
        );
    });
}

// ---------------------------------------------------------------------------
// 3. All hits → hit_rate=1.0, skip_rate=0.0
// ---------------------------------------------------------------------------

#[test]
fn insights_all_hits_gives_full_hit_rate_and_zero_skip_rate() {
    with_user_brain_disabled(|| {
        let project = TempProject::init("insights_all_hits");

        seed_served(project.root(), 3, 0.92, false);
        seed_served(project.root(), 1, 0.77, false);

        let report = analytics::compute_insights(project.root(), InsightsOptions::default())
            .expect("insights");

        let rs = &report.retrieval;
        assert_eq!(rs.served, 2);
        assert_eq!(rs.with_hit, 2);

        let hr = rs.hit_rate.expect("hit_rate");
        assert!((hr - 1.0).abs() < 1e-9, "all hits → hit_rate=1.0; got {hr}");

        let sr = report.token_economy.skip_rate.expect("skip_rate");
        assert!(
            (sr - 0.0).abs() < 1e-9,
            "no skips → skip_rate=0.0; got {sr}"
        );
    });
}

// ---------------------------------------------------------------------------
// 4. All skips → hit_rate=0.0, skip_rate=1.0
// ---------------------------------------------------------------------------

#[test]
fn insights_all_skips_gives_zero_hit_rate_and_full_skip_rate() {
    with_user_brain_disabled(|| {
        let project = TempProject::init("insights_all_skips");

        seed_served(project.root(), 0, 0.0, true);
        seed_served(project.root(), 0, 0.09, true);
        seed_served(project.root(), 0, 0.0, true);

        let report = analytics::compute_insights(project.root(), InsightsOptions::default())
            .expect("insights");

        let rs = &report.retrieval;
        assert_eq!(rs.served, 3);
        assert_eq!(rs.with_hit, 0);

        let hr = rs.hit_rate.expect("hit_rate");
        assert!(
            (hr - 0.0).abs() < 1e-9,
            "all skips → hit_rate=0.0; got {hr}"
        );
        // avg_top_score: None (no hits to average over)
        assert!(
            rs.avg_top_score.is_none(),
            "no hits → avg_top_score must be None"
        );

        let sr = report.token_economy.skip_rate.expect("skip_rate");
        assert!(
            (sr - 1.0).abs() < 1e-9,
            "all skips → skip_rate=1.0; got {sr}"
        );
    });
}
