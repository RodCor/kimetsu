import Link from 'next/link';
import {
  ArrowRight,
  Compass,
  Cpu,
  Database,
  Lock,
  MessageSquare,
  TrendingUp,
  Zap,
} from 'lucide-react';
import { appName, tagline, links } from '@/lib/shared';

const BASE = '/kimetsu';

const stats = [
  { value: '73.3%', label: 'BEAM 100K memory bench', note: 'matches the prior public SOTA, with no model in the pipeline' },
  { value: '66.0%', label: 'BEAM 1M memory bench', note: "ahead of mem0's self-reported 62% at the same bucket" },
  { value: '79.5%', label: 'LongMemEval', note: 'the public long-term-memory benchmark' },
  { value: '13×', label: 'cheaper per solved task', note: '$0.19 vs $2.47 on a 16-task Terminal-Bench slice' },
  { value: '~1M', label: 'memories in ~3 GB RAM', note: 'sub-2s retrieval, one SQLite file' },
  { value: '$0', label: 'API cost to remember', note: 'the memory pipeline calls no model' },
];

const features = [
  {
    icon: Database,
    title: 'Remembers what matters',
    body: 'Project conventions, failure patterns, the exact command that regenerates your schema. Captured once, retrieved by meaning.',
  },
  {
    icon: TrendingUp,
    title: 'Learns what helps',
    body: 'Memories the model cites before it solves a problem get promoted. Stale advice and silent passengers decay and get pruned.',
  },
  {
    icon: Compass,
    title: 'Never explores twice',
    body: 'A session-start digest and an episodic resume mean the first turn already knows the repo and what you were doing last time.',
  },
  {
    icon: MessageSquare,
    title: 'Answers, not just injects',
    body: 'kimetsu ask composes a grounded, cited answer from memory using a local model. Zero frontier tokens, works offline.',
  },
  {
    icon: Cpu,
    title: 'Model-free retrieval',
    body: 'FTS5, local embeddings, and a local cross-encoder reranker. Nothing in storage or retrieval calls an LLM.',
  },
  {
    icon: Lock,
    title: 'Yours on your machine',
    body: 'One SQLite file per project. No external vector database, no cloud, no telemetry. Back it up with cp.',
  },
];

const compareCols = ['Kimetsu', 'mem0', 'Cognee', 'Zep', 'Letta'];
const compareRows = [
  {
    label: 'Model in the memory pipeline',
    cells: ['None', 'LLM', 'LLM', 'LLM', 'LLM'],
    win: true,
  },
  {
    label: 'Cost to store and recall',
    cells: ['$0', 'Metered', 'Metered', 'Metered', 'Metered'],
    win: true,
  },
  {
    label: 'Runs fully on your machine',
    cells: ['Yes', 'Self-host / cloud', 'Self-host / cloud', 'Cloud', 'Self-host / cloud'],
    win: true,
  },
  {
    label: 'BEAM 1M accuracy',
    cells: ['66.0%', '62%', '—', '—', '—'],
    win: false,
  },
  {
    label: 'BEAM 100K accuracy',
    cells: ['73.3%', '—', '—', '—', '—'],
    win: false,
  },
];

