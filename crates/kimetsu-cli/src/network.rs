//! User-facing session for the memory bus.
//!
//! Invite, join, status, and leave touch only files. [`open_endpoint`] is the
//! only call to [`kimetsu_bus::BusEndpoint::bind`], and only `share` and
//! `request` use it. The relay stays disabled. Port mapping stays off unless
//! the command was passed `--map-ports`.

use std::fs;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use base64::Engine;
use kimetsu_brain::signed_pack::{PackManifest, PackSigner, TrustStore};
use kimetsu_bus::{
    BusEndpoint, BusRelay, EndpointConfig, PackRequest, RosterAdmin, RosterEndpoint, SignedRoster,
};
use kimetsu_core::memory::MountLevel;
use kimetsu_sdk::MountGrants;
use serde_json::{Value, json};

use crate::{
    NetworkCommand, NetworkInviteArgs, NetworkJoinArgs, NetworkRequestArgs, NetworkShareArgs,
    NetworkStateArgs, NetworkStatusArgs,
};

/// How many times this process has bound a bus socket. Membership commands
/// leave it unchanged.
static ENDPOINT_BINDS: AtomicU64 = AtomicU64::new(0);

struct Issued {
    code: String,
    group: String,
}

struct StatusReport {
    joined: bool,
    group: Option<String>,
    role: Option<String>,
    generation: Option<u64>,
    endpoint_id: Option<String>,
}

struct ShareInfo {
    endpoint_id: String,
    dial: String,
    peer: String,
    digest: String,
}

struct RequestInfo {
    digest: String,
    imported: usize,
    deduped: usize,
    quarantined: usize,
    superseded: usize,
}

struct SessionOpts {
    bind: Option<SocketAddrV4>,
    mdns: bool,
    map_ports: bool,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct StateFile {
    kimetsu_network: u32,
    role: String,
    group: String,
    admin_public: String,
    #[serde(default)]
    admin_secret: Option<String>,
    endpoint_secret: String,
    signer_secret: String,
    roster: String,
}

pub(crate) fn run(command: NetworkCommand) -> Result<(), String> {
    match command {
        NetworkCommand::Invite(args) => cmd_invite(args),
        NetworkCommand::Join(args) => cmd_join(args),
        NetworkCommand::Status(args) => cmd_status(args),
        NetworkCommand::Leave(args) => cmd_leave(args),
        NetworkCommand::Share(args) => cmd_share(args),
        NetworkCommand::Request(args) => cmd_request(args),
    }
}

pub(crate) fn mcp_tools() -> Vec<kimetsu_chat::mcp_server::McpToolExtension> {
    vec![
        kimetsu_chat::mcp_server::McpToolExtension {
            name: "kimetsu_network_status",
            writes: false,
            definition: json!({
                "name": "kimetsu_network_status",
                "description": "Whether this machine has joined a Kimetsu peer network. Does not listen, discover peers, or contact a relay. Sharing stays a terminal command: kimetsu network share.",
                "inputSchema": { "type": "object", "properties": {} }
            }),
            call: mcp_status,
        },
        kimetsu_chat::mcp_server::McpToolExtension {
            name: "kimetsu_network_request",
            writes: true,
            definition: json!({
                "name": "kimetsu_network_request",
                "description": "Fetch repo memories a peer is sharing. Verifies the pack signature before any write. Refuses packs that are not scoped to repo. Does nothing and opens no socket when this machine has not joined. Router port mapping stays off unless map_ports is true.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "peer": { "type": "string", "description": "endpoint-id@host:port from kimetsu network share" },
                        "discover": { "type": "boolean", "description": "Look for roster peers on the local network. Default false." },
                        "map_ports": { "type": "boolean", "description": "Ask the router to open a UDP port. Default false." }
                    }
                }
            }),
            call: mcp_request,
        },
    ]
}

fn mcp_status(_workspace: &Path, _arguments: &Value) -> Result<Value, String> {
    let dir = state_dir(None)?;
    Ok(status_json(&status_of(&dir)?))
}

