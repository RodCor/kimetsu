# Two physical hosts: `request_pack` measurement

This is the operator procedure for the ADR 0001 §7 direct-LAN measurement. It times `request_pack` until the signed pack is verified in memory. It does not import, it does not rebuild FTS, and it does not turn embeddings on.

The §7 two-physical-host row stays empty until you paste stdout from two real PCs. Loopback (`scripts/f3-hotpath.sh`), the veth pair (`scripts/f3-hotpath-lan.sh`), and this script's `self-test` are not that row. `10.200.0.1` and `10.200.0.2` belong to the veth harness. The tool rejects them here. An internet/WAN run is a different command and a different label: [f3-internet-wan.md](f3-internet-wan.md). It does not fill this row.

## What you need

- Two Linux PCs on the same LAN. Each has its own `/etc/machine-id` and its own IPv4 address. Cloned images that share a machine id are refused.
- The same git commit on both. `rustup` stable, new enough for `kimetsu-bus` (1.91 or newer). The script calls `cargo +stable`.
- A UDP path between the two addresses. Relay is disabled, so there is no fallback if UDP is blocked.
- No source edits. Addresses, ports, and the client public key are arguments.

Build, from the repo root, is what the script runs. Release, embeddings feature off:

```text
cargo +stable build -p kimetsu-bus --example f3_hotpath --release
```

Do not pass `--features embeddings`. The script also sets `KIMETSU_BRAIN_EMBEDDER=off`, `KIMETSU_USER_BRAIN=0`, and `KIMETSU_BRAIN_AMBIENT=off`.

Use absolute paths for `--state` and `--handoff`. The script changes to the repo root before it builds.

## Keys

The client generates its secret and keeps it. The server generates its own secret, the pack signer, and the roster admin, and keeps those in memory. They are not written down.

You copy two public things:

1. The `client_public` line, from the client PC to the server command.
2. `handoff.json`, from the server PC to the client PC. That file is a signed roster, the server endpoint id, the server address and port, the pack digest, and the server's hostname, machine id, commit, rustc, OS, and CPU. It has no private key.

Both endpoints are on that static roster before the dial. A key that is not on it does not finish `request_pack`. `BusRelay::Disabled` stays set. The timer is `fetch_verified`: signature check included, import excluded.

## Firewall

QUIC is UDP. Pick a server port with `--port` so the rule is stable. Allow UDP from the client address to that port on the server. A stateful client firewall usually allows the reply. A stateless one also needs the client's `--port` open. There is no relay to punch through a block.

The printed `bind` line is the address that was actually bound. If it does not match the address you passed, the run stops.

## Commands

On the client PC:

```sh
scripts/f3-hotpath-two-hosts.sh client-init --state /var/tmp/kimetsu-f3-client
```

Leave `client.secret` on this PC. Copy only the `client_public` value.

On the server PC, with that public key and this PC's LAN address:

```sh
scripts/f3-hotpath-two-hosts.sh serve \
  --bind 192.168.1.10 \
  --port 44144 \
  --client-public <client_public> \
  --state /var/tmp/kimetsu-f3-server \
  --confirm-two-physical-hosts
```

Wait until stdout says `ready: yes`. Copy `/var/tmp/kimetsu-f3-server/handoff.json` to the client. Leave the server process running. It stops when `--hold-secs` elapses (default 600, allowed 30 through 1800) or when you create `done` inside the state directory.

On the client PC:

```sh
scripts/f3-hotpath-two-hosts.sh fetch \
  --bind 192.168.1.20 \
  --peer 192.168.1.10 \
  --port 44145 \
  --state /var/tmp/kimetsu-f3-client \
  --handoff /var/tmp/kimetsu-f3-client/handoff.json \
  --confirm-two-physical-hosts
```

`--peer` is the server address you passed to `--bind` on the server. The port comes from the handoff, which is the port the server actually bound. Save the client stdout and the server stdout.

`--confirm-two-physical-hosts` is required on a real run. The script refuses it for `self-test`, for loopback, for the veth addresses, when both ends are the same address, when both machine ids match, and on a debug build.

## Fields

Client stdout carries the same measurement fields as the veth row, plus both hosts:

- `n`, `p50`, `p99`, `max`
- pack gzip bytes and memory count
- `bind` and `peer`
- `relay disabled`
- server and client hardware, hostname, machine id, commit, rustc, and OS
- `build_profile: release`
- `embeddings: off`
- import and FTS rebuild are outside the timer

A confirmed run ends with `two physical LAN hosts: measured on the server and client blocks above`. A self-test ends with `two physical LAN hosts: still pending`.

## Example only

The block below is shape, not a measurement. Do not copy it into the §7 table.

```text
EXAMPLE ONLY — not a measurement

topology: two physical LAN hosts
server_hostname: EXAMPLE-pc-a
client_hostname: EXAMPLE-pc-b
server_machine_id: EXAMPLE-machine-a
client_machine_id: EXAMPLE-machine-b
bind: EXAMPLE-client:port
peer: EXAMPLE-server:port
request_pack direct QUIC until verified in memory: two physical LAN hosts. n=EXAMPLE p50=EXAMPLE ms p99=EXAMPLE ms max=EXAMPLE ms
pack: EXAMPLE gzip bytes, 8 memories, relay disabled
build_profile: release
embeddings: off
import and FTS rebuild: not included in the request_pack timer
two physical LAN hosts: measured on the server and client blocks above
```

## Self-test

On one Linux machine, this checks the script and the binary without claiming a LAN result:

```sh
F3_CARGO_PROFILE=debug scripts/f3-hotpath-two-hosts.sh self-test --quic-samples 2
```

It binds `127.0.0.1` and `127.0.0.2`, prints `still pending`, and deletes its temporary state. The banner says it must not be copied into the §7 row.

`python3 scripts/test-f3-two-hosts.py` runs the script's failure cases. Set `F3_TWO_HOST_SMOKE=1` to add the debug self-test and the binary's refusal paths.

## After a real run

Follow the steps in [0001-f3-measurements.md](../adr/0001-f3-measurements.md) under "Two physical hosts". Paste the client stdout only when `topology` is `two physical LAN hosts`, the machine ids differ, and `build_profile` is `release`. Update that row alone. Leave the loopback and veth rows alone.

## Cleanup

State directories hold the client secret and the handoff. Remove only a directory this tool created:

```sh
scripts/f3-hotpath-two-hosts.sh cleanup --state /var/tmp/kimetsu-f3-client
scripts/f3-hotpath-two-hosts.sh cleanup --state /var/tmp/kimetsu-f3-server
```

Cleanup refuses a directory that lacks the `kimetsu-f3-two-host-state` marker, and it refuses `/`. The server's private keys were never in that directory. The client's secret was. Delete it when you are done.

## When it stops

A missing handoff, a bad address, a secret other people can read (mode broader than `0600`), a roster the local key is not on, or a peer that does not answer fails the process. Fetch gives up after `--fetch-timeout-secs` (default 120, allowed 5 through 300). Bind gives up after 20 seconds. The server gives up after the hold. Failure does not write a measurement report.
