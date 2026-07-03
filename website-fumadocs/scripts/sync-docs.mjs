// Generate the Fumadocs content set from the canonical docs.
//
// SINGLE SOURCE OF TRUTH: docs/*.md + README.md + CHANGELOG.md. This script
// regenerates `content/docs/` (gitignored is NOT required, but it is fully
// derived) on every `npm run build` via the prebuild hook, so the site never
// drifts from the real docs.
//
// SECURITY: the DOCS allowlist below is the ONLY thing ever published. The
// private roadmap under docs/superpowers/ is structurally excluded by simply
// not being listed here, so it can never leak onto the site.

import {
  readFileSync,
  writeFileSync,
  mkdirSync,
  rmSync,
  copyFileSync,
  readdirSync,
  statSync,
} from 'node:fs';
import { dirname, resolve, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const siteRoot = resolve(here, '..'); // website-fumadocs/scripts -> website-fumadocs
const repo = resolve(siteRoot, '..'); // -> repo root E:\Kimetsu
const outDir = resolve(siteRoot, 'content', 'docs');
const publicDir = resolve(siteRoot, 'public');

const GH = 'https://github.com/RodCor/kimetsu';
const BASE = ''; // served at the domain root (kimetsu.dev)

// Explicit allowlist. Order = sidebar/nav order. `out` names map to route slugs.
const DOCS = [
  { src: 'README.md',                 out: 'index.mdx',            title: 'Introduction' },
  { src: 'docs/INSTALL.md',           out: 'install.mdx',          title: 'Install & Host Wiring' },
  { src: 'docs/LOCAL-MODELS.md',      out: 'local-models.mdx',     title: 'Local Models' },
  { src: 'docs/REMOTE.md',            out: 'remote.mdx',           title: 'Kimetsu Remote' },
  { src: 'docs/ROI-METHODOLOGY.md',   out: 'roi-methodology.mdx',  title: 'Kimetsu Algorithm' },
  { src: 'docs/CONTRIBUTING.md',      out: 'contributing.mdx',     title: 'Contributing' },
  { src: 'docs/CODE_OF_CONDUCT.md',   out: 'code-of-conduct.mdx',  title: 'Code of Conduct' },
  { src: 'CHANGELOG.md',              out: 'changelog.mdx',        title: 'Changelog' },
];

// Multi-page sections rendered as a collapsible sidebar group (a folder with its
// own meta.json). "How Kimetsu Works" was one 900-line page; it is split into
// focused pages here. `after` positions the group in the root nav.
const FOLDERS = [
  {
    dir: 'how-kimetsu-works',
    title: 'How Kimetsu Works',
    after: 'index',
    pages: [
      { src: 'docs/how-kimetsu-works/index.md',         out: 'index.mdx',         title: 'Overview' },
      { src: 'docs/how-kimetsu-works/the-brain.md',     out: 'the-brain.mdx',     title: 'The brain' },
      { src: 'docs/how-kimetsu-works/the-broker.md',    out: 'the-broker.mdx',    title: 'The broker' },
      { src: 'docs/how-kimetsu-works/learning-loop.md', out: 'learning-loop.mdx', title: 'The learning loop' },
      { src: 'docs/how-kimetsu-works/interfaces.md',       out: 'interfaces.mdx',       title: 'Interfaces' },
      { src: 'docs/how-kimetsu-works/retrieval-models.md', out: 'retrieval-models.mdx', title: 'Retrieval models' },
      { src: 'docs/how-kimetsu-works/operations.md',       out: 'operations.mdx',       title: 'Operations' },
      { src: 'docs/how-kimetsu-works/configuration.md',    out: 'configuration.mdx',    title: 'Configuration' },
    ],
  },
  {
    dir: 'memory-benchmark',
    title: 'Memory Benchmark',
    after: 'remote',
    pages: [
      { src: 'docs/memory-benchmark/index.md',                     out: 'index.mdx',                     title: 'Overview' },
      { src: 'docs/memory-benchmark/retrieval-and-correctness.md', out: 'retrieval-and-correctness.mdx', title: 'Retrieval & correctness' },
      { src: 'docs/memory-benchmark/brainbench.md',                out: 'brainbench.mdx',                title: 'BrainBench' },
      { src: 'docs/memory-benchmark/longmemeval.md',               out: 'longmemeval.mdx',               title: 'LongMemEval' },
      { src: 'docs/memory-benchmark/beam.md',                      out: 'beam.mdx',                      title: 'BEAM' },
      { src: 'docs/memory-benchmark/comparison.md',                out: 'comparison.mdx',                title: 'How Kimetsu compares' },
    ],
  },
];

// Root nav order: flat slugs, with each folder inserted after its `after` slug.
const NAV_PAGES = (() => {
  const pages = DOCS.map((d) => d.out.replace(/\.mdx$/, ''));
  for (const f of FOLDERS) {
    const i = pages.indexOf(f.after);
    pages.splice(i < 0 ? pages.length : i + 1, 0, f.dir);
  }
  return pages;
})();

const stripBom = (s) => s.replace(/^﻿/, '');

const SITE = 'https://kimetsu.dev/docs/';

// --- Link / image transforms ---------------------------------------------
// Adapted from website/scripts/sync-docs.mjs for Fumadocs slugs + basePath.
function transformLinks(s) {
  return (
    s
      // README links to this very site (absolute) -> in-site slugs so the
      // Introduction page navigates in-site instead of full-reloading.
      .replaceAll(`${SITE}how-kimetsu-works`, 'how-kimetsu-works')
      .replaceAll(`${SITE}install`, 'install')
      .replaceAll(`${SITE}local-models`, 'local-models')
      .replaceAll(`${SITE}remote`, 'remote')
      .replaceAll(`${SITE}memory-benchmark`, 'memory-benchmark')
      .replaceAll(`${SITE}roi-methodology`, 'roi-methodology')
      .replaceAll(`${SITE}contributing`, 'contributing')
      .replaceAll(`${SITE}code-of-conduct`, 'code-of-conduct')
      .replaceAll(`${SITE}changelog`, 'changelog')
      .replaceAll(SITE, '/docs/') // any remaining doc-root links
      // Images -> static assets under the /kimetsu base path (raw <img> and
      // markdown image links both need the base-prefixed absolute path).
      .replaceAll('docs/assets/kimetsu-logo.png', `${BASE}/kimetsu-logo.png`)
      .replaceAll('docs/assets/demo.gif', `${BASE}/demo.gif`)
      .replaceAll('docs/assets/how-it-works.svg', `${BASE}/how-it-works.svg`)
      // inter-doc links (any of ./  ../  docs/  prefix, with or without .md) -> slugs
      .replace(/\]\((?:\.\/|\.\.\/|docs\/)?HOW-KIMETSU-WORKS(?:\.md)?\)/g, '](how-kimetsu-works)')
      .replace(/\]\((?:\.\/|\.\.\/|docs\/)?INSTALL(?:\.md)?\)/g, '](install)')
      .replace(/\]\((?:\.\/|\.\.\/|docs\/)?LOCAL-MODELS(?:\.md)?\)/g, '](local-models)')
      .replace(/\]\((?:\.\/|\.\.\/|docs\/)?REMOTE(?:\.md)?\)/g, '](remote)')
      .replace(/\]\((?:\.\/|\.\.\/|docs\/)?MEMORY-BENCHMARK(?:\.md)?\)/g, '](memory-benchmark)')
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