fn mcp_request(workspace: &Path, arguments: &Value) -> Result<Value, String> {
    let peer = arguments
        .get("peer")
        .and_then(Value::as_str)
        .map(str::to_string);
    let discover = arguments
        .get("discover")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let map_ports = arguments
        .get("map_ports")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if peer.is_none() && !discover {
        return Err(
            "pass peer (endpoint-id@host:port) or discover:true. Nothing was contacted.".into(),
        );
    }
    let dir = state_dir(None)?;
    if !state_path(&dir).exists() {
        return Err(
            "not on a network. Run `kimetsu network join <code>`. Nothing was contacted.".into(),
        );
    }
    let info = block_on(request(
        &dir,
        workspace,
        peer.as_deref(),
        &SessionOpts {
            bind: None,
            mdns: discover,
            map_ports,
        },
    ))?;
    Ok(request_json(&info))
}

fn cmd_invite(args: NetworkInviteArgs) -> Result<(), String> {
    let dir = state_dir(args.state_dir.as_deref())?;
    let issued = invite(&dir, &args.name)?;
    if args.json {
        println!(
            "{}",
            json!({
                "code": issued.code,
                "group": issued.group,
                "listening": false,
            })
        );
    } else {
        eprintln!("Anyone with this code can join and exchange repo memories.");
        eprintln!("It does not include your admin key. Kimetsu is not listening.");
        eprintln!("When you want to send memories, run: kimetsu network share");
        println!("{}", issued.code);
    }
    Ok(())
}

fn cmd_join(args: NetworkJoinArgs) -> Result<(), String> {
    let dir = state_dir(args.state_dir.as_deref())?;
    let group = join(&dir, &args.code)?;
    if args.json {
        println!(
            "{}",
            json!({ "joined": true, "group": group, "listening": false })
        );
    } else {
        println!("Joined \"{group}\". Repo memories only. Kimetsu is not listening.");
        println!("Run `kimetsu network share` to send, or `kimetsu network request` to fetch.");
    }
    Ok(())
}

fn cmd_status(args: NetworkStatusArgs) -> Result<(), String> {
    let dir = state_dir(args.state_dir.as_deref())?;
    let report = status_of(&dir)?;
    if args.json {
        println!("{}", status_json(&report));
    } else if report.joined {
        println!(
            "Peer network: joined \"{}\" ({}). Nothing is listening.",
            report.group.as_deref().unwrap_or("network"),
            report.role.as_deref().unwrap_or("member"),
        );
        println!(
            "Run `kimetsu network share` to send repo memories, or `kimetsu network request` to fetch them."
        );
    } else {
        println!("Peer network: not joined. Nothing is listening.");
        println!(
            "Create a code with `kimetsu network invite`, or join one with `kimetsu network join <code>`."
        );
    }
    Ok(())
}

fn cmd_leave(args: NetworkStateArgs) -> Result<(), String> {
    let dir = state_dir(args.state_dir.as_deref())?;
    let removed = leave(&dir)?;
    if removed {
        println!("Left the network. Nothing is listening.");
    } else {
        println!("Not on a network. Nothing is listening.");
    }
    Ok(())
}

fn cmd_share(args: NetworkShareArgs) -> Result<(), String> {
    let dir = state_dir(args.state_dir.as_deref())?;
    let workspace = args.workspace.unwrap_or_else(|| PathBuf::from("."));
    let session = session_opts(args.bind.as_deref(), args.no_mdns, args.map_ports)?;
    block_on(share(
        &dir,
        &workspace,
        &session,
        args.ready_file.as_deref(),
    ))
}

fn cmd_request(args: NetworkRequestArgs) -> Result<(), String> {
    let dir = state_dir(args.state_dir.as_deref())?;
    let workspace = args.workspace.unwrap_or_else(|| PathBuf::from("."));
    let session = session_opts(args.bind.as_deref(), args.no_mdns, args.map_ports)?;
    let info = block_on(request(&dir, &workspace, args.peer.as_deref(), &session))?;
    if args.json {
        println!("{}", request_json(&info));
    } else {
        println!(
            "Verified the signature. Imported {}, already had {}, quarantined {}.",
            info.imported, info.deduped, info.quarantined
        );
        if info.quarantined > 0 {
            println!("Quarantined memories stay out of retrieval until you accept them.");
        }
        println!("Nothing is listening now.");
    }
    Ok(())
}

