# Kimetsu Repository Threat Model

## Overview

Kimetsu is a Rust workspace that ships a local sidecar memory system for coding agents. The primary product is the `kimetsu` CLI binary from `crates/kimetsu-cli`, backed by reusable runtime crates:

- `crates/kimetsu-core`: shared configuration, paths, IDs, event and secret helper types.
- `crates/kimetsu-brain`: local SQLite brain storage, migrations, memory ingestion, retrieval, embeddings support, redaction, analytics, and locking.
- `crates/kimetsu-agent`: provider-neutral agent loop, model clients, harness/tool runtime, pipeline, and benchmark support.
- `crates/kimetsu-chat`: terminal REPL, MCP server, bridge tooling, skills, commands, and UI.
- `crates/kimetsu-cli`: top-level CLI, plugin installers, hooks, brain admin commands, update/uninstall, doctor, and process helpers.
- `crates/kimetsu-e2e`: in-process test harness, not a production runtime surface.

The real assets are local memories in `.kimetsu/brain.db` and `~/.kimetsu/brain.db`, host-agent configuration files written into project or user config directories, provider credentials read from environment/config, update/download integrity, and the user's local filesystem and shell. The repository deliberately offers a lean default build: embeddings, Pi, and OpenClaw integrations are opt-in features. Optional npm packages and GitHub release archives distribute prebuilt binaries.

## Threat Model, Trust Boundaries, and Assumptions

Kimetsu generally runs with the privileges of the local developer account. There is no multi-tenant server boundary in the default product. Most security impact therefore comes from local privilege misuse, malicious project content, malicious or compromised release/download inputs, secret leakage into memory or logs, and unsafe host-agent tool exposure.

Main trust boundaries:

- User/developer to Kimetsu CLI: command-line arguments, environment variables, current working directory, workspace paths, project config, and host-agent config paths are operator-controlled but can be influenced by malicious repositories.
- Host agent to MCP server: JSON-RPC/MCP requests may come from Claude Code, Codex, or other configured hosts. The host agent can pass task text, memory text, file paths, tool arguments, and bridge requests.
- Kimetsu to local filesystem: the CLI and brain create, migrate, back up, delete, and rewrite files in project `.kimetsu/`, user `.kimetsu/`, host config directories, cache directories, install locations, and update targets.
- Kimetsu to external model providers: provider clients send prompts/messages/tool outputs to Anthropic, OpenAI, or AWS Bedrock according to operator configuration. API keys and AWS credentials must not be logged or stored in memories.
- Kimetsu to release infrastructure: update and npm/prebuilt install flows trust GitHub release metadata, archives, package scripts/manifests, and platform selection logic.
- Kimetsu to SQLite: memory text, task text, repo file content, traces, embeddings, analytics, and migration metadata cross into durable local storage.
- Kimetsu to subprocesses: bridge, chat tool runtime, benchmarks, hooks, and update/uninstall flows may execute commands or discover binaries. Command construction and target validation matter because malicious workspace content can shape inputs.

Attacker-controlled or partially attacker-controlled inputs include MCP request payloads, memory text, task prompts, repository files ingested into the brain, project config files, `.env` content, host hook input JSON, CLI path arguments, release metadata if the release channel is compromised, and local files in untrusted repositories. Operator-controlled inputs include API keys, provider selection, model IDs, install scope, update confirmation flags, and explicit deletion flags. Developer-controlled inputs include source code, CI workflows, release packaging, npm manifests, and documentation.

Assumptions:

- The default deployment is local single-user software; local code execution by the user is often already equivalent to full access to Kimetsu data.
- A malicious repository opened by the user is realistic. It may contain hostile config, huge files, secrets, symlinks, paths, or prompts intended to manipulate agent memory.
- A malicious host agent or intentionally invoked tool may already be highly privileged, but Kimetsu should avoid adding silent command execution, unsafe deletion, secret persistence, or update compromise.
- SQLite database corruption, interrupted migrations, and concurrent Kimetsu invocations are realistic operational hazards.
- Network responses from model providers and GitHub should be treated as untrusted data until parsed and validated.

## Attack Surface, Mitigations, and Attacker Stories

Primary surfaces:

