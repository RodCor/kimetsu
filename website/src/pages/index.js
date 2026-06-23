import clsx from 'clsx';
import Link from '@docusaurus/Link';
import useDocusaurusContext from '@docusaurus/useDocusaurusContext';
import Layout from '@theme/Layout';
import Heading from '@theme/Heading';
import styles from './index.module.css';

function HomepageHeader() {
  const {siteConfig} = useDocusaurusContext();
  return (
    <header className={clsx('hero hero--primary', styles.heroBanner)}>
      <div className="container">
        <img
          src="/kimetsu/img/kimetsu-logo.png"
          alt="Kimetsu logo"
          style={{width: '120px', marginBottom: '1rem'}}
        />
        <Heading as="h1" className="hero__title">
          {siteConfig.title}
        </Heading>
        <p className="hero__subtitle">{siteConfig.tagline}</p>
        <p style={{fontSize: '1.1rem', marginTop: '0.5rem', opacity: 0.9}}>
          recall@4 <strong>0.949</strong> &middot; MRR <strong>0.914</strong> &middot; ~13x cheaper per win on Claude Code
        </p>
        <div className="padding-vert--sm">
          <code style={{
            background: 'rgba(0,0,0,0.3)',
            padding: '0.4rem 1rem',
            borderRadius: '4px',
            fontSize: '1rem',
            color: '#fff',
          }}>
            npm install -g kimetsu-ai
          </code>
        </div>
        <div className={styles.buttons} style={{marginTop: '1.5rem', gap: '1rem'}}>
          <Link
            className="button button--secondary button--lg"
            to="/docs/intro">
            Get Started
          </Link>
          <Link
            className="button button--outline button--secondary button--lg"
            href="https://github.com/RodCor/kimetsu">
            GitHub
          </Link>
        </div>
      </div>
    </header>
  );
}

function MetricsRow() {
  const metrics = [
    {value: '~13x', label: 'cheaper per win', detail: '$0.19 vs $2.47 on 16-task bench'},
    {value: '0.949', label: 'recall@4', detail: '100-memory / 210-case dataset'},
    {value: '0.914', label: 'MRR', detail: 'jina-v2-base-code + cross-encoder rerank'},
    {value: '~138 ms', label: 'retrieval latency', detail: 'default reranker, O(log N) ANN'},
  ];
  return (
    <section style={{padding: '2rem 0', background: 'var(--ifm-color-emphasis-100)'}}>
      <div className="container">
        <div className="row">
          {metrics.map((m, i) => (
            <div key={i} className="col col--3" style={{textAlign: 'center', padding: '1rem'}}>
              <div style={{fontSize: '2rem', fontWeight: 700, color: 'var(--ifm-color-primary)'}}>{m.value}</div>
              <div style={{fontWeight: 600}}>{m.label}</div>
              <div style={{fontSize: '0.85rem', opacity: 0.7}}>{m.detail}</div>
            </div>
          ))}
        </div>
      </div>
    </section>
  );
}

function FeatureList() {
  const features = [
    {
      title: 'Never explores twice',
      description: 'A session-start digest and episodic resume mean the agent already knows your repo and where you left off. No re-deriving the basics.',
    },
    {
      title: 'Learns what helps',
      description: 'Memories cited before solving a problem get promoted. Silent passengers and stale advice decay on a half-life curve and get pruned.',
    },
    {
      title: 'Yours, on your machine',
      description: 'One SQLite file per project. No external vector DB, no cloud, no telemetry. Back it up with cp.',
    },
    {
      title: 'Works with your agent',
      description: 'Claude Code, Codex, Pi, OpenClaw, Cursor, Gemini CLI — one command to wire any supported host. Or use kimetsu chat as a standalone assistant.',
    },
    {
      title: 'Semantic retrieval',
      description: 'Finds the right memory even when you used different words. Scales to ~1M memories in ~3 GB RAM with sub-2s retrieval via usearch HNSW.',
    },
    {
      title: 'Self-tuning',
      description: 'kimetsu brain tune adjusts retrieval weights against your own query history. The brain gets sharper the more you use it.',
    },
  ];
  return (
    <section style={{padding: '3rem 0'}}>
      <div className="container">
        <div className="row">
          {features.map((f, i) => (
            <div key={i} className="col col--4" style={{padding: '1rem'}}>
              <Heading as="h3">{f.title}</Heading>
              <p>{f.description}</p>
            </div>
          ))}
        </div>
      </div>
    </section>
  );
}

export default function Home() {
  const {siteConfig} = useDocusaurusContext();
  return (
    <Layout
      title={siteConfig.title}
      description="Proactive memory for AI coding agents. Kimetsu is a sidecar brain that learns which memories help and compounds knowledge across sessions.">
      <HomepageHeader />
      <MetricsRow />
      <FeatureList />
    </Layout>
  );
}
