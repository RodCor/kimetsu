import Link from 'next/link';
import { appName, tagline, links } from '@/lib/shared';

const BASE = '/kimetsu';

export default function HomePage() {
  return (
    <main className="flex flex-1 flex-col items-center justify-center px-4 py-24 text-center">
      {/* eslint-disable-next-line @next/next/no-img-element */}
      <img
        src={`${BASE}/kimetsu-logo.png`}
        alt="Kimetsu logo"
        width={96}
        height={96}
        className="mb-6 rounded-xl"
      />
      <h1 className="mb-3 text-4xl font-bold tracking-tight sm:text-5xl">
        {appName}
      </h1>
      <p className="mb-2 max-w-2xl text-lg text-fd-muted-foreground">
        {tagline}
      </p>
      <p className="mb-8 max-w-2xl text-base text-fd-muted-foreground">
        Give your coding agent a memory that gets sharper every run. A single
        Rust binary that runs next to your agent over MCP, remembers what
        matters, and learns what actually helps.
      </p>
      <div className="flex flex-wrap items-center justify-center gap-3">
        <Link
          href="/docs"
          className="rounded-lg bg-fd-primary px-6 py-2.5 font-medium text-fd-primary-foreground transition-opacity hover:opacity-90"
        >
          Get started
        </Link>
        <a
          href={links.github}
          className="rounded-lg border border-fd-border px-6 py-2.5 font-medium transition-colors hover:bg-fd-accent"
        >
          GitHub
        </a>
      </div>
    </main>
  );
}
