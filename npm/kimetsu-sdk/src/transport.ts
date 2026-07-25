/**
 * JSON-RPC over HTTP against Kimetsu Remote's MCP endpoint.
 *
 * Zero runtime dependencies: Node 18 ships `fetch`, and this SDK exists so that
 * Pi extensions, OpenClaw plugins, and VS Code integrations can embed a typed
 * client. Every dependency an embedded client carries is one the host also
 * carries, so it has none.
 */

import {
  KimetsuAuthError,
  KimetsuProtocolError,
  KimetsuRateLimitError,
  KimetsuToolError,
} from './errors.js';

/** MCP protocol version this client speaks. */
export const PROTOCOL_VERSION = '2024-11-05';

export type JsonValue =
  | null
  | boolean
  | number
  | string
  | JsonValue[]
  | { [key: string]: JsonValue };

export type Args = Record<string, unknown>;

/** What a transport must do, so a test can substitute one without a server. */
export interface Transport {
  call(name: string, args: Args): Promise<Record<string, unknown>>;
}

export function buildCall(name: string, args: Args, id: number): Record<string, unknown> {
  return {
    jsonrpc: '2.0',
    id,
    method: 'tools/call',
    params: { name, arguments: args },
  };
}

export function buildInitialize(id: number, protocolVersion: string): Record<string, unknown> {
  return {
    jsonrpc: '2.0',
    id,
    method: 'initialize',
    params: {
      protocolVersion,
      capabilities: {},
      clientInfo: { name: '@kimetsu-ai/sdk', version: '0.1.0' },
    },
  };
}

/**
 * Turn one HTTP response into either a JSON-RPC result or the right error.
 *
 * Exported because it is the part worth testing directly: every branch here is
 * a decision a caller would otherwise make from a status code.
 */
export function parseEnvelope(
  status: number,
  body: Record<string, unknown>,
  headers: Headers | Map<string, string>,
): Record<string, unknown> {
  if (status === 401 || status === 403) {
    throw new KimetsuAuthError(`authentication failed (HTTP ${status})`);
  }
  if (status === 429) {
    const raw = headers.get('retry-after');
    // Retry-After may be an HTTP-date rather than seconds. Rather than guess at
    // a date the caller cannot distinguish from a parse failure, leave it
    // undefined — an absent hint is honest, a wrong one causes a retry storm.
    const parsed = raw === null || raw === undefined ? Number.NaN : Number(raw);
    throw new KimetsuRateLimitError(
      'rate limited (HTTP 429)',
      Number.isFinite(parsed) ? parsed : undefined,
    );
  }
  if (status >= 400) {
    throw new KimetsuProtocolError(`unexpected HTTP ${status}`);
  }
  if ('error' in body && body.error !== null && typeof body.error === 'object') {
    const err = body.error as { message?: unknown; code?: unknown };
    throw new KimetsuToolError(
      typeof err.message === 'string' ? err.message : 'tool error',
      typeof err.code === 'number' ? err.code : undefined,
    );
  }
  if (!('result' in body)) {
    throw new KimetsuProtocolError('missing result in JSON-RPC envelope');
  }
  return body.result as Record<string, unknown>;
}

export interface HttpTransportOptions {
  /** Milliseconds before a request is aborted. Default 30_000. */
  timeoutMs?: number;
  protocolVersion?: string;
  /** Substitutable for tests; defaults to the global `fetch`. */
  fetchImpl?: typeof fetch;
}

export class HttpTransport implements Transport {
  readonly #url: string;
  readonly #headers: Record<string, string>;
  readonly #timeoutMs: number;
  readonly #protocolVersion: string;
  readonly #fetch: typeof fetch;
  #nextId = 1;
  /**
   * The in-flight (or completed) handshake.
   *
   * Held as a promise rather than a boolean so that concurrent first calls
   * share one `initialize` instead of racing to send several — the natural
   * shape in JavaScript, where nothing serialises calls for you.
   */
  #handshake: Promise<void> | undefined;

  constructor(baseUrl: string, token: string, repo: string, options: HttpTransportOptions = {}) {
    this.#url = `${baseUrl.replace(/\/+$/, '')}/mcp/${repo}`;
    this.#headers = {
      authorization: `Bearer ${token}`,
      'content-type': 'application/json',
    };
    this.#timeoutMs = options.timeoutMs ?? 30_000;
    this.#protocolVersion = options.protocolVersion ?? PROTOCOL_VERSION;
    this.#fetch = options.fetchImpl ?? globalThis.fetch;
    if (typeof this.#fetch !== 'function') {
      throw new KimetsuProtocolError(
        'no fetch available: this SDK needs Node 18+, or pass options.fetchImpl',
      );
    }
  }

  async #post(payload: Record<string, unknown>): Promise<Record<string, unknown>> {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), this.#timeoutMs);
    let response: Response;
    try {
      response = await this.#fetch(this.#url, {
        method: 'POST',
        headers: this.#headers,
        body: JSON.stringify(payload),
        signal: controller.signal,
      });
    } finally {
      clearTimeout(timer);
    }

    const text = await response.text();
    let body: Record<string, unknown> = {};
    if (text.length > 0) {
      try {
        body = JSON.parse(text) as Record<string, unknown>;
      } catch {
        // Left empty on purpose: parseEnvelope reports the status first, so a
        // non-JSON 502 from a proxy surfaces as "unexpected HTTP 502" rather
        // than as a JSON syntax error the caller cannot act on.
        body = {};
      }
    }
    return parseEnvelope(response.status, body, response.headers);
  }

  async #ensureInitialized(): Promise<void> {
    this.#handshake ??= (async () => {
      const result = await this.#post(buildInitialize(this.#nextId++, this.#protocolVersion));
      const got = result.protocolVersion;
      if (typeof got === 'string' && got !== this.#protocolVersion) {
        throw new KimetsuProtocolError(`server protocol ${got} != ${this.#protocolVersion}`);
      }
    })().catch((err: unknown) => {
      // A failed handshake must not be cached, or a client that hit one
      // network blip is permanently broken with no way to recover.
      this.#handshake = undefined;
      throw err;
    });
    return this.#handshake;
  }

  async call(name: string, args: Args): Promise<Record<string, unknown>> {
    await this.#ensureInitialized();
    return this.#post(buildCall(name, args, this.#nextId++));
  }
}

/** Drop keys whose value is `undefined`, so optional args are simply absent. */
export function clean(args: Args): Args {
  const out: Args = {};
  for (const [key, value] of Object.entries(args)) {
    if (value !== undefined) out[key] = value;
  }
  return out;
}
