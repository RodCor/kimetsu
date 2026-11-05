//! ADR 0001 F3 hot-path measurements.
//!
//! Release build, no `embeddings` feature:
//!
//! ```text
//! cargo run -p kimetsu-bus --example f3_hotpath --release
//! ```
//!
//! Prints hardware, corpus size, and cold versus warm next to every figure.
//! The `context` timer is local FTS with embeddings, the reranker, and the
//! network left out. The `request_pack` timer stops when the pack is verified
//! in memory. It does not import and does not rebuild FTS. That timer is
//! loopback: both endpoints on this host, dialed at `127.0.0.1`. ADR §7 asks
//! for direct LAN QUIC. That LAN measurement is still pending. Relay-down and
//! relay-up samples are separate `context` runs.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use kimetsu_brain::project;
use kimetsu_brain::signed_pack::{PackManifest, PackSigner};
use kimetsu_bus::protocol::Announcement;
use kimetsu_bus::{
    BusEndpoint, BusRelay, EndpointConfig, RosterAdmin, RosterEndpoint, SignedRoster,
};
use kimetsu_core::memory::{MemoryKind, MemoryScope, MountLevel};
use kimetsu_core::paths::git_init_boundary;
use kimetsu_sdk::MountGrants;

const POSIX_FADV_DONTNEED: i32 = 4;

unsafe extern "C" {
    fn posix_fadvise(fd: i32, offset: i64, len: i64, advice: i32) -> i32;
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    unsafe {
        std::env::set_var("KIMETSU_BRAIN_EMBEDDER", "off");
        std::env::set_var("KIMETSU_USER_BRAIN", "0");
        std::env::set_var("KIMETSU_BRAIN_AMBIENT", "off");
    }

    let hardware = hardware();
    eprintln!(
        "f3 harness: corpus {} memories, {} context samples, {} quic samples",
        args.corpus, args.samples, args.quic_samples
    );
    eprintln!("{}", hardware.summary());

    let root = temp_root("corpus");
    project::init_project_at_root(&root, true).unwrap();
    let inserted = load_corpus(&root, args.corpus);
    let bytes = std::fs::metadata(root.join(".kimetsu").join("brain.db"))
        .map(|meta| meta.len())
        .unwrap_or(0);

    let grants = {
        let mut grants = MountGrants::default();
        grants.allow_read(MountLevel::Repo);
        grants
    };
    let query = "lanternwick";

    drop_page_cache(&root);
    let cold = time_context(&root, &grants, query);
    let mut warm_off = Vec::with_capacity(args.samples);
    for _ in 0..20 {
        let _ = kimetsu_sdk::context(&root, &grants, query, 800).unwrap();
    }
    for _ in 0..args.samples {
        warm_off.push(time_context(&root, &grants, query));
    }
    let off = stats(&warm_off);

    let (relay_on, presence_note) = measure_with_relay(&root, &grants, query, args.samples).await;

    let quic = measure_quic(args.quic_samples).await;

    let report = format!(
        "\
hardware: {hw}
corpus: {corpus} accepted repo memories, brain.db {bytes} bytes, selective query `{query}` ({hits} warm hits)
embeddings: off (KIMETSU_BRAIN_EMBEDDER=off, crate built without the embeddings feature)
reranker: not called (kimetsu_sdk::context does not rerank)
network on context: none
cold context: {cold_ms:.3} ms (posix_fadvise DONTNEED, then the first call)
warm context, relay down: n={n} p50={p50:.3} ms p99={p99:.3} ms max={max:.3} ms
warm context, relay up: n={n2} p50={p50b:.3} ms p99={p99b:.3} ms max={maxb:.3} ms ({presence_note})
request_pack direct QUIC until verified in memory: loopback, both endpoints on this host, dialed at 127.0.0.1. n={qn} p50={qp50:.3} ms p99={qp99:.3} ms max={qmax:.3} ms
pack: {pack_bytes} gzip bytes, {pack_rows} memories, relay disabled
§7 direct LAN QUIC measurement: still pending. This run does not report a LAN number.
import and FTS rebuild: not included in the request_pack timer
",
        hw = hardware.summary(),
        corpus = inserted,
        hits = hit_count(&root, &grants, query),
        cold_ms = cold.as_secs_f64() * 1e3,
        n = off.n,
        p50 = off.p50_ms,
        p99 = off.p99_ms,
        max = off.max_ms,
        n2 = relay_on.n,
        p50b = relay_on.p50_ms,
        p99b = relay_on.p99_ms,
        maxb = relay_on.max_ms,
        qn = quic.samples.n,
        qp50 = quic.samples.p50_ms,
        qp99 = quic.samples.p99_ms,
        qmax = quic.samples.max_ms,
        pack_bytes = quic.bytes,
        pack_rows = quic.rows,
    );
    print!("{report}");
    if let Some(path) = &args.out {
        std::fs::write(path, &report).unwrap();
        eprintln!("wrote {}", path.display());
    }
}

