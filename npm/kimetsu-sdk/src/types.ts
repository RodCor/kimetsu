/**
 * Shapes the brain returns.
 *
 * Every interface here is open (`[key: string]: unknown`) for the same reason
 * the Python SDK's models set `extra="allow"`: the brain adds fields faster
 * than an SDK is republished, and a client that drops unknown keys silently
 * hides new capability from its callers. The named fields are the ones a caller
 * can rely on; the index signature is how everything else still reaches them.
 */

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

export type MemoryScope = 'global_user' | 'project' | 'repo' | 'run';

export type MemoryKind =
  | 'fact'
  | 'convention'
  | 'preference'
  | 'failure_pattern'
  | 'decision'
  | 'skill';
