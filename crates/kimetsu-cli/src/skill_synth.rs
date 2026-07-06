//! Flagship 2 — Memory → Skill synthesis: drafting, review, install.
//!
//! # 2.2 — Drafting
//!
//! For each synthesis candidate, draft a SKILL.md-style skill using the
//! configured cheap model (same `config.cheap_model()` / `resolve_distiller`
//! chain as the SessionEnd distiller and `brain ask`).
//!
//! The prompt is **GROUNDED-ONLY**: the model receives only the cited memory
//! texts as source material and is explicitly instructed not to invent steps.
//!
//! **cheap-model-absent degradation**: when no cheap model is configured (or
//! no credentials are available), the engine emits a human-readable report
//! listing the candidates without drafting anything — no hard failure.
//!
//! # 2.3 — Review + install
//!
//! `kimetsu brain skills` lists pending proposals and accepted skills.
//! Accepting a proposal:
//!   1. Writes the draft SKILL.md into `.kimetsu/skills/<slug>/` via the
//!      existing `SkillRegistry::install_as_kimetsu`.
//!   2. Writes `.kimetsu-skill-provenance.json` alongside SKILL.md, recording
//!      the source memory ids for staleness tracking (2.4).
//!   3. Calls `accept_skill_proposal` to mark the proposal accepted.
//!
//! NEVER auto-installs — explicit `--accept <proposal-id>` only.

use std::path::{Path, PathBuf};

use kimetsu_brain::project;
use kimetsu_brain::skill_synthesis::{
    SynthesisCandidate, accept_skill_proposal, check_staleness, find_synthesis_candidates,
    insert_skill_proposal, load_skill_proposal,
};
use kimetsu_core::KimetsuResult;

use crate::distiller::{make_provider_for_resolved, resolve_distiller};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum characters of memory text fed to the drafter per memory.
const MAX_MEMORY_CHARS: usize = 800;

/// System prompt for the skill drafter.
const SKILL_DRAFT_SYSTEM: &str = "\
You are Kimetsu's skill synthesizer. Draft a reusable SKILL.md for an AI coding agent, \
grounded STRICTLY in the memory capsules below. \
DO NOT invent steps, commands, or advice not present in the provided memories. \
If the memories do not contain enough information to draft actionable guidance, \
say so explicitly and keep the draft minimal.\n\n\
Output a SKILL.md with YAML front-matter (name, description) followed by \
imperative prose guidance. Keep the total output under 600 tokens.\n\n\
Format:\n\
---\n\
name: <skill-name>\n\
description: <one-line description>\n\
---\n\
# <Skill Name>\n\
<imperative guidance grounded in the memories below>\n";

// ---------------------------------------------------------------------------
// Public result types
// ---------------------------------------------------------------------------

/// Outcome of a skill synthesis run.
#[derive(Debug)]
pub struct SynthesisReport {
    /// Proposals that were created (or already existed).
    pub proposals: Vec<SynthesisProposalResult>,
    /// True when a cheap model was available for drafting.
    pub model_used: bool,
    /// Model id if drafting was attempted.
    pub model_id: Option<String>,
}

/// Result for one candidate.
#[derive(Debug)]
pub struct SynthesisProposalResult {
    pub candidate: SynthesisCandidate,
    /// `Some(proposal_id)` when a new proposal was created.
    /// `None` when the candidate already has a pending/accepted proposal.
    pub proposal_id: Option<String>,
    /// Whether a draft was generated (vs report-only).
    pub has_draft: bool,
}

// ---------------------------------------------------------------------------
// 2.2 — Main entry point: detect candidates + draft proposals
// ---------------------------------------------------------------------------

