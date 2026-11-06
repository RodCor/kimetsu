# ADR 0001 F3 measurements

Measured 2026-11-05 with `cargo run -p kimetsu-bus --example f3_hotpath --release`
(`scripts/f3-hotpath.sh`). Rustc 1.99.1. Release profile. The `embeddings`
feature was off. `KIMETSU_BRAIN_EMBEDDER=off`, `KIMETSU_USER_BRAIN=0`.
`kimetsu_sdk::context` does not call a reranker and does not dial the network.

Re-run that command to replace the loopback and `context` figures. The veth
row below is `scripts/f3-hotpath-lan.sh`. Nothing below is copied from the
audit microbench.

## Hardware, corpus, cold versus warm

| | |
| --- | --- |
| Hardware | Linux 6.12.94+ x86_64, Intel(R) Xeon(R) Processor, 4 cores, MemTotal 16387280 KiB |
| Corpus | 2000 accepted `repo` memories. `brain.db` 18939904 bytes. Query `lanternwick`, 5 hits |
| Cold | 25.286 ms. `posix_fadvise(POSIX_FADV_DONTNEED)` on `brain.db`, `-wal`, and `-shm`, then the first `context` |
| Warm | 20 untimed calls, then the samples below. Page cache already holds the index |

The acceptance row is the warm sample. Cold is reported beside it and is not the budget.

## §7 rows

| Path | Budget | Result | Verdict |
| --- | --- | --- | --- |
| Local `context`, warm FTS, no embeddings, no reranker, no network | p99 of a few milliseconds | relay down: n=200, p50 3.964 ms, p99 **4.226 ms**, max 4.274 ms | pass |
| `request_pack` of a small pack over direct QUIC, until verified in memory | §7 budget is around 100 ms on direct LAN QUIC | Loopback only: n=30, p50 1.198 ms, p99 **1.482 ms**, max 1.482 ms. Pack is 396 gzip bytes, 8 memories. Relay disabled. Both endpoints on this host, dialed at `127.0.0.1`. Import and FTS rebuild are not in the timer. This is not the §7 LAN topology. The LAN measurement is still pending | pending |
| `request_pack` of a small pack over direct QUIC between two Linux netns connected by a veth pair on one physical host, until verified in memory | §7 budget is around 100 ms on direct LAN QUIC between two LAN machines. This row is not that topology | n=30, p50 1.271 ms, p99 **1.511 ms**, max 1.511 ms. Pack is 394 gzip bytes, 8 memories. Relay disabled. Local bind `10.200.0.2:55666`, peer `10.200.0.1:49736`. Import and FTS rebuild are not in the timer. Measured 2026-11-06, rustc 1.99.1, on the hardware in the veth section below | not the §7 LAN row; two-physical-host LAN is still pending |
| Presence off versus on for `context` | a down relay does not change local recall latency | relay up, after the in-process relay reported connected: n=200, p50 3.975 ms, p99 **4.999 ms**, max 5.357 ms. p99 delta versus relay down is 0.773 ms | pass |

`context` does not dial the relay in either sample. The relay-up process was connected and then left running only so the comparison includes a live relay. Stopping it is the relay-down sample above.

This host has one machine. The loopback `request_pack` p99 above is direct loopback: both endpoints on this host, dialed at `127.0.0.1`. It is not the §7 measurement of direct LAN QUIC between two LAN hosts. The veth row is also one physical host. Two-physical-host LAN is still pending. No two-host LAN number is reported here. The loopback sample does not include a relay hop, a failed hole-punch, or an FTS rebuild. Signature verification is inside the timer.

## Veth pair, one physical host

Measured 2026-11-06T12:29:30Z with `scripts/f3-hotpath-lan.sh`. Release profile. The `embeddings` feature was off. Rustc 1.99.1 (58cf8f9c3 2026-10-13).

Topology label, as printed by the harness: two Linux netns connected by a veth pair on one physical host. Addresses `10.200.0.1` and `10.200.0.2`. This is not two LAN hosts and it does not fill the §7 direct-LAN-QUIC row.

| | |
| --- | --- |
| Hardware | Linux 6.12.113+ x86_64, Intel(R) Xeon(R) Processor, 4 cores, MemTotal 16387328 KiB |
| Pack | 394 gzip bytes, 8 memories. Relay disabled. The eight memory texts match the loopback timer |
| Samples | n=30, p50 1.271 ms, p99 1.511 ms, max 1.511 ms. Three untimed fetches, then the 30 timed `fetch_verified` calls. Import and FTS rebuild are not in the timer |
| Dial | local bind `10.200.0.2:55666`, peer `10.200.0.1:49736` |

Before the samples, the harness pinged `10.200.0.2` from the `10.200.0.1` namespace: 1 packet transmitted, 1 received, rtt 0.701 ms. That ping is not the `request_pack` timer.

Raw stdout from that run (`docs/adr/0001-f3-veth-stdout.txt`):

```text
f3 veth harness
date: 2026-11-06T12:29:30Z
rustc: rustc 1.99.1 (58cf8f9c3 2026-10-13)
topology: two Linux netns connected by a veth pair on one physical host
addresses: 10.200.0.1 <-> 10.200.0.2
hardware: Linux 6.12.113+ x86_64 | Intel(R) Xeon(R) Processor | 4 cores | MemTotal 16387328 KiB
topology: two Linux netns connected by a veth pair on one physical host
bind: 10.200.0.2:55666
peer: 10.200.0.1:49736
request_pack direct QUIC until verified in memory: two Linux netns connected by a veth pair on one physical host. n=30 p50=1.271 ms p99=1.511 ms max=1.511 ms
pack: 394 gzip bytes, 8 memories, relay disabled
dialed address is 10.200.0.1:49736; local bind is 10.200.0.2:55666
import and FTS rebuild: not included in the request_pack timer
two physical LAN hosts: still pending. This run is not direct LAN QUIC between two LAN machines.
```

Two physical LAN hosts are still pending. This environment has no second machine on a real LAN.

## Journal and lock scope

`PRAGMA journal_mode` is `wal` after project init. A reader during an open `BEGIN IMMEDIATE` that has stamped `superseded_by` still sees the previous snapshot, and `context` returns inside the test's 2 second bound (`wal_reader_misses_uncommitted_supersede_and_context_does_not_wait`). A finished `supersede` shows the stamp and the FTS delete together.

Announce persistence is a JSON file, not `brain.db` and not the FTS index. Recording an announcement while the project writer lock is held does not block `context` (`announce_persistence_is_outside_recall_and_the_rebuild_lock`).

## Phase

The warm local `context` rows meet their §7 budgets on this host. The §7 `request_pack` row is direct LAN QUIC between two LAN machines, and that measurement is still pending. The table's 1.482 ms p99 is the loopback sample. The 1.511 ms p99 is the veth pair on this one host. No presence work was moved onto `context`, and no model was called to produce these numbers.

Remote checkpoint limits stay 64 MiB compressed, 256 MiB decoded, and 2,000,000 rows. The per-request SQLite file stays a disposable projection. Remote does not dial the bus. See [remote-storage.md](../remote-storage.md) and [remote-namespace-roster.md](../operators/remote-namespace-roster.md).
