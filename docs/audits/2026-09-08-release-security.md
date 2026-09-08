# v2.8.0 release security review

Review date: 2026-09-08 UTC. Scope: the Kimetsu repository and PR #45, including
the Rust dependency lockfile, TypeScript SDK lockfile, existing CodeQL findings,
release version consistency, and release-note extraction. This is not a claim
that all possible vulnerabilities have been eliminated.

## Dependency findings

- A fresh `cargo audit --json` found zero known Rust vulnerabilities. This PR
  already pins `h2` 0.4.16, which fixes
  [RUSTSEC-2026-0258](https://rustsec.org/advisories/RUSTSEC-2026-0258.html).
- The SDK's `npm audit --json` found zero vulnerabilities. The CLI and remote npm
  manifests are release templates with no third-party runtime dependencies;
  their platform-package versions are stamped by the tag-driven workflow.
- GitHub Dependabot reported zero open alerts for `RodCor/kimetsu`.
- The initial audit identified yanked `der` 0.8.0. It is updated to 0.8.2; no
  other registry dependency changed in the main repository during this
  release-preparation step.
- `paste` 1.0.15 and `rustls-pemfile` 2.2.0 remain transitive dependencies with
  informational unmaintained notices, not vulnerability advisories. Existing CI
  exceptions name only these notices. They remain maintenance work; this review
  does not hide them or replace them with unverified forks.
- The companion benchmark lockfile also moves its Kimetsu path dependencies
  to 2.8.0. Its separate audit identified older `h2` and `quinn-proto`
  vulnerabilities, `anyhow` and `cxx` unsoundness notices, and yanked `der`.
  Updates to 0.4.16, 0.11.15, 1.0.103, 1.0.195, and 0.8.2 respectively leave
  zero known vulnerabilities and only the informational `paste` maintenance
  notice. These changes belong to
  [benchmark PR #4](https://github.com/RodCor/kimetsu-bench/pull/4).

## CodeQL triage

The successful CodeQL check on `b8990cf39d18adcaf1507d7a21bc5337945baa7c` still
contained 47 open high-severity findings. A green scan means the scan ran;
it does not establish that the alert list is empty. The branch analysis was
`1736930424`, category `/language:rust`. Querying only `refs/pull/45/merge` returned
no alerts, so this review also checked `refs/heads/codex/brain-hardening` and the
default branch. All 47 SARIF source-to-sink paths were inspected.

| Alerts | Finding and disposition |
| --- | --- |
| #5–#20, #22–#33, #35–#43, #45–#48 | 41 path findings in bridge/skill handlers. Their SARIF source is the remote HTTP router, but the remote allowlist rejects these host-only tools before `call_tool`. None of these reported remote paths reaches a filesystem sink. Classified as false positives for the reported source-to-sink path. |
| #2 | The CLI prints a stored conversation identifier in its local drift report. `DriftReport.session_id` is not an authentication session token; the identified source is the field itself, not a credential. Classified as a naming-heuristic false positive. |
| #3 | The CLI prints `InitSummary.api_key_env`, a configured variable **name**, when a credential is absent. The producer copies `config.model.api_key_env`; it never stores the resolved secret in that field. Classified as a false positive. |
| #4 | Bedrock already used an HTTPS URL, but region interpolation and redirect behavior deserved hardening. Region input is now restricted to a hostname label, model IDs are encoded as a path segment, the client is HTTPS-only, and redirects are disabled. |
| #21, #34, #44 | Repository ID input already rejected path separators and traversal. This review additionally rejects Windows device names/trailing-dot aliases, redirected repository roots, and pre-existing redirected state paths before the initialization fast path. |

The 41 host-tool findings are backed by
`every_excluded_catalog_tool_is_blocked_at_http_boundary`, which exercises the
actual HTTP router with an administrator token, asserts the critical host tools
cannot enter the remote catalog, and verifies the excluded calls return the
remote-mode denial without creating host configuration directories. No scanner
rule or language is disabled.

This is a source review and regression-test disposition, not a report that
GitHub has closed the alerts. No alerts were dismissed through the API during
this review. The updated branch needs a fresh CodeQL scan, and the reviewed
false positives still need a maintainer disposition in GitHub before the alert
list can be described as cleared.

Path checks protect against existing symlinks/junctions and filesystem aliases.
Configured repository IDs that use reserved device names or trailing dots now
need an unambiguous name on every operating system.
The service data directory must remain under operator control; these checks do
not claim protection against a local actor racing filesystem mutations between
validation and use. The local CLI intentionally retains explicit filesystem
operations requested by its user.

## Release preparation

- All seven workspace packages and every existing inter-crate version pin move
  from 2.7.0 to 2.8.0 using `scripts/bump-version.sh`.
- The SDK keeps its independent 0.1.0 version; npm binary wrappers keep their
  0.0.0 templates because the release workflow stamps them from the tag.
- Changelog extraction now accepts both `## vX.Y.Z` and `## vX.Y.Z: Title`, and
  an empty extraction fails the release job. The actual workflow's awk program
  was checked against the 2.8.0, 2.7.0, and 2.6.1 sections.
- The fact guard stays opt-in and existing model defaults stay unchanged. The
  benchmark artifacts retain their original measured versions and fingerprints.
- No tag, merge, package publication, or release workflow dispatch is performed
  as part of preparing this PR.

## Verification

The Windows path-alias regression failed against the original implementation
(`web.` was accepted), before the fix. Focused agent and remote tests pass with
the hardening changes, including Windows junction checks and HTTPS-only request
rejection before connecting.

The full Windows workspace run with CLI `embeddings`, `pi`, and `openclaw`, plus
remote `tls`, passed **1,500 tests**, with six intentionally ignored and zero
failures. The command was:

```text
cargo test --workspace --features kimetsu-cli/embeddings,kimetsu-cli/pi,kimetsu-cli/openclaw,kimetsu-remote/tls --locked --offline -j 1 -- --test-threads=1
```

`KIMETSU_USER_BRAIN=0` and `KIMETSU_EMBED_DAEMON=0` isolated the test run from the
user brain and background embed daemon. The newly built CLI and remote binaries
both report 2.8.0. Cross-platform CI and the new CodeQL analysis must run on the
updated PR head before release.

Formatting and workspace/all-target Clippy with `-D warnings` pass for both
the release feature set above and `--no-default-features`. Validation used Rust/Cargo 1.97.0 and cargo-audit
0.22.2. The manifest/lockfile consistency check covered all seven workspace
packages and every existing inter-crate version pin.