/// Detect synthesis candidates and create skill proposals in the database.
///
/// When a cheap model is available, each candidate is drafted via the model
/// (grounded-only prompt from the cited memory texts).  When no model is
/// configured, proposals are created without `draft_content` (report-only
/// mode — 2.2 DP-C degradation).
///
/// Skips candidates that already have a `pending` or `accepted` proposal
/// (idempotent: safe to call repeatedly).
pub fn run_skill_synthesis(workspace: &Path) -> KimetsuResult<SynthesisReport> {
    let (paths, _config, conn) =
        project::load_project(workspace).map_err(|e| format!("brain not initialized: {e}"))?;

    let candidates = find_synthesis_candidates(&conn)?;
    if candidates.is_empty() {
        return Ok(SynthesisReport {
            proposals: Vec::new(),
            model_used: false,
            model_id: None,
        });
    }

    // Resolve cheap model (optional).
    let resolved = resolve_distiller(&paths.repo_root);
    let model_id = resolved.as_ref().map(|r| r.model.clone());
    let model_used = resolved.is_some();

    let mut results = Vec::new();

    for candidate in candidates {
        // Check whether a pending/accepted proposal already exists for this
        // memory, to avoid duplicates.  We query by source_memory_ids containing
        // the candidate memory_id.  Simple substring check on the JSON column is
        // sufficient here.
        let existing: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM skill_proposals
                 WHERE status IN ('pending', 'accepted')
                   AND source_memory_ids_json LIKE ?1",
                rusqlite::params![format!("%{}%", candidate.memory_id)],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if existing > 0 {
            results.push(SynthesisProposalResult {
                candidate,
                proposal_id: None,
                has_draft: false,
            });
            continue;
        }

        // Load the memory text for grounding.
        let memory_texts = kimetsu_brain::skill_synthesis::load_memory_texts(
            &conn,
            std::slice::from_ref(&candidate.memory_id),
        )?;

        let (draft, has_draft) = if let Some(ref resolved_distiller) = resolved {
            // Attempt model-based drafting.
            let draft = draft_skill_with_model(&candidate, &memory_texts, resolved_distiller);
            let has_draft = draft.is_some();
            (draft, has_draft)
        } else {
            (None, false)
        };

        // Derive a skill name and description from the candidate.
        let (skill_name, description) =
            derive_skill_meta(&candidate, draft.as_deref(), &memory_texts);

        let source_ids = vec![candidate.memory_id.clone()];
        let proposal_id = insert_skill_proposal(
            &conn,
            &skill_name,
            &description,
            draft.as_deref(),
            &source_ids,
            &candidate.trigger_kind,
            candidate.trigger_count,
        )?;

        results.push(SynthesisProposalResult {
            candidate,
            proposal_id: Some(proposal_id),
            has_draft,
        });
    }

    Ok(SynthesisReport {
        proposals: results,
        model_used,
        model_id,
    })
}

// ---------------------------------------------------------------------------
// 2.2 — Grounded-only model draft
// ---------------------------------------------------------------------------

