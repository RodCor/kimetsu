#!/usr/bin/env bash
# Point git at the repo's tracked hooks. Run once after cloning:
#     ./scripts/install-hooks.sh
set -euo pipefail
cd "$(dirname "$0")/.."
git config core.hooksPath .githooks
chmod +x .githooks/* 2>/dev/null || true
echo "Installed: core.hooksPath -> .githooks"
echo "Pre-commit will now run fmt + clippy on staged Rust changes."
