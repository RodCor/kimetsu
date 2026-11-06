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
//! in memory. It does not import and does not rebuild FTS. With no `--role`,
//! that timer is loopback: both endpoints on this host, dialed at `127.0.0.1`.
//!
//! `--role serve` and `--role fetch` split the same pack across two processes.
//! Each process binds `--bind` (not `127.0.0.1`) and the fetch side dials
//! `--peer`. `scripts/f3-hotpath-lan.sh` runs that pair in two Linux network
//! namespaces joined by a veth pair on one physical host. That is not two LAN
//! machines. The §7 two-host measurement is still pending. Relay-down and
//! relay-up samples are separate `context` runs.

#[cfg(target_os = "linux")]
use std::fs::File;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use iroh::{EndpointAddr, EndpointId};
use kimetsu_brain::project;
use kimetsu_brain::signed_pack::{PackManifest, PackSigner};
use kimetsu_bus::protocol::Announcement;
use kimetsu_bus::{
    BusEndpoint, BusRelay, EndpointConfig, RosterAdmin, RosterEndpoint, SignedRoster,
};
use kimetsu_core::memory::{MemoryKind, MemoryScope, MountLevel};
use kimetsu_core::paths::git_init_boundary;
use kimetsu_sdk::MountGrants;

#[cfg(target_os = "linux")]
const POSIX_FADV_DONTNEED: i32 = 4;

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn posix_fadvise(fd: i32, offset: i64, len: i64, advice: i32) -> i32;
}

#[cfg(target_os = "linux")]
const COLD_NOTE: &str = "posix_fadvise DONTNEED, then the first call";
#[cfg(not(target_os = "linux"))]
const COLD_NOTE: &str = "first call, page cache not dropped";

#[tokio::main]
async fn main() {
    let args = Args::parse();
    unsafe {
        std::env::set_var("KIMETSU_BRAIN_EMBEDDER", "off");
        std::env::set_var("KIMETSU_USER_BRAIN", "0");
        std::env::set_var("KIMETSU_BRAIN_AMBIENT", "off");
    }
    if args.role != Role::Both {
        run_split(args).await;
        return;
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
cold context: {cold_ms:.3} ms ({cold_note})
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
        cold_note = COLD_NOTE,
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Both,
    Serve,
    Fetch,
}

struct Args {
    corpus: usize,
    samples: usize,
    quic_samples: usize,
    out: Option<PathBuf>,
    role: Role,
    bind: Option<Ipv4Addr>,
    peer: Option<Ipv4Addr>,
    session: Option<PathBuf>,
    topology: Option<String>,
}

impl Args {
    fn parse() -> Self {
        let mut corpus = 2000usize;
        let mut samples = 200usize;
        let mut quic_samples = 30usize;
        let mut out = None;
        let mut role = Role::Both;
        let mut bind = None;
        let mut peer = None;
        let mut session = None;
        let mut topology = None;
        let mut iter = std::env::args().skip(1);
        while let Some(flag) = iter.next() {
            match flag.as_str() {
                "--corpus" => corpus = iter.next().unwrap().parse().unwrap(),
                "--samples" => samples = iter.next().unwrap().parse().unwrap(),
                "--quic-samples" => quic_samples = iter.next().unwrap().parse().unwrap(),
                "--out" => out = Some(PathBuf::from(iter.next().unwrap())),
                "--role" => {
                    role = match iter.next().unwrap().as_str() {
                        "both" => Role::Both,
                        "serve" => Role::Serve,
                        "fetch" => Role::Fetch,
                        other => panic!("--role must be both, serve, or fetch, not {other}"),
                    }
                }
                "--bind" => bind = Some(parse_v4(&iter.next().unwrap(), "--bind")),
                "--peer" => peer = Some(parse_v4(&iter.next().unwrap(), "--peer")),
                "--session" => session = Some(PathBuf::from(iter.next().unwrap())),
                "--topology" => topology = Some(iter.next().unwrap()),
                other => panic!("unknown argument {other}"),
            }
        }
        match role {
            Role::Serve => {
                if bind.is_none() || session.is_none() {
                    panic!("--role serve requires --bind and --session");
                }
                if peer.is_some() {
                    panic!("--role serve does not take --peer");
                }
            }
            Role::Fetch => {
                if bind.is_none() || peer.is_none() || session.is_none() {
                    panic!("--role fetch requires --bind, --peer, and --session");
                }
            }
            Role::Both => {
                if bind.is_some() || peer.is_some() || session.is_some() || topology.is_some() {
                    panic!(
                        "--bind, --peer, --session, and --topology require --role serve or --role fetch"
                    );
                }
            }
        }
        Self {
            corpus,
            samples,
            quic_samples,
            out,
            role,
            bind,
            peer,
            session,
            topology,
        }
    }
}

fn parse_v4(text: &str, flag: &str) -> Ipv4Addr {
    text.parse::<Ipv4Addr>()
        .unwrap_or_else(|_| panic!("{flag} expects an IPv4 address, not {text}"))
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

#[cfg(target_os = "linux")]
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

#[cfg(not(target_os = "linux"))]
fn drop_page_cache(_root: &Path) {}

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
            let server =
                RosterEndpoint::new(server_key.public(), &[MountLevel::Repo], None::<&str>)
                    .unwrap();
            let client =
                RosterEndpoint::new(client_key.public(), &[MountLevel::Repo], None::<&str>)
                    .unwrap();
            let document = admin.sign("f3", &[server, client]).unwrap();
            let roster = SignedRoster::open(&document, &admin.public_key_hex()).unwrap();
            let server = bind_endpoint(
                server_key,
                roster.clone(),
                BusRelay::Custom(map.clone()),
                "on-a",
                None,
            )
            .await;
            let client =
                bind_endpoint(client_key, roster, BusRelay::Custom(map), "on-b", None).await;
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
    bind_v4: Option<SocketAddrV4>,
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
        roster_path: None,
        bind_v4,
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
    let client =
        RosterEndpoint::new(client_key.public(), &[MountLevel::Repo], None::<&str>).unwrap();
    let document = admin.sign("f3-quic", &[server, client]).unwrap();
    let roster = SignedRoster::open(&document, &admin.public_key_hex()).unwrap();
    let server = bind_endpoint(
        server_key,
        roster.clone(),
        BusRelay::Disabled,
        "quic-a",
        None,
    )
    .await;
    let client = bind_endpoint(client_key, roster, BusRelay::Disabled, "quic-b", None).await;
    let (manifest, memories) = small_pack();
    let announcement: Announcement = server
        .publish_memories(&manifest, &["repo".into()], &memories, &signer)
        .unwrap();
    let addr = server.direct_addr().await.unwrap();
    let brain = temp_root("quic-receiver");
    project::init_project_at_root(&brain, true).unwrap();
    let (samples, bytes) = time_fetches(&client, addr, &announcement.digest, samples).await;
    let (_, _, conn) = project::load_project_readonly(&brain).unwrap();
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 0, "verification must not import");
    server.close().await;
    client.close().await;
    QuicReport {
        samples,
        bytes,
        rows: memories.len(),
    }
}

