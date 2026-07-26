// Tests run against the compiled `dist/`, so they exercise what actually ships
// rather than the TypeScript sources — a build that emits something different
// from what the types promised would otherwise pass.
import assert from 'node:assert/strict';
import { after, before, describe, it } from 'node:test';

import {
  HttpTransport,
  KimetsuAuthError,
  KimetsuClient,
  KimetsuProtocolError,
  KimetsuRateLimitError,
  KimetsuToolError,
  buildCall,
  clean,
  parseEnvelope,
  trimTrailingSlashes,
} from '../dist/index.js';

/** A transport that records calls and replays canned results. */
function fakeTransport(results = {}) {
  const calls = [];
  return {
    calls,
    async call(name, args) {
      calls.push({ name, args });
      return results[name] ?? { ok: true };
    },
  };
}

const headers = (obj = {}) => new Map(Object.entries(obj));

describe('parseEnvelope', () => {
  // Each of these is a decision a caller would otherwise re-derive from a
  // status code, which is exactly the work an SDK exists to stop repeating.
  it('maps 401 and 403 to an auth error', () => {
    for (const status of [401, 403]) {
      assert.throws(() => parseEnvelope(status, {}, headers()), KimetsuAuthError);
    }
  });

  it('carries Retry-After through a 429', () => {
    try {
      parseEnvelope(429, {}, headers({ 'retry-after': '12' }));
      assert.fail('should have thrown');
    } catch (err) {
      assert.ok(err instanceof KimetsuRateLimitError);
      assert.equal(err.retryAfter, 12);
    }
  });

  // An HTTP-date Retry-After is real and this deliberately does not guess at
  // it: an absent hint is honest, a wrong one causes a retry storm.
  it('leaves retryAfter undefined when the header is not seconds', () => {
    try {
      parseEnvelope(429, {}, headers({ 'retry-after': 'Wed, 21 Oct 2026 07:28:00 GMT' }));
      assert.fail('should have thrown');
    } catch (err) {
      assert.ok(err instanceof KimetsuRateLimitError);
      assert.equal(err.retryAfter, undefined);
    }
  });

  it('reports a JSON-RPC error as a tool error, not a transport failure', () => {
    try {
      parseEnvelope(200, { error: { message: 'no such memory', code: -32602 } }, headers());
      assert.fail('should have thrown');
    } catch (err) {
      assert.ok(err instanceof KimetsuToolError);
      assert.equal(err.code, -32602);
      assert.match(err.message, /no such memory/);
    }
  });

  it('rejects an envelope with no result', () => {
    assert.throws(() => parseEnvelope(200, {}, headers()), KimetsuProtocolError);
  });

  it('returns the result on success', () => {
    assert.deepEqual(parseEnvelope(200, { result: { ok: true } }, headers()), { ok: true });
  });
});

describe('clean', () => {
  it('drops undefined so an unset option is simply absent', () => {
    assert.deepEqual(clean({ a: 1, b: undefined, c: null, d: false }), { a: 1, c: null, d: false });
  });
});

describe('trimTrailingSlashes', () => {
  it('drops a trailing slash', () => {
    assert.equal(trimTrailingSlashes('https://x/'), 'https://x');
  });

  it('drops several', () => {
    assert.equal(trimTrailingSlashes('https://x///'), 'https://x');
  });

  it('leaves a clean url alone', () => {
    assert.equal(trimTrailingSlashes('https://x'), 'https://x');
  });

  it('only trims the tail', () => {
    assert.equal(trimTrailingSlashes('https://x/a/b'), 'https://x/a/b');
  });

  // The regex this replaced (/\/+$/) is a polynomial ReDoS: on a long run of
  // slashes that does NOT end the string, the engine retries from every
  // position in the run. 50k slashes took seconds; a linear scan is instant.
  it('is linear on a long run of slashes that does not end the string', () => {
    const hostile = `https://x${'/'.repeat(50_000)}a`;
    const started = Date.now();
    assert.equal(trimTrailingSlashes(hostile), hostile);
    assert.ok(Date.now() - started < 1_000, 'must not backtrack');
  });
});

describe('buildCall', () => {
  it('produces a tools/call envelope', () => {
    assert.deepEqual(buildCall('kimetsu_brain_status', {}, 7), {
      jsonrpc: '2.0',
      id: 7,
      method: 'tools/call',
      params: { name: 'kimetsu_brain_status', arguments: {} },
    });
  });
});

