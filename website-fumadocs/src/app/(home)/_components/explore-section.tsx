'use client';

import { useState } from 'react';
import Link from 'next/link';
import {
  ArrowRight,
  BrainCircuit,
  CheckCircle2,
  Clock,
  Copy,
  Database,
  RefreshCw,
  Search,
  Star,
  Terminal,
  TrendingUp,
} from 'lucide-react';

/* ── data ─────────────────────────────────────────────────────────────── */

const loopSteps = [
  { icon: Terminal, title: 'Work', body: 'The agent solves a task and earns a lesson worth keeping.' },
  { icon: Database, title: 'Capture', body: 'It lands in the brain as a memory. No model, no cloud, no cost.' },
  { icon: Search, title: 'Recall', body: 'Next run it comes back by meaning, before the work even starts.' },
  { icon: CheckCircle2, title: 'Cite', body: 'The agent marks the memories that actually moved the task.' },
  { icon: TrendingUp, title: 'Learn', body: 'Cited memories rise in rank; stale, unused ones decay and prune.' },
];

const STAGES = ['Lexical FTS5', 'Semantic embeddings', 'Cross-encoder rerank', 'HyDE expansion'];
const levels = [
  { name: 'basic', active: 1, note: 'Keyword search, zero model load. Fastest, lightest.' },
  { name: 'flexible', active: 2, note: 'Adds local embeddings, so meaning matches beat exact wording.' },
  { name: 'deep', active: 3, note: 'Adds a local cross-encoder that re-ranks the top hits.', default: true },
  { name: 'advanced', active: 4, note: 'Adds HyDE query expansion for the hardest recall.' },
];

// Architecture edge — the rows Kimetsu wins outright.
const archCols = ['Kimetsu', 'mem0', 'Cognee', 'Zep', 'Letta'];
const archRows = [
  { label: 'Model in the memory pipeline', cells: ['None', 'LLM', 'LLM', 'LLM', 'LLM'] },
  { label: 'Cost to store and recall', cells: ['$0', 'Metered', 'Metered', 'Metered', 'Metered'] },
  { label: 'Runs fully on your machine', cells: ['Yes', 'Self-host / cloud', 'Self-host / cloud', 'Cloud', 'Self-host / cloud'] },
];

// BEAM by token scale. Blank = not publicly reported at that scale.
const beamCols = ['', 'Kimetsu', 'Cognee', 'mem0', 'Honcho'];
const beamRows = [
  { label: 'BEAM 100K', cells: ['73.3%', '79%', '—', '63%'] },
  { label: 'BEAM 1M', cells: ['66.0%', '—', '62%', '63%'] },
  { label: 'BEAM 10M', cells: ['—', '67%', '48.6%', '41%'] },
];

// BrainBench — our own reader-free benchmark. Scores from a live 142-scenario run.
const brainbench = {
  overall: '80.0%',
  dims: [
    { icon: Clock, name: 'Forgetting', score: '88%', n: 49, body: 'Stale and superseded memories decay and get pruned instead of lingering.' },
    { icon: Copy, name: 'Dedup', score: '77%', n: 50, body: 'Duplicate and reworded memories are detected before they pile up.' },
    { icon: Star, name: 'Importance', score: '76%', n: 38, body: 'Useful memories rank above noise, so the right lesson surfaces first.' },
    { icon: Search, name: 'Retrieval', score: '63%', n: 5, body: 'The memory a query needs comes back by meaning, not just keywords.' },
  ],
};

/* ── panels ───────────────────────────────────────────────────────────── */