fn session_opts(bind: Option<&str>, no_mdns: bool, map_ports: bool) -> Result<SessionOpts, String> {
    Ok(SessionOpts {
        bind: bind.map(parse_bind).transpose()?,
        mdns: !no_mdns,
        map_ports,
    })
}

fn parse_bind(raw: &str) -> Result<SocketAddrV4, String> {
    let addr: SocketAddr = raw.parse().map_err(|err| format!("--bind {raw}: {err}"))?;
    match addr {
        SocketAddr::V4(v4) => Ok(v4),
        SocketAddr::V6(_) => Err("--bind is an IPv4 address, for example 127.0.0.1:0".into()),
    }
}

fn invite(dir: &Path, name: &str) -> Result<Issued, String> {
    if state_path(dir).exists() {
        let mut state = load_state(dir)?;
        if state.role != "founder" {
            return Err(
                "only the person who created the network can invite. Nothing was contacted.".into(),
            );
        }
        let admin_secret = state
            .admin_secret
            .clone()
            .ok_or("the admin key is missing from this network")?;
        let admin = RosterAdmin::from_secret_bytes(decode_hex_32(&admin_secret)?);
        let current = SignedRoster::open(&state.roster, &state.admin_public)?;
        let issued = issue_slot(&admin, &current, &admin_secret)?;
        state.roster = issued.roster_document.clone();
        save_state(dir, &state)?;
        trust_signer(dir, &issued.signer_public)?;
        return Ok(Issued {
            code: issued.code,
            group: state.group,
        });
    }
    check_name(name)?;
    let admin_secret = random_secret()?;
    let admin = RosterAdmin::from_secret_bytes(admin_secret);
    let founder_key = kimetsu_bus::SecretKey::generate();
    let founder_signer = PackSigner::generate()?;
    let founder = RosterEndpoint::new(
        founder_key.public(),
        &[MountLevel::Repo],
        [founder_signer.public_key_hex()],
    )?;
    let genesis = SignedRoster::open(
        &admin.sign(name.trim(), &[founder])?,
        &admin.public_key_hex(),
    )?;
    let admin_hex_secret = encode_hex(&admin_secret);
    let issued = issue_slot(&admin, &genesis, &admin_hex_secret)?;
    let state = StateFile {
        kimetsu_network: 1,
        role: "founder".into(),
        group: name.trim().to_string(),
        admin_public: admin.public_key_hex(),
        admin_secret: Some(admin_hex_secret),
        endpoint_secret: encode_hex(&founder_key.to_bytes()),
        signer_secret: founder_signer.secret_key_hex(),
        roster: issued.roster_document,
    };
    save_state(dir, &state)?;
    trust_signer(dir, &founder_signer.public_key_hex())?;
    trust_signer(dir, &issued.signer_public)?;
    Ok(Issued {
        code: issued.code,
        group: state.group,
    })
}

struct Slot {
    code: String,
    roster_document: String,
    signer_public: String,
}

fn issue_slot(
    admin: &RosterAdmin,
    current: &SignedRoster,
    admin_secret_hex: &str,
) -> Result<Slot, String> {
    let guest_key = kimetsu_bus::SecretKey::generate();
    let guest_signer = PackSigner::generate()?;
    let guest = RosterEndpoint::new(
        guest_key.public(),
        &[MountLevel::Repo],
        [guest_signer.public_key_hex()],
    )?;
    let document = admin.invite(current, guest)?;
    let roster = SignedRoster::open(&document, &admin.public_key_hex())?;
    refuse_wider_than_repo(&roster)?;
    let code = encode_code(&json!({
        "v": 1,
        "admin": admin.public_key_hex(),
        "roster": roster.document(),
        "endpoint_secret": encode_hex(&guest_key.to_bytes()),
        "signer_secret": guest_signer.secret_key_hex(),
    }))?;
    if code.contains(admin_secret_hex) {
        return Err("refusing to put the admin key in a join code".into());
    }
    Ok(Slot {
        code,
        roster_document: roster.document().to_string(),
        signer_public: guest_signer.public_key_hex(),
    })
}

