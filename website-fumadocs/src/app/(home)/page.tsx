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
  { value: '13×', label: 'cheaper per solved task', note: '$0.19 vs $2.47 on a 16-task Terminal-Bench slice' },
  { value: '66.0%', label: 'BEAM 1M memory bench', note: "ahead of mem0's self-reported 62% at the same bucket" },
  { value: '79.5%', label: 'LongMemEval', note: 'the public long-term-memory benchmark' },
  { value: '0.949', label: 'recall@4', note: '0.914 MRR at ~138 ms per retrieval' },
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

const comparison = [
  { name: 'BEAM 1M (matched bucket)', kimetsu: '66.0%', vendor: '62%' },
  { name: 'BEAM 100K', kimetsu: '62.3%', vendor: 'n/a' },
  { name: 'LongMemEval', kimetsu: '79.5%', vendor: 'in the same band' },
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
        <div className="mx-auto grid max-w-6xl gap-8 px-4 py-20 lg:grid-cols-2 lg:items-center">
          <div>
            <div className="mb-3 inline-flex items-center gap-1.5 rounded-full border border-fd-border px-3 py-1 text-xs font-medium text-fd-muted-foreground">
              <Zap className="size-3.5 text-fd-primary" /> Benchmarked, not asserted
            </div>
            <h2 className="mb-4 font-mono text-2xl font-semibold tracking-tight sm:text-3xl">
              The accuracy of the paid clouds. None of the bill.
            </h2>
            <p className="mb-4 text-fd-muted-foreground">
              mem0, Zep, and Letta call a model to store and fetch memories, so
              every question carries an API cost. Kimetsu runs the whole pipeline
              on local compute. On the shared public benchmarks it lands in the
              same band, and at BEAM&apos;s 1M bucket it comes out ahead.
            </p>
            <Link
              href="/docs/memory-benchmark"
              className="inline-flex items-center gap-1.5 text-sm font-medium text-fd-primary hover:underline"
            >
              Read the full methodology <ArrowRight className="size-4" />
            </Link>
          </div>
          <div className="overflow-hidden rounded-xl border border-fd-border bg-fd-background">
            <table className="w-full text-sm">
              <thead>
                <tr className="border-b border-fd-border text-left text-fd-muted-foreground">
                  <th className="p-3 font-medium">Benchmark</th>
                  <th className="p-3 text-right font-medium text-fd-foreground">Kimetsu</th>
                  <th className="p-3 text-right font-medium">mem0</th>
                </tr>
              </thead>
              <tbody>
                {comparison.map((row) => (
                  <tr key={row.name} className="border-b border-fd-border/60 last:border-0">
                    <td className="p-3">{row.name}</td>
                    <td className="p-3 text-right font-semibold tabular-nums text-fd-foreground">{row.kimetsu}</td>
                    <td className="p-3 text-right tabular-nums text-fd-muted-foreground">{row.vendor}</td>
                  </tr>
                ))}
              </tbody>
            </table>
            <p className="border-t border-fd-border p-3 text-xs text-fd-muted-foreground">
              Kimetsu on a 200-question LongMemEval slice and 15 BEAM-1M
              conversations with a local retrieval pipeline. Vendor figures are
              self-reported. See the benchmark page for the exact setup.
            </p>
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
