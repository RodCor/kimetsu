//! ROI ledger — v1.5 story "kimetsu pays for itself".
//!
//! Estimates the token savings that Kimetsu's memory brain delivered
//! by surfacing relevant knowledge before a coding session, so the model
//! didn't have to (re-)discover it through expensive exploration.
//!
//! # Design philosophy: deliberate under-claiming
//!
//! Every constant in [`SAVED_TOKENS_PER_CITATION`] is a *conservative*
//! lower-bound estimate of the avoided exploration cost for that memory
//! kind.  We never inflate the numbers: the goal is that a user who sees
//! a "net positive" result can trust it.  The methodology document at
//! `docs/ROI-METHODOLOGY.md` explains the calibration approach and the
//! Terminal-Bench sanity anchor.

use kimetsu_core::{KimetsuResult, memory::MemoryKind};
use rusqlite::{OptionalExtension, params};
use serde::Serialize;

// ---------------------------------------------------------------------------
// Per-kind calibrated constants
// ---------------------------------------------------------------------------

/// Conservative lower-bound estimate of tokens saved per citation, by memory
/// kind.  These are deliberate *under*-estimates of the exploration cost the
/// model would have incurred without the brain context.
///
/// Calibration methodology (see `docs/ROI-METHODOLOGY.md` for details):
/// - `failure_pattern`: avoids the "try → fail → diagnose → fix" loop.
///   Typical loop: ~3 tool calls × ~500 tokens/call = ~1 500 tokens.
/// - `command`: avoids a web/docs lookup or `--help` trial.  ~1–2 tool
///   calls = ~400 tokens.
/// - `convention`: avoids a code-search to find the project pattern.
///   ~1–2 searches = ~300 tokens.
/// - `fact`: avoids asking the user or searching docs. ~1 exchange = ~500 t.
/// - `preference`: avoids one clarifying question. ~1 exchange = ~200 t.
///
/// These constants are the source of truth for the methodology doc.
pub const SAVED_TOKENS_PER_CITATION: &[(MemoryKind, u32)] = &[
    (MemoryKind::FailurePattern, 1500),
    (MemoryKind::Command, 400),
    (MemoryKind::Convention, 300),
    (MemoryKind::Fact, 500),
    (MemoryKind::Preference, 200),
];

// ---------------------------------------------------------------------------
// Built-in price table (input tokens, conservative single number per family)
// ---------------------------------------------------------------------------

/// Conservative input-token price in USD per million tokens ($/MTok) for
/// known model families.  These are approximate and marked as such in the
/// `--json` output (`usd` field carries them as estimates).
///
/// Matched against the project's `[model] model` config value by prefix
/// (longest match wins).  Unknown model → `usd: None` unless
/// `[model] price_per_mtok` is set in `project.toml`.
///
/// Last updated: 2026-06, approximate retail/API-key pricing.
const BUILTIN_PRICE_TABLE: &[(&str, f64)] = &[
    // Anthropic Claude 4 family (Opus > Sonnet > Haiku)
    ("claude-opus-4", 15.00),
    ("claude-sonnet-4", 3.00),
    ("claude-haiku-4", 0.80),
    // Anthropic Claude 3 family
    ("claude-3-opus", 15.00),
    ("claude-3-5-sonnet", 3.00),
    ("claude-3-5-haiku", 0.80),
    ("claude-3-sonnet", 3.00),
    ("claude-3-haiku", 0.25),
    // Anthropic Bedrock cross-region routing prefixes
    ("us.anthropic.claude-opus-4", 15.00),
    ("us.anthropic.claude-sonnet-4", 3.00),
    ("us.anthropic.claude-haiku-4", 0.80),
    // OpenAI gpt-5 family
    ("gpt-5", 2.00),
    ("gpt-4o", 2.50),
    ("gpt-4-turbo", 10.00),
    ("gpt-4", 30.00),
];