fn small_pack() -> (PackManifest, Vec<kimetsu_brain::packs::MemoryExport>) {
    let memories = (0..8)
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
    (manifest, memories)
}

async fn time_fetches(
    client: &BusEndpoint,
    addr: EndpointAddr,
    digest: &str,
    samples: usize,
) -> (Dist, usize) {
    for _ in 0..3 {
        client.fetch_verified(addr.clone(), digest).await.unwrap();
    }
    let mut timed = Vec::with_capacity(samples);
    let mut bytes = 0usize;
    for _ in 0..samples {
        let started = Instant::now();
        let fetched = client.fetch_verified(addr.clone(), digest).await.unwrap();
        timed.push(started.elapsed());
        bytes = fetched.gzip.len();
    }
    (stats(&timed), bytes)
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Ready {
    admin_public_hex: String,
    roster_json: String,
    client_secret_hex: String,
    server_endpoint_id: String,
    server_ip: String,
    server_port: u16,
    digest: String,
}

async fn run_split(args: Args) {
    match args.role {
        Role::Serve => serve(&args).await,
        Role::Fetch => fetch(&args).await,
        Role::Both => unreachable!("run_split is only for serve and fetch"),
    }
}

fn require_routable(ip: Ipv4Addr, flag: &str) {
    if ip.is_loopback() || ip.is_unspecified() || ip.is_broadcast() {
        panic!("{flag} {ip} must be a specific address other than 127.0.0.1");
    }
}

fn check_topology(label: &str) {
    let lower = label.to_ascii_lowercase();
    if lower.contains("two lan hosts")
        || lower.contains("two physical lan")
        || lower.contains("between two lan")
    {
        panic!("topology label claims two LAN hosts; this process has no second physical host");
    }
}

async fn serve(args: &Args) {
    let bind_ip = args.bind.expect("--bind");
    let session = args.session.as_deref().expect("--session");
    require_routable(bind_ip, "--bind");
    std::fs::create_dir_all(session).unwrap();
    let server_key = iroh::SecretKey::generate();
    let client_key = iroh::SecretKey::generate();
    let signer = PackSigner::generate().unwrap();
    let admin = RosterAdmin::generate().unwrap();
    let server_entry = RosterEndpoint::new(
        server_key.public(),
        &[MountLevel::Repo],
        [signer.public_key_hex()],
    )
    .unwrap();
    let client_entry =
        RosterEndpoint::new(client_key.public(), &[MountLevel::Repo], None::<&str>).unwrap();
    let document = admin
        .sign("f3-veth", &[server_entry, client_entry])
        .unwrap();
    let roster = SignedRoster::open(&document, &admin.public_key_hex()).unwrap();
    let server = bind_endpoint(
        server_key,
        roster,
        BusRelay::Disabled,
        "veth-a",
        Some(SocketAddrV4::new(bind_ip, 0)),
    )
    .await;
    let (manifest, memories) = small_pack();
    let announcement = server
        .publish_memories(&manifest, &["repo".into()], &memories, &signer)
        .unwrap();
    let socket = sole_ipv4(server.direct_addr().await.unwrap());
    assert_eq!(socket.ip(), std::net::IpAddr::V4(bind_ip));
    let ready = Ready {
        admin_public_hex: admin.public_key_hex(),
        roster_json: document,
        client_secret_hex: hex_encode(&client_key.to_bytes()),
        server_endpoint_id: server.id().to_string(),
        server_ip: bind_ip.to_string(),
        server_port: socket.port(),
        digest: announcement.digest,
    };
    let ready_path = session.join("ready.json");
    let tmp = session.join("ready.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&ready).unwrap()).unwrap();
    std::fs::rename(&tmp, &ready_path).unwrap();
    eprintln!("serve ready {socket} endpoint {}", server.id());
    let done = session.join("done");
    let deadline = Instant::now() + Duration::from_secs(180);
    while !done.exists() {
        if Instant::now() > deadline {
            panic!("fetch did not finish within 180s");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    server.close().await;
}

async fn fetch(args: &Args) {
    let bind_ip = args.bind.expect("--bind");
    let peer_ip = args.peer.expect("--peer");
    let session = args.session.as_deref().expect("--session");
    require_routable(bind_ip, "--bind");
    require_routable(peer_ip, "--peer");
    let topology = args
        .topology
        .clone()
        .unwrap_or_else(|| "unlabeled".to_string());
    check_topology(&topology);
    let ready = wait_ready(session).await;
    if ready.server_ip != peer_ip.to_string() {
        panic!(
            "ready file says server {} but --peer is {peer_ip}",
            ready.server_ip
        );
    }
    let server_id = EndpointId::from_str(&ready.server_endpoint_id)
        .unwrap_or_else(|err| panic!("server endpoint id: {err}"));
    let peer_socket = SocketAddr::from((peer_ip, ready.server_port));
    if peer_socket.ip().is_loopback() || peer_socket.ip().is_unspecified() {
        panic!("refusing to time a dial of {peer_socket}");
    }
    let addr = EndpointAddr::new(server_id).with_ip_addr(peer_socket);
    let client_key = iroh::SecretKey::from_bytes(&hex_decode_32(&ready.client_secret_hex));
    let roster = SignedRoster::open(&ready.roster_json, &ready.admin_public_hex).unwrap();
    let client = bind_endpoint(
        client_key,
        roster,
        BusRelay::Disabled,
        "veth-b",
        Some(SocketAddrV4::new(bind_ip, 0)),
    )
    .await;
    let local_socket = sole_ipv4(client.direct_addr().await.unwrap());
    assert_eq!(local_socket.ip(), std::net::IpAddr::V4(bind_ip));
    let brain = temp_root("veth-receiver");
    project::init_project_at_root(&brain, true).unwrap();
    let (samples, bytes) = time_fetches(&client, addr, &ready.digest, args.quic_samples).await;
    let (_, _, conn) = project::load_project_readonly(&brain).unwrap();
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 0, "verification must not import");
    let hardware = hardware();
    let report = format!(
        "\
hardware: {hw}
topology: {topology}
bind: {local_socket}
peer: {peer_socket}
request_pack direct QUIC until verified in memory: {topology}. n={n} p50={p50:.3} ms p99={p99:.3} ms max={max:.3} ms
pack: {bytes} gzip bytes, 8 memories, relay disabled
dialed address is {peer_socket}; local bind is {local_socket}
import and FTS rebuild: not included in the request_pack timer
two physical LAN hosts: still pending. This run is not direct LAN QUIC between two LAN machines.
",
        hw = hardware.summary(),
        n = samples.n,
        p50 = samples.p50_ms,
        p99 = samples.p99_ms,
        max = samples.max_ms,
    );
    print!("{report}");
    if let Some(path) = &args.out {
        std::fs::write(path, &report).unwrap();
        eprintln!("wrote {}", path.display());
    }
    std::fs::write(session.join("done"), b"ok\n").unwrap();
    client.close().await;
}

fn sole_ipv4(addr: EndpointAddr) -> SocketAddr {
    let sockets: Vec<_> = addr.ip_addrs().copied().collect();
    assert_eq!(
        sockets.len(),
        1,
        "expected one dial address, got {sockets:?}"
    );
    sockets[0]
}

async fn wait_ready(session: &Path) -> Ready {
    let path = session.join("ready.json");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Ok(bytes) = std::fs::read(&path)
            && let Ok(ready) = serde_json::from_slice::<Ready>(&bytes)
        {
            return ready;
        }
        if Instant::now() > deadline {
            panic!("timed out waiting for {}", path.display());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0xf) as usize] as char);
    }
    out
}

fn hex_decode_32(text: &str) -> [u8; 32] {
    let text = text.trim();
    assert_eq!(text.len(), 64, "expected 64 hex chars");
    let mut out = [0u8; 32];
    for (index, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16)
            .unwrap_or_else(|_| panic!("bad hex in client secret"));
    }
    out
}