/// Call the cheap model to draft a SKILL.md grounded in `memory_texts`.
/// Returns `None` on any error (caller uses report-only mode).
fn draft_skill_with_model(
    candidate: &SynthesisCandidate,
    memory_texts: &[(String, String)],
    resolved: &crate::distiller::ResolvedDistiller,
) -> Option<String> {
    use kimetsu_agent::model::{
        MessageContent, MessageRole, ModelMessage, ModelProvider, ModelRequest, ToolChoice,
    };

    let mut provider: Box<dyn ModelProvider> = make_provider_for_resolved(resolved)?;

    // Assemble a grounded context block from memory texts only.
    let context_block = memory_texts
        .iter()
        .enumerate()
        .map(|(i, (mid, text))| {
            let truncated = truncate_chars(text, MAX_MEMORY_CHARS);
            format!("[Memory #{} — {mid}]\n{truncated}", i + 1)
        })
        .collect::<Vec<_>>()
        .join("\n\n---\n\n");

    let user_msg = format!(
        "Memory capsules for skill synthesis (trigger: {} × {}):\n\n{context_block}\n\n\
         Draft a SKILL.md grounded STRICTLY in the memories above. \
         Suggest a skill name reflecting the pattern. \
         Do NOT invent steps not present in the memories.",
        candidate.trigger_kind, candidate.trigger_count
    );

    let request = ModelRequest {
        messages: vec![
            ModelMessage {
                role: MessageRole::System,
                content: vec![MessageContent::Text {
                    text: SKILL_DRAFT_SYSTEM.to_string(),
                }],
            },
            ModelMessage::user_text(&user_msg),
        ],
        tools: Vec::new(),
        tool_choice: ToolChoice::None,
        max_output_tokens: 700,
        temperature: 0.15,
        metadata: serde_json::Value::Null,
    };

    let response = provider.complete(request).ok()?;
    let text = response.text?.trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

// ---------------------------------------------------------------------------
// 2.3 — Install an accepted proposal into the host-native skill dir
// ---------------------------------------------------------------------------

/// Accept a pending skill proposal and install it into `.kimetsu/skills/`.
///
/// Steps:
/// 1. Load the proposal — error if not found or not pending.
/// 2. Validate draft_content is present (report-only proposals can't be auto-installed).
/// 3. Write a temp directory with SKILL.md + `.kimetsu-skill-provenance.json`.
/// 4. Install via `SkillRegistry::install_as_kimetsu` (copies the temp tree).
/// 5. Write provenance into the installed dir.
/// 6. Mark the proposal accepted in the database.
///
/// Returns the installed skill root path.
pub fn install_skill_proposal(workspace: &Path, proposal_id: &str) -> KimetsuResult<PathBuf> {
    use kimetsu_chat::skills::{SkillConfig, SkillRegistry};
    use std::fs;

    let (_paths, _config, conn) =
        project::load_project(workspace).map_err(|e| format!("brain not initialized: {e}"))?;

    let proposal = load_skill_proposal(&conn, proposal_id)?
        .ok_or_else(|| format!("proposal `{proposal_id}` not found"))?;

    if proposal.status != "pending" {
        return Err(format!(
            "proposal `{proposal_id}` is {} (only pending proposals can be installed)",
            proposal.status
        )
        .into());
    }

    let draft = proposal.draft_content.as_deref().ok_or_else(|| {
        format!(
            "proposal `{proposal_id}` has no draft (report-only mode); \
             configure a cheap model and re-run `kimetsu brain skills --detect` to draft"
        )
    })?;

    // Create a temp staging dir for the skill bundle.
    let staging = create_staging_dir(workspace, &proposal.skill_name, draft)?;

    // Write provenance.
    write_skill_provenance(&staging, proposal_id, &proposal.source_memory_ids)?;

    // Install via SkillRegistry (copies the staging tree into .kimetsu/skills/).
    let config = SkillConfig {
        roots: vec![staging.clone()],
        ..SkillConfig::default()
    };
    let registry = SkillRegistry::discover(workspace, &config)
        .map_err(|e| format!("skill registry error: {e}"))?;
    let installed = registry
        .install_as_kimetsu(&proposal.skill_name, false)
        .or_else(|_| {
            // Try force if already exists (re-accept).
            registry.install_as_kimetsu(&proposal.skill_name, true)
        })
        .map_err(|e| format!("skill install failed: {e}"))?;

    let installed_root = installed.root.to_string_lossy().to_string();

    // Copy the provenance file into the final installed location (install_as_kimetsu
    // copies the tree, but provenance may have been written after the copy; overwrite).
    let provenance_src = staging.join(".kimetsu-skill-provenance.json");
    let provenance_dst = installed.root.join(".kimetsu-skill-provenance.json");
    if provenance_src.exists() {
        fs::copy(&provenance_src, &provenance_dst).ok();
    }

    // Clean up staging.
    fs::remove_dir_all(&staging).ok();

    // Mark accepted in DB.
    accept_skill_proposal(&conn, proposal_id, &installed_root)?;

    Ok(installed.root)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Write SKILL.md into a temporary staging directory and return its path.
fn create_staging_dir(
    workspace: &Path,
    skill_name: &str,
    draft_content: &str,
) -> KimetsuResult<PathBuf> {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);

    let slug = slugify(skill_name);
    // Place the staging dir inside .kimetsu/ to keep it in the project tree.
    let staging = workspace
        .join(".kimetsu")
        .join("_skill_staging")
        .join(format!("{slug}-{ts}"));
    fs::create_dir_all(&staging).map_err(|e| format!("failed to create staging dir: {e}"))?;

    fs::write(staging.join("SKILL.md"), draft_content)
        .map_err(|e| format!("failed to write SKILL.md: {e}"))?;

    Ok(staging)
}

/// Write `.kimetsu-skill-provenance.json` into `dir`.
fn write_skill_provenance(
    dir: &Path,
    proposal_id: &str,
    source_memory_ids: &[String],
) -> KimetsuResult<()> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let provenance = serde_json::json!({
        "synthesized_from_proposal": proposal_id,
        "source_memory_ids": source_memory_ids,
        "synthesized_at_unix": ts,
    });
    let json = serde_json::to_string_pretty(&provenance)
        .map_err(|e| format!("failed to serialize provenance: {e}"))?;
    std::fs::write(dir.join(".kimetsu-skill-provenance.json"), json)
        .map_err(|e| format!("failed to write provenance: {e}"))
        .map_err(Into::into)
}