/// Resolve a $/MTok price for the given model id.
///
/// Precedence: `price_override` (from `[model] price_per_mtok`) >
/// longest-prefix match in [`BUILTIN_PRICE_TABLE`].
/// Returns `None` when neither applies.
pub fn resolve_price_per_mtok(model: &str, price_override: Option<f64>) -> Option<f64> {
    if let Some(p) = price_override {
        return Some(p);
    }
    let model_lower = model.to_lowercase();
    // Longest prefix match wins.
    let mut best: Option<(&str, f64)> = None;
    for (prefix, price) in BUILTIN_PRICE_TABLE {
        if model_lower.starts_with(prefix) && best.is_none_or(|(b, _)| prefix.len() > b.len()) {
            best = Some((prefix, *price));
        }
    }
    best.map(|(_, p)| p)
}

// ---------------------------------------------------------------------------
// Pure savings estimator
// ---------------------------------------------------------------------------

/// Estimate total tokens saved from a citation summary.
///
/// `citations` is a slice of `(kind, count)` pairs — how many times each
/// memory kind was cited in the window.  The function is intentionally pure
/// (no I/O) so it can be unit-tested without a DB.
///
/// The result is a conservative lower-bound: if a kind has no entry in
/// [`SAVED_TOKENS_PER_CITATION`] it contributes 0 (fail-safe).
pub fn estimate_savings(citations: &[(MemoryKind, u32)]) -> u64 {
    citations
        .iter()
        .map(|(kind, count)| {
            let per = SAVED_TOKENS_PER_CITATION
                .iter()
                .find(|(k, _)| k == kind)
                .map(|(_, v)| *v as u64)
                .unwrap_or(0);
            per * (*count as u64)
        })
        .sum()
}

// ---------------------------------------------------------------------------
// Report types
// ---------------------------------------------------------------------------

/// USD sub-report — only present when a price is resolvable.
#[derive(Debug, Clone, Serialize)]
pub struct RoiUsd {
    /// Estimated USD value of the tokens saved by the brain.
    pub saved: f64,
    /// Estimated USD cost of the brain overhead (injected tokens consumed
    /// at inference time).  Uses the same $/MTok price as `saved`.
    pub spent: f64,
    /// `saved − spent`.  Can be negative when overhead exceeds the
    /// estimated savings.
    pub net: f64,
}

/// Full ROI report for a time window.
#[derive(Debug, Clone, Serialize)]
pub struct RoiReport {
    /// Window length in days, or `None` for "all time".
    pub window_days: Option<u32>,
    /// Total tokens injected by the brain (sum of `used_tokens` from
    /// `context.injected` events in the window).
    pub injected_tokens: u64,
    /// Number of `context.served` events in the window.
    pub served_events: u64,
    /// Total citation count (rows in `memory_citations` for runs in the
    /// window).
    pub citations: u64,
    /// Estimated tokens saved (conservative lower-bound).
    pub estimated_saved_tokens: u64,
    /// `estimated_saved_tokens − injected_tokens`.  Can be negative.
    pub net_tokens: i64,
    /// USD sub-report; `None` when price is unknown.
    pub usd: Option<RoiUsd>,
}

// ---------------------------------------------------------------------------
// Window parsing
// ---------------------------------------------------------------------------

/// Recognised window strings → days.  Mirrors the CLI arg values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoiWindow {
    Days(u32),
    All,
}

impl RoiWindow {
    /// Parse "7d", "30d", or "all".
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_lowercase().as_str() {
            "all" => Ok(Self::All),
            other => {
                let digits = other.trim_end_matches('d');
                digits
                    .parse::<u32>()
                    .map(Self::Days)
                    .map_err(|_| format!("invalid window '{s}'; expected '7d', '30d', or 'all'"))
            }
        }
    }

    pub fn days(self) -> Option<u32> {
        match self {
            Self::Days(d) => Some(d),
            Self::All => None,
        }
    }
}

impl Default for RoiWindow {
    fn default() -> Self {
        Self::Days(30)
    }
}

// ---------------------------------------------------------------------------
// DB-backed report
// ---------------------------------------------------------------------------

