#!/bin/sh
# Re-run the ADR 0001 F3 hot-path harness. Release, no embeddings feature.
# The request_pack p99 it prints is loopback (127.0.0.1, both endpoints on
# this host). ADR §7's direct LAN QUIC measurement is still pending. This
# script does not produce a LAN number.
set -eu
cd "$(dirname "$0")/.."
exec cargo run -p kimetsu-bus --example f3_hotpath --release -- "$@"
