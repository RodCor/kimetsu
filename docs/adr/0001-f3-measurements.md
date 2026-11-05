# ADR 0001 F3 measurements

Measured 2026-11-05 with `cargo run -p kimetsu-bus --example f3_hotpath --release`
(`scripts/f3-hotpath.sh`). Rustc 1.99.1. Release profile. The `embeddings`
feature was off. `KIMETSU_BRAIN_EMBEDDER=off`, `KIMETSU_USER_BRAIN=0`.
`kimetsu_sdk::context` does not call a reranker and does not dial the network.

Re-run that command to replace these figures. Nothing below is copied from the
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
| Presence off versus on for `context` | a down relay does not change local recall latency | relay up, after the in-process relay reported connected: n=200, p50 3.975 ms, p99 **4.999 ms**, max 5.357 ms. p99 delta versus relay down is 0.773 ms | pass |

`context` does not dial the relay in either sample. The relay-up process was connected and then left running only so the comparison includes a live relay. Stopping it is the relay-down sample above.

This host has one machine. The `request_pack` p99 above is direct loopback: both endpoints on this host, dialed at `127.0.0.1`. It is not the §7 measurement of direct LAN QUIC between two LAN hosts. That LAN measurement is still pending. No LAN number is reported here. The loopback sample does not include a relay hop, a failed hole-punch, or an FTS rebuild. Signature verification is inside the timer.

## Journal and lock scope

`PRAGMA journal_mode` is `wal` after project init. A reader during an open `BEGIN IMMEDIATE` that has stamped `superseded_by` still sees the previous snapshot, and `context` returns inside the test's 2 second bound (`wal_reader_misses_uncommitted_supersede_and_context_does_not_wait`). A finished `supersede` shows the stamp and the FTS delete together.

Announce persistence is a JSON file, not `brain.db` and not the FTS index. Recording an announcement while the project writer lock is held does not block `context` (`announce_persistence_is_outside_recall_and_the_rebuild_lock`).

## Phase

The warm local `context` rows meet their §7 budgets on this host. The §7 `request_pack` row is direct LAN QUIC, and that measurement is still pending; the p99 in the table is the loopback sample above. No presence work was moved onto `context`, and no model was called to produce these numbers.

Remote checkpoint limits stay 64 MiB compressed, 256 MiB decoded, and 2,000,000 rows. The per-request SQLite file stays a disposable projection. Remote does not dial the bus. See [remote-storage.md](../remote-storage.md) and [remote-namespace-roster.md](../operators/remote-namespace-roster.md).