/// Derive a skill name and description from a candidate and optional draft.
fn derive_skill_meta(
    candidate: &SynthesisCandidate,
    draft: Option<&str>,
    memory_texts: &[(String, String)],
) -> (String, String) {
    // Try to extract name/description from draft frontmatter first.
    if let Some(draft) = draft {
        if let Some((name, desc)) = extract_frontmatter_meta(draft) {
            if !name.is_empty() {
                return (name, desc);
            }
        }
    }

    // Fall back: derive from the memory kind + text snippet.
    let kind_label = match candidate.kind.as_str() {
        "convention" => "convention",
        "failure_pattern" | "anti_pattern" => "anti-pattern",
        "command" => "command",
        _ => "lesson",
    };
    let text_snippet: String = memory_texts
        .first()
        .map(|(_, t)| t.chars().take(40).collect::<String>())
        .unwrap_or_else(|| candidate.memory_id.chars().take(20).collect());
    let name = format!("{kind_label}-{}", slugify(&text_snippet));
    let name = name.chars().take(48).collect::<String>();
    let name = name.trim_end_matches('-').to_string();
    let description = format!(
        "Synthesized from {} {} (trigger: {} × {})",
        candidate.trigger_kind,
        candidate.memory_id,
        candidate.trigger_kind,
        candidate.trigger_count
    );
    (name, description)
}

/// Try to extract `name:` and `description:` from YAML front-matter.
fn extract_frontmatter_meta(content: &str) -> Option<(String, String)> {
    let rest = content.trim_start_matches('\u{feff}').strip_prefix("---")?;
    let rest = rest
        .strip_prefix('\r')
        .or_else(|| rest.strip_prefix('\n'))
        .unwrap_or(rest);
    let end = rest.find("\n---").or_else(|| rest.find("\r\n---"))?;
    let frontmatter = &rest[..end];
    let mut name = String::new();
    let mut description = String::new();
    for line in frontmatter.lines() {
        if let Some(v) = line.strip_prefix("name:") {
            name = v.trim().trim_matches('"').trim_matches('\'').to_string();
        } else if let Some(v) = line.strip_prefix("description:") {
            description = v.trim().trim_matches('"').trim_matches('\'').to_string();
        }
    }
    Some((name, description))
}

fn slugify(s: &str) -> String {
    let mut slug = String::new();
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
        } else if (ch == '-' || ch == '_' || ch.is_whitespace()) && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        "skill".to_string()
    } else {
        slug
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_string()
    } else {
        format!("{}…", chars[..max].iter().collect::<String>())
    }
}

// ---------------------------------------------------------------------------
// CLI output helpers
// ---------------------------------------------------------------------------

