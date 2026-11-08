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
//! machines.
//!
//! `scripts/f3-hotpath-two-hosts.sh` is the two-physical-host run. It uses
//! `--role client-init`, `--role serve-public`, and `--role fetch-public`.
//! The server keeps its keys in memory. The client keeps its own secret.
//! The file copied between machines is a public handoff. `--self-test` is
//! loopback on one host and always prints the §7 row as still pending.
//! Relay-down and relay-up samples are separate `context` runs.

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

#[path = "policy.rs"]
mod two_host;

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
    match args.role {
        Role::ClientInit => {
            client_init(&args);
            return;
        }
        Role::ServePublic => {
            serve_public(&args).await;
            return;
        }
        Role::FetchPublic => {
            fetch_public(&args).await;
            return;
        }
        Role::WanServe => {
            wan_serve(&args).await;
            return;
        }
        Role::WanClient => {
            wan_client(&args).await;
            return;
        }
        Role::Serve | Role::Fetch => {
            run_split(args).await;
            return;
        }
        Role::Both => {}
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
    ClientInit,
    ServePublic,
    FetchPublic,
    WanServe,
    WanClient,
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
    state: Option<PathBuf>,
    client_public: Option<String>,
    handoff: Option<PathBuf>,
    confirm: bool,
    confirm_wan: bool,
    advertise: Option<SocketAddrV4>,
    stun_diagnostic: Option<SocketAddrV4>,
    self_test: bool,
    hold_secs: Option<u64>,
    port: Option<u16>,
    commit: Option<String>,
    rustc_version: Option<String>,
    fetch_timeout: Option<u64>,
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
        let mut state = None;
        let mut client_public = None;
        let mut handoff = None;
        let mut confirm = false;
        let mut confirm_wan = false;
        let mut advertise = None;
        let mut stun_diagnostic = None;
        let mut self_test = false;
        let mut hold_secs = None;
        let mut port = None;
        let mut commit = None;
        let mut rustc_version = None;
        let mut fetch_timeout = None;
        let mut iter = std::env::args().skip(1);
        while let Some(flag) = iter.next() {
            match flag.as_str() {
                "--help" => {
                    print_usage();
                    std::process::exit(0);
                }
                "--corpus" => corpus = require_arg(&mut iter, "--corpus").parse().unwrap(),
                "--samples" => samples = require_arg(&mut iter, "--samples").parse().unwrap(),
                "--quic-samples" => {
                    quic_samples = require_arg(&mut iter, "--quic-samples").parse().unwrap()
                }
                "--out" => out = Some(PathBuf::from(require_arg(&mut iter, "--out"))),
                "--role" => {
                    role = match require_arg(&mut iter, "--role").as_str() {
                        "both" => Role::Both,
                        "serve" => Role::Serve,
                        "fetch" => Role::Fetch,
                        "client-init" => Role::ClientInit,
                        "serve-public" => Role::ServePublic,
                        "fetch-public" => Role::FetchPublic,
                        "wan-serve" => Role::WanServe,
                        "wan-client" => Role::WanClient,
                        other => panic!(
                            "--role must be both, serve, fetch, client-init, serve-public, fetch-public, wan-serve, or wan-client, not {other}"
                        ),
                    }
                }
                "--bind" => bind = Some(parse_v4(&require_arg(&mut iter, "--bind"), "--bind")),
                "--peer" => peer = Some(parse_v4(&require_arg(&mut iter, "--peer"), "--peer")),
                "--session" => session = Some(PathBuf::from(require_arg(&mut iter, "--session"))),
                "--topology" => topology = Some(require_arg(&mut iter, "--topology")),
                "--state" => state = Some(PathBuf::from(require_arg(&mut iter, "--state"))),
                "--client-public" => {
                    client_public = Some(require_arg(&mut iter, "--client-public"))
                }
                "--handoff" => handoff = Some(PathBuf::from(require_arg(&mut iter, "--handoff"))),
                "--confirm-two-physical-hosts" => confirm = true,
                "--confirm-internet-wan" => confirm_wan = true,
                "--advertise" => {
                    advertise = Some(parse_advertise(&require_arg(&mut iter, "--advertise")))
                }
                "--stun-diagnostic" => {
                    stun_diagnostic = Some(parse_advertise(&require_arg(
                        &mut iter,
                        "--stun-diagnostic",
                    )))
                }
                "--self-test" => self_test = true,
                "--hold-secs" => {
                    hold_secs = Some(require_arg(&mut iter, "--hold-secs").parse().unwrap())
                }
                "--port" => port = Some(require_arg(&mut iter, "--port").parse().unwrap()),
                "--commit" => commit = Some(require_arg(&mut iter, "--commit")),
                "--rustc" => rustc_version = Some(require_arg(&mut iter, "--rustc")),
                "--fetch-timeout-secs" => {
                    fetch_timeout = Some(
                        require_arg(&mut iter, "--fetch-timeout-secs")
                            .parse()
                            .unwrap(),
                    )
                }
                other => panic!("unknown argument {other}"),
            }
        }
        if quic_samples == 0 {
            panic!("--quic-samples must be at least 1");
        }
        if let Some(secs) = hold_secs
            && !(30..=1800).contains(&secs)
        {
            panic!("--hold-secs must be 30..=1800, not {secs}");
        }
        if let Some(secs) = fetch_timeout
            && !(5..=300).contains(&secs)
        {
            panic!("--fetch-timeout-secs must be 5..=300, not {secs}");
        }
        let two_host_flags = confirm
            || confirm_wan
            || advertise.is_some()
            || stun_diagnostic.is_some()
            || self_test
            || state.is_some()
            || client_public.is_some()
            || handoff.is_some()
            || hold_secs.is_some()
            || port.is_some()
            || commit.is_some()
            || rustc_version.is_some()
            || fetch_timeout.is_some();
        match role {
            Role::Serve => {
                if bind.is_none() || session.is_none() {
                    panic!("--role serve requires --bind and --session");
                }
                if peer.is_some() {
                    panic!("--role serve does not take --peer");
                }
                if two_host_flags {
                    panic!(
                        "two-host flags require --role client-init, serve-public, or fetch-public"
                    );
                }
            }
            Role::Fetch => {
                if bind.is_none() || peer.is_none() || session.is_none() {
                    panic!("--role fetch requires --bind, --peer, and --session");
                }
                if two_host_flags {
                    panic!(
                        "two-host flags require --role client-init, serve-public, or fetch-public"
                    );
                }
            }
            Role::Both => {
                if bind.is_some() || peer.is_some() || session.is_some() || topology.is_some() {
                    panic!(
                        "--bind, --peer, --session, and --topology require --role serve or --role fetch"
                    );
                }
                if two_host_flags {
                    panic!(
                        "two-host flags require --role client-init, serve-public, or fetch-public"
                    );
                }
            }
            Role::ClientInit => {
                if state.is_none() {
                    panic!("--role client-init requires --state");
                }
                if bind.is_some()
                    || peer.is_some()
                    || session.is_some()
                    || topology.is_some()
                    || confirm
                    || confirm_wan
                    || advertise.is_some()
                    || stun_diagnostic.is_some()
                    || self_test
                    || client_public.is_some()
                    || handoff.is_some()
                    || hold_secs.is_some()
                    || port.is_some()
                    || fetch_timeout.is_some()
                {
                    panic!(
                        "--role client-init only takes --state (plus optional --commit and --rustc)"
                    );
                }
            }
            Role::ServePublic | Role::FetchPublic => {
                if session.is_some() || topology.is_some() {
                    panic!(
                        "--session and --topology are the veth exchange; two-host runs use --state and a public handoff"
                    );
                }
                if state.is_none() {
                    panic!("--role serve-public and fetch-public require --state");
                }
                if bind.is_none() {
                    panic!("--bind is required");
                }
                let policy = two_host::EndpointPolicy { self_test };
                two_host::require_mode(confirm, self_test).unwrap_or_else(|err| panic!("{err}"));
                two_host::validate_release_profile(confirm, cfg!(debug_assertions))
                    .unwrap_or_else(|err| panic!("{err}"));
                let bind_ip = bind.expect("--bind");
                two_host::validate_ip(bind_ip, policy).unwrap_or_else(|err| panic!("{err}"));
                if matches!(role, Role::ServePublic) {
                    if client_public.is_none() {
                        panic!("--role serve-public requires --client-public");
                    }
                    if peer.is_some() {
                        panic!("--role serve-public does not take --peer");
                    }
                }
                if matches!(role, Role::FetchPublic) {
                    if peer.is_none() || handoff.is_none() {
                        panic!("--role fetch-public requires --peer and --handoff");
                    }
                    if client_public.is_some() {
                        panic!(
                            "--role fetch-public reads the local secret; do not pass --client-public"
                        );
                    }
                    let peer_ip = peer.expect("--peer");
                    two_host::validate_pair(bind_ip, peer_ip, policy)
                        .unwrap_or_else(|err| panic!("{err}"));
                }
                if confirm_wan || advertise.is_some() || stun_diagnostic.is_some() {
                    panic!(
                        "internet/WAN flags require --role wan-serve or --role wan-client; they cannot relabel a LAN run"
                    );
                }
            }
            Role::WanServe | Role::WanClient => {
                if self_test || confirm || session.is_some() || topology.is_some() {
                    panic!(
                        "WAN mode cannot use --self-test, --confirm-two-physical-hosts, --session, or --topology"
                    );
                }
                if !confirm_wan {
                    panic!("WAN mode requires --confirm-internet-wan");
                }
                if state.is_none() || bind.is_none() {
                    panic!("WAN mode requires --state and --bind");
                }
                let bind_ip = bind.expect("--bind");
                two_host::validate_wan_bind(bind_ip).unwrap_or_else(|err| panic!("{err}"));
                let fixed =
                    two_host::require_fixed_udp_port(port).unwrap_or_else(|err| panic!("{err}"));
                let advertised = two_host::select_advertised(advertise, stun_diagnostic)
                    .unwrap_or_else(|err| panic!("{err}"));
                two_host::require_distinct_mapping(
                    bind_ip,
                    fixed,
                    *advertised.ip(),
                    advertised.port(),
                )
                .unwrap_or_else(|err| panic!("{err}"));
                two_host::validate_release_profile(true, cfg!(debug_assertions))
                    .map_err(|_| {
                        "refusing --confirm-internet-wan on a debug build; use the script's release build"
                    })
                    .unwrap_or_else(|err| panic!("{err}"));
                if matches!(role, Role::WanServe) && client_public.is_none() {
                    panic!("--role wan-serve requires --client-public");
                }
                if matches!(role, Role::WanClient) && client_public.is_some() {
                    panic!(
                        "--role wan-client keeps its key in this process; do not pass --client-public"
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
            state,
            client_public,
            handoff,
            confirm,
            confirm_wan,
            advertise,
            stun_diagnostic,
            self_test,
            hold_secs,
            port,
            commit,
            rustc_version,
            fetch_timeout,
        }
    }
}

fn parse_advertise(text: &str) -> SocketAddrV4 {
    two_host::parse_advertised_endpoint(text).unwrap_or_else(|err| panic!("{err}"))
}

fn require_arg(iter: &mut impl Iterator<Item = String>, flag: &str) -> String {
    iter.next()
        .unwrap_or_else(|| panic!("missing value for {flag}"))
}

fn print_usage() {
    println!(
        "\
f3_hotpath measures ADR 0001 request_pack.

Loopback (one host):
  cargo +stable run -p kimetsu-bus --example f3_hotpath --release

Veth pair on one host (not two LAN machines):
  scripts/f3-hotpath-lan.sh

Two physical LAN hosts:
  scripts/f3-hotpath-two-hosts.sh
    client-init --state DIR
    serve --bind IP --client-public HEX --state DIR --confirm-two-physical-hosts
    fetch --bind IP --peer IP --state DIR --handoff FILE --confirm-two-physical-hosts

Internet/WAN (not a LAN measurement):
  scripts/f3-hotpath-two-hosts.sh
    wan-client --bind IP --port N --advertise PUBLIC:PORT --state DIR --confirm-internet-wan
    wan-serve --bind IP --port N --advertise PUBLIC:PORT --client-public HEX --state DIR --confirm-internet-wan

--self-test is loopback on one machine. It does not fill the §7 row.
--stun-diagnostic prints a mapping and does not advertise it.
"
    );
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
                None,
            )
            .await;
            let client = bind_endpoint(
                client_key,
                roster,
                BusRelay::Custom(map),
                "on-b",
                None,
                None,
            )
            .await;
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
    presence_dir: Option<&Path>,
) -> BusEndpoint {
    let path = match presence_dir {
        Some(dir) => dir.join(format!("presence-{label}.json")),
        None => std::env::temp_dir().join(format!(
            "kimetsu-f3-presence-{label}-{}-{}.json",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        )),
    };
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
        None,
    )
    .await;
    let client = bind_endpoint(client_key, roster, BusRelay::Disabled, "quic-b", None, None).await;
    let (manifest, memories) = small_pack();
    let announcement: Announcement = server
        .publish_memories(&manifest, &["repo".into()], &memories, &signer)
        .unwrap();
    let addr = server.direct_addr().await.unwrap();
    let brain = temp_root("quic-receiver");
    project::init_project_at_root(&brain, true).unwrap();
    let (samples, bytes, _, _) = time_fetches(&client, addr, &announcement.digest, samples).await;
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
) -> (Dist, usize, Vec<f64>, Vec<f64>) {
    for _ in 0..3 {
        client.fetch_verified(addr.clone(), digest).await.unwrap();
    }
    let mut timed = Vec::with_capacity(samples);
    let mut bytes = 0usize;
    let mut quic_rtt_ms = Vec::new();
    for _ in 0..samples {
        let started = Instant::now();
        let fetched = client.fetch_verified(addr.clone(), digest).await.unwrap();
        timed.push(started.elapsed());
        bytes = fetched.gzip.len();
        if let Some(rtt) = fetched.quic_path_rtt {
            quic_rtt_ms.push(rtt.as_secs_f64() * 1e3);
        }
    }
    let raw_ms = timed
        .iter()
        .map(|sample| sample.as_secs_f64() * 1e3)
        .collect();
    (stats(&timed), bytes, raw_ms, quic_rtt_ms)
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
        Role::Both
        | Role::ClientInit
        | Role::ServePublic
        | Role::FetchPublic
        | Role::WanServe
        | Role::WanClient => unreachable!("run_split is only for serve and fetch"),
    }
}