// --- MDX hardening ---------------------------------------------------------
// MDX is stricter than CommonMark: bare `{ ... }` in prose is parsed as a JS
// expression, and `<word>` / `<number` can be parsed as a JSX tag. The docs
// contain both (e.g. "∈ {0.3, 0.4}", "--since <cursor>"). We escape these, but
// ONLY in regions that are neither fenced code blocks nor inline code spans —
// inside code, MDX does not interpret braces/tags, so they are already safe and
// must be left byte-for-byte intact.
//
// We also must not touch legitimate self-closing HTML we want to keep as JSX
// (the README's <img .../> and <div align="center"> ... </div>). Those are
// balanced/valid, so we leave a small allowlist of real tags alone.

const REAL_TAG = /^<\/?(?:img|div|br|hr|p|a|b|i|em|strong|ul|ol|li|code|pre|sub|sup|kbd|details|summary|table|thead|tbody|tr|td|th|span|h[1-6]|blockquote)(?:\s[^<>]*)?\/?>/i;

// Escape `<`, `{`, `}` in a plain-text (non-code) chunk, leaving real HTML tags.
function escapePlainText(text) {
  let out = '';
  let i = 0;
  while (i < text.length) {
    const ch = text[i];
    if (ch === '<') {
      const rest = text.slice(i);
      const m = rest.match(REAL_TAG);
      if (m) {
        out += m[0];
        i += m[0].length;
        continue;
      }
      out += '&lt;';
      i += 1;
      continue;
    }
    if (ch === '{') {
      out += '\\{';
      i += 1;
      continue;
    }
    if (ch === '}') {
      out += '\\}';
      i += 1;
      continue;
    }
    out += ch;
    i += 1;
  }
  return out;
}

