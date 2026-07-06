# Kimetsu demo — render with charmbracelet/vhs:
#
#   brew install vhs        # macOS        (needs ffmpeg + ttyd)
#   scoop install vhs       # Windows
#
#   From the repo root, with the current `kimetsu` on PATH:
#   vhs docs/assets/demo.tape     # writes docs/assets/demo.gif
#
# The tape runs REAL commands — kimetsu must be an embeddings build
# (`kimetsu --version` must print "(embeddings)"). Re-record after UX changes.

Output docs/assets/demo.gif

Set Shell "bash"
Set Theme "Catppuccin Mocha"
Set FontSize 13
Set Width 1100
Set Height 640
Set Padding 16
Set Margin 18
Set MarginFill "#0d1117"
Set BorderRadius 10
Set WindowBar Colorful
Set WindowBarSize 36
Set TypingSpeed 30ms
Set PlaybackSpeed 2.5

# ── Hidden setup: a fresh scratch project ────────────────────────────────────
Hide
Type "cd $(mktemp -d); mkdir kimetsu-demo; cd kimetsu-demo; clear"
Enter
Sleep 2s
Show

# ── Beat 1: the version + one command to wire the brain ──────────────────────
Type "kimetsu --version"
Enter
Sleep 3s

Type "kimetsu setup --host claude-code --no-setup"
Enter
# init + plugin install + selftest (--no-setup skips the interactive prompt)
Sleep 40s

Sleep 1s
Type "clear"
Enter

# ── Beat 2: record a lesson the way an agent would ───────────────────────────
Type `kimetsu brain memory add --scope project --kind failure_pattern "Tests pass alone but fail under 'cargo test --workspace': a new crate with default embeddings flips feature unification for every sibling crate. Keep new crates' default = [] and opt in explicitly."`
Enter
Sleep 16s

# ── Beat 3: retrieve it with DIFFERENT words — semantic recall ───────────────
Type `kimetsu brain context "why does my test only break in the full CI run?"`
Enter
Sleep 25s

Sleep 1s
Type "clear"
Enter

# ── Beat 4: retrieval depth is one dial ──────────────────────────────────────
Type "kimetsu config set retrieval.level advanced"
Enter
Sleep 6s

# ── Beat 5: share the brain — a scrubbed, portable pack ─────────────────────
Type `kimetsu brain export team-brain.json.gz --name onboarding --version 1.0.0`
Enter
Sleep 12s

Sleep 4s