fn join(dir: &Path, code: &str) -> Result<String, String> {
    if state_path(dir).exists() {
        return Err("already on a network. Run `kimetsu network leave` first.".into());
    }
    let value = decode_code(code)?;
    if value.get("v").and_then(Value::as_u64) != Some(1) {
        return Err("this join code is from a newer kimetsu".into());
    }
    let admin = value
        .get("admin")
        .and_then(Value::as_str)
        .ok_or("join code is missing the admin key")?
        .to_string();
    let roster_json = value
        .get("roster")
        .and_then(Value::as_str)
        .ok_or("join code is missing the roster")?;
    let endpoint_secret = value
        .get("endpoint_secret")
        .and_then(Value::as_str)
        .ok_or("join code is missing the machine key")?
        .to_string();
    let signer_secret = value
        .get("signer_secret")
        .and_then(Value::as_str)
        .ok_or("join code is missing the signing key")?
        .to_string();
    let roster = SignedRoster::open(roster_json, &admin)?;
    if roster.admin_hex() != admin.to_ascii_lowercase() {
        return Err("join code admin key does not match the roster".into());
    }
    refuse_wider_than_repo(&roster)?;
    let endpoint_key = kimetsu_bus::SecretKey::from_bytes(&decode_hex_32(&endpoint_secret)?);
    if !roster.allows(endpoint_key.public()) {
        return Err("this code is not a member of its own roster".into());
    }
    let signer = PackSigner::from_secret_hex(&signer_secret)?;
    if !roster.may_publish_signer(endpoint_key.public(), &signer.public_key_hex()) {
        return Err("this code's signing key is not on the roster".into());
    }
    let state = StateFile {
        kimetsu_network: 1,
        role: "member".into(),
        group: roster.group().to_string(),
        admin_public: admin,
        admin_secret: None,
        endpoint_secret,
        signer_secret,
        roster: roster.document().to_string(),
    };
    save_state(dir, &state)?;
    for entry in roster.endpoints() {
        if entry.mounts == [MountLevel::Repo] {
            for public in &entry.signers {
                trust_signer(dir, public)?;
            }
        }
    }
    Ok(state.group)
}

fn status_of(dir: &Path) -> Result<StatusReport, String> {
    if !state_path(dir).exists() {
        return Ok(StatusReport {
            joined: false,
            group: None,
            role: None,
            generation: None,
            endpoint_id: None,
        });
    }
    let state = load_state(dir)?;
    let roster = SignedRoster::open(&state.roster, &state.admin_public)?;
    let endpoint_id = kimetsu_bus::SecretKey::from_bytes(&decode_hex_32(&state.endpoint_secret)?)
        .public()
        .to_string();
    Ok(StatusReport {
        joined: true,
        group: Some(state.group),
        role: Some(state.role),
        generation: Some(roster.generation()),
        endpoint_id: Some(endpoint_id),
    })
}

fn leave(dir: &Path) -> Result<bool, String> {
    let existed = state_path(dir).exists();
    for name in ["state.json", "trust.json", "roster.json", "presence.json"] {
        let path = dir.join(name);
        if path.exists() {
            fs::remove_file(&path).map_err(|err| format!("remove {}: {err}", path.display()))?;
        }
    }
    if dir.exists()
        && dir
            .read_dir()
            .map(|mut d| d.next().is_none())
            .unwrap_or(false)
    {
        let _ = fs::remove_dir(dir);
    }
    Ok(existed)
}

