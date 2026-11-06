#!/bin/sh
# ADR 0001 §7 request_pack on two physical LAN hosts.
#
# Server on one PC, client on the other. The client secret stays on the
# client. The file copied to the client is a public handoff. Relay stays
# disabled. Embeddings stay off. The timer stops at verified request_pack.
#
# self-test is loopback on this machine. It prints the §7 row as still
# pending. It is not a LAN measurement. 10.200.0.1 and 10.200.0.2 are the
# veth harness and are rejected here.
#
# Operator page: docs/operators/f3-two-physical-hosts.md
set -eu
umask 077

ROOT=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
cd "$ROOT"

die() {
    printf '%s\n' "$1" >&2
    exit 1
}

usage() {
    cat <<'EOF'
usage:
  scripts/f3-hotpath-two-hosts.sh client-init --state DIR
  scripts/f3-hotpath-two-hosts.sh serve --bind IP --client-public HEX --state DIR [--port N] [--hold-secs N] [--confirm-two-physical-hosts]
  scripts/f3-hotpath-two-hosts.sh fetch --bind IP --peer IP --state DIR --handoff FILE [--port N] [--quic-samples N] [--fetch-timeout-secs N] [--confirm-two-physical-hosts]
  scripts/f3-hotpath-two-hosts.sh self-test [--quic-samples N]
  scripts/f3-hotpath-two-hosts.sh cleanup --state DIR

Paths should be absolute. The script cds to the repository root.
Pass --confirm-two-physical-hosts only when the two processes are on two PCs.
self-test cannot be confirmed and must not be copied into the §7 row.
EOF
}

if [ "$(uname -s)" != "Linux" ]; then
    die "two-physical-host harness requires Linux"
fi

valid_ipv4() {
    printf '%s\n' "$1" | awk -F. '
        NF != 4 { exit 1 }
        {
            for (i = 1; i <= 4; i++) {
                if ($i !~ /^[0-9]+$/) exit 1
                if (($i + 0) > 255) exit 1
            }
        }
    '
}

is_loopback() {
    case $1 in
        127.*) return 0 ;;
        *) return 1 ;;
    esac
}

is_veth() {
    [ "$1" = "10.200.0.1" ] || [ "$1" = "10.200.0.2" ]
}

reject_lan_address() {
    label=$1
    addr=$2
    valid_ipv4 "$addr" || die "$label $addr is not an IPv4 address"
    if is_veth "$addr"; then
        die "refusing veth harness address $addr; those numbers are not a physical LAN"
    fi
    if is_loopback "$addr"; then
        die "refusing loopback address $addr for a two-physical-host run"
    fi
}

need_int() {
    label=$1
    value=$2
    case $value in
        ''|*[!0-9]*) die "$label must be an integer, not $value" ;;
    esac
}

cmd=${1:-}
if [ -z "$cmd" ] || [ "$cmd" = "-h" ] || [ "$cmd" = "--help" ]; then
    usage
    if [ -z "$cmd" ]; then
        exit 1
    fi
    exit 0
fi
shift

STATE=
BIND=
PEER=
PUBLIC=
HANDOFF=
PORT=
HOLD=
SAMPLES=
FETCH_TIMEOUT=
CONFIRM=0
while [ $# -gt 0 ]; do
    case $1 in
        --state)
            STATE=${2:-}
            [ -n "$STATE" ] || die "missing value for --state"
            shift 2
            ;;
        --bind)
            BIND=${2:-}
            [ -n "$BIND" ] || die "missing value for --bind"
            shift 2
            ;;
        --peer)
            PEER=${2:-}
            [ -n "$PEER" ] || die "missing value for --peer"
            shift 2
            ;;
        --client-public)
            PUBLIC=${2:-}
            [ -n "$PUBLIC" ] || die "missing value for --client-public"
            shift 2
            ;;
        --handoff)
            HANDOFF=${2:-}
            [ -n "$HANDOFF" ] || die "missing value for --handoff"
            shift 2
            ;;
        --port)
            PORT=${2:-}
            [ -n "$PORT" ] || die "missing value for --port"
            shift 2
            ;;
        --hold-secs)
            HOLD=${2:-}
            [ -n "$HOLD" ] || die "missing value for --hold-secs"
            shift 2
            ;;
        --quic-samples)
            SAMPLES=${2:-}
            [ -n "$SAMPLES" ] || die "missing value for --quic-samples"
            shift 2
            ;;
        --fetch-timeout-secs)
            FETCH_TIMEOUT=${2:-}
            [ -n "${FETCH_TIMEOUT}" ] || die "missing value for --fetch-timeout-secs"
            shift 2
            ;;
        --confirm-two-physical-hosts)
            CONFIRM=1
            shift
            ;;
        *)
            die "unknown argument $1"
            ;;
    esac
done

