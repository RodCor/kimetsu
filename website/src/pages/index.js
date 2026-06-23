import clsx from 'clsx';
import Link from '@docusaurus/Link';
import useDocusaurusContext from '@docusaurus/useDocusaurusContext';
import Layout from '@theme/Layout';
import Heading from '@theme/Heading';
import styles from './index.module.css';

function Hero() {
  const {siteConfig} = useDocusaurusContext();
  return (
    <header className={styles.hero}>
      <div className={clsx('container', styles.heroInner)}>
        <img src="/kimetsu/img/kimetsu-logo.png" alt="Kimetsu logo" className={styles.heroLogo} />
        <Heading as="h1" className={styles.heroTitle}>
          Proactive memory for <span className={styles.accent}>AI coding agents</span>
        </Heading>
        <p className={styles.heroSubtitle}>
          A local sidecar brain that learns what actually solved problems, surfaces
          it before your agent asks, and gets sharper the more you use it.
        </p>

        <div className={styles.install}>
          <span className={styles.prompt}>$</span> npm install -g kimetsu-ai
        </div>

        <div className={styles.heroButtons}>
          <Link className={clsx('button button--primary button--lg', styles.cta)} to="/docs/intro">
            Get started
          </Link>
          <Link
            className={clsx('button button--lg', styles.ctaGhost)}
            href="https://github.com/RodCor/kimetsu">
            View on GitHub
          </Link>
        </div>
      </div>
    </header>
  );
}

function Metrics() {
  const metrics = [
    {value: '~13×', label: 'cheaper per win', detail: '$0.19 vs $2.47, 16-task bench'},
    {value: '0.949', label: 'recall@4', detail: '100-memory / 210-case dataset'},
    {value: '0.914', label: 'MRR', detail: 'jina-v2 + cross-encoder rerank'},
    {value: '~138ms', label: 'retrieval', detail: 'O(log N) ANN, default reranker'},
  ];
  return (
    <section className={styles.metrics}>
      <div className={clsx('container', styles.metricsGrid)}>
        {metrics.map((m, i) => (
          <div key={i} className={styles.metric}>
            <div className={styles.metricValue}>{m.value}</div>
            <div className={styles.metricLabel}>{m.label}</div>
            <div className={styles.metricDetail}>{m.detail}</div>
          </div>
        ))}
      </div>
    </section>
  );
}

function Features() {
  const features = [
    {
      title: 'Proactive, not reactive',
      body: 'Most memory tools wait to be queried. Kimetsu surfaces relevant context and warns about known failure patterns before the agent repeats a mistake.',
    },
    {
      title: 'Learns what helps',
      body: 'Memories cited before a solution get promoted. Silent passengers and stale advice decay on a half-life curve and get pruned.',
    },
    {
      title: 'Self-tuning',
      body: 'Retrieval weights tune against your own query history. The brain gets sharper the more you work, not just bigger.',
    },
    {
      title: 'Everything local',
      body: 'Retrieval, embeddings, and storage all run on your machine. One SQLite file per project. No external vector DB, no cloud, no telemetry.',
    },
    {
      title: 'Works with your agent',
      body: 'Claude Code, Codex, Pi, OpenClaw, Cursor, Gemini CLI. One command to wire any supported host, or use kimetsu chat standalone.',
    },
    {
      title: 'Configurable',
      body: 'Pick your embedding model, reranker, and the LLM for distillation and ask (OpenAI, Anthropic, or local Ollama). Pluggable retrieval backends.',
    },
  ];
  return (
    <section className={styles.features}>
      <div className="container">
        <div className={styles.featuresGrid}>
          {features.map((f, i) => (
            <div key={i} className={styles.card}>
              <Heading as="h3" className={styles.cardTitle}>{f.title}</Heading>
              <p className={styles.cardBody}>{f.body}</p>
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
      description="Proactive, local memory for AI coding agents. Kimetsu is a sidecar brain that learns which memories help and compounds knowledge across sessions.">
      <Hero />
      <Metrics />
      <Features />
    </Layout>
  );
}
