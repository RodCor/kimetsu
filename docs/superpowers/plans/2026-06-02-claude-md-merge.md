# CLAUDE.md Merge (append, never replace) — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `kimetsu plugin install` merge its guidance into an existing `CLAUDE.md` (workspace or global `~/.claude/CLAUDE.md`) inside markers — appending/upgrading, never overwriting the user's content.

**Architecture:** A `merge_claude_md` helper wraps Kimetsu's guidance in `<!-- kimetsu:begin -->`/`<!-- kimetsu:end -->` markers and merges idempotently (write-if-missing / append-if-absent / replace-the-marked-region-if-present). `write_claude_settings` calls it instead of the old overwrite-or-skip; `--force` no longer touches CLAUDE.md.

**Tech Stack:** Rust; `crates/kimetsu-chat/src/bridge.rs` (the install writer); `crates/kimetsu-cli/src/main.rs` (the `--force` flag).

**Spec:** `docs/superpowers/specs/2026-06-02-claude-md-merge-design.md`

## File Map
- **Modify** `crates/kimetsu-chat/src/bridge.rs` — add markers + `merge_claude_md`; call it from `write_claude_settings` (drop the `force` param); tests.
- **Modify** `crates/kimetsu-cli/src/main.rs` — `plugin_install_inner` `force` becomes unused; update the `--force` CLI help; CHANGELOG.

---

## Task 1: `merge_claude_md` helper + markers

