//! Running cost meter for a chat session.
//!
//! Wraps the model provider's reported `total_cost_usd` against a
//! caller-supplied budget. The REPL prints the running total on `/cost`
//! and refuses further model calls once the budget is exhausted.
//!
//! Kept deliberately tiny — this is bookkeeping, not policy. Future
//! per-tool / per-token breakdown lands here when we route the model
//! response's `usage` field through the meter directly.

/// Running cost tracker for one chat session.
#[derive(Debug, Clone)]
pub struct CostMeter {
    spent_usd: f32,
    budget_usd: f32,
    /// Highest single-turn cost observed. Useful for "the last turn
    /// cost $X" UX without dragging the full per-turn history into
    /// the public API yet.
    max_turn_usd: f32,
    turns: u32,
}

impl CostMeter {
    pub fn new(budget_usd: f32) -> Self {
        Self {
            spent_usd: 0.0,
            budget_usd: budget_usd.max(0.0),
            max_turn_usd: 0.0,
            turns: 0,
        }
    }

    /// Total spent so far.
    pub fn spent(&self) -> f32 {
        self.spent_usd
    }

    /// Configured budget (after `new`).
    pub fn budget(&self) -> f32 {
        self.budget_usd
    }

    /// Remaining budget = max(0, budget - spent).
    pub fn remaining(&self) -> f32 {
        (self.budget_usd - self.spent_usd).max(0.0)
    }

    /// Number of agent turns that have rolled cost through this meter.
    pub fn turns(&self) -> u32 {
        self.turns
    }

    /// Highest single-turn cost the meter has seen.
    pub fn max_turn(&self) -> f32 {
        self.max_turn_usd
    }

    /// Bump the meter by one turn worth of provider-reported cost.
    /// Returns whether we're still within budget AFTER the bump.
    pub fn record_turn(&mut self, turn_cost_usd: f32) -> bool {
        let turn_cost = turn_cost_usd.max(0.0);
        self.spent_usd += turn_cost;
        self.turns += 1;
        if turn_cost > self.max_turn_usd {
            self.max_turn_usd = turn_cost;
        }
        self.spent_usd < self.budget_usd
    }

    /// `true` once the meter has crossed its configured budget.
    pub fn over_budget(&self) -> bool {
        self.spent_usd >= self.budget_usd
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_meter_is_zeroed() {
        let m = CostMeter::new(10.0);
        assert_eq!(m.spent(), 0.0);
        assert_eq!(m.budget(), 10.0);
        assert_eq!(m.remaining(), 10.0);
        assert_eq!(m.turns(), 0);
        assert!(!m.over_budget());
    }

    #[test]
    fn record_turn_accumulates_and_tracks_max() {
        let mut m = CostMeter::new(10.0);
        assert!(m.record_turn(0.5));
        assert!(m.record_turn(2.0));
        assert!(m.record_turn(1.25));
        assert!((m.spent() - 3.75).abs() < 1e-4);
        assert!((m.max_turn() - 2.0).abs() < 1e-4);
        assert_eq!(m.turns(), 3);
        assert!((m.remaining() - 6.25).abs() < 1e-4);
    }

    #[test]
    fn record_turn_clamps_negative_input() {
        // Defensive: provider misreporting (negative or NaN-ish) shouldn't
        // refund the meter.
        let mut m = CostMeter::new(5.0);
        m.record_turn(-1.0);
        assert_eq!(m.spent(), 0.0);
    }

    #[test]
    fn budget_clamp_prevents_negative_budget() {
        let m = CostMeter::new(-3.0);
        assert_eq!(m.budget(), 0.0);
        assert_eq!(m.remaining(), 0.0);
        assert!(m.over_budget()); // 0 spent >= 0 budget => over
    }

    #[test]
    fn over_budget_triggers_when_crossed() {
        let mut m = CostMeter::new(2.0);
        assert!(m.record_turn(1.5));
        assert!(!m.over_budget());
        assert!(!m.record_turn(1.0)); // crosses budget
        assert!(m.over_budget());
        assert_eq!(m.remaining(), 0.0);
    }
}
