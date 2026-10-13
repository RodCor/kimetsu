import fs from 'node:fs';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { pathToFileURL } from 'node:url';

const registry = 'https://registry.npmjs.org';

function liveIO(directory) {
  return {
    manifest: () => JSON.parse(fs.readFileSync(path.join(directory, 'package.json'), 'utf8')),
    published: async (name, version) => {
      const response = await fetch(`${registry}/${encodeURIComponent(name)}/${version}`, {
        signal: AbortSignal.timeout(30000), redirect: 'error',
      });
      if (response.status === 200) {
        const body = await response.json();
        if (body.name !== name || body.version !== version) throw new Error('Unexpected registry version');
      }
      return { status: response.status };
    },
    run: (args) => {
      const result = spawnSync('npm', [...args, `--registry=${registry}`], {
        cwd: directory, encoding: 'utf8', stdio: ['ignore', 'pipe', 'inherit'],
      });
      if (result.error || result.status !== 0) throw new Error(`npm ${args.slice(0, 2).join(' ')} failed; no fallback attempted`);
      return JSON.parse(result.stdout);
    },
  };
}

export async function stagePackage(directory, io = liveIO(directory)) {
  const { name, version } = io.manifest();
  if (!/^(?:@kimetsu-ai\/[a-z0-9-]+|kimetsu-ai|kimetsu-remote)$/.test(name)
      || !/^\d+\.\d+\.\d+$/.test(version)) throw new Error('Unexpected package name/version');
  const published = await io.published(name, version);
  if (published.status === 200) return { name, version, state: 'published' };
  if (published.status !== 404) throw new Error(`Registry lookup failed (${published.status}); no staging attempted`);

  const packed = io.run(['pack', '--json', '--ignore-scripts']);
  if (!Array.isArray(packed) || packed.length !== 1 || !/^[a-f0-9]{40}$/.test(packed[0].shasum)
      || typeof packed[0].filename !== 'string'
      || path.basename(packed[0].filename) !== packed[0].filename) throw new Error('Invalid npm pack result');
  // npm 11.21 stage list accepts package NAME, not name@version.
  const stages = io.run(['stage', 'list', name, '--json']);
  if (!Array.isArray(stages)) throw new Error('Invalid stage list; no staging attempted');
  const matches = stages.filter(item => item.packageName === name && item.version === version);
  if (matches.length) {
    if (matches.length !== 1 || matches[0].shasum !== packed[0].shasum || matches[0].tag !== 'latest') {
      throw new Error('Existing stage differs; maintainer must review it before any retry');
    }
    if (typeof matches[0].id !== 'string' || !matches[0].id) throw new Error('Missing stage ID');
    return { name, version, state: 'already-staged', stageId: matches[0].id, shasum: packed[0].shasum };
  }
  const result = io.run(['stage', 'publish', path.resolve(directory, packed[0].filename),
    '--tag', 'latest', '--access', 'public', '--provenance', '--ignore-scripts', '--json']);
  if (typeof result.stageId !== 'string' || !result.stageId) throw new Error('Missing stage ID; inspect npm before retrying');
  return { name, version, state: 'staged', stageId: result.stageId, shasum: packed[0].shasum };
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  try {
    const result = await stagePackage(path.resolve(process.argv[2] ?? '.'));
    console.log(JSON.stringify(result));
    if (process.env.GITHUB_STEP_SUMMARY) {
      fs.appendFileSync(process.env.GITHUB_STEP_SUMMARY,
        `- ${result.name}@${result.version}: ${result.state}${result.stageId ? `; stage ID: ${result.stageId}; tarball SHA-1: ${result.shasum}` : ''}\n`);
    }
  } catch (error) {
    console.error(error.message);
    process.exitCode = 1;
  }
}
