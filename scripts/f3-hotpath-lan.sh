#!/bin/sh
# ADR 0001 F3 request_pack over a veth pair.
#
# Topology: two Linux netns connected by a veth pair on one physical host
# (10.200.0.1 <-> 10.200.0.2). This is not two LAN hosts and it does not
# fill the §7 "direct LAN QUIC between two LAN machines" row. That
# measurement is still pending.
#
# Same build as scripts/f3-hotpath.sh: release, embeddings feature off.
# The example also sets KIMETSU_BRAIN_EMBEDDER=off. Needs root or
# CAP_NET_ADMIN to create the namespaces and the veth pair.
set -eu
cd "$(dirname "$0")/.."

TOPOLOGY="two Linux netns connected by a veth pair on one physical host"
ADDR_A=10.200.0.1
ADDR_B=10.200.0.2
suffix=$(od -An -N2 -tx1 /dev/urandom | tr -d ' \n')
NS_A="kf3a-$suffix"
NS_B="kf3b-$suffix"
VETH_A="k3a$suffix"
VETH_B="k3b$suffix"
SESSION=$(mktemp -d "${TMPDIR:-/tmp}/kimetsu-f3-veth.XXXXXX")

cleanup() {
    sudo -n ip netns del "$NS_A" 2>/dev/null || true
    sudo -n ip netns del "$NS_B" 2>/dev/null || true
    sudo -n ip link del "$VETH_A" 2>/dev/null || true
    rm -rf "$SESSION"
}
trap cleanup EXIT

if ! sudo -n ip -V >/dev/null 2>&1; then
    echo "iproute2 via sudo is required to create network namespaces" >&2
    exit 1
fi

echo "f3 veth harness"
echo "date: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "rustc: $(rustc --version)"
echo "topology: $TOPOLOGY"
echo "addresses: $ADDR_A <-> $ADDR_B"

cargo build -p kimetsu-bus --example f3_hotpath --release
BIN="$(pwd)/target/release/examples/f3_hotpath"

sudo -n ip netns add "$NS_A"
sudo -n ip netns add "$NS_B"
sudo -n ip link add "$VETH_A" type veth peer name "$VETH_B"
sudo -n ip link set "$VETH_A" netns "$NS_A"
sudo -n ip link set "$VETH_B" netns "$NS_B"
sudo -n ip netns exec "$NS_A" ip addr add "$ADDR_A/24" dev "$VETH_A"
sudo -n ip netns exec "$NS_A" ip link set "$VETH_A" up
sudo -n ip netns exec "$NS_A" ip link set lo up
sudo -n ip netns exec "$NS_B" ip addr add "$ADDR_B/24" dev "$VETH_B"
sudo -n ip netns exec "$NS_B" ip link set "$VETH_B" up
sudo -n ip netns exec "$NS_B" ip link set lo up

if command -v ping >/dev/null 2>&1; then
    sudo -n ip netns exec "$NS_A" ping -c 1 -W 2 "$ADDR_B" >&2
fi

sudo -n ip netns exec "$NS_A" "$BIN" \
    --role serve \
    --bind "$ADDR_A" \
    --session "$SESSION" \
    >"$SESSION/serve.out" 2>"$SESSION/serve.err" &
SERVE_PID=$!

set +e
sudo -n ip netns exec "$NS_B" "$BIN" \
    --role fetch \
    --bind "$ADDR_B" \
    --peer "$ADDR_A" \
    --session "$SESSION" \
    --topology "$TOPOLOGY" \
    --quic-samples 30 \
    --out "$SESSION/report.txt"
FETCH_STATUS=$?
set -e

# fetch writes `done` as root. Create it only if that write did not happen,
# so a failed fetch still unblocks serve.
if [ ! -e "$SESSION/done" ]; then
    sudo -n touch "$SESSION/done"
fi
wait "$SERVE_PID" || true

if [ "$FETCH_STATUS" -ne 0 ]; then
    echo "fetch exited $FETCH_STATUS" >&2
    echo "--- serve stderr ---" >&2
    cat "$SESSION/serve.err" >&2 || true
    echo "--- fetch left no report ---" >&2
    exit "$FETCH_STATUS"
fi