if [ -n "$PORT" ]; then
    need_int "--port" "$PORT"
    [ "$PORT" -le 65535 ] || die "--port must be 0..=65535, not $PORT"
fi
if [ -n "$HOLD" ]; then
    need_int "--hold-secs" "$HOLD"
    [ "$HOLD" -ge 30 ] && [ "$HOLD" -le 1800 ] || die "--hold-secs must be 30..=1800, not $HOLD"
fi
if [ -n "$SAMPLES" ]; then
    need_int "--quic-samples" "$SAMPLES"
    [ "$SAMPLES" -ge 1 ] || die "--quic-samples must be at least 1"
fi
if [ -n "$FETCH_TIMEOUT" ]; then
    need_int "--fetch-timeout-secs" "$FETCH_TIMEOUT"
    [ "$FETCH_TIMEOUT" -ge 5 ] && [ "$FETCH_TIMEOUT" -le 300 ] || die "--fetch-timeout-secs must be 5..=300, not $FETCH_TIMEOUT"
fi

commit=$(git -C "$ROOT" rev-parse HEAD 2>/dev/null || printf '%s' unknown)
rustc_line=$(rustup run stable rustc --version 2>/dev/null || rustc --version 2>/dev/null || printf '%s' unknown)

prepare_bin() {
    if [ -n "${F3_HOTPATH_BIN:-}" ]; then
        BIN=$F3_HOTPATH_BIN
        return
    fi
    profile=${F3_CARGO_PROFILE:-release}
    case $profile in
        release)
            cargo +stable build -p kimetsu-bus --example f3_hotpath --release
            BIN=$ROOT/target/release/examples/f3_hotpath
            ;;
        debug)
            cargo +stable build -p kimetsu-bus --example f3_hotpath
            BIN=$ROOT/target/debug/examples/f3_hotpath
            ;;
        *)
            die "F3_CARGO_PROFILE must be release or debug"
            ;;
    esac
}

export KIMETSU_BRAIN_EMBEDDER=off
export KIMETSU_USER_BRAIN=0
export KIMETSU_BRAIN_AMBIENT=off

assert_self_test_output() {
    file=$1
    if grep -F 'topology: two physical LAN hosts' "$file" >/dev/null 2>&1; then
        die "self-test output claimed two physical LAN hosts"
    fi
    if grep -F 'two physical LAN hosts: measured' "$file" >/dev/null 2>&1; then
        die "self-test output claimed a two-physical-host measurement"
    fi
    if ! grep -F 'two physical LAN hosts: still pending. This run is not direct LAN QUIC between two LAN machines.' "$file" >/dev/null 2>&1; then
        die "self-test output missing the pending line"
    fi
}

run_self_test() {
    if [ "$CONFIRM" -eq 1 ]; then
        die "refusing --confirm-two-physical-hosts on a self-test"
    fi
    if [ -n "$BIND" ] || [ -n "$PEER" ] || [ -n "$STATE" ] || [ -n "$PUBLIC" ] || [ -n "$HANDOFF" ]; then
        die "self-test takes only --quic-samples"
    fi
    samples=${SAMPLES:-2}
    prepare_bin
    root=$(mktemp -d "${TMPDIR:-/tmp}/kimetsu-f3-twohost.XXXXXX")
    trap 'rm -rf "$root"' EXIT INT TERM
    client=$root/client
    server=$root/server
    "$BIN" --role client-init --state "$client" >"$root/client.out"
    public=$(sed -n 's/^client_public: //p' "$root/client.out" | head -n 1)
    [ -n "$public" ] || die "client-init did not print client_public"
    mode=$(stat -c %a "$client/client.secret")
    [ "$mode" = "600" ] || die "client secret mode is $mode, want 600"
    "$BIN" --role serve-public --self-test --bind 127.0.0.2 --client-public "$public" \
        --state "$server" --hold-secs 90 --commit "$commit" --rustc "$rustc_line" \
        >"$root/serve.out" 2>"$root/serve.err" &
    pid=$!
    i=0
    while [ ! -f "$server/handoff.json" ]; do
        if [ "$i" -ge 300 ]; then
            kill "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
            printf '%s\n' "--- serve stderr ---" >&2
            cat "$root/serve.err" >&2 || true
            die "server did not become ready within 30s"
        fi
        if ! kill -0 "$pid" 2>/dev/null; then
            printf '%s\n' "--- serve stderr ---" >&2
            cat "$root/serve.err" >&2 || true
            wait "$pid" 2>/dev/null || true
            die "server exited before ready"
        fi
        i=$((i + 1))
        sleep 0.1
    done
    set +e
    "$BIN" --role fetch-public --self-test --bind 127.0.0.1 --peer 127.0.0.2 \
        --state "$client" --handoff "$server/handoff.json" --quic-samples "$samples" \
        --fetch-timeout-secs 120 --commit "$commit" --rustc "$rustc_line" \
        >"$root/fetch.out" 2>"$root/fetch.err"
    status=$?
    set -e
    touch "$server/done"
    i=0
    while kill -0 "$pid" 2>/dev/null; do
        if [ "$i" -ge 50 ]; then
            kill "$pid" 2>/dev/null || true
            break
        fi
        i=$((i + 1))
        sleep 0.1
    done
    wait "$pid" 2>/dev/null || true
    if [ "$status" -ne 0 ]; then
        printf '%s\n' "--- serve stderr ---" >&2
        cat "$root/serve.err" >&2 || true
        printf '%s\n' "--- fetch stderr ---" >&2
        cat "$root/fetch.err" >&2 || true
        exit "$status"
    fi
    secret=$(tr -d ' \n' <"$client/client.secret")
    [ -n "$secret" ] || die "client secret is empty"
    if grep -R -F -q -- "$secret" "$server"; then
        die "server state contains the client secret"
    fi
    assert_self_test_output "$root/fetch.out"
    printf '%s\n' "self-test only. This is not a two-physical-host measurement and must not be copied into the §7 row."
    printf '%s\n' "--- server stdout ---"
    cat "$root/serve.out"
    printf '%s\n' "--- client stdout ---"
    cat "$root/fetch.out"
}

