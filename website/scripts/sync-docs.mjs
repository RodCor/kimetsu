// Generate the Docusaurus content set from the canonical docs.
//
// SINGLE SOURCE OF TRUTH: docs/*.md + README.md + CHANGELOG.md. This script
// copies the PUBLIC docs into `docs-site-content/` (gitignored, regenerated on
// every `npm run build` / `npm start` via the prebuild/prestart hooks) so the
// site never drifts from the real docs.
//
// SECURITY: the DOCS allowlist below is the ONLY thing ever published. The
// private roadmap under docs/superpowers/ is structurally excluded by simply
// not being listed here, so it can never leak onto the site.

import { readFileSync, writeFileSync, mkdirSync, rmSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const repo = resolve(here, '..', '..'); // website/scripts -> repo root
const outDir = resolve(repo, 'docs-site-content');

const GH = 'https://github.com/RodCor/kimetsu';

// Explicit allowlist. Order = sidebar order.
const DOCS = [
  { src: 'README.md',                out: 'intro.md',             title: 'Introduction',          pos: 1, slug: '/intro' },
  { src: 'docs/HOW-KIMETSU-WORKS.md', out: 'how-kimetsu-works.md', title: 'How Kimetsu Works',     pos: 2 },
  { src: 'docs/INSTALL.md',          out: 'install.md',           title: 'Install & Host Wiring', pos: 3 },
  { src: 'docs/LOCAL-MODELS.md',     out: 'local-models.md',      title: 'Local Models',          pos: 4 },
  { src: 'docs/REMOTE.md',           out: 'remote.md',            title: 'Kimetsu Remote',        pos: 5 },
  { src: 'docs/ROI-METHODOLOGY.md',  out: 'roi-methodology.md',   title: 'ROI Methodology',       pos: 6 },
  { src: 'docs/CONTRIBUTING.md',     out: 'contributing.md',      title: 'Contributing',          pos: 7 },
  { src: 'docs/CODE_OF_CONDUCT.md',  out: 'code-of-conduct.md',   title: 'Code of Conduct',       pos: 8 },
  { src: 'CHANGELOG.md',             out: 'changelog.md',         title: 'Changelog',             pos: 9 },
];

const stripBom = (s) => s.replace(/^﻿/, '');

function transformLinks(s) {
  return (
    s
      // README images -> Docusaurus static assets (raw <img> tags need the baseUrl)
      .replaceAll('docs/assets/kimetsu-logo.png', '/kimetsu/img/kimetsu-logo.png')
      .replaceAll('docs/assets/demo.gif', '/kimetsu/img/demo.gif')
      // inter-doc links (any of ./  ../  docs/  prefix, with or without .md) -> on-site slugs
      .replace(/\]\((?:\.\/|\.\.\/|docs\/)?HOW-KIMETSU-WORKS(?:\.md)?\)/g, '](how-kimetsu-works)')
      .replace(/\]\((?:\.\/|\.\.\/|docs\/)?INSTALL(?:\.md)?\)/g, '](install)')
      .replace(/\]\((?:\.\/|\.\.\/|docs\/)?LOCAL-MODELS(?:\.md)?\)/g, '](local-models)')
      .replace(/\]\((?:\.\/|\.\.\/|docs\/)?REMOTE(?:\.md)?\)/g, '](remote)')
      .replace(/\]\((?:\.\/|\.\.\/|docs\/)?ROI-METHODOLOGY(?:\.md)?\)/g, '](roi-methodology)')
      .replace(/\]\((?:\.\/|\.\.\/|docs\/)?CONTRIBUTING(?:\.md)?\)/g, '](contributing)')
      .replace(/\]\((?:\.\/|\.\.\/|docs\/)?CODE_OF_CONDUCT(?:\.md)?\)/g, '](code-of-conduct)')
      .replace(/\]\((?:\.\/|\.\.\/|docs\/)?CHANGELOG(?:\.md)?\)/g, '](changelog)')
      // repo-relative file/dir links that have no on-site page -> GitHub
      .replace(/\]\((?:\.\.\/)?npm\/?\)/g, `](${GH}/tree/main/npm)`)
      .replaceAll('docs/LICENSE-MIT', `${GH}/blob/main/docs/LICENSE-MIT`)
      .replaceAll('docs/LICENSE-APACHE', `${GH}/blob/main/docs/LICENSE-APACHE`)
  );
}

rmSync(outDir, { recursive: true, force: true });
mkdirSync(outDir, { recursive: true });

for (const d of DOCS) {
  const body = transformLinks(stripBom(readFileSync(resolve(repo, d.src), 'utf8')));
  const fm = [
    '---',
    `title: ${JSON.stringify(d.title)}`,
    `sidebar_position: ${d.pos}`,
    ...(d.slug ? [`slug: ${d.slug}`] : []),
    '---',
    '',
    '',
  ].join('\n');
  writeFileSync(resolve(outDir, d.out), fm + body, 'utf8');
  console.log(`synced ${d.src} -> docs-site-content/${d.out}`);
}

console.log(`\n${DOCS.length} docs synced (allowlist only; docs/superpowers/ is never included).`);