- Brain storage and retrieval in `crates/kimetsu-brain`: schema migrations, backup retention, event log projection, FTS queries, embeddings, repo ingest, redaction, user brain/project brain selection, and lock management.
- MCP/bridge/chat in `crates/kimetsu-chat`: JSON-RPC parsing, MCP method dispatch, bridge requests to other hosts, terminal commands, tool calls, and context injection.
- Agent runtime in `crates/kimetsu-agent`: prompt construction, provider clients, subprocess/tool execution, benchmark harnesses, pipeline traces, and recall ledger.
- CLI/update/install in `crates/kimetsu-cli`: plugin install/uninstall/status, `brain` admin commands, `doctor`, `update`, `uninstall`, hook handlers, path discovery, and process helpers.
- Distribution tooling: `.github/workflows`, `npm/kimetsu`, `scripts/`, and `bench/` source where it affects release packaging, credentials, or benchmark execution.

Existing mitigations and controls visible in repository context:

- Lean default feature sets in `Cargo.toml` keep embedding/ONNX-heavy dependencies out of normal builds unless explicitly enabled.
- `reqwest` is configured with `default-features = false` and `rustls-tls`, avoiding system OpenSSL coupling.
- `rusqlite` uses bundled SQLite, reducing platform variance.
- Brain migrations are documented as forward-only with online backups and transactional version bumps.
- The product uses local SQLite rather than an external vector DB or cloud telemetry.
- Secret redaction exists in `crates/kimetsu-brain/src/redact.rs`, and there is a shared secret helper in `crates/kimetsu-core/src/secret.rs`.
- Update/uninstall documentation says discovered binaries are limited to verified install locations rather than whole-disk scans.

Realistic attacker stories:

- A malicious repository causes Kimetsu to ingest secrets, huge files, or adversarial memory content, leading to secret persistence, excessive CPU/disk use, or prompt manipulation in later agent runs.
- A malformed MCP request or bridge payload triggers unintended file writes, command execution, or excessive memory allocation in chat/bridge/MCP handling.
- A compromised or spoofed update path delivers a binary archive that is not pinned or verified strongly enough before replacing an installed executable.
- A malicious project config or path argument tricks plugin install/uninstall/update code into modifying files outside intended host config or install directories.
- A provider credential leaks through logs, traces, memory records, panic output, or doctor/debug reports.
- Concurrent invocations corrupt the brain or race through migration/update/delete operations if locks and atomic writes are incomplete.

Out-of-scope or lower-likelihood stories:

- Cross-tenant data exposure is generally out of scope for local single-user execution unless Kimetsu is embedded into a shared service.
- Remote network exploitation of a public server is not the primary model because the product is a CLI/MCP sidecar, not a hosted web service.
- SQL injection becomes high impact only where untrusted input is interpolated into SQL syntax. Parameterized rusqlite statements reduce this class when used consistently.

## Severity Calibration (Critical, High, Medium, Low)

Critical:

- Silent arbitrary command execution or arbitrary file deletion/write reachable from an untrusted MCP payload, hook input, project config, or malicious repository without an explicit user action.
- Self-update accepting an attacker-controlled binary without sufficient release/source validation, then replacing the running or installed `kimetsu` binary.
- Secret exfiltration from provider credentials or stored brain content to an attacker-controlled network endpoint without explicit operator configuration.

High:

- Path traversal or symlink-following in plugin install, update, uninstall, or brain migration that overwrites host config, shell startup files, or executables outside the intended targets.
- Ingest or memory-record flows that reliably persist high-value secrets despite the intended redaction layer.
- MCP/bridge methods that permit filesystem or process actions with insufficient path/command validation when reachable from a host agent in an untrusted project.
- Data-loss bugs in migrations, backup pruning, update replacement, or uninstall that can destroy the brain or unrelated user files.

Medium:

- Denial of service from unbounded file ingestion, adversarially large MCP payloads, expensive regexes, runaway embedding calls, or unbounded prompt/context construction.
- Race conditions that can corrupt local brain state or leave partial updates under normal concurrent use.
- Logging or doctor output that exposes sensitive paths, model names, or partial secrets but not full credentials.
- Supply-chain hardening gaps in release workflow permissions, npm package metadata, or archive selection where exploitation requires compromise of release infrastructure.

Low:

- Documentation or configuration drift that causes users to install a heavier feature set than intended, or misunderstand lean versus embeddings builds.
- Inefficient cloning, avoidable allocation, repeated SQL prepare/parse work, or non-streaming file reads that degrade local performance without changing security posture.
- Minor Rust style issues, needless dependencies, or test-only code risks that are not reachable in production builds.