async fn share(
    dir: &Path,
    workspace: &Path,
    session: &SessionOpts,
    ready_file: Option<&Path>,
) -> Result<(), String> {
    let online = open_endpoint(dir, session).await?;
    let gzip = export_repo(workspace, &online.signer, &online.state.group)?;
    let announcement = online.endpoint.publish_pack(&gzip, &online.signer)?;
    let dial = dial_socket(&online.endpoint)?;
    let info = ShareInfo {
        endpoint_id: online.endpoint.id().to_string(),
        dial: dial.to_string(),
        peer: format!("{}@{dial}", online.endpoint.id()),
        digest: announcement.digest,
    };
    if let Some(path) = ready_file {
        write_ready(path, &info)?;
    }
    println!("Sharing repo memories.");
    println!("peer: {}", info.peer);
    println!("digest: {}", info.digest);
    println!(
        "Port mapping: {}. Press Ctrl-C to stop. Nothing listens after that.",
        if session.map_ports { "on" } else { "off" }
    );
    let _ = std::io::Write::flush(&mut std::io::stdout());
    wait_for_stop().await;
    let _ = absorb_roster_file(dir, &online.state);
    online.endpoint.close().await;
    Ok(())
}

async fn request(
    dir: &Path,
    workspace: &Path,
    peer: Option<&str>,
    session: &SessionOpts,
) -> Result<RequestInfo, String> {
    let online = open_endpoint(dir, session).await?;
    let targets = dial_targets(&online, peer, session.mdns)?;
    let trust = load_trust(dir)?;
    let mut grants = MountGrants::default();
    grants.allow_write(MountLevel::Repo);
    let mut imported = 0;
    let mut deduped = 0;
    let mut quarantined = 0;
    let mut superseded = 0;
    let mut digest = String::new();
    let mut last_err = String::from("no peer could be reached");
    for target in targets {
        let announcements = match tokio::time::timeout(
            Duration::from_secs(20),
            online.endpoint.exchange_announcements(target.clone()),
        )
        .await
        {
            Ok(Ok(items)) => items,
            Ok(Err(err)) => {
                last_err = err;
                continue;
            }
            Err(_) => {
                last_err = "timed out contacting that peer".into();
                continue;
            }
        };
        let wanted: Vec<_> = announcements
            .into_iter()
            .filter(|item| repo_only(&item.scopes))
            .collect();
        if wanted.is_empty() {
            last_err = "peer is not sharing a repo pack".into();
            continue;
        }
        for item in wanted {
            let summary = kimetsu_sdk::request_pack(
                &online.endpoint,
                PackRequest {
                    peer: target.clone(),
                    digest: item.digest.clone(),
                    destination: MountLevel::Repo,
                    grants: grants.clone(),
                    trust: trust.clone(),
                    brain: workspace.to_path_buf(),
                },
            )
            .await?;
            imported += summary.imported;
            deduped += summary.deduped;
            quarantined += summary.quarantined;
            superseded += summary.superseded;
            digest = item.digest;
        }
        let _ = absorb_roster_file(dir, &online.state);
        online.endpoint.close().await;
        return Ok(RequestInfo {
            digest,
            imported,
            deduped,
            quarantined,
            superseded,
        });
    }
    online.endpoint.close().await;
    Err(last_err)
}

fn repo_only(scopes: &[String]) -> bool {
    !scopes.is_empty() && scopes.iter().all(|scope| scope == "repo")
}

fn dial_targets(
    online: &Online,
    peer: Option<&str>,
    mdns: bool,
) -> Result<Vec<kimetsu_bus::EndpointAddr>, String> {
    if let Some(peer) = peer {
        let (id, socket) = parse_peer(peer)?;
        return Ok(vec![
            kimetsu_bus::EndpointAddr::new(id).with_ip_addr(socket),
        ]);
    }
    if !mdns {
        return Err(
            "pass --peer <endpoint-id>@host:port. Discovery is off, and nothing was contacted before this check."
                .into(),
        );
    }
    let me = online.endpoint.id();
    let roster = SignedRoster::open(&online.state.roster, &online.state.admin_public)?;
    let others: Vec<_> = roster
        .endpoints()
        .iter()
        .map(|entry| entry.endpoint_id)
        .filter(|id| *id != me)
        .map(kimetsu_bus::EndpointAddr::new)
        .collect();
    if others.is_empty() {
        return Err("nobody else is on this network yet".into());
    }
    Ok(others)
}