describe('HttpTransport', () => {
  it('handshakes once, then calls, and numbers ids monotonically', async () => {
    const seen = [];
    const fetchImpl = async (_url, init) => {
      const payload = JSON.parse(init.body);
      seen.push(payload);
      const result =
        payload.method === 'initialize' ? { protocolVersion: '2024-11-05' } : { ok: true };
      return new Response(JSON.stringify({ jsonrpc: '2.0', id: payload.id, result }), {
        status: 200,
      });
    };
    const t = new HttpTransport('https://brain.test/', 'tok', 'owner/repo', { fetchImpl });

    await t.call('kimetsu_brain_status', {});
    await t.call('kimetsu_brain_insights', {});

    assert.equal(seen.length, 3, 'one initialize, then two calls');
    assert.equal(seen[0].method, 'initialize');
    assert.deepEqual(
      seen.map((p) => p.id),
      [1, 2, 3],
    );
  });

  // Nothing serialises calls in JavaScript, so two concurrent first calls will
  // race unless the handshake is shared.
  it('shares one handshake across concurrent first calls', async () => {
    let initializes = 0;
    const fetchImpl = async (_url, init) => {
      const payload = JSON.parse(init.body);
      if (payload.method === 'initialize') initializes += 1;
      return new Response(
        JSON.stringify({ jsonrpc: '2.0', id: payload.id, result: { protocolVersion: '2024-11-05' } }),
        { status: 200 },
      );
    };
    const t = new HttpTransport('https://brain.test', 'tok', 'owner/repo', { fetchImpl });
    await Promise.all([t.call('a', {}), t.call('b', {}), t.call('c', {})]);
    assert.equal(initializes, 1);
  });

  // A client that caches a failed handshake is permanently broken by one blip,
  // with nothing the caller can do about it.
  it('does not cache a failed handshake', async () => {
    let attempts = 0;
    const fetchImpl = async (_url, init) => {
      const payload = JSON.parse(init.body);
      if (payload.method === 'initialize') {
        attempts += 1;
        if (attempts === 1) return new Response('', { status: 503 });
      }
      return new Response(
        JSON.stringify({ jsonrpc: '2.0', id: payload.id, result: { protocolVersion: '2024-11-05' } }),
        { status: 200 },
      );
    };
    const t = new HttpTransport('https://brain.test', 'tok', 'owner/repo', { fetchImpl });
    await assert.rejects(() => t.call('a', {}), KimetsuProtocolError);
    await t.call('a', {});
    assert.equal(attempts, 2, 'the second call retried the handshake');
  });

  it('rejects a server speaking a different protocol version', async () => {
    const fetchImpl = async (_url, init) => {
      const payload = JSON.parse(init.body);
      return new Response(
        JSON.stringify({ jsonrpc: '2.0', id: payload.id, result: { protocolVersion: '1999-01-01' } }),
        { status: 200 },
      );
    };
    const t = new HttpTransport('https://brain.test', 'tok', 'owner/repo', { fetchImpl });
    await assert.rejects(() => t.call('a', {}), KimetsuProtocolError);
  });

  // A proxy returning an HTML 502 must surface as "HTTP 502", not as a JSON
  // syntax error the caller cannot act on.
  it('reports the status, not the parse failure, on non-JSON errors', async () => {
    const fetchImpl = async () => new Response('<html>bad gateway</html>', { status: 502 });
    const t = new HttpTransport('https://brain.test', 'tok', 'owner/repo', { fetchImpl });
    await assert.rejects(() => t.call('a', {}), /HTTP 502/);
  });

  it('builds the repo-scoped MCP url without a doubled slash', async () => {
    let url;
    const fetchImpl = async (u, init) => {
      url = u;
      const payload = JSON.parse(init.body);
      return new Response(
        JSON.stringify({ jsonrpc: '2.0', id: payload.id, result: { protocolVersion: '2024-11-05' } }),
        { status: 200 },
      );
    };
    const t = new HttpTransport('https://brain.test///', 'tok', 'owner/repo', { fetchImpl });
    await t.call('a', {});
    assert.equal(url, 'https://brain.test/mcp/owner/repo');
  });
});

