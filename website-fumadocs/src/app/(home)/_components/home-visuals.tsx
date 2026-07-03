import { ArrowLeftRight, BellRing, BookOpen, GitMerge, PackageOpen, Radio } from 'lucide-react';
import { Reveal } from './reveal';

/* Proactive — the second differentiator beside "free". Standalone section. */

const proactiveBehaviors = [
  {
    icon: BellRing,
    title: 'Session-start digest',
    body: 'Open a new session and the brain has already handed the agent the conventions, gotchas, and decisions that matter here.',
  },
  {
    icon: BookOpen,
    title: 'Episodic resume',
    body: 'It picks up where you left off: what you were doing last time and the state you left it in, without you re-explaining.',
  },
  {
    icon: Radio,
    title: 'Context before the ask',
    body: 'Before a non-trivial task, the relevant lesson is surfaced automatically, so the agent avoids the mistake instead of repeating it.',
  },
];

export function Proactive() {
  return (
    <section className="w-full border-t border-fd-border bg-fd-card/30">
      <div className="mx-auto max-w-6xl px-4 py-20">
        <div className="mx-auto mb-12 max-w-2xl text-center">
          <div className="mb-3 inline-flex items-center gap-1.5 rounded-full border border-fd-border px-3 py-1 text-xs font-medium text-fd-muted-foreground">
            <Radio className="size-3.5 text-fd-primary" /> Free is half the story
          </div>
          <h2 className="mb-4 font-mono text-2xl font-semibold tracking-tight sm:text-3xl">
            Most memory waits to be asked. Kimetsu speaks first.
          </h2>
          <p className="text-fd-muted-foreground">
            A vector store sits idle until you query it. Kimetsu is proactive: it
            reads the moment, decides what the agent needs, and puts it on the
            table before the first turn, so knowledge shows up when it changes the
            outcome, not after.
          </p>
        </div>

        {/* passive vs proactive contrast */}
        <div className="mx-auto mb-12 grid max-w-3xl gap-4 sm:grid-cols-2">
          <div className="rounded-xl border border-fd-border bg-fd-background p-5 opacity-70">
            <span className="font-mono text-xs font-medium uppercase tracking-wider text-fd-muted-foreground">
              Passive memory
            </span>
            <p className="mt-2 font-mono text-sm text-fd-muted-foreground">
              you ask <span className="text-fd-foreground">→</span> it fetches
            </p>
            <p className="mt-3 text-sm text-fd-muted-foreground">
              Nothing happens until a query arrives. If the agent does not know to
              ask, the memory never surfaces.
            </p>
          </div>
          <div className="rounded-xl border border-fd-primary/50 bg-fd-primary/5 p-5">
            <span className="font-mono text-xs font-medium uppercase tracking-wider text-fd-primary">
              Proactive memory
            </span>
            <p className="mt-2 font-mono text-sm">
              it anticipates <span className="text-fd-primary">→</span> then delivers
            </p>
            <p className="mt-3 text-sm text-fd-muted-foreground">
              The brain surfaces the right context on its own, at session start and
              ahead of each task, before the agent can repeat a solved mistake.
            </p>
          </div>
        </div>

        <div className="grid grid-cols-1 gap-4 sm:grid-cols-3">
          {proactiveBehaviors.map((b, i) => (
            <Reveal key={b.title} delay={i * 80}>
              <div className="h-full rounded-xl border border-fd-border bg-fd-background p-6 transition-all duration-300 hover:-translate-y-1 hover:border-fd-primary/40 hover:shadow-lg hover:shadow-fd-primary/5">
                <div className="mb-3 inline-flex rounded-lg border border-fd-border bg-fd-card p-2 text-fd-primary">
                  <b.icon className="size-5" aria-hidden />
                </div>
                <h3 className="mb-1.5 font-semibold">{b.title}</h3>
                <p className="text-sm text-fd-muted-foreground">{b.body}</p>
              </div>
            </Reveal>
          ))}
        </div>
      </div>
    </section>
  );
}