function LoopPanel() {
  return (
    <div>
      <p className="mx-auto mb-12 max-w-2xl text-center text-fd-muted-foreground">
        Most memory tools are a bucket you read and write. Kimetsu is a cycle:
        every session feeds the next one, and the knowledge that proves useful is
        the knowledge that survives.
      </p>
      <div className="relative">
        <div
          aria-hidden
          className="absolute left-6 top-0 hidden h-full w-px bg-gradient-to-b from-fd-primary/40 via-fd-primary/20 to-transparent lg:left-0 lg:top-6 lg:h-px lg:w-full lg:bg-gradient-to-r"
        />
        <ol className="relative grid gap-8 lg:grid-cols-5 lg:gap-4">
          {loopSteps.map((s, i) => (
            <li key={s.title} className="flex gap-4 lg:flex-col lg:gap-3">
              <div className="relative z-10 flex size-12 shrink-0 items-center justify-center rounded-xl border border-fd-border bg-fd-background text-fd-primary">
                <s.icon className="size-5" aria-hidden />
                <span className="absolute -right-1.5 -top-1.5 flex size-5 items-center justify-center rounded-full border border-fd-border bg-fd-card font-mono text-[10px] font-bold text-fd-muted-foreground">
                  {i + 1}
                </span>
              </div>
              <div className="lg:pr-4">
                <h3 className="mb-1 font-mono text-sm font-semibold tracking-tight">{s.title}</h3>
                <p className="text-sm text-fd-muted-foreground">{s.body}</p>
              </div>
            </li>
          ))}
        </ol>
      </div>
      <div className="mt-10 flex items-center gap-3">
        <div aria-hidden className="h-px flex-1 bg-gradient-to-r from-transparent to-fd-primary/40" />
        <span className="inline-flex items-center gap-2 rounded-full border border-fd-primary/40 bg-fd-primary/5 px-4 py-1.5 font-mono text-xs font-medium text-fd-primary">
          <RefreshCw className="size-3.5" aria-hidden />
          feeds the next session
        </span>
        <div aria-hidden className="h-px flex-1 bg-gradient-to-l from-transparent to-fd-primary/40" />
      </div>
    </div>
  );
}

function LevelsPanel() {
  return (
    <div className="mx-auto max-w-3xl">
      <p className="mx-auto mb-8 max-w-2xl text-center text-fd-muted-foreground">
        Each level stacks one more local stage onto the last. Every stage runs on
        your machine, so more accuracy costs compute, never a token.
      </p>
      <div className="mb-4 flex items-center justify-between px-1 font-mono text-xs text-fd-muted-foreground">
        <span>← faster, lighter</span>
        <span>more accurate, heavier →</span>
      </div>
      <div className="flex flex-col gap-3">
        {levels.map((lvl) => (
          <div
            key={lvl.name}
            className={`rounded-xl border p-4 sm:p-5 transition-colors hover:border-fd-primary/40 ${
              lvl.default ? 'border-fd-primary/50 bg-fd-primary/5' : 'border-fd-border bg-fd-background'
            }`}
          >
            <div className="mb-3 flex flex-wrap items-center gap-2">
              <code
                className={`rounded-md px-2 py-0.5 font-mono text-sm font-semibold ${
                  lvl.default ? 'bg-fd-primary/15 text-fd-primary' : 'bg-fd-card text-fd-foreground'
                }`}
              >
                {lvl.name}
              </code>
              {lvl.default && (
                <span className="rounded-full border border-fd-primary/40 px-2 py-0.5 font-mono text-[10px] font-medium uppercase tracking-wider text-fd-primary">
                  default for new projects
                </span>
              )}
              <span className="ml-auto text-sm text-fd-muted-foreground">{lvl.note}</span>
            </div>
            <div className="flex flex-wrap gap-1.5">
              {STAGES.map((stage, i) => {
                const on = i < lvl.active;
                return (
                  <span
                    key={stage}
                    className={`rounded-md border px-2.5 py-1 font-mono text-xs ${
                      on
                        ? 'border-fd-primary/40 bg-fd-primary/10 text-fd-primary'
                        : 'border-dashed border-fd-border text-fd-muted-foreground/50'
                    }`}
                  >
                    {stage}
                  </span>
                );
              })}
            </div>
          </div>
        ))}
      </div>
      <p className="mt-5 text-center text-xs text-fd-muted-foreground">
        Prefer full control? Set <code className="font-mono text-fd-foreground">level = &quot;custom&quot;</code> and
        tune the embedder, reranker, and HyDE by hand.
      </p>
    </div>
  );
}