fn parse_peer(raw: &str) -> Result<(kimetsu_bus::EndpointId, SocketAddr), String> {
    let (id, addr) = raw.trim().rsplit_once('@').ok_or(
        "peer should look like <endpoint-id>@host:port, the line `kimetsu network share` prints",
    )?;
    let id = id.parse().map_err(|err| format!("endpoint id: {err}"))?;
    let addr = addr.parse().map_err(|err| format!("peer address: {err}"))?;
    Ok((id, addr))
}

struct Online {
    endpoint: BusEndpoint,
    state: StateFile,
    signer: PackSigner,
}

async fn open_endpoint(dir: &Path, session: &SessionOpts) -> Result<Online, String> {
    let state = load_state(dir)?;
    absorb_roster_file(dir, &state)?;
    let state = load_state(dir)?;
    let secret = kimetsu_bus::SecretKey::from_bytes(&decode_hex_32(&state.endpoint_secret)?);
    let roster = SignedRoster::open(&state.roster, &state.admin_public)?;
    if !roster.allows(secret.public()) {
        return Err("this machine was removed from the network".into());
    }
    write_secret_file(&dir.join("roster.json"), state.roster.as_bytes())?;
    ENDPOINT_BINDS.fetch_add(1, Ordering::SeqCst);
    let endpoint = BusEndpoint::bind(EndpointConfig {
        secret_key: secret,
        roster,
        relay: BusRelay::Disabled,
        mdns: session.mdns,
        insecure_relay_tls: false,
        presence_path: dir.join("presence.json"),
        roster_path: Some(dir.join("roster.json")),
        bind_v4: session.bind,
        map_ports: session.map_ports,
    })
    .await?;
    let signer = PackSigner::from_secret_hex(&state.signer_secret)?;
    Ok(Online {
        endpoint,
        state,
        signer,
    })
}

fn export_repo(workspace: &Path, signer: &PackSigner, group: &str) -> Result<Vec<u8>, String> {
    let manifest = PackManifest {
        name: Some(group.to_string()),
        version: Some("1".into()),
        description: Some("repo memories shared with the kimetsu network".into()),
        exported_at: None,
    };
    let (gzip, scrub) = kimetsu_sdk::export_signed_pack(
        workspace,
        &MountGrants::repo_read_only(),
        MountLevel::Repo,
        false,
        false,
        &manifest,
        true,
        signer,
    )
    .map_err(|err| err.to_string())?;
    if !scrub.is_clean() {
        return Err(format!(
            "refusing to share: the security scrub found {}. Fix those memories, then try again.",
            scrub.summary()
        ));
    }
    Ok(gzip)
}

fn dial_socket(endpoint: &BusEndpoint) -> Result<SocketAddr, String> {
    let sockets = endpoint.bound_sockets();
    let socket = sockets
        .iter()
        .copied()
        .find(|socket| socket.is_ipv4() && !socket.ip().is_unspecified())
        .or_else(|| sockets.iter().copied().find(|socket| socket.is_ipv4()))
        .ok_or_else(|| "endpoint has no IPv4 socket".to_string())?;
    Ok(match socket {
        SocketAddr::V4(addr) if addr.ip().is_unspecified() => {
            SocketAddr::from((Ipv4Addr::LOCALHOST, addr.port()))
        }
        other => other,
    })
}

fn write_ready(path: &Path, info: &ShareInfo) -> Result<(), String> {
    let body = serde_json::to_vec_pretty(&json!({
        "endpoint_id": info.endpoint_id,
        "dial": info.dial,
        "peer": info.peer,
        "digest": info.digest,
        "listening": true,
    }))
    .map_err(|err| err.to_string())?;
    write_secret_file(path, &body)?;
    Ok(())
}

fn absorb_roster_file(dir: &Path, state: &StateFile) -> Result<(), String> {
    let path = dir.join("roster.json");
    let Ok(raw) = fs::read_to_string(&path) else {
        return Ok(());
    };
    let current = SignedRoster::open(&state.roster, &state.admin_public)?;
    let Ok(incoming) = SignedRoster::open(&raw, &state.admin_public) else {
        return Ok(());
    };
    if !matches!(
        current.compare(&incoming),
        kimetsu_bus::RosterSupersede::IncomingWins
    ) {
        return Ok(());
    }
    refuse_wider_than_repo(&incoming)?;
    let mut updated = load_state(dir)?;
    updated.roster = incoming.document().to_string();
    updated.group = incoming.group().to_string();
    save_state(dir, &updated)?;
    for entry in incoming.endpoints() {
        if entry.mounts.iter().all(|mount| *mount == MountLevel::Repo) {
            for public in &entry.signers {
                trust_signer(dir, public)?;
            }
        }
    }
    Ok(())
}