/* Brain sharing — export, merge, swap. A brain is a portable file. */

const sharingModes = [
  {
    icon: PackageOpen,
    title: 'Export',
    body: 'One command turns your brain into a shareable pack: gzip-compressed and security-scrubbed, so credentials and PII never leave your machine.',
  },
  {
    icon: GitMerge,
    title: 'Merge',
    body: 'Import a teammate’s pack additively. It dedups against what you already know, and re-importing is always safe.',
  },
  {
    icon: ArrowLeftRight,
    title: 'Swap',
    body: 'Replace your current memories with a pack’s, reversibly. Old memories are superseded, never deleted, so you can always swap back.',
  },
];

export function BrainSharing() {
  return (
    <section className="w-full border-t border-fd-border bg-fd-card/30">
      <div className="mx-auto max-w-6xl px-4 py-20">
        <div className="mx-auto mb-12 max-w-2xl text-center">
          <div className="mb-3 inline-flex items-center gap-1.5 rounded-full border border-fd-border px-3 py-1 text-xs font-medium text-fd-muted-foreground">
            <PackageOpen className="size-3.5 text-fd-primary" /> Brains are portable
          </div>
          <h2 className="mb-4 font-mono text-2xl font-semibold tracking-tight sm:text-3xl">
            Export, merge, and swap brains freely
          </h2>
          <p className="text-fd-muted-foreground">
            A brain is a file, not a hostage. Pack up what your agent has learned,
            hand it to a teammate, install one from a URL, or swap whole brains in
            and out. Onboard a new machine or a new hire with one import.
          </p>
        </div>

        <div className="mx-auto mb-10 grid max-w-5xl gap-4 sm:grid-cols-3">
          {sharingModes.map((m, i) => (
            <Reveal key={m.title} delay={i * 80}>
              <div className="h-full rounded-xl border border-fd-border bg-fd-background p-6 transition-all duration-300 hover:-translate-y-1 hover:border-fd-primary/40 hover:shadow-lg hover:shadow-fd-primary/5">
                <div className="mb-3 inline-flex rounded-lg border border-fd-border bg-fd-card p-2 text-fd-primary">
                  <m.icon className="size-5" aria-hidden />
                </div>
                <h3 className="mb-1.5 font-semibold">{m.title}</h3>
                <p className="text-sm text-fd-muted-foreground">{m.body}</p>
              </div>
            </Reveal>
          ))}
        </div>

        <Reveal className="mx-auto max-w-3xl">
          <div className="overflow-x-auto rounded-xl border border-fd-border bg-fd-background p-5 font-mono text-sm leading-7">
            <div className="whitespace-nowrap">
              <span className="select-none text-fd-primary">$ </span>
              <span className="text-fd-foreground">kimetsu brain export team.json.gz --name rust-conventions</span>
              <span className="text-fd-muted-foreground">   # scrubbed pack</span>
            </div>
            <div className="whitespace-nowrap">
              <span className="select-none text-fd-primary">$ </span>
              <span className="text-fd-foreground">kimetsu brain import team.json.gz</span>
              <span className="text-fd-muted-foreground">                       # merge + dedup</span>
            </div>
            <div className="whitespace-nowrap">
              <span className="select-none text-fd-primary">$ </span>
              <span className="text-fd-foreground">kimetsu brain import https://example.com/pack.json.gz</span>
              <span className="text-fd-muted-foreground">    # from a URL</span>
            </div>
            <div className="whitespace-nowrap">
              <span className="select-none text-fd-primary">$ </span>
              <span className="text-fd-foreground">kimetsu brain import other.json.gz --mode replace --yes</span>
              <span className="text-fd-muted-foreground">  # swap, reversible</span>
            </div>
          </div>
        </Reveal>
      </div>
    </section>
  );
}