/// Print a human-readable synthesis report.
pub fn print_synthesis_report(report: &SynthesisReport) {
    if report.proposals.is_empty() {
        println!("No synthesis candidates found.");
        println!(
            "(Memories need ≥3 citations across distinct runs, or form a tight semantic cluster.)"
        );
        return;
    }

    if !report.model_used {
        println!(
            "Note: no cheap model configured — candidates listed without drafts.\n\
             Configure [cheap_model] in project.toml to enable drafting.\n"
        );
    } else if let Some(ref mid) = report.model_id {
        println!("Drafting with model: {mid}\n");
    }

    let mut new_count = 0usize;
    let mut skip_count = 0usize;
    for r in &report.proposals {
        if let Some(ref pid) = r.proposal_id {
            new_count += 1;
            let draft_label = if r.has_draft {
                "with draft"
            } else {
                "report-only"
            };
            println!("  [NEW] proposal {} ({draft_label})", pid);
            println!(
                "        memory: {} | trigger: {} × {}",
                r.candidate.memory_id, r.candidate.trigger_kind, r.candidate.trigger_count
            );
        } else {
            skip_count += 1;
        }
    }
    if skip_count > 0 {
        println!("  ({skip_count} candidate(s) already have pending/accepted proposals — skipped)");
    }
    println!();
    if new_count > 0 {
        println!("Run `kimetsu brain skills --review` to inspect proposals.");
        println!("Run `kimetsu brain skills --accept <proposal-id>` to install a skill.");
    }
}

/// Print pending + accepted skill proposals for review.
pub fn print_skill_review(conn: &rusqlite::Connection) -> KimetsuResult<()> {
    let pending = kimetsu_brain::skill_synthesis::list_skill_proposals(conn, Some("pending"))?;
    let accepted = kimetsu_brain::skill_synthesis::list_skill_proposals(conn, Some("accepted"))?;

    if pending.is_empty() && accepted.is_empty() {
        println!("No skill proposals. Run `kimetsu brain skills --detect` to generate candidates.");
        return Ok(());
    }

    if !pending.is_empty() {
        println!("=== PENDING ({}) ===", pending.len());
        for p in &pending {
            println!(
                "  {} | {} | trigger: {} × {} | {}",
                p.proposal_id,
                p.skill_name,
                p.trigger_kind,
                p.trigger_count,
                if p.draft_content.is_some() {
                    "has draft"
                } else {
                    "no draft"
                }
            );
            println!("    sources: {}", p.source_memory_ids.join(", "));
        }
        println!();
    }

    if !accepted.is_empty() {
        println!("=== ACCEPTED ({}) ===", accepted.len());
        for p in &accepted {
            let path = p.installed_path.as_deref().unwrap_or("(unknown)");
            println!(
                "  {} | {} | installed: {}",
                p.proposal_id, p.skill_name, path
            );
        }
    }

    Ok(())
}

