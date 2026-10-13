import assert from 'node:assert/strict';
import { test } from 'node:test';
import { stagePackage } from './stage-package.mjs';

const shasum = 'a'.repeat(40);

function fixture({ status = 404, stages = [], response = { stageId: 'new-stage' } } = {}) {
  const calls = [];
  return {
    calls,
    io: {
      manifest: () => ({ name: '@kimetsu-ai/linux-x64', version: '2.9.0' }),
      published: async () => ({ status }),
      run: (args) => {
        calls.push(args);
        if (args[0] === 'pack') return [{ filename: 'package.tgz', shasum }];
        if (args[1] === 'list') return stages;
        if (args[1] === 'publish') return response;
        throw new Error('Unexpected command');
      },
    },
  };
}

test('public version skips without packing or authenticating', async () => {
  const f = fixture({ status: 200 });
  assert.equal((await stagePackage('/package', f.io)).state, 'published');
  assert.deepEqual(f.calls, []);
});
test('registry errors cannot be mistaken for an unpublished version', async () => {
  for (const status of [401, 403, 429, 500]) {
    const f = fixture({ status });
    await assert.rejects(stagePackage('/package', f.io), /Registry lookup failed/);
    assert.deepEqual(f.calls, []);
  }
});
test('new version stages exact packed tarball and never approves', async () => {
  const f = fixture();
  const result = await stagePackage('/package', f.io);
  assert.equal(result.stageId, 'new-stage');
  assert.equal(result.state, 'staged');
  assert.deepEqual(f.calls[1].slice(0, 3), ['stage', 'list', '@kimetsu-ai/linux-x64']);
  assert.deepEqual(f.calls[2].slice(0, 2), ['stage', 'publish']);
  assert.ok(f.calls[2].includes('--ignore-scripts'));
  assert.ok(!f.calls.some(args => args.includes('approve')));
});
test('identical existing stage is reused without resubmission', async () => {
  const f = fixture({ stages: [{ packageName: '@kimetsu-ai/linux-x64', version: '2.9.0', id: 'existing', tag: 'latest', shasum }] });
  assert.equal((await stagePackage('/package', f.io)).stageId, 'existing');
  assert.equal(f.calls.length, 2);
});
test('conflicting staged bytes or tag fail without submission', async () => {
  for (const changes of [{ shasum: 'different' }, { tag: 'next' }]) {
    const f = fixture({ stages: [{ packageName: '@kimetsu-ai/linux-x64', version: '2.9.0', id: 'existing', tag: 'latest', shasum, ...changes }] });
    await assert.rejects(stagePackage('/package', f.io), /Existing stage differs/);
    assert.equal(f.calls.length, 2);
  }
});
test('malformed or failed stage listing never triggers submission', async () => {
  const f = fixture({ stages: {} });
  await assert.rejects(stagePackage('/package', f.io), /Invalid stage list/);
  assert.equal(f.calls.length, 2);
});
test('a successful response without a stage ID is not called staged', async () => {
  const f = fixture({ response: {} });
  await assert.rejects(stagePackage('/package', f.io), /Missing stage ID/);
});

test('stage-list failure is propagated without submission', async () => {
  const f = fixture();
  const run = f.io.run;
  f.io.run = args => {
    if (args[1] === 'list') throw new Error('Authentication failed');
    return run(args);
  };
  await assert.rejects(stagePackage('/package', f.io), /Authentication failed/);
  assert.equal(f.calls.length, 1);
});

test('ambiguous duplicate stages stop without submission', async () => {
  const entry = { packageName: '@kimetsu-ai/linux-x64', version: '2.9.0', id: 'existing', tag: 'latest', shasum };
  const f = fixture({ stages: [entry, entry] });
  await assert.rejects(stagePackage('/package', f.io), /Existing stage differs/);
  assert.equal(f.calls.length, 2);
});