**Files:** Modify `crates/kimetsu-chat/src/bridge.rs` (add constants + the helper near `CLAUDE_MD_CONTENT` ~line 826; tests in `mod tests`). `temp_root(label)`, `strip_bom`, and `write_text_file` already exist in this file.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests`:

```rust
    #[test]
    fn merge_claude_md_fresh_file() {
        let root = temp_root("claude_md_fresh");
        let p = root.join("CLAUDE.md");
        merge_claude_md(&p).unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(text.contains(CLAUDE_MD_BEGIN));
        assert!(text.contains("# Kimetsu brain"));
        assert!(text.contains(CLAUDE_MD_END));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn merge_claude_md_preserves_user_content() {
        let root = temp_root("claude_md_preserve");
        let p = root.join("CLAUDE.md");
        fs::write(&p, "# My rules\nAlways use tabs.\n").unwrap();
        merge_claude_md(&p).unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(text.contains("# My rules"));
        assert!(text.contains("Always use tabs."));
        assert!(text.contains("# Kimetsu brain"));
        assert!(
            text.find("My rules").unwrap() < text.find(CLAUDE_MD_BEGIN).unwrap(),
            "user content precedes the kimetsu block"
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn merge_claude_md_idempotent() {
        let root = temp_root("claude_md_idem");
        let p = root.join("CLAUDE.md");
        fs::write(&p, "# Mine\n").unwrap();
        merge_claude_md(&p).unwrap();
        merge_claude_md(&p).unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert_eq!(text.matches(CLAUDE_MD_BEGIN).count(), 1, "no duplicate block");
        assert_eq!(text.matches(CLAUDE_MD_END).count(), 1);
        assert!(text.contains("# Mine"));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn merge_claude_md_upgrades_in_place() {
        let root = temp_root("claude_md_upgrade");
        let p = root.join("CLAUDE.md");
        fs::write(
            &p,
            format!("# Top\n\n{CLAUDE_MD_BEGIN}\nOLD STALE\n{CLAUDE_MD_END}\n\n# Bottom\n"),
        )
        .unwrap();
        merge_claude_md(&p).unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(!text.contains("OLD STALE"), "stale block replaced");
        assert!(text.contains("# Kimetsu brain"));
        assert!(text.contains("# Top"));
        assert!(text.contains("# Bottom"));
        assert_eq!(text.matches(CLAUDE_MD_BEGIN).count(), 1);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn merge_claude_md_tolerates_bom() {
        let root = temp_root("claude_md_bom");
        let p = root.join("CLAUDE.md");
        fs::write(&p, format!("\u{feff}# My rules\n")).unwrap();
        merge_claude_md(&p).unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(text.contains("# My rules"));
        assert!(text.contains("# Kimetsu brain"));
        fs::remove_dir_all(root).ok();
    }
```

(Note: `temp_root` already creates the dir, so the pre-seed tests write `CLAUDE.md` directly.)

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p kimetsu-chat merge_claude_md`
Expected: FAIL — `merge_claude_md` / `CLAUDE_MD_BEGIN` not found.

- [ ] **Step 3: Add the markers + helper**

In `bridge.rs`, just above or below the `CLAUDE_MD_CONTENT` constant (~line 826), add:

```rust
const CLAUDE_MD_BEGIN: &str = "<!-- kimetsu:begin -->";
const CLAUDE_MD_END: &str = "<!-- kimetsu:end -->";

/// Merge Kimetsu's guidance block into a `CLAUDE.md` without ever clobbering
/// the user's content. The guidance is wrapped in HTML-comment markers so it
/// can be found and updated idempotently:
///   * missing file        -> write the block
///   * markers absent       -> append the block after the user's content
///   * markers present       -> replace just the marked region (upgrade in place)
/// Used for both the workspace `.claude/CLAUDE.md` and the global
/// `~/.claude/CLAUDE.md`.
fn merge_claude_md(path: &Path) -> Result<(), String> {
    let block = format!("{CLAUDE_MD_BEGIN}\n{CLAUDE_MD_CONTENT}{CLAUDE_MD_END}\n");
    let raw = if path.is_file() {
        fs::read_to_string(path).map_err(|err| format!("read {}: {err}", path.display()))?
    } else {
        String::new()
    };
    let existing = strip_bom(&raw);
    let merged = match (existing.find(CLAUDE_MD_BEGIN), existing.find(CLAUDE_MD_END)) {
        (Some(start), Some(end_start)) if end_start >= start => {
            let end = end_start + CLAUDE_MD_END.len();
            let after = existing[end..].strip_prefix('\n').unwrap_or(&existing[end..]);
            format!("{}{block}{after}", &existing[..start])
        }
        _ => {
            let mut out = existing.to_string();
            if !out.is_empty() {
                if !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push('\n'); // blank line separating user content from our block
            }
            out.push_str(&block);
            out
        }
    };
    write_text_file(path, &merged, true)
}
```

(`CLAUDE_MD_CONTENT` already ends with a newline, so the block's `END` marker lands on its own line.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p kimetsu-chat merge_claude_md`
Expected: PASS (5 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/kimetsu-chat/src/bridge.rs
git commit -m "feat: merge_claude_md — append/upgrade Kimetsu guidance, never clobber"
```

---

## Task 2: Use the merge in `write_claude_settings` (drop the `force` overwrite)

**Files:** Modify `crates/kimetsu-chat/src/bridge.rs` (`write_claude_settings` ~890 + its caller ~495 + `plugin_install_inner`); test in `mod tests`.

- [ ] **Step 1: Write the failing test**

Add to `mod tests`:

```rust
    #[test]
    fn install_preserves_existing_user_claude_md() {
        let root = temp_root("install_claude_md");
        let claude = root.join(".claude");
        fs::create_dir_all(&claude).unwrap();
        fs::write(claude.join("CLAUDE.md"), "# Personal global instructions\nDo X.\n").unwrap();

        let mut files = Vec::new();
        write_claude_settings(&claude, false, &mut files).unwrap();

        let text = fs::read_to_string(claude.join("CLAUDE.md")).unwrap();
        assert!(text.contains("# Personal global instructions"), "user content kept");
        assert!(text.contains("Do X."));
        assert!(text.contains("# Kimetsu brain"), "kimetsu block appended");
        assert!(text.contains(CLAUDE_MD_BEGIN));
        fs::remove_dir_all(root).ok();
    }
```

(`write_claude_settings` is called here with the NEW 3-arg signature — `(claude_dir, proactive, files)` — which is what Step 3 implements, so this won't compile until then.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p kimetsu-chat install_preserves_existing_user_claude_md`
Expected: FAIL — arity mismatch (`write_claude_settings` still takes 4 args).

- [ ] **Step 3: Rewrite the CLAUDE.md handling + drop `force`**

In `write_claude_settings`, change the signature (drop `force`) and the CLAUDE.md block:

```rust
fn write_claude_settings(
    claude_dir: &Path,
    proactive: bool,
    files: &mut Vec<PathBuf>,
) -> Result<(), String> {
    fs::create_dir_all(claude_dir)
        .map_err(|err| format!("create {}: {err}", claude_dir.display()))?;

    // CLAUDE.md: merge our guidance into whatever is there (or create it),
    // never overwriting the user's content. See `merge_claude_md`.
    let claude_md = claude_dir.join("CLAUDE.md");
    merge_claude_md(&claude_md)?;
    files.push(normalize_path(&claude_md));

    let settings = claude_dir.join("settings.json");
    write_claude_hooks(&settings, proactive)?;
    files.push(normalize_path(&settings));
    Ok(())
}
```

Update the caller in `plugin_install_inner` (currently `write_claude_settings(&claude_dir, force, proactive, &mut files)?;`):

```rust
            write_claude_settings(&claude_dir, proactive, &mut files)?;
```

`force` is now unused in `plugin_install_inner`. Confirm by reading the function: if `force` has no other use, rename the parameter in the `plugin_install_inner` signature from `force` to `_force` (the public `plugin_install` wrapper passes it positionally, so the rename is local and avoids an unused-variable warning). If `force` IS still used elsewhere in `plugin_install_inner`, leave the name as-is.

- [ ] **Step 4: Run tests + build**

Run: `cargo test -p kimetsu-chat`
Expected: PASS (new test green; existing Claude install tests still pass — the global-scope test asserts `.claude/CLAUDE.md` exists, which `merge_claude_md` still creates).

Run: `cargo build -p kimetsu-chat`
Expected: clean (no unused-variable warning for `force`).

- [ ] **Step 5: Commit**

```bash
git add crates/kimetsu-chat/src/bridge.rs
git commit -m "feat: install merges CLAUDE.md instead of overwrite/skip; drop force gate"
```

---

## Task 3: `--force` help + CHANGELOG + verification

**Files:** Modify `crates/kimetsu-cli/src/main.rs` (the `--force` arg doc), `CHANGELOG.md`.

- [ ] **Step 1: Update the `--force` help text**

In `PluginInstallArgs` (`main.rs`), the `force` field currently reads:

```rust
    /// Overwrite an existing CLAUDE.md (with `--scope global` this replaces
    /// your global ~/.claude/CLAUDE.md). MCP config, hooks, and generated docs
    /// always refresh idempotently and never need this.
    #[arg(long)]
    force: bool,
```

Replace the doc comment with:

```rust
    /// Retained for compatibility; has no effect. The installer is fully
    /// idempotent and non-destructive — CLAUDE.md guidance is merged (never
    /// overwritten), and hooks / MCP config / generated docs refresh in place.
    #[arg(long)]
    force: bool,
```

- [ ] **Step 2: Update the CHANGELOG**

In `CHANGELOG.md`, the v0.9.0 "Install polish" FIXED bullet currently says `--force` overwrites `CLAUDE.md`. Replace that clause so it reads (keep the rest of the bullet intact):

```markdown
  * **Install polish.** The installer now **merges** Kimetsu's guidance into an
    existing `CLAUDE.md` — workspace `.claude/CLAUDE.md` or the global
    `~/.claude/CLAUDE.md` — inside `<!-- kimetsu:begin -->`/`<!-- kimetsu:end -->`
    markers, appending and upgrading in place, never overwriting the user's
    content. `--force` no longer overwrites `CLAUDE.md` (the whole install is
    idempotent and non-destructive; the flag is retained only for compatibility).
    A `--scope global` on the workspace-only `kimetsu` target warns instead of
    silently doing nothing; `--workspace` is canonicalized leniently so a global
    install doesn't fail on a missing workspace path.
```

(If the existing bullet's exact wording differs, preserve the non-CLAUDE.md sentences and only swap the `--force`/CLAUDE.md clause for the above.)

- [ ] **Step 3: Full verification**

Run: `cargo build --workspace` → clean.
Run: `cargo test --workspace` → all green.
Run: `cargo fmt --all && cargo fmt --all --check` → clean.

- [ ] **Step 4: Manual smoke (existing user CLAUDE.md is preserved)**

```bash
d=$(mktemp -d); dwin=$(cygpath -m "$d")
mkdir -p "$d/.claude"
printf '# My project rules\nUse 2-space indent.\n' > "$d/.claude/CLAUDE.md"
target/debug/kimetsu.exe plugin install claude-code --workspace "$dwin" --no-setup
echo '--- merged CLAUDE.md ---'; cat "$d/.claude/CLAUDE.md"
# Expect: "# My project rules" + "Use 2-space indent." preserved, then the
# <!-- kimetsu:begin --> ... # Kimetsu brain ... <!-- kimetsu:end --> block.
# Re-run is idempotent:
target/debug/kimetsu.exe plugin install claude-code --workspace "$dwin" --no-setup
grep -c 'kimetsu:begin' "$d/.claude/CLAUDE.md"   # -> 1
rm -rf "$d"
```

- [ ] **Step 5: Commit**

```bash
git add crates/kimetsu-cli/src/main.rs CHANGELOG.md
git commit -m "docs: --force is a no-op; document CLAUDE.md merge"
```

---

## Done

`kimetsu plugin install` now merges its guidance into any existing `CLAUDE.md` (workspace or global) inside markers — appending and upgrading idempotently, never clobbering the user's content — and `--force` no longer overwrites it.
