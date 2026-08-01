/**
 * A typed client for Kimetsu Remote.
 *
 * Mirrors the Python SDK's surface — `client.memory.*`, `client.config.*`,
 * `client.models.*`, `client.benchmark.*`, and the four top-level calls — so a
 * team using both reads one API with two syntaxes.
 *
 * One deliberate difference: there is no sync/async split. Python needs
 * `KimetsuClient` and `AsyncKimetsuClient` because its HTTP libraries come in
 * both flavours; in JavaScript every call is a promise and a second class would
 * be a second thing to keep in step for no gain.
 *
 * ## Why this exists
 *
 * The ecosystem Kimetsu integrates with — Pi extensions, OpenClaw plugins,
 * Cursor, VS Code, MCP clients — is TypeScript, and every one of those
 * integrations was shelling out to the binary and parsing its text output. That
 * is how the Pi extension and its published npm copy drifted apart without
 * anyone noticing: there was no shared typed surface for them to share.
 */

import {
  clean,
  HttpTransport,
  type Args,
  type HttpTransportOptions,
  type Transport,
} from './transport.js';
import type {
  BrainStatus,
  ContextBundle,
  Memory,
  MemoryKind,
  MemoryScope,
  ToolResult,
} from './types.js';

/** Memory CRUD, review queue, conflicts, and pruning. */
class MemoryNamespace {
  constructor(private readonly t: Transport) {}

  search(query: string, extra: Args = {}): Promise<ToolResult> {
    return this.t.call('kimetsu_brain_memory_search', clean({ query, ...extra }));
  }

  async add(
    text: string,
    options: { scope: MemoryScope | string; kind?: MemoryKind | string } & Args,
  ): Promise<Memory> {
    const { scope, kind, ...extra } = options;
    return (await this.t.call(
      'kimetsu_brain_memory_add',
      clean({ text, scope, kind, ...extra }),
    )) as Memory;
  }

  list(extra: Args = {}): Promise<ToolResult> {
    return this.t.call('kimetsu_brain_memory_list', clean(extra));
  }

  top(extra: Args = {}): Promise<ToolResult> {
    return this.t.call('kimetsu_brain_memory_top', clean(extra));
  }

  accept(proposalId: string, extra: Args = {}): Promise<ToolResult> {
    return this.t.call('kimetsu_brain_memory_accept', clean({ proposal_id: proposalId, ...extra }));
  }

  reject(proposalId: string, extra: Args = {}): Promise<ToolResult> {
    return this.t.call('kimetsu_brain_memory_reject', clean({ proposal_id: proposalId, ...extra }));
  }

  invalidate(memoryId: string, extra: Args = {}): Promise<ToolResult> {
    return this.t.call(
      'kimetsu_brain_memory_invalidate',
      clean({ memory_id: memoryId, ...extra }),
    );
  }

  /** Per-run citation attribution: which memories a run leaned on. */
  blame(runId: string, extra: Args = {}): Promise<ToolResult> {
    return this.t.call('kimetsu_brain_memory_blame', clean({ run_id: runId, ...extra }));
  }

  /** Pending proposals awaiting a decision — including quarantined imports. */
  proposals(extra: Args = {}): Promise<ToolResult> {
    return this.t.call('kimetsu_brain_memory_proposals', clean(extra));
  }

  conflicts(extra: Args = {}): Promise<ToolResult> {
    return this.t.call('kimetsu_brain_memory_conflicts', clean(extra));
  }

  conflictResolve(extra: Args = {}): Promise<ToolResult> {
    return this.t.call('kimetsu_brain_conflict_resolve', clean(extra));
  }

  prune(extra: Args = {}): Promise<ToolResult> {
    return this.t.call('kimetsu_brain_prune', clean(extra));
  }
}

class ConfigNamespace {
  constructor(private readonly t: Transport) {}

  show(extra: Args = {}): Promise<ToolResult> {
    return this.t.call('kimetsu_brain_config_show', clean(extra));
  }
}

class ModelsNamespace {
  constructor(private readonly t: Transport) {}

  list(extra: Args = {}): Promise<ToolResult> {
    return this.t.call('kimetsu_brain_model_list', clean(extra));
  }
}

class BenchmarkNamespace {
  constructor(private readonly t: Transport) {}

  context(query: string, extra: Args = {}): Promise<ToolResult> {
    return this.t.call('kimetsu_benchmark_context', clean({ query, ...extra }));
  }

