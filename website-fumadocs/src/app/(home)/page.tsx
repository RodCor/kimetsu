import Link from 'next/link';
import {
  ArrowRight,
  Compass,
  Cpu,
  Database,
  Lock,
  MessageSquare,
  TrendingUp,
} from 'lucide-react';
import { appName, tagline, links } from '@/lib/shared';
import { BrainSharing, Proactive } from './_components/home-visuals';
import { ExploreSection } from './_components/explore-section';
import { Reveal } from './_components/reveal';

const BASE = '';

const stats = [
  { value: '89.4%', label: 'LoCoMo', note: 'the long-conversation memory benchmark, full 1,540-question set' },
  { value: '83.0%', label: 'LongMemEval', note: 'the public long-term-memory benchmark' },
  { value: '73.3%', label: 'BEAM 100K memory bench', note: 'matches the prior public SOTA, with no model in the pipeline' },
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
          className="km-enter mb-6 rounded-2xl"
          style={{ animationDelay: '0ms' }}
        />
        <div
          className="km-enter mb-5 flex flex-wrap items-center justify-center gap-2 text-xs font-medium text-fd-muted-foreground"
          style={{ animationDelay: '80ms' }}
        >
          <span className="rounded-full border border-fd-border px-3 py-1">100% local</span>
          <span className="rounded-full border border-fd-border px-3 py-1">No cloud, no telemetry</span>
          <span className="rounded-full border border-fd-border px-3 py-1">MIT / Apache-2.0</span>
        </div>
        <h1
          className="km-enter mb-4 max-w-3xl font-mono text-4xl font-bold tracking-tight text-fd-foreground sm:text-6xl"
          style={{ animationDelay: '160ms' }}
        >
          Memory for your coding agent that gets sharper every run
        </h1>
        <p
          className="km-enter mb-8 max-w-2xl text-lg text-fd-muted-foreground"
          style={{ animationDelay: '240ms' }}
        >
          {appName} is a single Rust binary that runs next to your agent over MCP.
          It remembers what matters, learns which memories actually helped, and
          lets that knowledge compound across sessions. {tagline}.
        </p>
        <div
          className="km-enter mb-6 flex flex-wrap items-center justify-center gap-3"
          style={{ animationDelay: '320ms' }}
        >
          <Link
            href="/docs"
            className="group inline-flex items-center gap-1.5 rounded-lg bg-fd-primary px-6 py-2.5 font-medium text-fd-primary-foreground transition-opacity hover:opacity-90"
          >
            Get started <ArrowRight className="size-4 transition-transform group-hover:translate-x-0.5" />
          </Link>
          <a
            href={links.github}
            className="inline-flex items-center gap-1.5 rounded-lg border border-fd-border px-6 py-2.5 font-medium transition-colors hover:bg-fd-accent"
          >
            GitHub
          </a>
        </div>
        <code
          className="km-enter rounded-lg border border-fd-border bg-fd-card px-4 py-2 font-mono text-sm text-fd-muted-foreground"
          style={{ animationDelay: '400ms' }}
        >
          <span className="select-none text-fd-primary">$ </span>
          npm install -g kimetsu-ai
        </code>
      </section>

      {/* Metrics */}
      <section className="w-full border-t border-fd-border bg-fd-card/30">
        <Reveal>
          <div className="mx-auto grid max-w-6xl grid-cols-2 gap-px overflow-hidden md:grid-cols-3">
            {stats.map((s) => (
              <div
                key={s.label}
                className="group flex flex-col gap-1 border border-fd-border/60 bg-fd-background p-6 transition-colors hover:bg-fd-card/40"
              >
                <span className="font-mono text-3xl font-bold tabular-nums tracking-tight text-fd-foreground transition-colors group-hover:text-fd-primary">
                  {s.value}
                </span>
                <span className="text-sm font-medium text-fd-foreground">{s.label}</span>
                <span className="text-xs text-fd-muted-foreground">{s.note}</span>
              </div>
            ))}
          </div>
        </Reveal>
      </section>

      {/* Explore: how it works / retrieval levels / benchmarks (tabbed) */}
      <ExploreSection />

      {/* Proactive differentiator */}
      <Proactive />

      {/* Features */}
      <section className="w-full border-t border-fd-border">
        <div className="mx-auto max-w-6xl px-4 py-20">
          <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-3">
            {features.map((f, i) => (
              <Reveal key={f.title} delay={(i % 3) * 80}>
                <div className="h-full rounded-xl border border-fd-border bg-fd-card p-6 transition-all duration-300 hover:-translate-y-1 hover:border-fd-primary/40 hover:shadow-lg hover:shadow-fd-primary/5">
                  <div className="mb-3 inline-flex rounded-lg border border-fd-border bg-fd-background p-2 text-fd-primary">
                    <f.icon className="size-5" aria-hidden />
                  </div>
                  <h3 className="mb-1.5 font-semibold">{f.title}</h3>
                  <p className="text-sm text-fd-muted-foreground">{f.body}</p>
                </div>
              </Reveal>
            ))}
          </div>
        </div>
      </section>

      {/* Brain sharing: export / merge / swap */}
      <BrainSharing />

      {/* Final CTA */}
      <section className="w-full border-t border-fd-border">
        <Reveal className="mx-auto max-w-3xl px-4 py-24 text-center">
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
              className="group inline-flex items-center gap-1.5 rounded-lg bg-fd-primary px-6 py-2.5 font-medium text-fd-primary-foreground transition-opacity hover:opacity-90"
            >
              Read the docs <ArrowRight className="size-4 transition-transform group-hover:translate-x-0.5" />
            </Link>
            <a href={links.crates} className="rounded-lg border border-fd-border px-6 py-2.5 font-medium transition-colors hover:bg-fd-accent">
              crates.io
            </a>
            <a href={links.npm} className="rounded-lg border border-fd-border px-6 py-2.5 font-medium transition-colors hover:bg-fd-accent">
              npm
            </a>
          </div>
        </Reveal>
      </section>
    </main>
  );
}