async fn wait_for_stop() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        if let Ok(mut terminate) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = terminate.recv() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

fn refuse_wider_than_repo(roster: &SignedRoster) -> Result<(), String> {
    for entry in roster.endpoints() {
        if entry.mounts.iter().any(|mount| *mount != MountLevel::Repo) {
            return Err(
                "this network shares more than the repo brain. Kimetsu will not join it.".into(),
            );
        }
    }
    Ok(())
}

fn check_name(name: &str) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() || name.len() > 64 {
        return Err("network name must be 1 to 64 characters".into());
    }
    if !name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        return Err("network name can use letters, numbers, '-' and '_'".into());
    }
    Ok(())
}

fn trust_signer(dir: &Path, public_hex: &str) -> Result<(), String> {
    let path = dir.join("trust.json");
    let mut store = if path.exists() {
        TrustStore::load(&path)?
    } else {
        TrustStore::empty()
    };
    store.allow(public_hex, &[MountLevel::Repo])?;
    store.save(&path)?;
    tighten(path)
}

fn load_trust(dir: &Path) -> Result<TrustStore, String> {
    let path = dir.join("trust.json");
    if path.exists() {
        TrustStore::load(&path)
    } else {
        Ok(TrustStore::empty())
    }
}

fn state_dir(flag: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(path) = flag {
        return Ok(path.to_path_buf());
    }
    if let Some(env) = std::env::var_os("KIMETSU_NETWORK_DIR")
        && !env.is_empty()
    {
        return Ok(PathBuf::from(env));
    }
    kimetsu_core::paths::user_kimetsu_dir()
        .map(|dir| dir.join("network"))
        .ok_or_else(|| "no home directory for network state. Pass --state-dir.".into())
}

fn state_path(dir: &Path) -> PathBuf {
    dir.join("state.json")
}

fn load_state(dir: &Path) -> Result<StateFile, String> {
    let raw = fs::read_to_string(state_path(dir)).map_err(|err| {
        format!("not on a network ({err}). Run `kimetsu network join <code>` first.")
    })?;
    let state: StateFile =
        serde_json::from_str(&raw).map_err(|err| format!("network state: {err}"))?;
    if state.kimetsu_network != 1 {
        return Err("this network state is from a newer kimetsu".into());
    }
    Ok(state)
}

fn save_state(dir: &Path, state: &StateFile) -> Result<(), String> {
    let body = serde_json::to_vec_pretty(state).map_err(|err| err.to_string())?;
    write_secret_file(&state_path(dir), &body)
}

fn write_secret_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|err| format!("{}: {err}", parent.display()))?;
        tighten(parent.to_path_buf())?;
    }
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .map_err(|err| format!("{}: {err}", path.display()))?;
        file.write_all(bytes)
            .map_err(|err| format!("{}: {err}", path.display()))?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        fs::write(path, bytes).map_err(|err| format!("{}: {err}", path.display()))
    }
}

fn tighten(path: PathBuf) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if path.is_dir() { 0o700 } else { 0o600 };
        fs::set_permissions(&path, fs::Permissions::from_mode(mode))
            .map_err(|err| format!("{}: {err}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

fn encode_code(value: &Value) -> Result<String, String> {
    let json = serde_json::to_vec(value).map_err(|err| err.to_string())?;
    Ok(format!(
        "knet1.{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
    ))
}

fn decode_code(code: &str) -> Result<Value, String> {
    let payload = code
        .trim()
        .strip_prefix("knet1.")
        .ok_or("join code should start with knet1.")?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| "join code is not valid. Copy the whole line.".to_string())?;
    serde_json::from_slice(&bytes)
        .map_err(|_| "join code is not valid. Copy the whole line.".to_string())
}

fn random_secret() -> Result<[u8; 32], String> {
    decode_hex_32(&PackSigner::generate()?.secret_key_hex())
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0xf) as usize] as char);
    }
    out
}