// Walk the whole document, tracking fenced code blocks (``` / ~~~) and inline
// code spans (backtick runs), escaping only plain-text regions.
function hardenMdx(src) {
  const lines = src.split('\n');
  const outLines = [];
  let inFence = false;
  let fenceMarker = '';

  for (const line of lines) {
    const fenceMatch = line.match(/^\s*(```+|~~~+)/);
    if (fenceMatch) {
      const marker = fenceMatch[1][0].repeat(3); // normalize to a family
      if (!inFence) {
        inFence = true;
        fenceMarker = fenceMatch[1][0];
      } else if (fenceMatch[1][0] === fenceMarker) {
        inFence = false;
        fenceMarker = '';
      }
      outLines.push(line); // fence lines + their content are left verbatim
      continue;
    }
    if (inFence) {
      outLines.push(line);
      continue;
    }
    outLines.push(hardenLine(line));
  }
  return outLines.join('\n');
}

// For a single non-fenced line, split it into inline-code spans (kept verbatim)
// and plain text (escaped). Handles multiple backtick spans; a lone unmatched
// backtick is treated as literal text.
function hardenLine(line) {
  let out = '';
  let i = 0;
  while (i < line.length) {
    if (line[i] === '`') {
      // count the run of backticks
      let n = 0;
      while (line[i + n] === '`') n++;
      const open = '`'.repeat(n);
      const close = open;
      const closeIdx = line.indexOf(close, i + n);
      if (closeIdx !== -1) {
        // inline code span: keep verbatim including delimiters
        const end = closeIdx + n;
        out += line.slice(i, end);
        i = end;
        continue;
      }
      // no closing run on this line: treat the backticks as literal text and
      // continue escaping the remainder.
      out += line.slice(i, i + n);
      i += n;
      continue;
    }
    // accumulate a plain-text chunk up to the next backtick
    let j = i;
    while (j < line.length && line[j] !== '`') j++;
    out += escapePlainText(line.slice(i, j));
    i = j;
  }
  return out;
}

// --- description from first prose paragraph --------------------------------
function firstParagraph(body) {
  const lines = body.split('\n');
  const buf = [];
  let started = false;
  for (const raw of lines) {
    const line = raw.trim();
    if (!started) {
      // skip headings, html, images, badges, hr, blank
      if (!line) continue;
      if (/^[#>\-*|]/.test(line)) continue;
      if (/^</.test(line)) continue;
      if (/^\[!\[/.test(line) || /^!\[/.test(line)) continue;
      if (/^\[/.test(line)) continue;
      started = true;
      buf.push(line);
    } else {
      if (!line) break;
      if (/^[#>|]/.test(line)) break;
      buf.push(line);
    }
  }
  let text = buf.join(' ').replace(/\s+/g, ' ').trim();
  // strip markdown emphasis/links for a clean description
  text = text
    .replace(/!\[[^\]]*\]\([^)]*\)/g, '')
    .replace(/\[([^\]]*)\]\([^)]*\)/g, '$1')
    .replace(/[*_`]/g, '')
    .trim();
  return clampDescription(text);
}

// Trim to a clean, complete description: whole sentences up to ~170 chars,
// never cut mid-word (Fumadocs shows this in the doc cards + <meta>).
function clampDescription(text, max = 170) {
  if (text.length <= max) return text;
  const sentences = text.match(/[^.!?]+[.!?]+(?:\s|$)/g) || [text];
  let out = '';
  for (const s of sentences) {
    if (out && (out + s).trim().length > max) break;
    out += s;
    if (out.trim().length >= max) break;
  }
  out = out.trim();
  // A single sentence longer than the limit: cut at the last word boundary.
  if (out.length > max) {
    out = out.slice(0, max);
    const sp = out.lastIndexOf(' ');
    if (sp > 40) out = out.slice(0, sp);
    out = out.replace(/[,;:\s]+$/, '') + '…';
  }
  return out;
}

// YAML double-quoted scalar escaping.
const yamlStr = (s) => '"' + s.replace(/\\/g, '\\\\').replace(/"/g, '\\"') + '"';

// --- assets ---------------------------------------------------------------
function copyAssets() {
  mkdirSync(publicDir, { recursive: true });
  // Single committed source: docs/assets (self-contained; no dependency on the
  // retired Docusaurus website/).
  const sources = [resolve(repo, 'docs', 'assets')];
  const wanted = new Set([
    'kimetsu-logo.png',
    'demo.gif',
    'favicon.ico',
    'logo.svg',
    'how-it-works.svg',
  ]);
  for (const dir of sources) {
    let entries;
    try {
      entries = readdirSync(dir);
    } catch {
      continue;
    }
    for (const name of entries) {
      const full = join(dir, name);
      try {
        if (!statSync(full).isFile()) continue;
      } catch {
        continue;
      }
      // copy the branding-critical assets from either source; docs/assets wins
      if (wanted.has(name)) {
        copyFileSync(full, join(publicDir, name));
        console.log(`asset ${dir === sources[0] ? 'docs/assets' : 'website/static/img'}/${name} -> public/${name}`);
      }
    }
  }
}

// --- main -----------------------------------------------------------------
rmSync(outDir, { recursive: true, force: true });
mkdirSync(outDir, { recursive: true });

copyAssets();

function writePage(src, outRel, title) {
  const raw = stripBom(readFileSync(resolve(repo, src), 'utf8'));
  const linked = transformLinks(raw);
  const description = firstParagraph(linked);
  const hardened = hardenMdx(linked);
  const fm = [
    '---',
    `title: ${yamlStr(title)}`,
    ...(description ? [`description: ${yamlStr(description)}`] : []),
    '---',
    '',
    '',
  ].join('\n');
  const dest = resolve(outDir, outRel);
  mkdirSync(dirname(dest), { recursive: true });
  writeFileSync(dest, fm + hardened, 'utf8');
  console.log(`synced ${src} -> content/docs/${outRel.replace(/\\/g, '/')}`);
}

let count = 0;

for (const d of DOCS) {
  writePage(d.src, d.out, d.title);
  count++;
}

// Folder sections: pages + a per-folder meta.json (the sidebar group).
for (const f of FOLDERS) {
  for (const pg of f.pages) {
    writePage(pg.src, join(f.dir, pg.out), pg.title);
    count++;
  }
  writeFileSync(
    resolve(outDir, f.dir, 'meta.json'),
    JSON.stringify(
      { title: f.title, pages: f.pages.map((p) => p.out.replace(/\.mdx$/, '')) },
      null,
      2,
    ) + '\n',
    'utf8',
  );
  console.log(`wrote content/docs/${f.dir}/meta.json`);
}

// Root meta.json — nav order (index is the docs root; folders inserted).
writeFileSync(
  resolve(outDir, 'meta.json'),
  JSON.stringify({ title: 'Docs', pages: NAV_PAGES }, null, 2) + '\n',
  'utf8',
);
console.log('wrote content/docs/meta.json');

console.log(
  `\n${count} docs synced (allowlist only; docs/superpowers/ is never included).`,
);