  recordOutcome(task: string, extra: Args = {}): Promise<ToolResult> {
    return this.t.call('kimetsu_benchmark_record_outcome', clean({ task, ...extra }));
  }
}

export interface KimetsuClientOptions extends HttpTransportOptions {
  baseUrl?: string;
  token?: string;
  repo?: string;
  /**
   * Use this transport instead of building one. Lets a test drive the whole
   * client without a server, and lets a host supply its own HTTP stack.
   * When set, `baseUrl` / `token` / `repo` are ignored.
   */
  transport?: Transport;
}

export class KimetsuClient {
  readonly memory: MemoryNamespace;
  readonly config: ConfigNamespace;
  readonly models: ModelsNamespace;
  readonly benchmark: BenchmarkNamespace;
  readonly #t: Transport;

  /**
   * Credentials fall back to `KIMETSU_REMOTE_URL` / `_TOKEN` / `_REPO`, the
   * same variables the Python SDK reads, so one environment configures both.
   */
  constructor(options: KimetsuClientOptions = {}) {
    if (options.transport) {
      this.#t = options.transport;
    } else {
      const baseUrl = options.baseUrl ?? process.env.KIMETSU_REMOTE_URL;
      const token = options.token ?? process.env.KIMETSU_REMOTE_TOKEN;
      const repo = options.repo ?? process.env.KIMETSU_REMOTE_REPO;
      // Named individually rather than as one "missing config" error: the
      // usual cause is one unset variable, and saying which one is the whole
      // difference between a fix and a search.
      for (const [name, value] of [
        ['KIMETSU_REMOTE_URL', baseUrl],
        ['KIMETSU_REMOTE_TOKEN', token],
        ['KIMETSU_REMOTE_REPO', repo],
      ] as const) {
        if (!value) {
          throw new Error(`KimetsuClient: no ${name.toLowerCase().replace(/_/g, ' ')} — pass it explicitly or set ${name}`);
        }
      }
      this.#t = new HttpTransport(baseUrl!, token!, repo!, options);
    }
    this.memory = new MemoryNamespace(this.#t);
    this.config = new ConfigNamespace(this.#t);
    this.models = new ModelsNamespace(this.#t);
    this.benchmark = new BenchmarkNamespace(this.#t);
  }

  /**
   * Retrieve a context bundle.
   *
   * Check `skipped` and `evidence_coverage` before treating the capsules as an
   * answer — an empty bundle means the brain had nothing relevant, and a
   * low-coverage one means it has part of the answer and knows it.
   */
  async context(
    query: string,
    options: { tags?: string[] } & Args = {},
  ): Promise<ContextBundle> {
    const { tags, ...extra } = options;
    const result = (await this.t.call(
      'kimetsu_brain_context',
      clean({ query, tags, ...extra }),
    )) as ContextBundle;
    // A skipped bundle omits `capsules` entirely; callers should be able to
    // iterate the field unconditionally rather than guarding every use.
    return { ...result, capsules: result.capsules ?? [] };
  }

  /** Capture a lesson. Runs the brain's semantic dedup. */
  record(
    lesson: string,
    options: { tags: string[]; kind?: MemoryKind | string } & Args,
  ): Promise<ToolResult> {
    const { tags, kind, ...extra } = options;
    return this.t.call('kimetsu_brain_record', clean({ lesson, tags, kind, ...extra }));
  }

  async status(): Promise<BrainStatus> {
    return (await this.t.call('kimetsu_brain_status', {})) as BrainStatus;
  }

  insights(): Promise<ToolResult> {
    return this.t.call('kimetsu_brain_insights', {});
  }

  /** Record that a memory materially helped — the brain's usefulness signal. */
  cite(memoryId: string, extra: Args = {}): Promise<ToolResult> {
    return this.t.call('kimetsu_brain_cite', clean({ memory_id: memoryId, ...extra }));
  }

  /**
   * Call any tool by name, including ones newer than this SDK.
   *
   * The escape hatch that keeps a version mismatch from being a blocker: the
   * brain grows tools faster than the SDK is republished, and a client with no
   * way to reach a new one sends its users back to shelling out.
   */
  call(name: string, args: Args = {}): Promise<ToolResult> {
    return this.t.call(name, clean(args));
  }

  private get t(): Transport {
    return this.#t;
  }
}
