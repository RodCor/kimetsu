//! Lightweight, dependency-free request metrics. Aggregate counters by outcome
//! only (no per-repo labels — `/metrics` is unauthenticated, so it must not leak
//! repo ids). Rendered in Prometheus text format.

use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    Unauthorized,
    Forbidden,
    RateLimited,
    BadRequest,
    Error,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Unauthorized => "unauthorized",
            Outcome::Forbidden => "forbidden",
            Outcome::RateLimited => "rate_limited",
            Outcome::BadRequest => "bad_request",
            Outcome::Error => "error",
        }
    }
}

#[derive(Default)]
pub struct Metrics {
    ok: AtomicU64,
    unauthorized: AtomicU64,
    forbidden: AtomicU64,
    rate_limited: AtomicU64,
    bad_request: AtomicU64,
    error: AtomicU64,
    /// v3.0 #3 Slice C: successful memory-write tool calls (aggregate, no repo
    /// label — `/metrics` is unauthenticated and must not leak repo ids).
    writes: AtomicU64,
}

impl Metrics {
    /// Count one successful write-tool call.
    pub fn record_write(&self) {
        self.writes.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record(&self, outcome: Outcome) {
        let counter = match outcome {
            Outcome::Ok => &self.ok,
            Outcome::Unauthorized => &self.unauthorized,
            Outcome::Forbidden => &self.forbidden,
            Outcome::RateLimited => &self.rate_limited,
            Outcome::BadRequest => &self.bad_request,
            Outcome::Error => &self.error,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    fn get(&self, outcome: Outcome) -> u64 {
        let counter = match outcome {
            Outcome::Ok => &self.ok,
            Outcome::Unauthorized => &self.unauthorized,
            Outcome::Forbidden => &self.forbidden,
            Outcome::RateLimited => &self.rate_limited,
            Outcome::BadRequest => &self.bad_request,
            Outcome::Error => &self.error,
        };
        counter.load(Ordering::Relaxed)
    }

    pub fn render_prometheus(&self) -> String {
        let mut out = String::new();
        out.push_str("# HELP kimetsu_remote_requests_total Total MCP requests by outcome.\n");
        out.push_str("# TYPE kimetsu_remote_requests_total counter\n");
        for o in [
            Outcome::Ok,
            Outcome::Unauthorized,
            Outcome::Forbidden,
            Outcome::RateLimited,
            Outcome::BadRequest,
            Outcome::Error,
        ] {
            out.push_str(&format!(
                "kimetsu_remote_requests_total{{outcome=\"{}\"}} {}\n",
                o.as_str(),
                self.get(o)
            ));
        }
        out.push_str(
            "# HELP kimetsu_remote_memory_writes_total Successful memory-write tool calls.\n",
        );
        out.push_str("# TYPE kimetsu_remote_memory_writes_total counter\n");
        out.push_str(&format!(
            "kimetsu_remote_memory_writes_total {}\n",
            self.writes.load(Ordering::Relaxed)
        ));
        out
    }
}

/// v3.0 #3 Slice C: tool names that MUTATE the brain (used to count writes +,
/// in rpc, to know a request did a write). Reads are excluded.
pub fn is_write_tool(name: &str) -> bool {
    matches!(
        name,
        "kimetsu_brain_memory_add"
            | "kimetsu_brain_record"
            | "kimetsu_brain_memory_accept"
            | "kimetsu_brain_memory_reject"
            | "kimetsu_brain_memory_invalidate"
            | "kimetsu_brain_cite"
            | "kimetsu_benchmark_record_outcome"
            | "kimetsu_brain_conflict_resolve"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_and_renders() {
        let m = Metrics::default();
        m.record(Outcome::Ok);
        m.record(Outcome::Ok);
        m.record(Outcome::RateLimited);
        let text = m.render_prometheus();
        assert!(text.contains("kimetsu_remote_requests_total{outcome=\"ok\"} 2"));
        assert!(text.contains("kimetsu_remote_requests_total{outcome=\"rate_limited\"} 1"));
        assert!(text.contains("kimetsu_remote_requests_total{outcome=\"error\"} 0"));
        // No repo labels leak.
        assert!(!text.contains("repo"));
    }
}
