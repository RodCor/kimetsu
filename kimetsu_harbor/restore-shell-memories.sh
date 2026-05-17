#!/bin/bash
# MP-17j — re-add the 3 generic shell-workflow memories that MP-17b
# wrongly retired. The MP-17 brain-only gauntlet at 21:32Z showed a
# 7→5 wins regression vs MP-14d, with the 3 newly-lost tasks
# (overfull-hbox, compile-compcert, log-summary-date-ranges) being
# exactly the workflow shapes the retired capsules used to cover.
#
# These memories sit ALONGSIDE the MP-17b tool-aware capsules — they
# don't compete, they complement: MP-17b says "when to use which
# typed tool"; these say "how to actually do the underlying work."
#
# Idempotent: dedup at add_memory time (MP-17 #14) won't matter here
# because the retired rows are tombstoned (invalidated_at IS NOT NULL)
# and the dedup query filters on active rows only — so this will
# create NEW active rows. Running this twice creates duplicates;
# don't.

set -uo pipefail

PROJECT="${KIMETSU_HARBOR_PROJECT:-/home/kimetsu/kimetsu-bench-project}"
BIN="${KIMETSU_BIN:-/mnt/e/Kimetsu/target/release/kimetsu}"

if [[ ! -d "$PROJECT/.kimetsu" ]]; then
    echo "PROJECT=$PROJECT has no .kimetsu/ subdir; aborting" >&2
    exit 1
fi
if [[ ! -x "$BIN" ]]; then
    echo "kimetsu binary not found at $BIN" >&2
    exit 1
fi

cd "$PROJECT"
echo "[$(date -u +%H:%M:%SZ)] restoring shell-workflow capsules at $PROJECT"

add() {
    local kind="$1"
    local text="$2"
    echo "  + $kind: ${text:0:80}..."
    "$BIN" brain memory add --scope global_user --kind "$kind" "$text" 2>&1 | sed 's/^/    /' | head -2
}

# 1. Build-log redirect pattern. The MP-17b retire reason said "use
#    shell_background" but that only addresses LONG-running builds.
#    For most builds the existing "redirect to /tmp/build.log + tail"
#    pattern is cheaper than spawning a background process AND polling.
add preference \
"For builds (make, cargo build, npm install), redirect stdout+stderr to /tmp/build.log and tail the last 100 lines. Don't try to print the full build log back through the agent context — it blows the token budget. For builds expected to take >5 minutes, use shell_background instead so you can do other work while it runs."

# 2. In-place edits. The MP-17b retire reason said "use edit_file"
#    but edit_file is for SPECIFIC string substitution. For
#    regex/pattern-based edits, sed is still the right tool, and the
#    here-doc pattern is still the simplest way to create new files
#    from a shell context.
add preference \
"For specific line replacement use edit_file (hash-checked, simpler). For regex/pattern-based in-place edits, use sed -i 's/pattern/replacement/g' path. For new files, write_file (typed) or cat > path <<'EOF' ... EOF in shell_command both work — pick whichever fits the calling context. Don't use vim or less in non-interactive shells (they expect a TTY)."

# 3. Workspace orientation. The MP-17b retire reason said "use
#    list_files + multi_read" but the explicit shell orientation
#    chain is FASTER (one shell call vs N typed tool calls) AND
#    surfaces specific build-system markers the model can pattern-match
#    on. Keep both: use list_files for general layout, use this for
#    targeted build detection.
add convention \
"For unfamiliar build tasks, run this orientation chain in one shell_command first: 'ls -la; cat README* INSTRUCTIONS* INSTALL* 2>/dev/null; find . -maxdepth 2 -name Makefile -o -name CMakeLists.txt -o -name setup.py -o -name pyproject.toml -o -name package.json -o -name Cargo.toml 2>/dev/null'. This single shell call is cheaper than running list_files + multi_read for the same orientation."

echo
echo "[$(date -u +%H:%M:%SZ)] restore complete. Active capsules now:"
total=$("$BIN" brain memory list 2>&1 | grep -cE '^[A-Z0-9]{20,}')
echo "  total in list: $total"
echo "  (active = total - invalidated; the retrieval pipeline filters tombstones)"
