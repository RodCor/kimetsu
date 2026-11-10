/**
 * Shapes the brain returns.
 *
 * Every interface here is open (`[key: string]: unknown`) for the same reason
 * the Python SDK's models set `extra="allow"`: the brain adds fields faster
 * than an SDK is republished, and a client that drops unknown keys silently
 * hides new capability from its callers. The named fields are the ones a caller
 * can rely on; the index signature is how everything else still reaches them.
 */

/**
 * Floors one `kimetsu_brain_context` call applied.
 *
 * Present only when the brain emitted `diagnostics`. Older servers omit it,
 * and a response drops it when the object would exceed the output budget.
 * All four keys are present when the object is.
 */
export interface ContextDiagnostics {
  /** Resolved cosine floor. A default bge brain reports `0.35`. */
  min_semantic_score: number;
  /** Rerank floor passed into arbitration. */
  rerank_min_score: number;
  /** Resolved lexical coverage floor. */
  min_lexical_coverage: number;
  /** Fusion mode: `"linear"` or `"rrf"`. */
  fusion: string;
}

/** One retrieved memory or repo capsule. */
export interface ContextCapsule {
  id?: string;
  /** `"memory"`, `"repo_file"`, or `"repo_manifest"`. */
  kind?: string;
  /** Rendered text. For memory capsules, `"scope:kind - text"`. */
  summary?: string;
  token_estimate?: number;
  /** Stable handle (`memory:<id>`, `file:<path>`) — use this for provenance. */
  expansion_handle?: string;
  confidence?: number;
  freshness?: number;
  relevance?: number;
  scope_weight?: number;
  score?: number;
  [key: string]: unknown;
}

/**
 * A retrieved context bundle.
 *
 * Read `evidenceCoverage` before treating the capsules as an answer: a bundle
 * that covers half the question looks exactly like one that answers it, and
 * that indifference is what the sycophancy literature measures. When
 * `partial_evidence_notice` is present, it is a sentence written for the reader.
 */
export interface ContextBundle {
  stage?: string;
  query?: string;
  augmented_query?: string;
  budget_tokens?: number;
  used_tokens?: number;
  capsule_count?: number;
  excluded_count?: number;
  capsules: ContextCapsule[];
  excluded?: ContextCapsule[];
  /** True when nothing cleared the score floor. Zero tokens were spent. */
  skipped?: boolean;
  top_score?: number;
  /** Share of the query's discriminating terms the capsules cover, `[0, 1]`. */
  evidence_coverage?: number;
  /** Discriminating query terms no capsule mentions. */
  uncovered_terms?: string[];
  /** Present when coverage is partial: what to tell the reader. */
  partial_evidence_notice?: string | null;
  /** True when the query asked about order, so capsules are oldest-first and dated. */
  chronological?: boolean;
  chronological_note?: string | null;
  /** Present only on a session's first call from a host without hooks. */
  warm_start?: string | null;
  /**
   * Floors this retrieval applied. Absent on older responses and when the
   * object did not fit the output budget.
   */
  diagnostics?: ContextDiagnostics;
  [key: string]: unknown;
}

export interface Memory {
  id?: string;
  memory_id?: string;
  text?: string;
  scope?: string;
  kind?: string;
  tags?: string[];
  confidence?: number;
  created_at?: string;
  [key: string]: unknown;
}

export interface BrainStatus {
  initialized?: boolean;
  accepted?: number;
  [key: string]: unknown;
}

/** Any tool response this SDK does not model more specifically. */
export type ToolResult = Record<string, unknown>;

/** Citation outcome. Omitted on the wire is `noop` and does not raise usefulness. */
export type CiteOutcome = 'helped' | 'noop' | 'harmed';

/** `project` is an input alias of `repo`. `user` is an alias of `global_user`. */
export type MemoryScope =
  | 'global_user'
  | 'user'
  | 'project'
  | 'repo'
  | 'run'
  | 'session'
  | 'org'
  | 'skill';

export type MemoryKind =
  | 'fact'
  | 'convention'
  | 'preference'
  | 'failure_pattern'
  | 'decision'
  | 'skill';
