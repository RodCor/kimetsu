# Dependencies

Kimetsu keeps dependencies narrow. Each non-test dependency must provide boring infrastructure that is expensive or risky to implement incorrectly.

## Current v0.1 Dependencies

| Crate | Purpose | Where Used | Why Custom Code Is Insufficient | Removal Cost | License |
| --- | --- | --- | --- | --- | --- |
| `base64` | Decode OpenAI image generation responses. | `kimetsu-chat` `/image`. | Correct, constant-edge-case Base64 decoding is infrastructure. | Low. | MIT OR Apache-2.0 |
| `blake3` | Fast content hashes, config hashes, and future failure fingerprints. | `kimetsu-brain` ingestion/admin traces; planned tooling. | Hash implementation correctness and performance are not product logic. | Medium. | Apache-2.0 OR CC0-1.0 |
| `clap` | CLI parsing. | `kimetsu-cli`. | CLI parsing edge cases are not core differentiation. | Medium. | MIT OR Apache-2.0 |
| `crossterm` | Raw terminal input and key events for the chat command palette. | `kimetsu-chat` interactive REPL. | Cross-platform terminal raw mode is OS-specific and easy to get wrong. | Medium. | MIT |
| `ignore` | Gitignore-aware repo walking. | `kimetsu-brain` repo ingestion. | Correct ignore semantics are easy to get wrong. | Medium. | Unlicense OR MIT |
| `regex` | Search tool patterns and secret redaction. | `kimetsu-agent` tools. | Regular expression engine is infrastructure. | Low. | MIT OR Apache-2.0 |
| `reqwest` | Provider HTTP calls and OpenAI image generation requests. | `kimetsu-chat` `/image`; planned model adapter. | HTTP/TLS is not product logic. | Medium. | MIT OR Apache-2.0 |
| `rusqlite` | SQLite projection with bundled SQLite/FTS5. | `kimetsu-brain`. | Database driver correctness is infrastructure. | High. | MIT |
| `serde` | Serialization. | All crates. | Wire formats are central, but serializer implementation is infrastructure. | High. | MIT OR Apache-2.0 |
| `serde_json` | Event payloads and artifacts. | Core/brain/CLI. | JSON parser/writer correctness is infrastructure. | High. | MIT OR Apache-2.0 |
| `similar` | Unified diff rendering for whole-file patch output. | `kimetsu-agent` `apply_patch`. | Diff algorithms are not product logic. | Medium. | Apache-2.0 |
| `time` | UTC timestamps. | `kimetsu-core`. | Time parsing/formatting correctness is infrastructure. | Medium. | MIT OR Apache-2.0 |
| `tokio` | Async runtime and future process/model streaming. | Planned agent/tooling. | Async runtime implementation is infrastructure. | High. | MIT |
| `toml` | `project.toml` parsing/writing. | Core/brain. | TOML parser/writer correctness is infrastructure. | Low. | MIT OR Apache-2.0 |
| `tracing` | Internal logs. | CLI now, all crates later. | Structured logging implementation is infrastructure. | Medium. | MIT |
| `tracing-subscriber` | Log subscriber. | `kimetsu-cli`. | Log formatting/filtering is infrastructure. | Low. | MIT |
| `ulid` | Sortable IDs. | `kimetsu-core`. | Sortable ID implementation correctness matters and is not product logic. | Medium. | MIT |

## Rule

Before adding a dependency, update this file with the reason, use site, removal cost, and license.