struct Args {
    corpus: usize,
    samples: usize,
    quic_samples: usize,
    out: Option<PathBuf>,
}

impl Args {
    fn parse() -> Self {
        let mut corpus = 2000usize;
        let mut samples = 200usize;
        let mut quic_samples = 30usize;
        let mut out = None;
        let mut iter = std::env::args().skip(1);
        while let Some(flag) = iter.next() {
            match flag.as_str() {
                "--corpus" => corpus = iter.next().unwrap().parse().unwrap(),
                "--samples" => samples = iter.next().unwrap().parse().unwrap(),
                "--quic-samples" => quic_samples = iter.next().unwrap().parse().unwrap(),
                "--out" => out = Some(PathBuf::from(iter.next().unwrap())),
                other => panic!("unknown argument {other}"),
            }
        }
        Self {
            corpus,
            samples,
            quic_samples,
            out,
        }
    }
}

struct Hardware {
    uname: String,
    cpu: String,
    cores: usize,
    mem_kib: u64,
}

impl Hardware {
    fn summary(&self) -> String {
        format!(
            "{} | {} | {} cores | MemTotal {} KiB",
            self.uname.trim(),
            self.cpu.trim(),
            self.cores,
            self.mem_kib
        )
    }
}

fn hardware() -> Hardware {
    let uname = std::process::Command::new("uname")
        .args(["-srm"])
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_else(|_| "uname unavailable".into());
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let cpu = cpuinfo
        .lines()
        .find(|line| line.starts_with("model name"))
        .and_then(|line| line.split(':').nth(1))
        .unwrap_or("unknown cpu")
        .trim()
        .to_string();
    let cores = cpuinfo
        .lines()
        .filter(|line| line.starts_with("processor"))
        .count();
    let mem = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let mem_kib = mem
        .lines()
        .find(|line| line.starts_with("MemTotal:"))
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    Hardware {
        uname,
        cpu,
        cores,
        mem_kib,
    }
}

fn temp_root(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "kimetsu-f3-{label}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    git_init_boundary(&path);
    path
}

fn load_corpus(root: &Path, n: usize) -> usize {
    let mut inserted = 0usize;
    for i in 0..n {
        let text = if i % 400 == 0 {
            format!("memory {i} lanternwick selective f3 token")
        } else {
            format!("memory {i} ordinary corpus row about sqlite checkpoints")
        };
        project::add_memory(root, MemoryScope::Repo, MemoryKind::Fact, &text).unwrap();
        inserted += 1;
        if inserted.is_multiple_of(500) {
            eprintln!("inserted {inserted}/{n}");
        }
    }
    inserted
}

fn time_context(root: &Path, grants: &MountGrants, query: &str) -> Duration {
    let started = Instant::now();
    kimetsu_sdk::context(root, grants, query, 800).unwrap();
    started.elapsed()
}

fn hit_count(root: &Path, grants: &MountGrants, query: &str) -> usize {
    kimetsu_sdk::context(root, grants, query, 800)
        .unwrap()
        .capsules
        .len()
}

fn drop_page_cache(root: &Path) {
    for name in ["brain.db", "brain.db-wal", "brain.db-shm"] {
        let path = root.join(".kimetsu").join(name);
        if let Ok(file) = File::open(&path) {
            unsafe {
                let _ = posix_fadvise(file.as_raw_fd(), 0, 0, POSIX_FADV_DONTNEED);
            }
        }
    }
}

struct Dist {
    n: usize,
    p50_ms: f64,
    p99_ms: f64,
    max_ms: f64,
}