/// Compute a full ROI report from the project brain.
///
/// `window` controls how far back to look.  `price_per_mtok_override`
/// comes from `[model] price_per_mtok` in `project.toml`; `model_name` is
/// `[model] model`.
pub fn roi_report(
    conn: &rusqlite::Connection,
    window: RoiWindow,
    model_name: &str,
    price_per_mtok_override: Option<f64>,
) -> KimetsuResult<RoiReport> {
    // Compute window boundary timestamp (ISO-8601 string).
    let window_since: Option<String> = match window {
        RoiWindow::All => None,
        RoiWindow::Days(days) => {
            // Compute `now − days` as an ISO string using the `time` crate
            // that is already a workspace dep.
            let secs = days as i64 * 86_400;
            let now = time::OffsetDateTime::now_utc();
            let cutoff = now - time::Duration::seconds(secs);
            let fmt = time::format_description::well_known::Rfc3339;
            Some(cutoff.format(&fmt).unwrap_or_default())
        }
    };

    // --- served_events (context.served) ---
    let served_events: u64 = match &window_since {
        Some(ts) => conn.query_row(
            "SELECT COUNT(*) FROM events WHERE kind = 'context.served' AND ts >= ?1",
            params![ts],
            |r| r.get(0),
        )?,
        None => conn.query_row(
            "SELECT COUNT(*) FROM events WHERE kind = 'context.served'",
            [],
            |r| r.get(0),
        )?,
    };

    // --- injected_tokens (sum of used_tokens across context.injected events) ---
    let injected_tokens: u64 = {
        let payloads: Vec<String> = match &window_since {
            Some(ts) => {
                let mut stmt = conn.prepare(
                    "SELECT payload_json FROM events WHERE kind = 'context.injected' AND ts >= ?1",
                )?;
                let rows = stmt.query_map(params![ts], |r| r.get::<_, String>(0))?;
                rows.collect::<Result<Vec<_>, _>>()?
            }
            None => {
                let mut stmt = conn
                    .prepare("SELECT payload_json FROM events WHERE kind = 'context.injected'")?;
                let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
                rows.collect::<Result<Vec<_>, _>>()?
            }
        };
        let mut sum: u64 = 0;
        for p in &payloads {
            let v: serde_json::Value = serde_json::from_str(p)?;
            if let Some(t) = v.get("used_tokens").and_then(|x| x.as_u64()) {
                sum += t;
            }
        }
        sum
    };

    // --- citations per kind ---
    // We join memory_citations → memories to get the kind for each citation.
    // Window applied via run_id → runs.started_at for pipeline runs, or
    // via the event ts for hook-originated citations.
    //
    // Strategy: collect citation rows that belong to runs in the window
    // (started_at >= window_since) OR, for the sentinel hook run_id
    // (all-zeroes), filter by the cited_at timestamp.
    let citations_by_kind: Vec<(MemoryKind, u32)> = {
        // Collect all (memory_id, count) pairs from citations in window.
        struct Row {
            memory_id: String,
            count: u32,
        }

        let rows: Vec<Row> = match &window_since {
            Some(ts) => {
                let mut stmt = conn.prepare(
                    "SELECT mc.memory_id, COUNT(*) \
                     FROM memory_citations mc \
                     LEFT JOIN runs r ON mc.run_id = r.run_id \
                     WHERE r.started_at >= ?1 \
                        OR (r.run_id IS NULL AND mc.cited_at >= ?1) \
                     GROUP BY mc.memory_id",
                )?;
                let rows = stmt.query_map(params![ts], |r| {
                    Ok(Row {
                        memory_id: r.get(0)?,
                        count: r.get(1)?,
                    })
                })?;
                rows.collect::<Result<Vec<_>, _>>()?
            }
            None => {
                let mut stmt = conn.prepare(
                    "SELECT memory_id, COUNT(*) FROM memory_citations GROUP BY memory_id",
                )?;
                let rows = stmt.query_map([], |r| {
                    Ok(Row {
                        memory_id: r.get(0)?,
                        count: r.get(1)?,
                    })
                })?;
                rows.collect::<Result<Vec<_>, _>>()?
            }
        };

        // For each memory_id, resolve its kind from the memories table.
        // Unknown / invalidated memories default to Fact (conservative).
        let mut by_kind: std::collections::HashMap<MemoryKind, u32> =
            std::collections::HashMap::new();
        for row in &rows {
            let kind: Option<String> = conn
                .query_row(
                    "SELECT kind FROM memories WHERE memory_id = ?1",
                    params![row.memory_id],
                    |r| r.get(0),
                )
                .optional()?;
            let mk = kind
                .as_deref()
                .and_then(|s| s.parse::<MemoryKind>().ok())
                .unwrap_or(MemoryKind::Fact);
            *by_kind.entry(mk).or_insert(0) += row.count;
        }
        by_kind.into_iter().collect()
    };

    let total_citations: u64 = citations_by_kind.iter().map(|(_, c)| *c as u64).sum();
    let estimated_saved_tokens = estimate_savings(&citations_by_kind);
    let net_tokens = estimated_saved_tokens as i64 - injected_tokens as i64;

    // --- USD ---
    let price = resolve_price_per_mtok(model_name, price_per_mtok_override);
    let usd = price.map(|p_per_mtok| {
        let saved_usd = estimated_saved_tokens as f64 / 1_000_000.0 * p_per_mtok;
        let spent_usd = injected_tokens as f64 / 1_000_000.0 * p_per_mtok;
        RoiUsd {
            saved: saved_usd,
            spent: spent_usd,
            net: saved_usd - spent_usd,
        }
    });

    Ok(RoiReport {
        window_days: window.days(),
        injected_tokens,
        served_events,
        citations: total_citations,
        estimated_saved_tokens,
        net_tokens,
        usd,
    })
}

