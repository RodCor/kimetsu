# Kimetsu demo — render with charmbracelet/vhs:
#
#   brew install vhs        # macOS        (needs ffmpeg + ttyd)
#   scoop install vhs       # Windows
#   go install github.com/charmbracelet/vhs@latest
#
#   cd a scratch directory with `kimetsu` on PATH, then:
#   vhs path/to/demo.tape   # writes docs/assets/demo.gif
#
# The tape runs REAL commands — kimetsu must be installed (the embeddings
# build makes the semantic-recall beat land). Re-record after big UX changes.

Output docs/assets/demo.gif

Set Theme "Catppuccin Mocha"
Set FontSize 12
Set Width 1100
Set Height 620
Set Padding 12
Set TypingSpeed 40ms

# ── Hidden setup: a fresh scratch project ────────────────────────────────────
Hide
Type "mkdir kimetsu-demo"
Enter
Type "cd kimetsu-demo"
Enter
Type "git init -q"
Enter
Type "clear"
Enter
Sleep 2s
Show

# ── Beat 1: one command to prove the brain works ─────────────────────────────
Type "kimetsu setup --host claude-code"
Enter
Sleep 6s

Type "kimetsu doctor --selftest"
Enter
Sleep 5s
# prints: ✓ recorded a memory and retrieved it — the brain works

Sleep 1s
Type "clear"
Enter

# ── Beat 2: record a lesson the way an agent would ───────────────────────────
Type `kimetsu brain memory add --scope project --kind failure_pattern "Tests pass alone but fail under 'cargo test --workspace': a new crate with default embeddings flips feature unification for every sibling crate. Keep new crates' default = [] and opt in explicitly."`
Enter
Sleep 3s

# ── Beat 3: retrieve it with DIFFERENT words — semantic recall ───────────────
Type `kimetsu brain context "why does my test only break in the full CI run?"`
Enter
Sleep 6s

# ── Beat 4: the brain measures itself ────────────────────────────────────────
Type "kimetsu brain insights"
Enter
Sleep 5s

Sleep 3s
