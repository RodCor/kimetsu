# Structured fact evidence

Approved intent: user asked to proceed with structured subject/attribute/value/environment/validity/provenance evidence and partial answers. Existing isolated worktree is retained. No deployment or live configuration changes.

## Choice
A rebuildable SQLite projection derived from redacted memory text is preferable to more query-time regex scanning or an additional generative extractor. It preserves replay and avoids model calls. Conservative extraction leaves unsupported language unstructured. Existing relevance and lifecycle gates remain authoritative.

## Contract
`facts::FactClaim { subject: String, attribute: String, value: String, environment: Option<String>, evidence: String }`, serde serializable. `facts::extract(text: &str) -> Vec<FactClaim>` extracts only explicit affirmative facts or explicit absence, at most32 per memory. Preserve exact evidence excerpt; normalize matching fields. Never infer project or component from unrelated clauses.

`fact_store::StoredFact { memory_id: String, claim_revision: String, source_event_id: String, valid_from: Option<String>, valid_to: Option<String>, claim: FactClaim }`. Schema15 stores derived current facts. Acceptance, correction and temporal updates refresh within event transaction; rebuild clears/replays; migration backfills. Reads require matching claim revision and current lifecycle/validity. Historical retrieval must never attach current facts to older text; no match means no projected evidence.

ContextCapsule carries optional `facts: Vec<StoredFact>` internally, hydrated with the text snapshot. Existing constructors initialize empty. Non-memory capsules and daemon compatibility remain valid.

`fact_query::parse(query) -> Option<FactRequest>` recognizes direct single or coordinated attribute value questions with explicit subject and optional environment. Unsupported or ambiguous questions pass through. `fact_query::evaluate(query, capsules) -> Option<FactAssessment>` reports supported, missing and conflicting attributes with source handles from delivered capsules. Exact normalized subject/environment compatibility is mandatory; no guessing unknown scope. `filter_bundle` may exclude irrelevant capsules only for recognized requests with structured evidence available. Legacy guard remains fallback otherwise. Claims of support must also be visible in final rendered text.

Serving emits optional answerability metadata inside final serialized budget accounting, recomputed for the final capsule slice. Full, partial, missing, conflicting statuses concern available evidence, not verified truth. Compression must retain evidence; no extra inference calls. Hook adds a concise missing/conflicting notice based on final visible evidence. Feature remains behind existing explicit_fact_guard defaultfalse.

## Validation
Extraction subject/environment/negation/sensitive text tests; migration/backfill/replay/correction/lifecycle tests; query compound/unknown/contradiction tests; final-budget loss and compression tests; CLI/MCP smoke. Paired BrainBenchmark guardoff/on and earlier-guard/newguard comparisons on existing development plus frozen new subject/environment/partial controls. Measure delivered recall/noise and latency; do not tune on frozen results. One Cargo build at a time with --locked --offline -j1; no build/inference concurrency. Source and benchmark edits stay in their separate audit worktrees.
