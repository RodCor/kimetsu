# Internet/WAN: `request_pack` measurement

This page is the direct-UDP path between two hosts that are not on one LAN. The usual pair is a Windows 11 Home PC behind a home NAT, using WSL Ubuntu, and a Linux cloud host. It is not the §7 physical-LAN row. Loopback, the veth pair (`10.200.0.1` and `10.200.0.2`), and `self-test` stay on their own labels.

No WAN figure is recorded until both hosts print a report whose `topology` line is `internet/WAN, not a physical LAN`. A failed dial, a STUN probe, or a local argument check is not that report.

## What the commands do

`wan-client` and `wan-serve` use the same verified `request_pack` timer as the LAN harness: signature check included, import and FTS rebuild excluded, embeddings off, `BusRelay::Disabled`. There is no relay and no second protocol.

Each side binds one explicit local IPv4 address and one fixed UDP port. `--advertise ip:port` is the external address the other side can dial. It may differ from the bind address, or only the mapped port may differ. The tool refuses `0.0.0.0`, loopback, the veth addresses, and a private or shared address in `--advertise`. It also refuses to advertise the local bind unchanged.

`--stun-diagnostic ip:port` sends one binding request from that same local socket, waits at most 3 seconds, and prints the mapped address on stderr. The mapping is not written to the handoff and is not dialed. A mapping often changes when the socket closes. Pass a numeric STUN address; a hostname can resolve to an address that is not the STUN server.

The server writes `handoff.json` (version 2, `mode` `internet-wan`). The client dials the advertised address in that file. A version 1 LAN handoff is rejected, and a WAN handoff is rejected by the LAN commands.

## Keys

The client key stays in the `wan-client` process. It is not written to a file, not passed on the command line, and not copied. Copy only the `client_public` line to the server.

The server keeps its endpoint key, pack signer, and roster admin in memory. The handoff carries the admin public key, the signed roster, the server endpoint id, the advertised address, the server's local bind, the pack digest, and host facts. A roster whose signature does not check is rejected before a dial. An endpoint id that is not on that roster is rejected before any pack work (`non-member handshake refused before dial`).

The LAN `client-init` command still writes `client.secret` as mode `0600` hex because those two commands are separate processes. WAN does not use that file. If `client.secret` is already in the WAN state directory, `wan-client` stops.

`cleanup --state DIR` removes only a directory with the `kimetsu-f3-two-host-state` marker. Run it on both machines after the processes exit. It refuses `/`.

## Reachability is two different layers

Nothing here changes a firewall or a router. Either step can be necessary. Neither is a promise that the path will work.

1. **Home router UDP forward.** One external UDP port forwarded to the PC's LAN address and the same port the process binds, when you want the cloud host to accept packets that arrive from the internet. This is the NAT mapping `--advertise` names. If the router rewrites the port, `--advertise` uses the external port and `--port` stays the port bound on the host.
2. **Windows Firewall, one UDP port, one program.** An inbound allow rule scoped to that port and to the program that owns the socket (WSL's relay or the binary you run). A router forward does not open the Windows filter. A firewall rule does not create a mapping on the router.

**WSL2.** Ubuntu 20.04 under WSL2 has its own virtual NIC and its own NAT behind Windows. A process in WSL binds the WSL address, not `192.168.1.29`, unless the distro is in mirrored mode. Mirrored mode is not the Ubuntu 20.04 WSL default. A forward to `192.168.1.29` does not land on a socket bound only inside WSL. You either bind a Windows-reachable address and forward into WSL yourself, or you accept that this harness has not been run on that WSL. This repository was not executed on Windows or on Ubuntu 20.04 for this change.

**CGNAT.** If the address on the router's WAN port is not the address `--advertise` uses (often `100.64.0.0/10`, or any WAN address that differs from a STUN mapping), a forward on that router is not reachable from the internet. STUN can still succeed. That does not prove a path between the two hosts. The 2026-11-08 attempt saw STUN succeed from both sides and a simultaneous hole-punch return zero echoes. Those STUN round trips are not peer RTTs and are not a `request_pack` result.

## Commands

Build with stable Rust, 1.91 or newer (`kimetsu-bus`'s `rust-version`). Do not pass `--features embeddings`. WSL rustc 1.95 is new enough for that floor; this change was checked with `cargo +stable` on Linux, not on that WSL.

On the client (the process that dials), start it first and leave it running:

```sh
scripts/f3-hotpath-two-hosts.sh wan-client \
  --bind 192.168.1.29 \
  --port 44144 \
  --advertise 203.0.113.20:44144 \
  --state /var/tmp/kimetsu-f3-wan-client \
  --confirm-internet-wan
```

The addresses above are documentation placeholders (`203.0.113.0/24` is TEST-NET-3). Use the host's real bind address and the external mapping you configured. Copy `client_public` to the server.

On the server:

```sh
scripts/f3-hotpath-two-hosts.sh wan-serve \
  --bind 172.30.0.2 \
  --port 44144 \
  --advertise 203.0.113.10:44144 \
  --client-public <client_public> \
  --state /var/tmp/kimetsu-f3-wan-server \
  --confirm-internet-wan
```

Copy `handoff.json` into the client state directory while `wan-client` is still running. The client dials the advertised address from that file. The server exits when `--hold-secs` elapses (default 600) or when `done` appears in its state directory. The client writes `done` only in its own state directory.

Optional, on either side, before the QUIC bind:

```text
--stun-diagnostic 203.0.113.5:3478
```

## What a real report contains

`peer_rtt_ms` is the median of the timed verified `request_pack` samples on the client. It is not STUN and not ICMP. `raw_ms` is those samples, in order, so a later reader can compute another percentile. `p50`, `p99`, and `max` match the LAN fields. `n=0` does not print a report. `path` says direct UDP to the advertised endpoint with the relay disabled. `physical LAN section 7 row: not this run` is part of the text.

## Example only

```text
EXAMPLE ONLY — not a measurement

topology: internet/WAN, not a physical LAN
path: direct UDP to the advertised endpoint, relay disabled
stun: not a route and not advertised
peer_rtt_metric: median of the timed verified request_pack samples on the client; not STUN and not ICMP
peer_rtt_ms: EXAMPLE
raw_ms: EXAMPLE
bind: EXAMPLE-local:port
advertised: EXAMPLE-public:port
client_advertised: EXAMPLE-public:port
pack: EXAMPLE gzip bytes, 8 memories, relay disabled
physical LAN section 7 row: not this run
```

## Rollback

Stop both processes. On the router, delete the single UDP forward you added. On Windows, delete the single inbound firewall rule you added. Do not leave a broad allow behind. Then:

```sh
scripts/f3-hotpath-two-hosts.sh cleanup --state /var/tmp/kimetsu-f3-wan-client
scripts/f3-hotpath-two-hosts.sh cleanup --state /var/tmp/kimetsu-f3-wan-server
```

No shared install is part of this procedure. Do not replace another machine's `kimetsu` binary or open its brain.

## Local checks

`python3 scripts/test-f3-two-hosts.py` checks the script's refusals. `F3_TWO_HOST_SMOKE=1` also runs the debug binary's refusals and the LAN loopback self-test. That self-test is still not a WAN measurement and not a LAN measurement. A debug build cannot confirm an internet/WAN run.