// ---------------------------------------------------------------------------
// Per-session mini-report (for the Stop hook)
// ---------------------------------------------------------------------------

/// Compute a per-session ROI mini-report for the Stop hook.
///
/// Attribution strategy (conservative):
/// 1. `context.served` events with matching `session_id` in payload → used
///    for `served_events` and `injected_tokens`.
/// 2. Citations: we cannot directly attribute `memory_citations` rows to a
///    session_id (citations are keyed by run_id, not session_id).  Instead
///    we fall back to a time-window bounded by the earliest and latest
///    `context.served` event timestamps for this session.  If session_id
///    is absent (old hook payload), the time window covers the last 24 hours
///    as a rough proxy.
/// 3. ZERO citations → returns `None` (silence; no savings line emitted).
///
/// All errors are swallowed and `None` returned — the hook must never fail.
pub fn session_roi(
    conn: &rusqlite::Connection,
    session_id: Option<&str>,
    model_name: &str,
    price_per_mtok_override: Option<f64>,
) -> Option<SessionRoi> {
    session_roi_inner(conn, session_id, model_name, price_per_mtok_override).unwrap_or(None)
}

fn session_roi_inner(
    conn: &rusqlite::Connection,
    session_id: Option<&str>,
    model_name: &str,
    price_per_mtok_override: Option<f64>,
) -> KimetsuResult<Option<SessionRoi>> {
    // 1. Find context.served events for this session.
    let (served_events, injected_tokens, earliest_ts, latest_ts) =
        session_served_stats(conn, session_id)?;

    // 2. Determine citation time window.
    let (ts_lo, ts_hi) = match (earliest_ts.as_deref(), latest_ts.as_deref()) {
        (Some(lo), Some(hi)) => (lo.to_string(), hi.to_string()),
        _ => {
            // Fall back to last 24h.
            let now = time::OffsetDateTime::now_utc();
            let fmt = time::format_description::well_known::Rfc3339;
            let lo = (now - time::Duration::seconds(86_400))
                .format(&fmt)
                .unwrap_or_default();
            let hi = now.format(&fmt).unwrap_or_default();
            (lo, hi)
        }
    };

    // 3. Collect citations in the time window.
    let citations_by_kind = citations_in_window(conn, &ts_lo, &ts_hi)?;
    let total_citations: u64 = citations_by_kind.iter().map(|(_, c)| *c as u64).sum();

    // Silence when nothing was cited this session.
    if total_citations == 0 {
        return Ok(None);
    }

    let estimated_saved_tokens = estimate_savings(&citations_by_kind);
    let net_tokens = estimated_saved_tokens as i64 - injected_tokens as i64;

    let price = resolve_price_per_mtok(model_name, price_per_mtok_override);
    let usd = price.map(|p_per_mtok| {
        let saved_usd = estimated_saved_tokens as f64 / 1_000_000.0 * p_per_mtok;
        let spent_usd = injected_tokens as f64 / 1_000_000.0 * p_per_mtok;
        RoiUsd {
            saved: saved_usd,
            spent: spent_usd,
            net: saved_usd - spent_usd,
        }
    });

    Ok(Some(SessionRoi {
        served_events,
        injected_tokens,
        citations: total_citations,
        estimated_saved_tokens,
        net_tokens,
        usd,
    }))
}