fn stats(samples: &[Duration]) -> Dist {
    let mut ms: Vec<f64> = samples.iter().map(|d| d.as_secs_f64() * 1e3).collect();
    ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    Dist {
        n: ms.len(),
        p50_ms: percentile(&ms, 0.50),
        p99_ms: percentile(&ms, 0.99),
        max_ms: ms.last().copied().unwrap_or(0.0),
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = ((sorted.len() as f64) * p).ceil() as usize;
    let index = rank.saturating_sub(1).min(sorted.len() - 1);
    sorted[index]
}

async fn measure_with_relay(
    root: &Path,
    grants: &MountGrants,
    query: &str,
    samples: usize,
) -> (Dist, String) {
    let started = Instant::now();
    let relay = match tokio::time::timeout(
        Duration::from_secs(20),
        iroh::test_utils::run_relay_server(),
    )
    .await
    {
        Ok(Ok((map, _url, relay))) => {
            let server_key = iroh::SecretKey::generate();
            let client_key = iroh::SecretKey::generate();
            let admin = RosterAdmin::generate().unwrap();
            let server = RosterEndpoint::new(server_key.public(), &[MountLevel::Repo], None::<&str>).unwrap();
            let client = RosterEndpoint::new(client_key.public(), &[MountLevel::Repo], None::<&str>).unwrap();
            let document = admin.sign("f3", &[server, client]).unwrap();
            let roster = SignedRoster::open(&document, &admin.public_key_hex()).unwrap();
            let server = bind_endpoint(server_key, roster.clone(), BusRelay::Custom(map.clone()), "on-a").await;
            let client = bind_endpoint(client_key, roster, BusRelay::Custom(map), "on-b").await;
            let online = tokio::time::timeout(Duration::from_secs(20), async {
                server.online().await;
                client.online().await;
            })
            .await;
            let note = match online {
                Ok(()) => "in-process relay connected; context does not dial it".to_string(),
                Err(_) => "relay did not report online within 20s; samples still local".to_string(),
            };
            for _ in 0..5 {
                let _ = kimetsu_sdk::context(root, grants, query, 800).unwrap();
            }
            let mut warm = Vec::with_capacity(samples);
            for _ in 0..samples {
                warm.push(time_context(root, grants, query));
            }
            server.close().await;
            client.close().await;
            drop(relay);
            let _ = started;
            return (stats(&warm), note);
        }
        Ok(Err(err)) => format!("relay failed to start: {err}"),
        Err(_) => "relay start timed out".into(),
    };
    let mut warm = Vec::with_capacity(samples);
    for _ in 0..samples {
        warm.push(time_context(root, grants, query));
    }
    (stats(&warm), relay)
}

async fn bind_endpoint(
    secret_key: iroh::SecretKey,
    roster: SignedRoster,
    relay: BusRelay,
    label: &str,
) -> BusEndpoint {
    let path = std::env::temp_dir().join(format!(
        "kimetsu-f3-presence-{label}-{}-{}.json",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    BusEndpoint::bind(EndpointConfig {
        secret_key,
        roster,
        relay,
        mdns: false,
        insecure_relay_tls: true,
        presence_path: path,
    })
    .await
    .unwrap()
}

struct QuicReport {
    samples: Dist,
    bytes: usize,
    rows: usize,
}

async fn measure_quic(samples: usize) -> QuicReport {
    let server_key = iroh::SecretKey::generate();
    let client_key = iroh::SecretKey::generate();
    let signer = PackSigner::generate().unwrap();
    let admin = RosterAdmin::generate().unwrap();
    let server = RosterEndpoint::new(
        server_key.public(),
        &[MountLevel::Repo],
        [signer.public_key_hex()],
    )
    .unwrap();
    let client = RosterEndpoint::new(client_key.public(), &[MountLevel::Repo], None::<&str>).unwrap();
    let document = admin.sign("f3-quic", &[server, client]).unwrap();
    let roster = SignedRoster::open(&document, &admin.public_key_hex()).unwrap();
    let server = bind_endpoint(server_key, roster.clone(), BusRelay::Disabled, "quic-a").await;
    let client = bind_endpoint(client_key, roster, BusRelay::Disabled, "quic-b").await;
    let memories: Vec<_> = (0..8)
        .map(|i| kimetsu_brain::packs::MemoryExport {
            text: format!("onboarding lesson {i}: keep the wal checkpoint local"),
            scope: "repo".into(),
            kind: "fact".into(),
            confidence: 0.9,
            created_at: Some("2026-01-01T00:00:00Z".into()),
        })
        .collect();
    let manifest = PackManifest {
        name: Some("onboarding".into()),
        version: Some("1".into()),
        description: Some("small pack".into()),
        exported_at: Some("2026-01-01T00:00:00Z".into()),
    };
    let announcement: Announcement = server
        .publish_memories(&manifest, &["repo".into()], &memories, &signer)
        .unwrap();
    let addr = server.direct_addr().await.unwrap();
    let brain = temp_root("quic-receiver");
    project::init_project_at_root(&brain, true).unwrap();
    for _ in 0..3 {
        client
            .fetch_verified(addr.clone(), &announcement.digest)
            .await
            .unwrap();
    }
    let mut timed = Vec::with_capacity(samples);
    let mut bytes = 0usize;
    for _ in 0..samples {
        let started = Instant::now();
        let fetched = client
            .fetch_verified(addr.clone(), &announcement.digest)
            .await
            .unwrap();
        timed.push(started.elapsed());
        bytes = fetched.gzip.len();
    }
    let (_, _, conn) = project::load_project_readonly(&brain).unwrap();
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 0, "verification must not import");
    server.close().await;
    client.close().await;
    QuicReport {
        samples: stats(&timed),
        bytes,
        rows: memories.len(),
    }
}