case $cmd in
    client-init)
        [ -n "$STATE" ] || die "client-init requires --state"
        [ -z "$BIND" ] && [ -z "$PEER" ] && [ -z "$PUBLIC" ] && [ "$CONFIRM" -eq 0 ] || die "client-init only takes --state"
        prepare_bin
        exec "$BIN" --role client-init --state "$STATE"
        ;;
    serve)
        [ -n "$STATE" ] && [ -n "$BIND" ] && [ -n "$PUBLIC" ] || die "serve requires --bind, --client-public, and --state"
        [ -z "$PEER" ] && [ -z "$HANDOFF" ] || die "serve does not take --peer or --handoff"
        reject_lan_address "--bind" "$BIND"
        prepare_bin
        set -- --role serve-public --bind "$BIND" --client-public "$PUBLIC" --state "$STATE" --commit "$commit" --rustc "$rustc_line"
        if [ -n "$PORT" ]; then
            set -- "$@" --port "$PORT"
        fi
        if [ -n "$HOLD" ]; then
            set -- "$@" --hold-secs "$HOLD"
        fi
        if [ "$CONFIRM" -eq 1 ]; then
            set -- "$@" --confirm-two-physical-hosts
        fi
        exec "$BIN" "$@"
        ;;
    fetch)
        [ -n "$STATE" ] && [ -n "$BIND" ] && [ -n "$PEER" ] && [ -n "$HANDOFF" ] || die "fetch requires --bind, --peer, --state, and --handoff"
        [ -z "$PUBLIC" ] || die "fetch does not take --client-public"
        reject_lan_address "--bind" "$BIND"
        reject_lan_address "--peer" "$PEER"
        [ "$BIND" != "$PEER" ] || die "refusing identical bind and peer $BIND; a two-host run needs two addresses"
        [ -f "$HANDOFF" ] || die "handoff $HANDOFF is missing"
        prepare_bin
        set -- --role fetch-public --bind "$BIND" --peer "$PEER" --state "$STATE" --handoff "$HANDOFF" --commit "$commit" --rustc "$rustc_line"
        if [ -n "$PORT" ]; then
            set -- "$@" --port "$PORT"
        fi
        if [ -n "$SAMPLES" ]; then
            set -- "$@" --quic-samples "$SAMPLES"
        fi
        if [ -n "$FETCH_TIMEOUT" ]; then
            set -- "$@" --fetch-timeout-secs "$FETCH_TIMEOUT"
        fi
        if [ "$CONFIRM" -eq 1 ]; then
            set -- "$@" --confirm-two-physical-hosts
        fi
        exec "$BIN" "$@"
        ;;
    self-test)
        run_self_test
        ;;
    cleanup)
        [ -n "$STATE" ] || die "cleanup requires --state"
        [ -z "$BIND" ] && [ -z "$PEER" ] && [ "$CONFIRM" -eq 0 ] || die "cleanup only takes --state"
        case $STATE in
            /|.|..) die "refusing to delete $STATE" ;;
        esac
        [ -d "$STATE" ] || die "state directory $STATE does not exist"
        marker=$STATE/kimetsu-f3-two-host-state
        [ -f "$marker" ] || die "refusing to delete $STATE: missing kimetsu-f3-two-host-state marker"
        body=$(tr -d ' \n' <"$marker")
        [ "$body" = "kimetsu-f3-two-host-state" ] || die "refusing to delete $STATE: marker mismatch"
        rm -rf "$STATE"
        printf '%s\n' "removed $STATE"
        ;;
    *)
        die "unknown command $cmd"
        ;;
esac
