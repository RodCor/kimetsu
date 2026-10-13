# Optional decision providers (experimental)

Kimetsu supports an explicit decision pass on **`brain context` only**. It runs
after local retrieval and abstention, sends the query and selected capsule
summaries in one batch, then removes capsules below your chosen threshold and
orders survivors by probability. It never resurrects excluded evidence or
writes memories. JSON output includes a separate `decisions` report; local
retrieval scores retain their original meaning. Coverage is conservatively
reset when evidence is removed.

```sh
kimetsu config set decisions.provider jev
kimetsu brain context "Which port does Orchid use?" --no-ambient --json \
  --decisions --decision-min-probability 0.8
```

The numeric threshold above is an example, **not a calibrated recommendation**.
Choose one using your own labeled holdouts. Set the provider to `openai` or
`perplexity` for those services. Supply an already provisioned key through the
matching environment variable below. Do not put keys in project.toml.
`kimetsu config get decisions.provider` verifies the selection without making
an API call. The default is `disabled`; configuring a provider alone never
contacts it. Both `--decisions` and an explicit threshold are required.

This is runtime CLI integration, not automatic Pi/MCP/hook or distiller support.
Normal commands and those host integrations retain their existing behavior.
A decision error fails the explicitly requested operation without emitting a
filtered result. There are no retries, redirects, or provider fallback. Empty or
abstained bundles make no provider call. Requests are limited to 32 selected
capsules and 64 KiB; responses to 64 KiB; timeout is ten seconds. The flag
transmits selected evidence and can incur provider charges. No real-service
calls were made during development.

## Dry-run evaluation helper

This standalone Python 3.10+ example lets you inspect or explicitly send one
memory-relevance question. It is **not a Kimetsu retrieval backend or distiller**.
It never runs automatically, changes configuration, reads a brain, or selects a
production threshold. Existing Kimetsu defaults are unchanged.

Official contracts checked on 2026-10-13:

| Provider flag | Product/model | Endpoint | Existing key environment variable |
| --- | --- | --- | --- |
| `jev` | TypeSafe Jev / `jev-latest` | `https://api.typesafe.ai/v1/systemone` | `TYPESAFE_API_KEY` |
| `openai` | OpenAI Decisions API / `gpt-6-luna` | `https://api.openai.com/v1/decisions` | `OPENAI_API_KEY` |
| `perplexity` | Perplexity Decisions / `pplx-decider-v1.1-27b` | `https://api.perplexity.ai/v1/decisions` | `PERPLEXITY_API_KEY` |

Sources: [TypeSafe HTTP API](https://docs.typesafe.ai/api),
[OpenAI Decisions](https://developers.openai.com/api/docs/guides/decisions),
[Perplexity Decisions](https://docs.perplexity.ai/docs/decisions/quickstart).
These services return typed decisions. They cannot replace the prose/JSON
generation required by Kimetsu's lesson distiller. OpenAI uses an `input` field,
an array of named `predicate` questions, and an answers array; the other two
use `state`, named `noul` questions, and an answers map. The example implements
only this yes/no relevance probe, not every provider feature.

## Inspect a request without credentials or network access

Put only the query and memory you want evaluated in `evidence.txt`, for example:

```text
Query: Which port does Orchid use?
Memory: The Orchid production gateway uses port 4000.
```

PowerShell:

```powershell
Get-Content -Raw evidence.txt | python examples/decisions/evaluate.py --provider jev
```

POSIX shell:

```sh
python3 examples/decisions/evaluate.py --provider openai < evidence.txt
```

Default output is a dry-run request. No key is read and no provider is contacted.
To opt in later, supply your already provisioned provider key through the named
environment variable and add `--live`. This transmits stdin evidence to the
selected service and can incur charges under that account. The example does not
create credentials, purchase credits, accept agreements, or use ChatGPT login.
It caps input/output, uses a ten-second timeout, refuses redirects, and does not
retry or fall back to another provider. An error produces no probability.

## Verification and adoption limits

```sh
python3 -B -m unittest discover -s examples/decisions -p 'test_*.py'
```

The Rust integration additionally tests real HTTP transport against a loopback
mock server, filtering/budget/conflict invariants, abstention preservation, and
CLI opt-in/configuration/credential failures. Run `cargo test -p kimetsu-agent
decisions::tests` and the CLI integration suite for those gates.

The Python tests check documented request/response shapes, malformed probability rejection,
and dry-run isolation. They use synthetic/mock responses: **no real-service
quality, latency, calibration, account access, or pricing validation is claimed**.
Before integrating these scores into retrieval, evaluate answerable/no-answer,
correction, multi-entity, and adversarial-memory holdouts against the current
local baseline, including latency, privacy and cost. Preserve abstention and
never interpret a raw retrieval score as a calibrated provider probability.