/// Print staleness status for all accepted skill proposals.
pub fn print_staleness_status(conn: &rusqlite::Connection) -> KimetsuResult<()> {
    let reports = check_staleness(conn)?;
    if reports.is_empty() {
        println!("No accepted skill proposals to check.");
        return Ok(());
    }

    let stale_count = reports.iter().filter(|r| r.is_stale).count();
    println!("Skill staleness status ({} accepted):", reports.len());
    for report in &reports {
        if report.is_stale {
            println!(
                "  [STALE] {} ({})",
                report.skill_name,
                report.installed_path.as_deref().unwrap_or("?")
            );
            println!(
                "    Stale source memories: {}",
                report.stale_memory_ids.join(", ")
            );
        } else {
            println!(
                "  [OK]    {} ({})",
                report.skill_name,
                report.installed_path.as_deref().unwrap_or("?")
            );
        }
    }
    if stale_count > 0 {
        println!();
        println!(
            "{stale_count} stale skill(s) — review source memories and re-run \
             `kimetsu brain skills --detect` to update."
        );
    } else {
        println!("All skills are up-to-date.");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use kimetsu_brain::skill_synthesis::insert_skill_proposal;

    #[allow(dead_code)]
    fn init_conn() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().expect("open_in_memory");
        kimetsu_brain::schema::initialize(&conn).expect("initialize");
        conn
    }

    // -----------------------------------------------------------------------
    // slugify
    // -----------------------------------------------------------------------
    #[test]
    fn slugify_normalizes_whitespace_and_special_chars() {
        assert_eq!(slugify("Hello World!"), "hello-world");
        assert_eq!(slugify("  --trim--  "), "trim");
        assert_eq!(slugify(""), "skill");
        assert_eq!(slugify("convention-a_b"), "convention-a-b");
    }

    // -----------------------------------------------------------------------
    // extract_frontmatter_meta
    // -----------------------------------------------------------------------
    #[test]
    fn extract_frontmatter_parses_name_and_description() {
        let draft = "---\nname: my-skill\ndescription: Does the thing.\n---\n# My Skill\nDo it.";
        let (name, desc) = extract_frontmatter_meta(draft).expect("must parse");
        assert_eq!(name, "my-skill");
        assert_eq!(desc, "Does the thing.");
    }

    #[test]
    fn extract_frontmatter_returns_none_for_no_frontmatter() {
        assert!(extract_frontmatter_meta("# No frontmatter here").is_none());
    }

    // -----------------------------------------------------------------------
    // derive_skill_meta
    // -----------------------------------------------------------------------
    #[test]
    fn derive_skill_meta_uses_frontmatter_when_present() {
        let candidate = SynthesisCandidate {
            memory_id: "m1".to_string(),
            scope: "project".to_string(),
            kind: "convention".to_string(),
            text: "Always run fmt.".to_string(),
            trigger_kind: "citations".to_string(),
            trigger_count: 3,
        };
        let draft =
            "---\nname: run-fmt\ndescription: Always run cargo fmt.\n---\n# Run Fmt\nDo it.";
        let (name, desc) = derive_skill_meta(&candidate, Some(draft), &[]);
        assert_eq!(name, "run-fmt");
        assert_eq!(desc, "Always run cargo fmt.");
    }

    #[test]
    fn derive_skill_meta_fallback_when_no_draft() {
        let candidate = SynthesisCandidate {
            memory_id: "m2".to_string(),
            scope: "project".to_string(),
            kind: "command".to_string(),
            text: "cargo test --all".to_string(),
            trigger_kind: "citations".to_string(),
            trigger_count: 4,
        };
        let (name, _desc) = derive_skill_meta(
            &candidate,
            None,
            &[("m2".to_string(), "cargo test --all".to_string())],
        );
        assert!(
            name.starts_with("command"),
            "fallback name should start with kind, got: {name}"
        );
    }

    // -----------------------------------------------------------------------
    // write_skill_provenance
    // -----------------------------------------------------------------------
    #[test]
    fn write_skill_provenance_creates_valid_json() {
        use std::fs;
        let tmp = std::env::temp_dir().join(format!(
            "kimetsu_prov_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&tmp).unwrap();
        write_skill_provenance(&tmp, "prop-1", &["mem-a".to_string(), "mem-b".to_string()])
            .expect("write provenance");
        let raw = fs::read_to_string(tmp.join(".kimetsu-skill-provenance.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid json");
        assert_eq!(parsed["synthesized_from_proposal"], "prop-1");
        let ids: Vec<String> = serde_json::from_value(parsed["source_memory_ids"].clone()).unwrap();
        assert_eq!(ids, vec!["mem-a", "mem-b"]);
        fs::remove_dir_all(&tmp).ok();
    }

    // -----------------------------------------------------------------------
    // Integration: detect → draft → install → provenance round-trip
    //
    // This test exercises 2.1 → 2.3 without a real model (report-only mode
    // since no cheap model credentials are present in test env).  It verifies:
    //   - A cited-≥3 memory is detected as a candidate.
    //   - A skill proposal is created (report-only / no draft).
    //   - The proposal can be accepted after manually setting draft_content.
    //   - Provenance JSON is written alongside SKILL.md in the installed dir.
    //   - `check_staleness` reports the skill as live.
    // -----------------------------------------------------------------------
    #[test]
    fn cited_ge3_candidate_to_install_provenance_round_trip() {
        use kimetsu_brain::skill_synthesis::{
            check_staleness, find_synthesis_candidates, load_skill_proposal,
        };
        use std::fs;

        // ── Set up isolated project brain ────────────────────────────────
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("kimetsu_f2_rt_{ts}"));
        fs::create_dir_all(&root).unwrap();
        kimetsu_core::paths::git_init_boundary(&root);

        kimetsu_brain::user_brain::with_user_brain_disabled(|| {
            kimetsu_brain::project::init_project(&root, true).expect("init brain");

            let (_paths, _config, conn) =
                kimetsu_brain::project::load_project(&root).expect("load brain");

            // Insert one memory.
            conn.execute(
                "INSERT INTO memories
                   (memory_id, scope, kind, text, normalized_text, confidence,
                    provenance_snapshot_json, created_at, use_count, usefulness_score)
                 VALUES ('f2-hot', 'project', 'convention',
                         'Always run cargo fmt before committing. [tags: rust, ci]',
                         'always run cargo fmt before committing',
                         0.9, '{}', '2026-01-01T00:00:00Z', 3, 3.0)",
                [],
            )
            .expect("insert memory");

            // Cite it from 3 distinct runs.
            for run_id in ["rt-run-1", "rt-run-2", "rt-run-3"] {
                conn.execute(
                    "INSERT INTO memory_citations (run_id, memory_id, turn, cited_at)
                     VALUES (?1, 'f2-hot', 1, '2026-01-01T00:00:00Z')",
                    rusqlite::params![run_id],
                )
                .expect("insert citation");
            }

            // ── 2.1: Detect candidates ───────────────────────────────────
            let candidates = find_synthesis_candidates(&conn).expect("find");
            assert!(
                candidates.iter().any(|c| c.memory_id == "f2-hot"),
                "f2-hot must be detected as a candidate"
            );

            // ── 2.2: Insert a proposal (simulate report-only + draft) ─────
            // In test env there is no cheap model; simulate by inserting
            // a proposal with a draft we craft ourselves.
            let draft = "---\nname: cargo-fmt-convention\ndescription: Always run cargo fmt.\n---\n\
                         # cargo-fmt-convention\n\
                         Always run `cargo fmt --all` before committing. [tags: rust, ci]";
            let proposal_id = insert_skill_proposal(
                &conn,
                "cargo-fmt-convention",
                "Always run cargo fmt.",
                Some(draft),
                &["f2-hot".to_string()],
                "citations",
                3,
            )
            .expect("insert proposal");

            // Proposal is pending.
            let row = load_skill_proposal(&conn, &proposal_id)
                .expect("load")
                .expect("must exist");
            assert_eq!(row.status, "pending");
            assert!(row.draft_content.is_some());
            assert_eq!(row.source_memory_ids, vec!["f2-hot"]);

            // ── 2.3: Install ─────────────────────────────────────────────
            let installed_root =
                install_skill_proposal(&root, &proposal_id).expect("install skill");

            assert!(
                installed_root.exists(),
                "installed skill root must exist: {}",
                installed_root.display()
            );
            assert!(
                installed_root.join("SKILL.md").is_file(),
                "SKILL.md must exist in installed dir"
            );
            assert!(
                installed_root
                    .join(".kimetsu-skill-provenance.json")
                    .is_file(),
                ".kimetsu-skill-provenance.json must exist in installed dir"
            );

            // Verify provenance content.
            let prov_raw =
                fs::read_to_string(installed_root.join(".kimetsu-skill-provenance.json"))
                    .expect("read provenance");
            let prov: serde_json::Value =
                serde_json::from_str(&prov_raw).expect("valid provenance json");
            assert_eq!(prov["synthesized_from_proposal"], proposal_id);
            let ids: Vec<String> =
                serde_json::from_value(prov["source_memory_ids"].clone()).unwrap();
            assert!(
                ids.contains(&"f2-hot".to_string()),
                "source memory id must be in provenance"
            );

            // Proposal is now accepted.
            let row = load_skill_proposal(&conn, &proposal_id)
                .expect("load")
                .expect("must exist");
            assert_eq!(row.status, "accepted");
            assert!(row.installed_path.is_some());

            // ── 2.4: Staleness — source memory still live ─────────────────
            let staleness = check_staleness(&conn).expect("staleness");
            let report = staleness.iter().find(|r| r.proposal_id == proposal_id);
            assert!(
                report.is_some(),
                "proposal must appear in staleness reports"
            );
            assert!(
                !report.unwrap().is_stale,
                "skill must not be stale (source memory is live)"
            );
        });

        fs::remove_dir_all(&root).ok();
    }
}