export default function HomePage() {
  return (
    <main className="flex flex-1 flex-col items-center">
      {/* Hero */}
      <section className="flex w-full flex-col items-center px-4 pt-20 pb-16 text-center">
        {/* eslint-disable-next-line @next/next/no-img-element */}
        <img
          src={`${BASE}/kimetsu-logo.png`}
          alt="Kimetsu logo"
          width={80}
          height={80}
          className="mb-6 rounded-2xl"
        />
        <div className="mb-5 flex flex-wrap items-center justify-center gap-2 text-xs font-medium text-fd-muted-foreground">
          <span className="rounded-full border border-fd-border px-3 py-1">100% local</span>
          <span className="rounded-full border border-fd-border px-3 py-1">No cloud, no telemetry</span>
          <span className="rounded-full border border-fd-border px-3 py-1">MIT / Apache-2.0</span>
        </div>
        <h1 className="mb-4 max-w-3xl font-mono text-4xl font-bold tracking-tight text-fd-foreground sm:text-6xl">
          Memory for your coding agent that gets sharper every run
        </h1>
        <p className="mb-8 max-w-2xl text-lg text-fd-muted-foreground">
          {appName} is a single Rust binary that runs next to your agent over MCP.
          It remembers what matters, learns which memories actually helped, and
          lets that knowledge compound across sessions. {tagline}.
        </p>
        <div className="mb-6 flex flex-wrap items-center justify-center gap-3">
          <Link
            href="/docs"
            className="inline-flex items-center gap-1.5 rounded-lg bg-fd-primary px-6 py-2.5 font-medium text-fd-primary-foreground transition-opacity hover:opacity-90"
          >
            Get started <ArrowRight className="size-4" />
          </Link>
          <a
            href={links.github}
            className="inline-flex items-center gap-1.5 rounded-lg border border-fd-border px-6 py-2.5 font-medium transition-colors hover:bg-fd-accent"
          >
            GitHub
          </a>
        </div>
        <code className="rounded-lg border border-fd-border bg-fd-card px-4 py-2 font-mono text-sm text-fd-muted-foreground">
          <span className="select-none text-fd-primary">$ </span>
          npm install -g kimetsu-ai
        </code>
      </section>

      {/* Metrics */}
      <section className="w-full border-t border-fd-border bg-fd-card/30">
        <div className="mx-auto grid max-w-6xl grid-cols-2 gap-px overflow-hidden md:grid-cols-3">
          {stats.map((s) => (
            <div key={s.label} className="flex flex-col gap-1 border border-fd-border/60 bg-fd-background p-6">
              <span className="font-mono text-3xl font-bold tabular-nums tracking-tight text-fd-foreground">
                {s.value}
              </span>
              <span className="text-sm font-medium text-fd-foreground">{s.label}</span>
              <span className="text-xs text-fd-muted-foreground">{s.note}</span>
            </div>
          ))}
        </div>
      </section>

      {/* What it is */}
      <section className="mx-auto w-full max-w-3xl px-4 py-20 text-center">
        <h2 className="mb-4 font-mono text-2xl font-semibold tracking-tight sm:text-3xl">
          Coding agents are brilliant and forgetful
        </h2>
        <p className="text-fd-muted-foreground">
          Every session starts from zero: the same wrong turns, the same
          re-explaining of your conventions, the same expensive exploration you
          already paid for last week. Kimetsu is a sidecar brain that fixes the
          forgetting. It captures the lessons an agent earns, keeps the ones that
          prove useful, and hands them back on the next run.
        </p>
      </section>

      {/* Features */}
      <section className="mx-auto w-full max-w-6xl px-4 pb-20">
        <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-3">
          {features.map((f) => (
            <div key={f.title} className="rounded-xl border border-fd-border bg-fd-card p-6">
              <div className="mb-3 inline-flex rounded-lg border border-fd-border bg-fd-background p-2 text-fd-primary">
                <f.icon className="size-5" aria-hidden />
              </div>
              <h3 className="mb-1.5 font-semibold">{f.title}</h3>
              <p className="text-sm text-fd-muted-foreground">{f.body}</p>
            </div>
          ))}
        </div>
      </section>

      {/* Benchmark comparison */}
      <section className="w-full border-t border-fd-border bg-fd-card/30">
        <div className="mx-auto max-w-5xl px-4 py-20">
          <div className="mx-auto mb-10 max-w-3xl text-center">
            <div className="mb-3 inline-flex items-center gap-1.5 rounded-full border border-fd-border px-3 py-1 text-xs font-medium text-fd-muted-foreground">
              <Zap className="size-3.5 text-fd-primary" /> Benchmarked, not asserted
            </div>
            <h2 className="mb-4 font-mono text-2xl font-semibold tracking-tight sm:text-3xl">
              The accuracy of the paid clouds. None of the bill.
            </h2>
            <p className="text-fd-muted-foreground">
              mem0, Cognee, Zep, and Letta all call a model to build and query
              memory, so every stored fact and every lookup carries token cost.
              Kimetsu runs the whole pipeline on local compute. It hits the prior
              public state of the art at BEAM&apos;s 100K bucket and comes out
              ahead of mem0 at 1M, with no model in the loop.
            </p>
          </div>

          <div className="overflow-x-auto rounded-xl border border-fd-border bg-fd-background">
            <table className="w-full min-w-[640px] text-sm">
              <thead>
                <tr className="border-b border-fd-border text-fd-muted-foreground">
                  <th className="p-3 text-left font-medium">&nbsp;</th>
                  {compareCols.map((c, i) => (
                    <th
                      key={c}
                      className={`p-3 text-right font-medium ${
                        i === 0 ? 'text-fd-primary' : ''
                      }`}
                    >
                      {c}
                    </th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {compareRows.map((row) => (
                  <tr key={row.label} className="border-b border-fd-border/60 last:border-0">
                    <td className="p-3 text-fd-muted-foreground">{row.label}</td>
                    {row.cells.map((cell, i) => (
                      <td
                        key={i}
                        className={`p-3 text-right tabular-nums ${
                          i === 0
                            ? `font-semibold ${row.win ? 'text-fd-primary' : 'text-fd-foreground'}`
                            : 'text-fd-muted-foreground'
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
              Kimetsu&apos;s 73.3% is the full BEAM 100K set (400 probes) and
              matches the prior public state of the art on that bucket, with no
              model in the retrieval path. mem0, Cognee, and Letta are also
              self-hostable, but all four systems call an LLM to store and recall,
              so retrieval carries token cost. Vendor accuracy is self-reported;
              a blank cell means no comparable public BEAM number. Full per-ability
              results and the exact harness are on the benchmark page.
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
      </section>

      {/* Final CTA */}
      <section className="mx-auto w-full max-w-3xl px-4 py-24 text-center">
        <h2 className="mb-4 font-mono text-3xl font-semibold tracking-tight">
          Set it up in two commands
        </h2>
        <div className="mx-auto mb-8 max-w-md rounded-lg border border-fd-border bg-fd-card p-4 text-left font-mono text-sm">
          <div className="text-fd-muted-foreground">
            <span className="select-none text-fd-primary">$ </span>npm install -g kimetsu-ai
          </div>
          <div className="text-fd-muted-foreground">
            <span className="select-none text-fd-primary">$ </span>kimetsu setup --host claude-code
          </div>
        </div>
        <div className="flex flex-wrap items-center justify-center gap-3">
          <Link
            href="/docs"
            className="inline-flex items-center gap-1.5 rounded-lg bg-fd-primary px-6 py-2.5 font-medium text-fd-primary-foreground transition-opacity hover:opacity-90"
          >
            Read the docs <ArrowRight className="size-4" />
          </Link>
          <a href={links.crates} className="rounded-lg border border-fd-border px-6 py-2.5 font-medium transition-colors hover:bg-fd-accent">
            crates.io
          </a>
          <a href={links.npm} className="rounded-lg border border-fd-border px-6 py-2.5 font-medium transition-colors hover:bg-fd-accent">
            npm
          </a>
        </div>
      </section>
    </main>
  );
}
