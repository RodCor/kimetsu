#!/bin/sh
# Re-run the ADR 0001 F3 hot-path harness. Release, no embeddings feature.
set -eu
cd "$(dirname "$0")/.."
exec cargo run -p kimetsu-bus --example f3_hotpath --release -- "$@"