function BenchmarksPanel() {
  return (
    <div className="mx-auto max-w-4xl">
      <p className="mx-auto mb-8 max-w-2xl text-center text-fd-muted-foreground">
        mem0, Cognee, Zep, Honcho, and Letta all call a model to build and query
        memory, so every stored fact and every lookup carries token cost. Kimetsu
        runs the whole memory pipeline on local compute, at $0.
      </p>

      {/* Architecture edge */}
      <div className="mb-8 overflow-x-auto rounded-xl border border-fd-border bg-fd-background">
        <table className="w-full min-w-[640px] text-sm">
          <thead>
            <tr className="border-b border-fd-border text-fd-muted-foreground">
              <th className="p-3 text-left font-medium">The architecture edge</th>
              {archCols.map((c, i) => (
                <th key={c} className={`p-3 text-right font-medium ${i === 0 ? 'text-fd-primary' : ''}`}>
                  {c}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {archRows.map((row) => (
              <tr key={row.label} className="border-b border-fd-border/60 last:border-0">
                <td className="p-3 text-fd-muted-foreground">{row.label}</td>
                {row.cells.map((cell, i) => (
                  <td
                    key={i}
                    className={`p-3 text-right tabular-nums ${
                      i === 0 ? 'font-semibold text-fd-primary' : 'text-fd-muted-foreground'
                    }`}
                  >
                    {cell}
                  </td>
                ))}
              </tr>
            ))}
          </tbody>
        </table>
      </div>

      {/* LoCoMo */}
      <h3 className="mb-2 font-mono text-sm font-semibold tracking-tight">
        LoCoMo, the long-conversation benchmark
      </h3>
      <div className="mb-8 overflow-x-auto rounded-xl border border-fd-border bg-fd-background">
        <table className="w-full min-w-[560px] text-sm">
          <thead>
            <tr className="border-b border-fd-border text-fd-muted-foreground">
              <th className="p-3 text-left font-medium">System</th>
              <th className="p-3 text-right font-medium text-fd-primary">Kimetsu</th>
              <th className="p-3 text-right font-medium">mem0</th>
              <th className="p-3 text-right font-medium">Honcho</th>
              <th className="p-3 text-right font-medium">Zep</th>
              <th className="p-3 text-right font-medium">Letta</th>
            </tr>
          </thead>
          <tbody>
            <tr>
              <td className="p-3 text-fd-muted-foreground">LoCoMo (1,540 questions)</td>
              <td className="p-3 text-right font-semibold tabular-nums text-fd-primary">89.4%</td>
              <td className="p-3 text-right tabular-nums text-fd-muted-foreground">92.5%</td>
              <td className="p-3 text-right tabular-nums text-fd-muted-foreground">89.9%</td>
              <td className="p-3 text-right tabular-nums text-fd-muted-foreground">75.1%</td>
              <td className="p-3 text-right tabular-nums text-fd-muted-foreground">74.0%</td>
            </tr>
          </tbody>
        </table>
        <p className="border-t border-fd-border p-3 text-xs text-fd-muted-foreground">
          LLM-judged accuracy on the standard 1,540-question set, as published by
          each vendor. Kimetsu is the only system here with no LLM in the memory
          pipeline. Harness and full methodology are public.
        </p>
      </div>

      {/* BEAM by scale */}
      <h3 className="mb-2 font-mono text-sm font-semibold tracking-tight">
        BEAM, by conversation size
      </h3>
      <div className="mb-8 overflow-x-auto rounded-xl border border-fd-border bg-fd-background">
        <table className="w-full min-w-[560px] text-sm">
          <thead>
            <tr className="border-b border-fd-border text-fd-muted-foreground">
              {beamCols.map((c, i) => (
                <th key={c || 'lbl'} className={`p-3 font-medium ${i === 0 ? 'text-left' : 'text-right'} ${i === 1 ? 'text-fd-primary' : ''}`}>
                  {c}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {beamRows.map((row) => (
              <tr key={row.label} className="border-b border-fd-border/60 last:border-0">
                <td className="p-3 text-fd-muted-foreground">{row.label}</td>
                {row.cells.map((cell, i) => (
                  <td
                    key={i}
                    className={`p-3 text-right tabular-nums ${
                      i === 0 ? 'font-semibold text-fd-primary' : 'text-fd-muted-foreground'
                    }`}
                  >
                    {cell}
                  </td>
                ))}
              </tr>
            ))}
          </tbody>
        </table>
        <p className="border-t border-fd-border p-3 text-xs text-fd-muted-foreground">
          Kimetsu&apos;s 73.3% (100K, 400 probes) matches the prior public state of
          the art on that bucket and it leads mem0 at 1M, with no model in the
          retrieval path. Cognee (a knowledge-graph system with an LLM in the loop)
          leads at 100K/10M. Vendor numbers are self-reported; a blank cell means no
          comparable public figure at that scale.
        </p>
      </div>

      <div className="mt-6 text-center">
        <Link
          href="/docs/memory-benchmark"
          className="inline-flex items-center gap-1.5 text-sm font-medium text-fd-primary hover:underline"
        >
          Read the full methodology <ArrowRight className="size-4" />
        </Link>
      </div>
    </div>
  );
}

function BrainBenchPanel() {
  return (
    <div className="mx-auto max-w-4xl">
      <p className="mx-auto mb-8 max-w-2xl text-center text-fd-muted-foreground">
        Every benchmark above grades an LLM reading from memory, so the reader model
        colors the score. BrainBench is the one we built to grade the memory
        <span className="text-fd-foreground"> itself</span> — reader-free, no model
        in the loop. It scores four things a brain has to get right, over 142
        scenarios.
      </p>

      {/* Overall score + dimensions */}
      <div className="grid gap-4 md:grid-cols-[auto_1fr] md:items-stretch">
        <div className="flex flex-col items-center justify-center rounded-xl border border-fd-primary/50 bg-fd-primary/5 p-8 text-center">
          <span className="mb-1 inline-flex items-center gap-1.5 font-mono text-xs font-medium uppercase tracking-wider text-fd-primary">
            <BrainCircuit className="size-3.5" /> Brain Quality Index
          </span>
          <span className="font-mono text-5xl font-bold tabular-nums text-fd-primary">
            {brainbench.overall}
          </span>
          <span className="mt-1 text-xs text-fd-muted-foreground">142 scenarios, reader-free</span>
        </div>

        <div className="grid gap-3 sm:grid-cols-2">
          {brainbench.dims.map((d) => (
            <div key={d.name} className="rounded-xl border border-fd-border bg-fd-background p-4">
              <div className="mb-1.5 flex items-center gap-2">
                <span className="inline-flex rounded-md border border-fd-border bg-fd-card p-1.5 text-fd-primary">
                  <d.icon className="size-4" aria-hidden />
                </span>
                <span className="font-mono text-sm font-semibold">{d.name}</span>
                <span className="font-mono text-[10px] text-fd-muted-foreground">n={d.n}</span>
                <span className="ml-auto font-mono text-sm font-bold tabular-nums text-fd-primary">{d.score}</span>
              </div>
              {/* score bar */}
              <div className="mb-2 h-1.5 w-full overflow-hidden rounded-full bg-fd-border/50">
                <div className="h-full rounded-full bg-fd-primary" style={{ width: d.score }} />
              </div>
              <p className="text-xs text-fd-muted-foreground">{d.body}</p>
            </div>
          ))}
        </div>
      </div>

      <p className="mt-6 text-center text-xs text-fd-muted-foreground">
        A live run of the open <code className="font-mono text-fd-foreground">kbench brainbench</code> harness.
        Because no LLM reads the answers, this score is a direct measure of the brain
        and does not drift with the reader model. We publish it as-is, including
        dedup, where we still have headroom.
      </p>
    </div>
  );
}

/* ── tabbed container ─────────────────────────────────────────────────── */

const TABS = [
  { id: 'how', label: 'How it works', Panel: LoopPanel },
  { id: 'levels', label: 'Retrieval levels', Panel: LevelsPanel },
  { id: 'bench', label: 'Benchmarks', Panel: BenchmarksPanel },
  { id: 'brainbench', label: 'BrainBench', Panel: BrainBenchPanel },
] as const;

export function ExploreSection() {
  const [active, setActive] = useState<(typeof TABS)[number]['id']>('how');
  const ActivePanel = TABS.find((t) => t.id === active)!.Panel;

  return (
    <section className="w-full border-t border-fd-border">
      <div className="mx-auto max-w-6xl px-4 py-20">
        <div className="mx-auto mb-8 max-w-2xl text-center">
          <div className="mb-3 inline-flex items-center gap-1.5 rounded-full border border-fd-border px-3 py-1 text-xs font-medium text-fd-muted-foreground">
            <Search className="size-3.5 text-fd-primary" /> Under the hood
          </div>
          <h2 className="font-mono text-2xl font-semibold tracking-tight sm:text-3xl">
            The parts worth a closer look
          </h2>
        </div>

        {/* tab bar */}
        <div className="mb-10 flex justify-center">
          <div
            role="tablist"
            aria-label="Explore Kimetsu"
            className="inline-flex flex-wrap justify-center gap-1 rounded-xl border border-fd-border bg-fd-background p-1"
          >
            {TABS.map((t) => (
              <button
                key={t.id}
                role="tab"
                type="button"
                aria-selected={active === t.id}
                aria-controls={`panel-${t.id}`}
                id={`tab-${t.id}`}
                onClick={() => setActive(t.id)}
                className={`rounded-lg px-4 py-2 font-mono text-sm font-medium transition-colors ${
                  active === t.id
                    ? 'bg-fd-primary/10 text-fd-primary'
                    : 'text-fd-muted-foreground hover:text-fd-foreground'
                }`}
              >
                {t.label}
              </button>
            ))}
          </div>
        </div>

        {/* active panel */}
        <div
          key={active}
          role="tabpanel"
          id={`panel-${active}`}
          aria-labelledby={`tab-${active}`}
          className="km-panel-in"
        >
          <ActivePanel />
        </div>
      </div>
    </section>
  );
}