describe('KimetsuClient', () => {
  it('names the missing environment variable', () => {
    const saved = {
      KIMETSU_REMOTE_URL: process.env.KIMETSU_REMOTE_URL,
      KIMETSU_REMOTE_TOKEN: process.env.KIMETSU_REMOTE_TOKEN,
      KIMETSU_REMOTE_REPO: process.env.KIMETSU_REMOTE_REPO,
    };
    delete process.env.KIMETSU_REMOTE_URL;
    delete process.env.KIMETSU_REMOTE_TOKEN;
    delete process.env.KIMETSU_REMOTE_REPO;
    try {
      assert.throws(() => new KimetsuClient(), /KIMETSU_REMOTE_URL/);
      assert.throws(
        () => new KimetsuClient({ baseUrl: 'https://x', repo: 'a/b' }),
        /KIMETSU_REMOTE_TOKEN/,
      );
    } finally {
      for (const [k, v] of Object.entries(saved)) if (v !== undefined) process.env[k] = v;
    }
  });

  it('maps namespace methods onto their tool names', async () => {
    const t = fakeTransport();
    const client = new KimetsuClient({ transport: t });

    await client.memory.add('prefer thiserror', { scope: 'project', kind: 'preference' });
    await client.memory.invalidate('m_1');
    await client.memory.blame('run_1');
    await client.memory.conflictResolve();
    await client.config.show();
    await client.models.list();
    await client.benchmark.recordOutcome('task-1');
    await client.cite('m_2');
    await client.insights();

    assert.deepEqual(
      t.calls.map((c) => c.name),
      [
        'kimetsu_brain_memory_add',
        'kimetsu_brain_memory_invalidate',
        'kimetsu_brain_memory_blame',
        'kimetsu_brain_conflict_resolve',
        'kimetsu_brain_config_show',
        'kimetsu_brain_model_list',
        'kimetsu_benchmark_record_outcome',
        'kimetsu_brain_cite',
        'kimetsu_brain_insights',
      ],
    );
    // camelCase in TypeScript, snake_case on the wire — the wire is what the
    // brain reads, and getting that wrong fails at runtime with an empty result
    // rather than at the type level.
    assert.deepEqual(t.calls[1].args, { memory_id: 'm_1' });
    assert.deepEqual(t.calls[2].args, { run_id: 'run_1' });
  });

  it('omits an unset optional rather than sending undefined', async () => {
    const t = fakeTransport();
    const client = new KimetsuClient({ transport: t });
    await client.memory.add('a fact', { scope: 'project' });
    assert.deepEqual(t.calls[0].args, { text: 'a fact', scope: 'project' });
  });

  // A skipped bundle omits `capsules`; callers should not have to guard every
  // iteration against that.
  it('always returns an iterable capsules array', async () => {
    const t = fakeTransport({ kimetsu_brain_context: { skipped: true, top_score: 0.1 } });
    const client = new KimetsuClient({ transport: t });
    const bundle = await client.context('something the brain does not know');
    assert.deepEqual(bundle.capsules, []);
    assert.equal(bundle.skipped, true);
  });

  it('passes coverage fields through untouched', async () => {
    const t = fakeTransport({
      kimetsu_brain_context: {
        capsules: [{ summary: 'project:fact - a thing', expansion_handle: 'memory:m1' }],
        evidence_coverage: 0.3,
        uncovered_terms: ['kubernetes', 'rollout'],
        partial_evidence_notice: 'nothing above covers kubernetes, rollout.',
      },
    });
    const client = new KimetsuClient({ transport: t });
    const bundle = await client.context('how do we do a kubernetes rollout');
    assert.equal(bundle.evidence_coverage, 0.3);
    assert.deepEqual(bundle.uncovered_terms, ['kubernetes', 'rollout']);
    assert.match(bundle.partial_evidence_notice, /kubernetes/);
  });

  // The brain grows tools faster than the SDK is republished; without this a
  // version mismatch sends users back to shelling out.
  it('can call a tool the SDK does not model', async () => {
    const t = fakeTransport();
    const client = new KimetsuClient({ transport: t });
    await client.call('kimetsu_brain_some_future_tool', { a: 1, b: undefined });
    assert.deepEqual(t.calls[0], { name: 'kimetsu_brain_some_future_tool', args: { a: 1 } });
  });
});
