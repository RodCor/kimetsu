#!/bin/sh
# Re-run the ADR 0001 F3 hot-path harness. Release, no embeddings feature.
# The request_pack p99 it prints is loopback (127.0.0.1, both endpoints on
# this host). scripts/f3-hotpath-lan.sh times the same pack across a veth
# pair (two Linux netns on one physical host). ADR §7's direct LAN QUIC
# measurement between two LAN machines is still pending. Run that on two
# PCs with scripts/f3-hotpath-two-hosts.sh. This script does not produce
# that number.
set -eu
cd "$(dirname "$0")/.."
exec cargo run -p kimetsu-bus --example f3_hotpath --release -- "$@"
