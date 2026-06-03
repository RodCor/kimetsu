# Merge Kimetsu guidance into CLAUDE.md (append, never replace)

**Date:** 2026-06-02
**Status:** Approved (pending spec review)
**Builds on:** the v0.9.0 install line (`write_claude_settings` in `bridge.rs`)

## Context

`kimetsu plugin install claude-code` writes a `CLAUDE.md` with Kimetsu's brain
guidance. Today (`bridge.rs` `write_claude_settings`):
```rust
let claude_md = claude_dir.join("CLAUDE.md");
if !claude_md.is_file() || force {
    write_text_file(&claude_md, CLAUDE_MD_CONTENT, true)?;  // whole-file overwrite
}
```
- Missing → writes Kimetsu's content (fine).
- Exists, `--force` → **overwrites the whole file**, destroying the user's content.
- Exists, no force → skips, so Kimetsu's guidance never lands in an existing file.

For a **global** install (`claude_dir = ~/.claude`), `~/.claude/CLAUDE.md` is the
user's personal cross-project instructions — clobbering it (or never adding the
guidance) is both destructive and useless. The fix is the same "merge, never
clobber" principle already used for hooks: **append** Kimetsu's guidance,
preserving all user content, idempotently.

## Decisions (settled in brainstorming)

- **Marker-delimited block**, merged append-or-replace-in-place (idempotent +
  upgradeable on re-run). Preferred over plain append (no idempotency) or a
  separate file (Claude Code wouldn't load it).
- **Applies to both** workspace (`<ws>/.claude/CLAUDE.md`) and global
  (`~/.claude/CLAUDE.md`) — the merge is universal and never destructive.
- **`--force` no longer overwrites CLAUDE.md** (the merge is always safe). The
  flag stays accepted for back-compat but no longer affects CLAUDE.md.

## Components & files

### 1. Marker block + `merge_claude_md` (`crates/kimetsu-chat/src/bridge.rs`)
Add markers and a merge helper:
```rust
const CLAUDE_MD_BEGIN: &str = "<!-- kimetsu:begin -->";
const CLAUDE_MD_END: &str = "<!-- kimetsu:end -->";
```
The Kimetsu block written into the file is
`"{BEGIN}\n{CLAUDE_MD_CONTENT}{END}\n"` (CLAUDE_MD_CONTENT already ends with a
newline). `merge_claude_md(path)`:
1. Read the existing file (or `""` when missing); strip a leading UTF-8 BOM
   (reuse `strip_bom`) before scanning, for parity with the other readers.
2. If BOTH `BEGIN` and `END` markers are present → **replace just the
   `BEGIN..=END` region** with the fresh block, preserving everything before
   `BEGIN` and after `END` (collapsing a single following newline so re-runs
   don't grow blank lines).
3. Else → **append**: ensure the existing content ends with a newline, add a
   blank-line separator (only when there is existing content), then the block.
4. Write the result via `write_text_file(path, &merged, true)`.

Best-effort: a read error (unreadable existing file) surfaces as the install
error, same as the other config writers.

### 2. Use the merge in `write_claude_settings`
Replace the `if !claude_md.is_file() || force { write_text_file(...) }` block with
`merge_claude_md(&claude_md)?;` (still `files.push(normalize_path(&claude_md))`).
`write_claude_settings`'s `force` parameter is then unused → **remove it from the
function signature** and update the one caller (`plugin_install_inner`).

### 3. `force` becomes a no-op for install (`crates/kimetsu-cli/src/main.rs`)
After §2, `force` is no longer used anywhere in `plugin_install_inner` (MCP/codex
writers are idempotent; generated docs always refresh). Keep `force` on the public
`plugin_install`/`plugin_install_inner` signatures + the CLI `--force` flag + the
MCP `force` arg for back-compat, but it has no effect. Add `let _ = force;` (with
a one-line comment) in `plugin_install_inner` to avoid an unused-variable warning.
Update the `--force` CLI help text: it no longer overwrites `CLAUDE.md`; the
install is fully non-destructive and idempotent, and the flag is retained only for
compatibility.

### 4. Docs
Update the v0.9.0 CHANGELOG "Install polish" line: `--force` no longer overwrites
`CLAUDE.md`; instead the installer **merges** Kimetsu's guidance into an existing
`CLAUDE.md` (workspace or global `~/.claude/CLAUDE.md`) inside `<!-- kimetsu:begin
-->`/`<!-- kimetsu:end -->` markers — appending, never replacing, idempotent on
re-run.

## Data flow

```
install (workspace or --scope global) → write_claude_settings(claude_dir)
  → merge_claude_md(<claude_dir>/CLAUDE.md)
      missing      → write the marked block
      no markers   → append the marked block after the user's content
      has markers  → replace just the marked region (idempotent / upgrade)
```

## Error handling

Best-effort/non-destructive: a missing file is created; an unreadable existing
file surfaces the read error (install reports it) rather than overwriting. The
merge never touches bytes outside the markers.

## Testing (`bridge.rs` `mod tests`)

- **Fresh file:** `merge_claude_md` on a non-existent path writes a file
  containing `BEGIN`, the guidance, and `END`.
- **Preserve user content:** seed `CLAUDE.md` with `# My rules\n...`; after merge,
  the user text is intact AND the Kimetsu block is appended below it.
- **Idempotent re-run:** merge twice → exactly one `BEGIN` and one `END`
  (`matches().count() == 1`), user content unchanged.
- **Upgrade in place:** seed a file whose Kimetsu block has stale content between
  the markers; after merge the region equals the current block and the user
  content around it is preserved.
- **Install path:** `write_claude_settings` on a `claude_dir` with a pre-existing
  user `CLAUDE.md` preserves it and adds the block (no `force` param).
- **BOM:** a user `CLAUDE.md` saved with a leading BOM merges without error.

## Risks / trade-offs

- **`--force` semantics change** (no longer clobbers CLAUDE.md) — intended; the
  flag becomes a documented no-op for install. No other behavior depended on it.
- **Marker collision** — if a user already wrote literal `<!-- kimetsu:begin -->`
  in their CLAUDE.md, the merge would treat it as ours. Extremely unlikely;
  accepted.

## Out of scope / follow-ups

- Codex `AGENTS.md`/skill guidance merge (separate surface; the Codex skill file
  is Kimetsu-owned and fully overwritten — not user content).
- A `kimetsu plugin uninstall` that strips the marked block.
