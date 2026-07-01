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
const BASE = '/kimetsu'; // GitHub Pages base path

// Explicit allowlist. Order = sidebar/nav order. `out` names map to route slugs.
const DOCS = [
  { src: 'README.md',                 out: 'index.mdx',            title: 'Introduction' },
  { src: 'docs/HOW-KIMETSU-WORKS.md', out: 'how-kimetsu-works.mdx', title: 'How Kimetsu Works' },
  { src: 'docs/INSTALL.md',           out: 'install.mdx',          title: 'Install & Host Wiring' },
  { src: 'docs/LOCAL-MODELS.md',      out: 'local-models.mdx',     title: 'Local Models' },
  { src: 'docs/REMOTE.md',            out: 'remote.mdx',           title: 'Kimetsu Remote' },
  { src: 'docs/MEMORY-BENCHMARK.md',  out: 'memory-benchmark.mdx', title: 'Memory Benchmark' },
  { src: 'docs/ROI-METHODOLOGY.md',   out: 'roi-methodology.mdx',  title: 'ROI Methodology' },
  { src: 'docs/CONTRIBUTING.md',      out: 'contributing.mdx',     title: 'Contributing' },
  { src: 'docs/CODE_OF_CONDUCT.md',   out: 'code-of-conduct.mdx',  title: 'Code of Conduct' },
  { src: 'CHANGELOG.md',              out: 'changelog.mdx',        title: 'Changelog' },
];

// Nav order = the route slugs (out without .mdx). index is the docs root.
const NAV_PAGES = DOCS.map((d) => d.out.replace(/\.mdx$/, ''));

const stripBom = (s) => s.replace(/^﻿/, '');

const SITE = 'https://rodcor.github.io/kimetsu/docs/';

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
      .replaceAll(SITE, '/kimetsu/docs/') // any remaining doc-root links
      // Images -> static assets under the /kimetsu base path (raw <img> and
      // markdown image links both need the base-prefixed absolute path).
      .replaceAll('docs/assets/kimetsu-logo.png', `${BASE}/kimetsu-logo.png`)
      .replaceAll('docs/assets/demo.gif', `${BASE}/demo.gif`)
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
  if (text.length > 200) text = text.slice(0, 197).trimEnd() + '...';
  return text;
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

for (const d of DOCS) {
  const raw = stripBom(readFileSync(resolve(repo, d.src), 'utf8'));
  const linked = transformLinks(raw);
  const description = firstParagraph(linked);
  const hardened = hardenMdx(linked);
  const fm = [
    '---',
    `title: ${yamlStr(d.title)}`,
    ...(description ? [`description: ${yamlStr(description)}`] : []),
    '---',
    '',
    '',
  ].join('\n');
  writeFileSync(resolve(outDir, d.out), fm + hardened, 'utf8');
  console.log(`synced ${d.src} -> content/docs/${d.out}`);
}

// meta.json — nav order (index is the docs root).
writeFileSync(
  resolve(outDir, 'meta.json'),
  JSON.stringify({ title: 'Docs', pages: NAV_PAGES }, null, 2) + '\n',
  'utf8',
);
console.log('wrote content/docs/meta.json');

console.log(
  `\n${DOCS.length} docs synced (allowlist only; docs/superpowers/ is never included).`,
);