/// Lightweight per-session ROI summary used by the Stop hook.
#[derive(Debug, Clone)]
pub struct SessionRoi {
    pub served_events: u64,
    pub injected_tokens: u64,
    pub citations: u64,
    pub estimated_saved_tokens: u64,
    pub net_tokens: i64,
    pub usd: Option<RoiUsd>,
}

impl SessionRoi {
    /// Build a one-line savings sentence for the Stop hook `systemMessage`.
    /// Returns a human-readable string like:
    ///   "[Kimetsu] Brain saved ~1 200 tokens (~$0.004) this session."
    pub fn savings_sentence(&self) -> String {
        match &self.usd {
            Some(u) if u.net >= 0.0 => format!(
                "[Kimetsu] Brain saved ~{} tokens (~${:.4}) this session.",
                format_tokens(self.estimated_saved_tokens),
                u.saved,
            ),
            Some(u) => format!(
                "[Kimetsu] Brain used ~{} tokens (net −${:.4}) this session.",
                format_tokens(self.injected_tokens),
                u.spent - u.saved,
            ),
            None => format!(
                "[Kimetsu] Brain saved ~{} tokens this session.",
                format_tokens(self.estimated_saved_tokens),
            ),
        }
    }
}

fn format_tokens(n: u64) -> String {
    // Human-friendly: thousands separator via simple manual formatting.
    if n < 1_000 {
        return n.to_string();
    }
    let s = n.to_string();
    let mut out = String::new();
    let rem = s.len() % 3;
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (i % 3 == rem) {
            out.push(' ');
        }
        out.push(ch);
    }
    out
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Returns (served_events, injected_tokens, earliest_ts, latest_ts) for a
/// given session_id.  When session_id is None the query returns all events.
fn session_served_stats(
    conn: &rusqlite::Connection,
    session_id: Option<&str>,
) -> KimetsuResult<(u64, u64, Option<String>, Option<String>)> {
    // context.served events for this session.
    let served_payloads: Vec<String> = match session_id {
        Some(sid) => {
            let mut stmt = conn.prepare(
                "SELECT payload_json FROM events \
                 WHERE kind = 'context.served' \
                   AND json_extract(payload_json, '$.session_id') = ?1",
            )?;
            let rows = stmt.query_map(params![sid], |r| r.get::<_, String>(0))?;
            rows.collect::<Result<Vec<_>, _>>()?
        }
        None => {
            // No session_id available — return empty; caller falls back to 24h window.
            return Ok((0, 0, None, None));
        }
    };

    // Also collect context.injected in the same session window by time.
    // Derive earliest/latest ts from the served events first.
    let mut earliest: Option<String> = None;
    let mut latest: Option<String> = None;
    let served_count = served_payloads.len() as u64;

    // Parse timestamps from the events table for the served events.
    if let Some(sid) = session_id {
        if served_count > 0 {
            let ts_row: (Option<String>, Option<String>) = conn.query_row(
                "SELECT MIN(ts), MAX(ts) FROM events \
                 WHERE kind = 'context.served' \
                   AND json_extract(payload_json, '$.session_id') = ?1",
                params![sid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            earliest = ts_row.0;
            latest = ts_row.1;
        }
    }

    // Injected tokens: sum from context.injected events in [earliest, latest].
    let injected_tokens: u64 = match (earliest.as_deref(), latest.as_deref()) {
        (Some(lo), Some(hi)) => {
            let payloads: Vec<String> = {
                let mut stmt = conn.prepare(
                    "SELECT payload_json FROM events \
                     WHERE kind = 'context.injected' AND ts >= ?1 AND ts <= ?2",
                )?;
                let rows = stmt.query_map(params![lo, hi], |r| r.get::<_, String>(0))?;
                rows.collect::<Result<Vec<_>, _>>()?
            };
            let mut sum: u64 = 0;
            for p in &payloads {
                let v: serde_json::Value = serde_json::from_str(p)?;
                if let Some(t) = v.get("used_tokens").and_then(|x| x.as_u64()) {
                    sum += t;
                }
            }
            sum
        }
        _ => 0,
    };

    Ok((served_count, injected_tokens, earliest, latest))
}

/// Collect (MemoryKind, count) citation pairs for citations whose `cited_at`
/// falls in `[ts_lo, ts_hi]`.
fn citations_in_window(
    conn: &rusqlite::Connection,
    ts_lo: &str,
    ts_hi: &str,
) -> KimetsuResult<Vec<(MemoryKind, u32)>> {
    struct Row {
        memory_id: String,
        count: u32,
    }
    let mut stmt = conn.prepare(
        "SELECT memory_id, COUNT(*) FROM memory_citations \
         WHERE cited_at >= ?1 AND cited_at <= ?2 \
         GROUP BY memory_id",
    )?;
    let rows = stmt.query_map(params![ts_lo, ts_hi], |r| {
        Ok(Row {
            memory_id: r.get(0)?,
            count: r.get(1)?,
        })
    })?;
    let rows: Vec<Row> = rows.collect::<Result<Vec<_>, _>>()?;

    let mut by_kind: std::collections::HashMap<MemoryKind, u32> = std::collections::HashMap::new();
    for row in &rows {
        let kind: Option<String> = conn
            .query_row(
                "SELECT kind FROM memories WHERE memory_id = ?1",
                params![row.memory_id],
                |r| r.get(0),
            )
            .optional()?;
        let mk = kind
            .as_deref()
            .and_then(|s| s.parse::<MemoryKind>().ok())
            .unwrap_or(MemoryKind::Fact);
        *by_kind.entry(mk).or_insert(0) += row.count;
    }
    Ok(by_kind.into_iter().collect())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use kimetsu_core::memory::MemoryKind;

    // --- Pure function tests ---

    #[test]
    fn estimate_savings_zero_when_empty() {
        assert_eq!(estimate_savings(&[]), 0);
    }

    #[test]
    fn estimate_savings_single_kind() {
        // 2 failure_pattern citations × 1500 = 3000
        assert_eq!(estimate_savings(&[(MemoryKind::FailurePattern, 2)]), 3_000);
    }

    #[test]
    fn estimate_savings_multi_kind() {
        let citations = vec![
            (MemoryKind::FailurePattern, 1), // 1500
            (MemoryKind::Command, 2),        // 800
            (MemoryKind::Convention, 1),     // 300
            (MemoryKind::Fact, 1),           // 500
            (MemoryKind::Preference, 3),     // 600
        ];
        assert_eq!(estimate_savings(&citations), 1500 + 800 + 300 + 500 + 600);
    }

    #[test]
    fn estimate_savings_all_kinds_covered() {
        // Every kind must appear in SAVED_TOKENS_PER_CITATION.
        for kind in [
            MemoryKind::FailurePattern,
            MemoryKind::Command,
            MemoryKind::Convention,
            MemoryKind::Fact,
            MemoryKind::Preference,
        ] {
            let v = SAVED_TOKENS_PER_CITATION
                .iter()
                .find(|(k, _)| k == &kind)
                .map(|(_, v)| *v);
            assert!(
                v.is_some(),
                "kind {:?} missing from SAVED_TOKENS_PER_CITATION",
                kind
            );
            assert!(v.unwrap() > 0, "kind {:?} has zero constant", kind);
        }
    }

    #[test]
    fn resolve_price_override_wins() {
        assert_eq!(
            resolve_price_per_mtok("claude-sonnet-4-7", Some(5.0)),
            Some(5.0)
        );
    }

    #[test]
    fn resolve_price_known_model() {
        let p = resolve_price_per_mtok("claude-sonnet-4-7", None);
        assert!(p.is_some(), "claude-sonnet-4 should match");
        assert!((p.unwrap() - 3.0).abs() < 1e-9);
    }

    #[test]
    fn resolve_price_unknown_model_none() {
        assert!(resolve_price_per_mtok("my-custom-llm-v9", None).is_none());
    }

    #[test]
    fn resolve_price_longest_prefix_wins() {
        // "claude-opus-4" and "claude-opus-4" — make sure haiku doesn't
        // match opus prefix.
        let opus_p = resolve_price_per_mtok("claude-opus-4-5", None).unwrap_or(0.0);
        let haiku_p = resolve_price_per_mtok("claude-haiku-4-5", None).unwrap_or(0.0);
        assert!(opus_p > haiku_p, "opus should be more expensive than haiku");
    }

    #[test]
    fn roi_window_parse() {
        assert_eq!(RoiWindow::parse("7d").unwrap(), RoiWindow::Days(7));
        assert_eq!(RoiWindow::parse("30d").unwrap(), RoiWindow::Days(30));
        assert_eq!(RoiWindow::parse("all").unwrap(), RoiWindow::All);
        assert_eq!(RoiWindow::parse("ALL").unwrap(), RoiWindow::All);
        assert!(RoiWindow::parse("bad").is_err());
    }

    #[test]
    fn format_tokens_below_1000() {
        assert_eq!(format_tokens(42), "42");
        assert_eq!(format_tokens(999), "999");
    }

    #[test]
    fn format_tokens_thousands() {
        assert_eq!(format_tokens(1_000), "1 000");
        assert_eq!(format_tokens(12_345), "12 345");
        assert_eq!(format_tokens(1_234_567), "1 234 567");
    }

    // --- DB-backed tests ---

    use crate::{
        project::{init_project, load_project},
        projector,
        user_brain::with_user_brain_disabled,
    };
    use kimetsu_core::{event::Event, ids::RunId, memory::MemoryScope};
    use ulid::Ulid;

    fn test_root() -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("kimetsu-roi-test-{}", Ulid::new()));
        kimetsu_core::paths::git_init_boundary(&root);
        root
    }

    fn seed_memory(root: &std::path::Path, kind: MemoryKind, text: &str) -> String {
        crate::project::add_memory(root, MemoryScope::Project, kind, text).expect("add_memory")
    }

    fn seed_injected_event(conn: &rusqlite::Connection, run_id: RunId, used_tokens: u64) {
        let ev = Event::new(
            run_id,
            "context.injected",
            serde_json::json!({
                "stage": "localization",
                "memory_ids": [],
                "used_tokens": used_tokens,
                "capsule_count": 1,
            }),
        );
        projector::apply_events(conn, &[ev]).expect("seed injected");
    }

    fn seed_citation(conn: &rusqlite::Connection, run_id: RunId, memory_id: &str, turn: i64) {
        let ev = Event::new(
            run_id,
            "memory.cited",
            serde_json::json!({
                "memory_id": memory_id,
                "turn": turn,
            }),
        );
        projector::apply_events(conn, &[ev]).expect("seed citation");
    }

    #[test]
    fn roi_report_empty_db_returns_zeros() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");
            let (_paths, config, conn) = load_project(&root).expect("load");
            let report =
                roi_report(&conn, RoiWindow::All, &config.model.model, None).expect("roi_report");
            assert_eq!(report.injected_tokens, 0);
            assert_eq!(report.citations, 0);
            assert_eq!(report.estimated_saved_tokens, 0);
            assert_eq!(report.net_tokens, 0);
        });
    }

    #[test]
    fn roi_report_with_citations_computes_savings() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");
            let m1 = seed_memory(&root, MemoryKind::FailurePattern, "fp1");
            let (_paths, config, conn) = load_project(&root).expect("load");
            let run_id = RunId::new();
            seed_injected_event(&conn, run_id, 300);
            seed_citation(&conn, run_id, &m1, 1);

            let report =
                roi_report(&conn, RoiWindow::All, &config.model.model, None).expect("roi_report");
            // 1 failure_pattern citation = 1500 saved tokens.
            assert_eq!(report.estimated_saved_tokens, 1500);
            assert_eq!(report.injected_tokens, 300);
            assert_eq!(report.net_tokens, 1500 - 300);
            assert_eq!(report.citations, 1);
        });
    }

    #[test]
    fn roi_report_negative_net_when_overhead_exceeds_savings() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");
            let m1 = seed_memory(&root, MemoryKind::Preference, "pref1");
            let (_paths, config, conn) = load_project(&root).expect("load");
            let run_id = RunId::new();
            // Inject 500 tokens but cite 1 preference (200 saved) → net = -300.
            seed_injected_event(&conn, run_id, 500);
            seed_citation(&conn, run_id, &m1, 1);

            let report =
                roi_report(&conn, RoiWindow::All, &config.model.model, None).expect("roi_report");
            assert_eq!(report.estimated_saved_tokens, 200);
            assert_eq!(report.net_tokens, 200 - 500); // −300
        });
    }

    #[test]
    fn roi_report_usd_with_known_model() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");
            let m1 = seed_memory(&root, MemoryKind::Command, "cmd1");
            let (_paths, _config, conn) = load_project(&root).expect("load");
            let run_id = RunId::new();
            seed_injected_event(&conn, run_id, 200);
            seed_citation(&conn, run_id, &m1, 1);

            // Use a known model directly.
            let report =
                roi_report(&conn, RoiWindow::All, "claude-sonnet-4-7", None).expect("roi_report");
            let usd = report.usd.expect("usd must be Some for known model");
            // 400 saved tokens @ $3/MTok = $0.0012
            assert!((usd.saved - 400.0 / 1_000_000.0 * 3.0).abs() < 1e-9);
            // 200 injected tokens @ $3/MTok
            assert!((usd.spent - 200.0 / 1_000_000.0 * 3.0).abs() < 1e-9);
            assert!((usd.net - (usd.saved - usd.spent)).abs() < 1e-12);
        });
    }

    #[test]
    fn roi_report_usd_with_override() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");
            let m1 = seed_memory(&root, MemoryKind::Fact, "fact1");
            let (_paths, _config, conn) = load_project(&root).expect("load");
            let run_id = RunId::new();
            seed_injected_event(&conn, run_id, 0);
            seed_citation(&conn, run_id, &m1, 1);

            let report =
                roi_report(&conn, RoiWindow::All, "my-custom-llm", Some(10.0)).expect("roi_report");
            // 500 saved tokens @ $10/MTok = $0.005
            let usd = report.usd.expect("usd with override");
            assert!((usd.saved - 500.0 / 1_000_000.0 * 10.0).abs() < 1e-9);
        });
    }

    #[test]
    fn roi_report_unknown_model_no_usd() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");
            let (_paths, _config, conn) = load_project(&root).expect("load");

            let report = roi_report(&conn, RoiWindow::All, "totally-unknown-llm-xyz", None)
                .expect("roi_report");
            assert!(report.usd.is_none(), "usd must be None for unknown model");
        });
    }

    #[test]
    fn session_roi_returns_none_when_no_citations() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");
            let (_paths, _config, conn) = load_project(&root).expect("load");

            let result = session_roi(&conn, Some("sess-abc"), "claude-sonnet-4", None);
            assert!(result.is_none(), "no citations → no session roi");
        });
    }

    #[test]
    fn savings_sentence_positive_no_usd() {
        let sr = SessionRoi {
            served_events: 3,
            injected_tokens: 100,
            citations: 2,
            estimated_saved_tokens: 1200,
            net_tokens: 1100,
            usd: None,
        };
        let s = sr.savings_sentence();
        assert!(s.contains("1 200"), "expected formatted token count");
        assert!(s.contains("[Kimetsu]"), "must have brand prefix");
    }

    #[test]
    fn savings_sentence_positive_with_usd() {
        let sr = SessionRoi {
            served_events: 3,
            injected_tokens: 100,
            citations: 2,
            estimated_saved_tokens: 1500,
            net_tokens: 1400,
            usd: Some(RoiUsd {
                saved: 0.0045,
                spent: 0.0003,
                net: 0.0042,
            }),
        };
        let s = sr.savings_sentence();
        assert!(s.contains("$"), "must include dollar sign when usd present");
    }
}