fn decode_hex_32(text: &str) -> Result<[u8; 32], String> {
    let text = text.trim();
    if text.len() != 64 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("expected 64 hex characters".into());
    }
    let mut out = [0u8; 32];
    for (index, chunk) in text.as_bytes().chunks(2).enumerate() {
        let digits = std::str::from_utf8(chunk).map_err(|_| "invalid hex")?;
        out[index] = u8::from_str_radix(digits, 16).map_err(|_| "invalid hex")?;
    }
    Ok(out)
}

fn status_json(report: &StatusReport) -> Value {
    json!({
        "compiled": true,
        "joined": report.joined,
        "listening": false,
        "group": report.group,
        "role": report.role,
        "generation": report.generation,
        "endpoint_id": report.endpoint_id,
    })
}

fn request_json(info: &RequestInfo) -> Value {
    json!({
        "verified": true,
        "digest": info.digest,
        "imported": info.imported,
        "deduped": info.deduped,
        "quarantined": info.quarantined,
        "superseded": info.superseded,
        "listening": false,
    })
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.block_on(future)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "kimetsu-net-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn membership_commands_do_not_bind_a_socket() {
        let before = ENDPOINT_BINDS.load(Ordering::SeqCst);
        let founder = temp_dir("founder");
        let guest = temp_dir("guest");
        let issued = invite(&founder, "lab").unwrap();
        assert!(issued.code.starts_with("knet1."));
        let state = fs::read_to_string(founder.join("state.json")).unwrap();
        let parsed: StateFile = serde_json::from_str(&state).unwrap();
        let secret = parsed.admin_secret.expect("founder keeps the admin key");
        assert!(
            !issued.code.contains(&secret),
            "join code must not carry the admin secret"
        );
        assert!(!issued.code.contains("memory"));
        let group = join(&guest, &issued.code).unwrap();
        assert_eq!(group, "lab");
        let report = status_of(&guest).unwrap();
        assert!(report.joined);
        assert_eq!(report.group.as_deref(), Some("lab"));
        assert!(leave(&guest).unwrap());
        assert!(!status_of(&guest).unwrap().joined);
        assert_eq!(
            ENDPOINT_BINDS.load(Ordering::SeqCst),
            before,
            "invite, join, status, and leave must not bind"
        );
        let _ = fs::remove_dir_all(founder);
    }

    #[test]
    fn a_code_that_shares_more_than_repo_is_refused() {
        let before = ENDPOINT_BINDS.load(Ordering::SeqCst);
        let admin_secret = random_secret().unwrap();
        let admin = RosterAdmin::from_secret_bytes(admin_secret);
        let key = kimetsu_bus::SecretKey::generate();
        let signer = PackSigner::generate().unwrap();
        let entry = RosterEndpoint::new(
            key.public(),
            &[MountLevel::Repo, MountLevel::User],
            [signer.public_key_hex()],
        )
        .unwrap();
        let document = admin.sign("wide", &[entry]).unwrap();
        let code = encode_code(&json!({
            "v": 1,
            "admin": admin.public_key_hex(),
            "roster": document,
            "endpoint_secret": encode_hex(&key.to_bytes()),
            "signer_secret": signer.secret_key_hex(),
        }))
        .unwrap();
        let dir = temp_dir("wide");
        let err = join(&dir, &code).unwrap_err();
        assert!(err.contains("more than the repo"), "{err}");
        assert!(!state_path(&dir).exists());
        assert_eq!(ENDPOINT_BINDS.load(Ordering::SeqCst), before);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn status_without_a_network_is_not_listening() {
        let dir = temp_dir("empty");
        let report = status_of(&dir).unwrap();
        let body = status_json(&report);
        assert_eq!(body["joined"], false);
        assert_eq!(body["listening"], false);
        assert_eq!(body["compiled"], true);
        let _ = fs::remove_dir_all(dir);
    }
}