fn require_routable(ip: Ipv4Addr, flag: &str) {
    if ip.is_loopback() || ip.is_unspecified() || ip.is_broadcast() {
        panic!("{flag} {ip} must be a specific address other than 127.0.0.1");
    }
}

fn check_topology(label: &str) {
    if let Err(err) = two_host::reject_false_lan_label(label) {
        panic!("{err}");
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
        None,
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
        None,
    )
    .await;
    let local_socket = sole_ipv4(client.direct_addr().await.unwrap());
    assert_eq!(local_socket.ip(), std::net::IpAddr::V4(bind_ip));
    let brain = temp_root("veth-receiver");
    project::init_project_at_root(&brain, true).unwrap();
    let (samples, bytes, _, _) =
        time_fetches(&client, addr, &ready.digest, args.quic_samples).await;
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

fn client_init(args: &Args) {
    let state = args.state.as_deref().expect("--state");
    ensure_state(state);
    let secret_path = state.join("client.secret");
    if secret_path.exists() {
        panic!(
            "client secret already exists at {}; refusing to overwrite",
            secret_path.display()
        );
    }
    let key = iroh::SecretKey::generate();
    let secret_hex = hex_encode(&key.to_bytes());
    write_secret_file(&secret_path, &secret_hex);
    let public = key.public().to_string();
    EndpointId::from_str(&public).unwrap_or_else(|err| panic!("client public key: {err}"));
    if public.contains(&secret_hex) {
        panic!("client public key must not echo the secret");
    }
    flush_stdout(&format!(
        "\
role: client-init
client_public: {public}
state: {state}
secret_file: {secret}
note: this secret stays on this machine. Do not copy it to the server.
",
        state = state.display(),
        secret = secret_path.display(),
    ));
}

async fn serve_public(args: &Args) {
    let state = args.state.as_deref().expect("--state");
    let bind_ip = args.bind.expect("--bind");
    ensure_state(state);
    let client_public = args.client_public.as_deref().expect("--client-public");
    let client_id = EndpointId::from_str(client_public.trim())
        .unwrap_or_else(|err| panic!("--client-public is not an endpoint id ({err})"));
    let server_key = iroh::SecretKey::generate();
    if server_key.public() == client_id {
        panic!("--client-public matches the server key; generate the client on the other machine");
    }
    let signer = PackSigner::generate().unwrap();
    let admin = RosterAdmin::generate().unwrap();
    let server_entry = RosterEndpoint::new(
        server_key.public(),
        &[MountLevel::Repo],
        [signer.public_key_hex()],
    )
    .unwrap();
    let client_entry = RosterEndpoint::new(client_id, &[MountLevel::Repo], None::<&str>).unwrap();
    let document = admin
        .sign("f3-two-host", &[server_entry, client_entry])
        .unwrap();
    let roster = SignedRoster::open(&document, &admin.public_key_hex()).unwrap();
    if !roster.allows(client_id) {
        panic!("roster did not admit the client public key");
    }
    let port = args.port.unwrap_or(0);
    let server = tokio::time::timeout(
        Duration::from_secs(20),
        bind_endpoint(
            server_key,
            roster,
            BusRelay::Disabled,
            "twohost-a",
            Some(SocketAddrV4::new(bind_ip, port)),
            Some(state),
        ),
    )
    .await
    .unwrap_or_else(|_| panic!("bind did not become ready within 20s"));
    let (manifest, memories) = small_pack();
    let announcement = server
        .publish_memories(&manifest, &["repo".into()], &memories, &signer)
        .unwrap();
    let socket = sole_ipv4(server.direct_addr().await.unwrap());
    assert_eq!(
        socket.ip(),
        std::net::IpAddr::V4(bind_ip),
        "bound address {socket} does not match --bind {bind_ip}"
    );
    let facts = host_facts(args);
    let handoff = two_host::PublicHandoff {
        admin_public_hex: admin.public_key_hex(),
        roster_json: document,
        server_endpoint_id: server.id().to_string(),
        server_ip: bind_ip.to_string(),
        server_port: socket.port(),
        digest: announcement.digest,
        server: facts.clone(),
        mode: two_host::HandoffMode::Lan,
        bind_ip: bind_ip.to_string(),
        bind_port: socket.port(),
    };
    let text = two_host::build_handoff(&handoff).unwrap_or_else(|err| panic!("{err}"));
    if text.to_ascii_lowercase().contains("secret") {
        panic!("refusing to write a handoff that contains a secret");
    }
    let handoff_path = args
        .handoff
        .clone()
        .unwrap_or_else(|| state.join("handoff.json"));
    write_atomic(&handoff_path, text.as_bytes());
    let hold = args
        .hold_secs
        .unwrap_or(if args.self_test { 90 } else { 600 });
    flush_stdout(&format!(
        "\
role: serve
ready: yes
handoff: {path}
bind: {socket}
relay: disabled
embeddings: off
timer: verified request_pack, not import
hold_secs: {hold}
server_hostname: {hostname}
server_machine_id: {machine}
server_commit: {commit}
server_rustc: {rustc}
note: handoff has no private key. Copy that file to the client and leave this process running.
",
        path = handoff_path.display(),
        hostname = facts.hostname,
        machine = facts.machine_id,
        commit = facts.commit,
        rustc = facts.rustc,
    ));
    let done = state.join("done");
    let deadline = Instant::now() + Duration::from_secs(hold);
    while !done.exists() {
        if Instant::now() > deadline {
            eprintln!("hold of {hold}s elapsed");
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    server.close().await;
}

async fn fetch_public(args: &Args) {
    let state = args.state.as_deref().expect("--state");
    let bind_ip = args.bind.expect("--bind");
    let peer_ip = args.peer.expect("--peer");
    let handoff_path = args.handoff.as_deref().expect("--handoff");
    ensure_state(state);
    if !handoff_path.is_file() {
        panic!("handoff {} is missing", handoff_path.display());
    }
    let secret_path = state.join("client.secret");
    if !secret_path.is_file() {
        panic!(
            "client secret {} is missing; run client-init on this machine",
            secret_path.display()
        );
    }
    refuse_loose_secret(&secret_path);
    let secret_hex = std::fs::read_to_string(&secret_path).unwrap_or_else(|err| {
        panic!("read {}: {err}", secret_path.display());
    });
    let handoff_text = std::fs::read_to_string(handoff_path)
        .unwrap_or_else(|err| panic!("read {}: {err}", handoff_path.display()));
    let handoff =
        two_host::parse_public_handoff(&handoff_text).unwrap_or_else(|err| panic!("{err}"));
    two_host::require_handoff_mode(two_host::HandoffMode::Lan, handoff.mode)
        .unwrap_or_else(|err| panic!("{err}"));
    if handoff.server_ip != peer_ip.to_string() {
        panic!(
            "handoff server address {} does not match --peer {peer_ip}",
            handoff.server_ip
        );
    }
    let kind =
        two_host::require_mode(args.confirm, args.self_test).unwrap_or_else(|err| panic!("{err}"));
    let client_facts = host_facts(args);
    two_host::validate_two_host_claim(
        args.confirm,
        args.self_test,
        &handoff.server.machine_id,
        &client_facts.machine_id,
    )
    .unwrap_or_else(|err| panic!("{err}"));
    if args.confirm
        && (handoff.server.hostname.trim().is_empty() || client_facts.hostname.trim().is_empty())
    {
        panic!("refusing --confirm-two-physical-hosts: hostname is missing on one side");
    }
    let server_id = EndpointId::from_str(&handoff.server_endpoint_id)
        .unwrap_or_else(|err| panic!("server endpoint id: {err}"));
    let peer_socket = SocketAddr::from((peer_ip, handoff.server_port));
    let policy = two_host::EndpointPolicy {
        self_test: args.self_test,
    };
    two_host::validate_ip(peer_ip, policy).unwrap_or_else(|err| panic!("{err}"));
    if !args.self_test && (peer_socket.ip().is_loopback() || peer_socket.ip().is_unspecified()) {
        panic!("refusing to time a dial of {peer_socket}");
    }
    let client_key = iroh::SecretKey::from_bytes(&hex_decode_32(&secret_hex));
    let roster = SignedRoster::open(&handoff.roster_json, &handoff.admin_public_hex)
        .unwrap_or_else(|err| panic!("roster: {err}"));
    if !roster.allows(client_key.public()) {
        panic!(
            "local client key is not on the roster; --client-public on the server does not match this machine"
        );
    }
    if !roster.allows(server_id) {
        panic!("handoff server endpoint is not on the roster");
    }
    let port = args.port.unwrap_or(0);
    let client = tokio::time::timeout(
        Duration::from_secs(20),
        bind_endpoint(
            client_key,
            roster,
            BusRelay::Disabled,
            "twohost-b",
            Some(SocketAddrV4::new(bind_ip, port)),
            Some(state),
        ),
    )
    .await
    .unwrap_or_else(|_| panic!("bind did not become ready within 20s"));
    let local_socket = sole_ipv4(client.direct_addr().await.unwrap());
    assert_eq!(
        local_socket.ip(),
        std::net::IpAddr::V4(bind_ip),
        "bound address {local_socket} does not match --bind {bind_ip}"
    );
    let addr = EndpointAddr::new(server_id).with_ip_addr(peer_socket);
    let brain = RmDir(temp_root("twohost-receiver"));
    project::init_project_at_root(&brain.0, true).unwrap();
    let budget = Duration::from_secs(args.fetch_timeout.unwrap_or(120));
    let timed = tokio::time::timeout(
        budget,
        time_fetches(&client, addr, &handoff.digest, args.quic_samples),
    )
    .await
    .unwrap_or_else(|_| panic!("fetch did not finish within {}s", budget.as_secs()));
    let (samples, bytes, _raw_ms, _quic_rtt_ms) = timed;
    let (_, _, conn) = project::load_project_readonly(&brain.0).unwrap();
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 0, "verification must not import");
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let report = two_host::render_report(&two_host::ReportInput {
        kind,
        n: samples.n,
        p50_ms: samples.p50_ms,
        p99_ms: samples.p99_ms,
        max_ms: samples.max_ms,
        pack_bytes: bytes,
        memories: 8,
        bind: local_socket.to_string(),
        peer: peer_socket.to_string(),
        server: handoff.server,
        client: client_facts,
        build_profile: profile.into(),
        raw_ms: Vec::new(),
        advertised: String::new(),
        client_advertised: String::new(),
        quic_transport_rtt_ms: None,
    })
    .unwrap_or_else(|err| panic!("{err}"));
    flush_stdout(&report);
    if let Some(path) = &args.out {
        std::fs::write(path, &report).unwrap();
        eprintln!("wrote {}", path.display());
    }
    client.close().await;
}

async fn wan_serve(args: &Args) {
    if !args.confirm_wan {
        panic!("WAN mode requires --confirm-internet-wan");
    }
    let state = args.state.as_deref().expect("--state");
    let bind_ip = args.bind.expect("--bind");
    let bind_port =
        two_host::require_fixed_udp_port(args.port).unwrap_or_else(|err| panic!("{err}"));
    let advertised = args.advertise.expect("--advertise");
    ensure_state(state);
    stun_diagnostic_line(args, SocketAddrV4::new(bind_ip, bind_port)).await;
    let client_public = args.client_public.as_deref().expect("--client-public");
    let client_id = EndpointId::from_str(client_public.trim())
        .unwrap_or_else(|err| panic!("--client-public is not an endpoint id ({err})"));
    let server_key = iroh::SecretKey::generate();
    if server_key.public() == client_id {
        panic!("--client-public matches the server key; generate the client on the other machine");
    }
    let signer = PackSigner::generate().unwrap();
    let admin = RosterAdmin::generate().unwrap();
    let server_entry = RosterEndpoint::new(
        server_key.public(),
        &[MountLevel::Repo],
        [signer.public_key_hex()],
    )
    .unwrap();
    let client_entry = RosterEndpoint::new(client_id, &[MountLevel::Repo], None::<&str>).unwrap();
    let document = admin.sign("f3-wan", &[server_entry, client_entry]).unwrap();
    let roster = SignedRoster::open(&document, &admin.public_key_hex()).unwrap();
    if !roster.allows(client_id) || !roster.allows(server_key.public()) {
        panic!("roster did not admit the expected endpoint before any pack work");
    }
    let server = tokio::time::timeout(
        Duration::from_secs(20),
        bind_endpoint(
            server_key,
            roster,
            BusRelay::Disabled,
            "wan-a",
            Some(SocketAddrV4::new(bind_ip, bind_port)),
            Some(state),
        ),
    )
    .await
    .unwrap_or_else(|_| panic!("bind did not become ready within 20s"));
    let socket = sole_ipv4(server.direct_addr().await.unwrap());
    assert_eq!(socket.ip(), std::net::IpAddr::V4(bind_ip));
    assert_eq!(
        socket.port(),
        bind_port,
        "bound port is not the explicit UDP port"
    );
    let (manifest, memories) = small_pack();
    let announcement = server
        .publish_memories(&manifest, &["repo".into()], &memories, &signer)
        .unwrap();
    let facts = host_facts(args);
    let handoff = two_host::PublicHandoff {
        admin_public_hex: admin.public_key_hex(),
        roster_json: document,
        server_endpoint_id: server.id().to_string(),
        server_ip: advertised.ip().to_string(),
        server_port: advertised.port(),
        digest: announcement.digest,
        server: facts.clone(),
        mode: two_host::HandoffMode::InternetWan,
        bind_ip: bind_ip.to_string(),
        bind_port,
    };
    let text = two_host::build_handoff(&handoff).unwrap_or_else(|err| panic!("{err}"));
    refuse_secret_material(&text);
    let handoff_path = state.join("handoff.json");
    write_atomic(&handoff_path, text.as_bytes());
    let hold = args.hold_secs.unwrap_or(600);
    flush_stdout(&format!(
        "\
role: wan-serve
ready: yes
mode: internet-wan
handoff: {path}
bind: {socket}
advertised: {advertised}
relay: disabled
embeddings: off
timer: verified request_pack, not import
stun: not advertised
hold_secs: {hold}
server_hostname: {hostname}
server_machine_id: {machine}
server_commit: {commit}
server_rustc: {rustc}
note: handoff has no private key. Copy that file into the client state directory and leave this process running.
",
        path = handoff_path.display(),
        hostname = facts.hostname,
        machine = facts.machine_id,
        commit = facts.commit,
        rustc = facts.rustc,
    ));
    let done = state.join("done");
    let deadline = Instant::now() + Duration::from_secs(hold);
    while !done.exists() {
        if Instant::now() > deadline {
            eprintln!("hold of {hold}s elapsed");
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    server.close().await;
    let _ = std::fs::remove_file(state.join("presence-wan-a.json"));
}

async fn wan_client(args: &Args) {
    if !args.confirm_wan {
        panic!("WAN mode requires --confirm-internet-wan");
    }
    let state = args.state.as_deref().expect("--state");
    let bind_ip = args.bind.expect("--bind");
    let bind_port =
        two_host::require_fixed_udp_port(args.port).unwrap_or_else(|err| panic!("{err}"));
    let client_advertised = args.advertise.expect("--advertise");
    ensure_state(state);
    if state.join("client.secret").exists() {
        panic!(
            "WAN client refuses an existing client.secret; this mode does not store a private key"
        );
    }
    stun_diagnostic_line(args, SocketAddrV4::new(bind_ip, bind_port)).await;
    let client_key = iroh::SecretKey::generate();
    let public = client_key.public().to_string();
    flush_stdout(&format!(
        "\
role: wan-client
ready: waiting-for-handoff
client_public: {public}
bind: {bind_ip}:{bind_port}
client_advertised: {client_advertised}
note: this process holds the client key in memory. Copy only client_public to the server. Copy the server handoff.json into this state directory. Do not write the key down.
"
    ));
    let handoff_path = state.join("handoff.json");
    let hold = args.hold_secs.unwrap_or(600);
    let deadline = Instant::now() + Duration::from_secs(hold);
    loop {
        if handoff_path.is_file() {
            break;
        }
        if Instant::now() > deadline {
            panic!(
                "timed out waiting for {} after {hold}s",
                handoff_path.display()
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let handoff_text = std::fs::read_to_string(&handoff_path)
        .unwrap_or_else(|err| panic!("read {}: {err}", handoff_path.display()));
    refuse_secret_material(&handoff_text);
    let handoff =
        two_host::parse_public_handoff(&handoff_text).unwrap_or_else(|err| panic!("{err}"));
    two_host::require_handoff_mode(two_host::HandoffMode::InternetWan, handoff.mode)
        .unwrap_or_else(|err| panic!("{err}"));
    let server_id = EndpointId::from_str(&handoff.server_endpoint_id)
        .unwrap_or_else(|err| panic!("server endpoint id: {err}"));
    let roster = SignedRoster::open(&handoff.roster_json, &handoff.admin_public_hex)
        .unwrap_or_else(|err| panic!("roster: {err}"));
    if !roster.allows(client_key.public()) {
        panic!(
            "local client key is not on the roster; unauthorized peer rejected before any pack work"
        );
    }
    if !roster.allows(server_id) {
        panic!("handoff server endpoint is not on the roster; rejected before any pack work");
    }
    let peer_ip: Ipv4Addr = handoff.server_ip.parse().unwrap_or_else(|_| {
        panic!(
            "handoff advertised address {} is not IPv4",
            handoff.server_ip
        )
    });
    let peer_socket = SocketAddr::from((peer_ip, handoff.server_port));
    let client = tokio::time::timeout(
        Duration::from_secs(20),
        bind_endpoint(
            client_key,
            roster,
            BusRelay::Disabled,
            "wan-b",
            Some(SocketAddrV4::new(bind_ip, bind_port)),
            Some(state),
        ),
    )
    .await
    .unwrap_or_else(|_| panic!("bind did not become ready within 20s"));
    let local_socket = sole_ipv4(client.direct_addr().await.unwrap());
    assert_eq!(local_socket.ip(), std::net::IpAddr::V4(bind_ip));
    assert_eq!(
        local_socket.port(),
        bind_port,
        "bound port is not the explicit UDP port"
    );
    let addr = EndpointAddr::new(server_id).with_ip_addr(peer_socket);
    let brain = RmDir(temp_root("wan-receiver"));
    project::init_project_at_root(&brain.0, true).unwrap();
    let budget = Duration::from_secs(args.fetch_timeout.unwrap_or(120));
    let (samples, bytes, raw_ms, quic_rtt_ms) = tokio::time::timeout(
        budget,
        time_fetches(&client, addr, &handoff.digest, args.quic_samples),
    )
    .await
    .unwrap_or_else(|_| panic!("fetch did not finish within {}s", budget.as_secs()));
    let (_, _, conn) = project::load_project_readonly(&brain.0).unwrap();
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 0, "verification must not import");
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let report = two_host::render_report(&two_host::ReportInput {
        kind: two_host::ReportKind::InternetWan,
        n: samples.n,
        p50_ms: samples.p50_ms,
        p99_ms: samples.p99_ms,
        max_ms: samples.max_ms,
        pack_bytes: bytes,
        memories: 8,
        bind: local_socket.to_string(),
        peer: peer_socket.to_string(),
        server: handoff.server,
        client: host_facts(args),
        build_profile: profile.into(),
        raw_ms,
        advertised: format!("{}:{}", handoff.server_ip, handoff.server_port),
        client_advertised: client_advertised.to_string(),
        quic_transport_rtt_ms: Some(quic_rtt_ms),
    })
    .unwrap_or_else(|err| panic!("{err}"));
    refuse_secret_material(&report);
    flush_stdout(&report);
    if let Some(path) = &args.out {
        std::fs::write(path, &report).unwrap();
        eprintln!("wrote {}", path.display());
    }
    std::fs::write(state.join("done"), b"ok\n").unwrap();
    client.close().await;
    let _ = std::fs::remove_file(state.join("presence-wan-b.json"));
}

async fn stun_diagnostic_line(args: &Args, local: SocketAddrV4) {
    let Some(server) = args.stun_diagnostic else {
        return;
    };
    match query_stun(local, server).await {
        Ok(mapped) => eprintln!(
            "stun_diagnostic: mapped {mapped} from local {local}. This is not a route and was not written to the handoff. A mapping often changes after this socket closes; --advertise stays {advertise}.",
            advertise = args.advertise.expect("--advertise")
        ),
        Err(err) => eprintln!(
            "stun_diagnostic failed: {err}. --advertise is unchanged and STUN was not used as a route."
        ),
    }
}

async fn query_stun(local: SocketAddrV4, server: SocketAddrV4) -> Result<SocketAddrV4, String> {
    let socket = tokio::net::UdpSocket::bind(local)
        .await
        .map_err(|err| format!("STUN diagnostic bind {local}: {err}"))?;
    let mut transaction_id = [0u8; 12];
    getrandom::fill(&mut transaction_id).map_err(|err| format!("STUN transaction id: {err}"))?;
    let request = two_host::stun_binding_request(&transaction_id);
    socket
        .send_to(&request, server)
        .await
        .map_err(|err| format!("STUN send: {err}"))?;
    let mut buffer = [0u8; 512];
    let received = tokio::time::timeout(Duration::from_secs(3), socket.recv(&mut buffer))
        .await
        .map_err(|_| "STUN diagnostic timed out after 3s".to_string())?
        .map_err(|err| format!("STUN recv: {err}"))?;
    two_host::parse_stun_xor_mapped(&buffer[..received], &transaction_id)
}

fn refuse_secret_material(text: &str) {
    let lower = text.to_ascii_lowercase();
    if lower.contains("client_secret")
        || lower.contains("private_key")
        || lower.contains("secret_key")
    {
        panic!("refusing to emit private-key material");
    }
}

fn host_facts(args: &Args) -> two_host::HostFacts {
    let hardware = hardware();
    two_host::HostFacts {
        hostname: read_hostname(),
        machine_id: read_machine_id(),
        uname: hardware.uname.trim().to_string(),
        cpu: hardware.cpu.trim().to_string(),
        cores: hardware.cores.to_string(),
        mem_kib: hardware.mem_kib.to_string(),
        commit: args
            .commit
            .clone()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "unknown".into()),
        rustc: args
            .rustc_version
            .clone()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "unknown".into()),
        os: read_os(),
    }
}

fn read_hostname() -> String {
    if let Ok(text) = std::fs::read_to_string("/etc/hostname") {
        let name = text.trim();
        if !name.is_empty() {
            return name.to_string();
        }
    }
    std::process::Command::new("hostname")
        .output()
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_default()
}

fn read_machine_id() -> String {
    std::fs::read_to_string("/etc/machine-id")
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn read_os() -> String {
    if let Ok(text) = std::fs::read_to_string("/etc/os-release") {
        for line in text.lines() {
            if let Some(value) = line.strip_prefix("PRETTY_NAME=") {
                return value.trim_matches('"').trim().to_string();
            }
        }
    }
    "unknown".into()
}

fn ensure_state(state: &Path) {
    if state.as_os_str().is_empty() || state == Path::new("/") {
        panic!("refusing state directory {}", state.display());
    }
    std::fs::create_dir_all(state).unwrap_or_else(|err| panic!("state dir: {err}"));
    let marker = state.join("kimetsu-f3-two-host-state");
    if marker.exists() {
        let body = std::fs::read_to_string(&marker).unwrap_or_default();
        if body.trim() != "kimetsu-f3-two-host-state" {
            panic!("state marker mismatch at {}", marker.display());
        }
    } else {
        write_atomic(&marker, b"kimetsu-f3-two-host-state\n");
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).unwrap_or_else(|err| panic!("write {}: {err}", tmp.display()));
    std::fs::rename(&tmp, path)
        .unwrap_or_else(|err| panic!("rename {} to {}: {err}", tmp.display(), path.display()));
}

fn write_secret_file(path: &Path, hex_text: &str) {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .unwrap_or_else(|err| panic!("create {}: {err}", path.display()));
    file.write_all(hex_text.as_bytes()).unwrap();
    file.write_all(b"\n").unwrap();
}

#[cfg(unix)]
fn refuse_loose_secret(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)
        .unwrap_or_else(|err| panic!("stat {}: {err}", path.display()))
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        panic!(
            "refusing to read {}: mode {:o} is broader than 0600",
            path.display(),
            mode & 0o777
        );
    }
}

#[cfg(not(unix))]
fn refuse_loose_secret(_path: &Path) {}

fn flush_stdout(text: &str) {
    use std::io::Write;
    let mut stdout = std::io::stdout();
    stdout
        .write_all(text.as_bytes())
        .unwrap_or_else(|err| panic!("stdout: {err}"));
    stdout.flush().unwrap();
}

struct RmDir(PathBuf);

impl Drop for RmDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
