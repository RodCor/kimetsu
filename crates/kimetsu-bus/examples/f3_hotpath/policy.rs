//! Address, handoff, and report contract for the two-physical-host harness.
//!
//! The veth example calls [`reject_false_lan_label`] so a netns run cannot
//! rename itself into the §7 row. The two-host path does not take a topology
//! string: [`render_report`] picks the label from [`ReportKind`].

use std::net::{Ipv4Addr, SocketAddrV4};

use serde_json::{Map, Value};

pub const HANDOFF_KIND: &str = "kimetsu-f3-two-host-handoff";
pub const VETH_A: Ipv4Addr = Ipv4Addr::new(10, 200, 0, 1);
pub const VETH_B: Ipv4Addr = Ipv4Addr::new(10, 200, 0, 2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReportKind {
    TwoPhysicalHosts,
    LocalSelfTest,
    InternetWan,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandoffMode {
    Lan,
    InternetWan,
}

#[derive(Clone, Copy, Debug)]
pub struct EndpointPolicy {
    pub self_test: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostFacts {
    pub hostname: String,
    pub machine_id: String,
    pub uname: String,
    pub cpu: String,
    pub cores: String,
    pub mem_kib: String,
    pub commit: String,
    pub rustc: String,
    pub os: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicHandoff {
    pub admin_public_hex: String,
    pub roster_json: String,
    pub server_endpoint_id: String,
    pub server_ip: String,
    pub server_port: u16,
    pub digest: String,
    pub server: HostFacts,
    pub mode: HandoffMode,
    pub bind_ip: String,
    pub bind_port: u16,
}

pub struct ReportInput {
    pub kind: ReportKind,
    pub n: usize,
    pub p50_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
    pub pack_bytes: usize,
    pub memories: usize,
    pub bind: String,
    pub peer: String,
    pub server: HostFacts,
    pub client: HostFacts,
    pub build_profile: String,
    pub raw_ms: Vec<f64>,
    pub advertised: String,
    pub client_advertised: String,
    /// `None` on a LAN report. `Some` is the set of QUIC path estimates that
    /// iroh actually returned; empty means the transport RTT was unavailable.
    pub quic_transport_rtt_ms: Option<Vec<f64>>,
}

pub fn require_mode(confirm: bool, self_test: bool) -> Result<ReportKind, String> {
    match (confirm, self_test) {
        (true, true) => Err(
            "refusing --confirm-two-physical-hosts on a self-test".into(),
        ),
        (false, false) => Err(
            "pass --confirm-two-physical-hosts on each physical machine, or run the script self-test subcommand. This process will not time a run that has no topology."
                .into(),
        ),
        (true, false) => Ok(ReportKind::TwoPhysicalHosts),
        (false, true) => Ok(ReportKind::LocalSelfTest),
    }
}

pub fn validate_release_profile(confirm: bool, debug_assertions: bool) -> Result<(), String> {
    if confirm && debug_assertions {
        Err(
            "refusing --confirm-two-physical-hosts on a debug build; use the script's release build"
                .into(),
        )
    } else {
        Ok(())
    }
}

pub fn validate_two_host_claim(
    confirm: bool,
    self_test: bool,
    server_machine_id: &str,
    client_machine_id: &str,
) -> Result<(), String> {
    if self_test && confirm {
        return Err("refusing --confirm-two-physical-hosts on a self-test".into());
    }
    if !confirm {
        return Ok(());
    }
    let server = server_machine_id.trim();
    let client = client_machine_id.trim();
    if server.is_empty() || client.is_empty() {
        return Err(
            "refusing --confirm-two-physical-hosts: machine id is missing on one side".into(),
        );
    }
    if server == client {
        return Err(format!(
            "refusing --confirm-two-physical-hosts: server and client machine ids match ({server})"
        ));
    }
    Ok(())
}

pub fn reject_false_lan_label(label: &str) -> Result<(), String> {
    let lower = label.to_ascii_lowercase();
    if lower.contains("two lan hosts")
        || lower.contains("two physical lan")
        || lower.contains("between two lan")
    {
        return Err(
            "topology label claims two LAN hosts; this process has no second physical host".into(),
        );
    }
    Ok(())
}

pub fn validate_ip(ip: Ipv4Addr, policy: EndpointPolicy) -> Result<(), String> {
    validate_ipv4(ip, policy)
}

pub fn validate_pair(bind: Ipv4Addr, peer: Ipv4Addr, policy: EndpointPolicy) -> Result<(), String> {
    validate_ipv4(bind, policy)?;
    validate_ipv4(peer, policy)?;
    if bind == peer {
        return Err(format!(
            "refusing identical bind and peer {bind}; a two-host run needs two addresses"
        ));
    }
    Ok(())
}

fn validate_ipv4(ip: Ipv4Addr, policy: EndpointPolicy) -> Result<(), String> {
    if ip == VETH_A || ip == VETH_B {
        return Err(format!(
            "refusing veth harness address {ip}; those numbers are not a physical LAN"
        ));
    }
    if policy.self_test {
        if !ip.is_loopback() {
            return Err(format!("self-test only binds loopback addresses, not {ip}"));
        }
        return Ok(());
    }
    if ip.is_loopback() {
        return Err(format!(
            "refusing loopback address {ip} for a two-physical-host run"
        ));
    }
    if ip.is_unspecified() || ip.is_broadcast() || ip.is_multicast() {
        return Err(format!("refusing address {ip}"));
    }
    Ok(())
}

pub fn validate_wan_bind(ip: Ipv4Addr) -> Result<(), String> {
    if ip == VETH_A || ip == VETH_B {
        return Err(format!(
            "refusing veth harness address {ip}; those numbers are not an internet path"
        ));
    }
    if ip.is_unspecified() || ip.is_broadcast() || ip.is_multicast() || ip.is_loopback() {
        return Err(format!(
            "refusing address {ip}; WAN bind is one explicit host address, not a wildcard, loopback, or broadcast"
        ));
    }
    Ok(())
}

pub fn validate_wan_advertise(ip: Ipv4Addr, port: u16) -> Result<(), String> {
    if port == 0 {
        return Err("advertised UDP port must be explicit, not 0".into());
    }
    if ip == VETH_A || ip == VETH_B {
        return Err(format!(
            "refusing veth harness address {ip}; those numbers are not an internet path"
        ));
    }
    if ip.is_unspecified() || ip.is_broadcast() || ip.is_multicast() || ip.is_loopback() {
        return Err(format!("refusing advertised address {ip}"));
    }
    if !ipv4_globally_reachable(ip) {
        return Err(format!(
            "refusing advertised address {ip}; an internet endpoint must be an IPv4 global unicast address under the IANA IPv4 Special-Purpose Address Registry"
        ));
    }
    Ok(())
}

/// IPv4 address the IANA special-purpose registry marks globally reachable,
/// and which is unicast. `192.0.0.9/32` and `192.0.0.10/32` stay accepted
/// because that registry marks them globally reachable. `192.88.99.0/24` is
/// rejected: the 6to4 anycast allocation ended in 2015, and `192.88.99.2/32`
/// is not globally reachable.
pub fn ipv4_globally_reachable(ip: Ipv4Addr) -> bool {
    let [a, b, c, d] = ip.octets();
    if a == 0
        || ip.is_private()
        || shared_address_space(ip)
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_documentation()
        || benchmarking_range(ip)
        || reserved_for_future(ip)
        || ip.is_broadcast()
        || ip.is_multicast()
    {
        return false;
    }
    // 192.0.0.0/24 is IETF protocol assignments and is not globally reachable,
    // except the two anycast addresses the registry marks globally reachable.
    if a == 192 && b == 0 && c == 0 && d != 9 && d != 10 {
        return false;
    }
    // Deprecated 6to4 relay anycast (RFC 7526). The leftover 192.88.99.2/32
    // entry is not globally reachable either.
    if a == 192 && b == 88 && c == 99 {
        return false;
    }
    true
}

fn shared_address_space(ip: Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    a == 100 && (b & 0b1100_0000) == 0b0100_0000
}

fn benchmarking_range(ip: Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    a == 198 && (b & 0xfe) == 18
}

fn reserved_for_future(ip: Ipv4Addr) -> bool {
    ip.octets()[0] >= 240 && !ip.is_broadcast()
}

/// IPv6 is not a dial target for this harness. The bus bind is IPv4.
pub fn parse_advertised_endpoint(text: &str) -> Result<SocketAddrV4, String> {
    let text = text.trim();
    if text.starts_with('[') || text.matches(':').count() > 1 {
        return Err(
            "IPv6 is not accepted; an advertised endpoint must be an IPv4 global unicast address"
                .into(),
        );
    }
    let addr: SocketAddrV4 = text
        .parse()
        .map_err(|_| format!("--advertise expects ipv4:port, not {text}"))?;
    validate_wan_advertise(*addr.ip(), addr.port())?;
    Ok(addr)
}

pub fn require_fixed_udp_port(port: Option<u16>) -> Result<u16, String> {
    match port {
        Some(port) if (1..=65535).contains(&port) => Ok(port),
        Some(port) => Err(format!("WAN UDP port {port} is not usable")),
        None => Err(
            "WAN requires one explicit fixed UDP port; an ephemeral bind is not a forwardable mapping"
                .into(),
        ),
    }
}

pub fn require_distinct_mapping(
    bind: Ipv4Addr,
    bind_port: u16,
    advertise: Ipv4Addr,
    advertise_port: u16,
) -> Result<(), String> {
    if bind == advertise && bind_port == advertise_port {
        return Err(
            "refusing to advertise the local bind; --advertise must be the external ip:port, which can differ by address or by mapped port"
                .into(),
        );
    }
    Ok(())
}

/// The dial target is only the operator's `--advertise`. A STUN result is ignored
/// even when it is present, so a mapping is never promoted to a route.
pub fn select_advertised(
    explicit: Option<SocketAddrV4>,
    stun: Option<SocketAddrV4>,
) -> Result<SocketAddrV4, String> {
    let Some(explicit) = explicit else {
        let _ = stun;
        return Err(
            "refusing to advertise a STUN mapping; pass --advertise ip:port. STUN is a diagnostic, not a route"
                .into(),
        );
    };
    validate_wan_advertise(*explicit.ip(), explicit.port())?;
    Ok(explicit)
}

pub fn require_handoff_mode(expected: HandoffMode, actual: HandoffMode) -> Result<(), String> {
    if expected != actual {
        return Err(format!(
            "handoff mode {actual:?} does not match this command ({expected:?})"
        ));
    }
    Ok(())
}

pub fn stun_binding_request(transaction_id: &[u8; 12]) -> Vec<u8> {
    let mut msg = Vec::with_capacity(20);
    msg.extend_from_slice(&0x0001u16.to_be_bytes());
    msg.extend_from_slice(&0u16.to_be_bytes());
    msg.extend_from_slice(&STUN_MAGIC.to_be_bytes());
    msg.extend_from_slice(transaction_id);
    msg
}

pub fn parse_stun_xor_mapped(
    bytes: &[u8],
    transaction_id: &[u8; 12],
) -> Result<SocketAddrV4, String> {
    if bytes.len() < 20 {
        return Err("STUN response is too short".into());
    }
    let message_type = u16::from_be_bytes([bytes[0], bytes[1]]);
    if message_type != 0x0101 {
        return Err(format!(
            "STUN message type {message_type:#x} is not a binding success"
        ));
    }
    let length = u16::from_be_bytes([bytes[2], bytes[3]]) as usize;
    let magic = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    if magic != STUN_MAGIC {
        return Err("STUN magic cookie does not match".into());
    }
    if &bytes[8..20] != transaction_id {
        return Err("STUN transaction id does not match this diagnostic".into());
    }
    if bytes.len() < 20 + length {
        return Err("STUN response is truncated".into());
    }
    let mut offset = 20;
    let end = 20 + length;
    while offset + 4 <= end {
        let attr_type = u16::from_be_bytes([bytes[offset], bytes[offset + 1]]);
        let attr_len = u16::from_be_bytes([bytes[offset + 2], bytes[offset + 3]]) as usize;
        let value_at = offset + 4;
        let value_end = value_at + attr_len;
        if value_end > end {
            return Err("STUN attribute is truncated".into());
        }
        if attr_type == 0x0020 && attr_len >= 8 {
            let family = bytes[value_at + 1];
            if family != 0x01 {
                return Err("STUN mapped address is not IPv4".into());
            }
            let xport = u16::from_be_bytes([bytes[value_at + 2], bytes[value_at + 3]]);
            let xaddr = u32::from_be_bytes([
                bytes[value_at + 4],
                bytes[value_at + 5],
                bytes[value_at + 6],
                bytes[value_at + 7],
            ]);
            let port = xport ^ ((STUN_MAGIC >> 16) as u16);
            let ip = Ipv4Addr::from(xaddr ^ STUN_MAGIC);
            return Ok(SocketAddrV4::new(ip, port));
        }
        let padded = attr_len.div_ceil(4) * 4;
        offset = value_at + padded;
    }
    Err("STUN response has no XOR-MAPPED-ADDRESS".into())
}

const STUN_MAGIC: u32 = 0x2112_A442;

pub fn build_handoff(handoff: &PublicHandoff) -> Result<String, String> {
    let value = Value::Object({
        let mut map = Map::new();
        map.insert("kind".into(), Value::String(HANDOFF_KIND.into()));
        match handoff.mode {
            HandoffMode::Lan => {
                map.insert("version".into(), Value::from(1));
            }
            HandoffMode::InternetWan => {
                map.insert("version".into(), Value::from(2));
                map.insert("mode".into(), Value::String("internet-wan".into()));
                map.insert("bind_ip".into(), Value::String(handoff.bind_ip.clone()));
                map.insert("bind_port".into(), Value::from(handoff.bind_port));
            }
        }
        map.insert(
            "admin_public_hex".into(),
            Value::String(handoff.admin_public_hex.clone()),
        );
        map.insert(
            "roster_json".into(),
            Value::String(handoff.roster_json.clone()),
        );
        map.insert(
            "server_endpoint_id".into(),
            Value::String(handoff.server_endpoint_id.clone()),
        );
        map.insert("server_ip".into(), Value::String(handoff.server_ip.clone()));
        map.insert("server_port".into(), Value::from(handoff.server_port));
        map.insert("digest".into(), Value::String(handoff.digest.clone()));
        map.insert("relay".into(), Value::String("disabled".into()));
        map.insert("embeddings".into(), Value::String("off".into()));
        map.insert(
            "timer".into(),
            Value::String("verified request_pack, not import".into()),
        );
        map.insert("server".into(), host_value(&handoff.server));
        map
    });
    reject_private_keys(&value)?;
    let text = serde_json::to_string_pretty(&value).map_err(|err| err.to_string())?;
    let _ = parse_public_handoff(&text)?;
    Ok(text)
}

pub fn parse_public_handoff(text: &str) -> Result<PublicHandoff, String> {
    let value: Value = serde_json::from_str(text).map_err(|err| format!("handoff: {err}"))?;
    reject_private_keys(&value)?;
    let object = value.as_object().ok_or("handoff must be a JSON object")?;
    let kind = required_str(object, "kind")?;
    if kind != HANDOFF_KIND {
        return Err(format!("handoff kind {kind} is not a two-host handoff"));
    }
    let version = object
        .get("version")
        .and_then(Value::as_u64)
        .ok_or("handoff version is missing")?;
    let mode = match version {
        1 => {
            if object.contains_key("mode") {
                return Err(
                    "a version 1 handoff is the LAN exchange and cannot carry an internet mode"
                        .into(),
                );
            }
            HandoffMode::Lan
        }
        2 => {
            let mode = required_str(object, "mode")?;
            if mode != "internet-wan" {
                return Err(format!(
                    "handoff mode {mode} is not internet-wan; this harness will not relabel it"
                ));
            }
            HandoffMode::InternetWan
        }
        other => return Err(format!("handoff version {other} is not supported")),
    };
    let relay = required_str(object, "relay")?;
    if relay != "disabled" {
        return Err(format!(
            "handoff relay must be disabled, not {relay}; BusRelay::Disabled is the measurement"
        ));
    }
    let embeddings = required_str(object, "embeddings")?;
    if embeddings != "off" {
        return Err("handoff embeddings must be off".into());
    }
    let timer = required_str(object, "timer")?;
    if timer != "verified request_pack, not import" {
        return Err("handoff timer must be verified request_pack, not import".into());
    }
    let admin = required_str(object, "admin_public_hex")?.to_ascii_lowercase();
    if !is_hex_64(&admin) {
        return Err("handoff admin public key must be 64 hex chars".into());
    }
    let roster_json = required_str(object, "roster_json")?;
    if !roster_json.contains("kimetsu_roster") {
        return Err("handoff roster is not a kimetsu_roster document".into());
    }
    let server_endpoint_id = required_str(object, "server_endpoint_id")?;
    if server_endpoint_id.is_empty() {
        return Err("handoff is missing the server endpoint id".into());
    }
    let server_ip = required_str(object, "server_ip")?;
    let _: Ipv4Addr = server_ip
        .parse()
        .map_err(|_| format!("handoff server address {server_ip} is not IPv4"))?;
    let server_port = object
        .get("server_port")
        .and_then(Value::as_u64)
        .ok_or("handoff server port is missing")?;
    if !(1..65536).contains(&server_port) {
        return Err(format!("handoff server port {server_port} is not usable"));
    }
    let digest = required_str(object, "digest")?.to_ascii_lowercase();
    if !is_hex_64(&digest) {
        return Err("handoff digest must be 64 hex chars".into());
    }
    let server = object
        .get("server")
        .and_then(Value::as_object)
        .ok_or("handoff is missing the server host block")?;
    let server_port = u16::try_from(server_port).map_err(|_| "handoff server port")?;
    let (bind_ip, bind_port) = match mode {
        HandoffMode::Lan => (server_ip.clone(), server_port),
        HandoffMode::InternetWan => {
            let bind_ip = required_str(object, "bind_ip")?;
            let bind_parsed: Ipv4Addr = bind_ip
                .parse()
                .map_err(|_| format!("handoff bind address {bind_ip} is not IPv4"))?;
            validate_wan_bind(bind_parsed)?;
            let advertise: Ipv4Addr = server_ip
                .parse()
                .map_err(|_| format!("handoff advertised address {server_ip} is not IPv4"))?;
            validate_wan_advertise(advertise, server_port)?;
            let bind_port = object
                .get("bind_port")
                .and_then(Value::as_u64)
                .ok_or("handoff bind port is missing")?;
            if !(1..65536).contains(&bind_port) {
                return Err(format!("handoff bind port {bind_port} is not usable"));
            }
            let bind_port = u16::try_from(bind_port).map_err(|_| "handoff bind port")?;
            require_distinct_mapping(bind_parsed, bind_port, advertise, server_port)?;
            (bind_ip, bind_port)
        }
    };
    Ok(PublicHandoff {
        admin_public_hex: admin,
        roster_json,
        server_endpoint_id,
        server_ip,
        server_port,
        digest,
        server: host_from(server)?,
        mode,
        bind_ip,
        bind_port,
    })
}

pub fn render_report(input: &ReportInput) -> Result<String, String> {
    if input.kind == ReportKind::InternetWan {
        return render_wan_report(input);
    }
    let topology = match input.kind {
        ReportKind::TwoPhysicalHosts => {
            validate_two_host_claim(
                true,
                false,
                &input.server.machine_id,
                &input.client.machine_id,
            )?;
            if input.server.hostname.trim().is_empty() || input.client.hostname.trim().is_empty() {
                return Err(
                    "refusing --confirm-two-physical-hosts: hostname is missing on one side".into(),
                );
            }
            if input.build_profile != "release" {
                return Err(
                    "refusing --confirm-two-physical-hosts on a debug build; use the script's release build"
                        .into(),
                );
            }
            "two physical LAN hosts"
        }
        ReportKind::LocalSelfTest => "local self-test on one host, not two physical LAN hosts",
        ReportKind::InternetWan => unreachable!("WAN reports use render_wan_report"),
    };
    let closing = match input.kind {
        ReportKind::TwoPhysicalHosts => {
            "two physical LAN hosts: measured on the server and client blocks above\n"
        }
        ReportKind::LocalSelfTest => {
            "two physical LAN hosts: still pending. This run is not direct LAN QUIC between two LAN machines.\n"
        }
        ReportKind::InternetWan => unreachable!("WAN reports use render_wan_report"),
    };
    let server_hw = hardware_line(&input.server);
    let client_hw = hardware_line(&input.client);
    Ok(format!(
        "\
topology: {topology}
server_hardware: {server_hw}
client_hardware: {client_hw}
server_hostname: {server_hostname}
client_hostname: {client_hostname}
server_machine_id: {server_machine}
client_machine_id: {client_machine}
server_commit: {server_commit}
client_commit: {client_commit}
server_rustc: {server_rustc}
client_rustc: {client_rustc}
server_os: {server_os}
client_os: {client_os}
build_profile: {profile}
bind: {bind}
peer: {peer}
request_pack direct QUIC until verified in memory: {topology}. n={n} p50={p50:.3} ms p99={p99:.3} ms max={max:.3} ms
pack: {bytes} gzip bytes, {memories} memories, relay disabled
dialed address is {peer}; local bind is {bind}
import and FTS rebuild: not included in the request_pack timer
embeddings: off
{closing}",
        server_hostname = input.server.hostname,
        client_hostname = input.client.hostname,
        server_machine = input.server.machine_id,
        client_machine = input.client.machine_id,
        server_commit = input.server.commit,
        client_commit = input.client.commit,
        server_rustc = input.server.rustc,
        client_rustc = input.client.rustc,
        server_os = input.server.os,
        client_os = input.client.os,
        profile = input.build_profile,
        bind = input.bind,
        peer = input.peer,
        n = input.n,
        p50 = input.p50_ms,
        p99 = input.p99_ms,
        max = input.max_ms,
        bytes = input.pack_bytes,
        memories = input.memories,
    ))
}

fn render_wan_report(input: &ReportInput) -> Result<String, String> {
    let server_machine = input.server.machine_id.trim();
    let client_machine = input.client.machine_id.trim();
    if server_machine.is_empty() || client_machine.is_empty() {
        return Err("refusing --confirm-internet-wan: machine id is missing on one side".into());
    }
    if server_machine == client_machine {
        return Err(format!(
            "refusing --confirm-internet-wan: server and client machine ids match ({server_machine})"
        ));
    }
    if input.server.hostname.trim().is_empty() || input.client.hostname.trim().is_empty() {
        return Err("refusing --confirm-internet-wan: hostname is missing on one side".into());
    }
    if input.build_profile != "release" {
        return Err(
            "refusing --confirm-internet-wan on a debug build; use the script's release build"
                .into(),
        );
    }
    if input.raw_ms.len() != input.n || input.n == 0 {
        return Err(
            "no successful request_pack samples; refusing to invent request_pack timings or a transport RTT"
                .into(),
        );
    }
    if input.advertised.trim().is_empty() {
        return Err("WAN report is missing the advertised endpoint".into());
    }
    let mut sorted = input.raw_ms.clone();
    sorted.sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
    let p95 = percentile_nearest(&sorted, 0.95);
    let raw = input
        .raw_ms
        .iter()
        .map(|sample| format!("{sample:.3}"))
        .collect::<Vec<_>>()
        .join(",");
    let transport = transport_rtt_lines(input.quic_transport_rtt_ms.as_deref());
    let server_hw = hardware_line(&input.server);
    let client_hw = hardware_line(&input.client);
    let topology = "internet/WAN, not a physical LAN";
    Ok(format!(
        "\
topology: {topology}
path: direct UDP to the advertised endpoint, relay disabled
stun: not a route and not advertised
request_pack_elapsed_metric: median client-side elapsed time of verified request_pack, from send through in-memory signature verification; not a QUIC transport RTT and not STUN
request_pack_elapsed_p50_ms: {p50:.3}
request_pack_elapsed_p95_ms: {p95:.3}
request_pack_elapsed_raw_ms: {raw}
{transport}server_hardware: {server_hw}
client_hardware: {client_hw}
server_hostname: {server_hostname}
client_hostname: {client_hostname}
server_machine_id: {server_machine}
client_machine_id: {client_machine}
server_commit: {server_commit}
client_commit: {client_commit}
server_rustc: {server_rustc}
client_rustc: {client_rustc}
server_os: {server_os}
client_os: {client_os}
build_profile: {profile}
bind: {bind}
advertised: {advertised}
client_advertised: {client_advertised}
peer: {peer}
request_pack direct QUIC until verified in memory: {topology}. n={n} p50={p50:.3} ms p99={p99:.3} ms max={max:.3} ms
pack: {bytes} gzip bytes, {memories} memories, relay disabled
dialed address is {peer}; local bind is {bind}; advertised endpoint is {advertised}
import and FTS rebuild: not included in the request_pack timer
embeddings: off
physical LAN section 7 row: not this run. No internet/WAN figure is recorded until two hosts produce this stdout.
",
        p50 = input.p50_ms,
        server_hostname = input.server.hostname,
        client_hostname = input.client.hostname,
        server_machine = input.server.machine_id,
        client_machine = input.client.machine_id,
        server_commit = input.server.commit,
        client_commit = input.client.commit,
        server_rustc = input.server.rustc,
        client_rustc = input.client.rustc,
        server_os = input.server.os,
        client_os = input.client.os,
        profile = input.build_profile,
        bind = input.bind,
        advertised = input.advertised,
        client_advertised = input.client_advertised,
        peer = input.peer,
        n = input.n,
        p99 = input.p99_ms,
        max = input.max_ms,
        bytes = input.pack_bytes,
        memories = input.memories,
    ))
}

fn transport_rtt_lines(samples: Option<&[f64]>) -> String {
    let Some(samples) = samples else {
        return "quic_transport_rtt: unavailable\nquic_transport_rtt_source: not sampled\n"
            .to_string();
    };
    if samples.is_empty() {
        return "quic_transport_rtt: unavailable\nquic_transport_rtt_source: iroh Path::rtt on the selected direct IP path after each verified fetch; no direct-path estimate was reported\n"
            .to_string();
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
    let median = percentile_nearest(&sorted, 0.50);
    let raw = samples
        .iter()
        .map(|sample| format!("{sample:.3}"))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "quic_transport_rtt_ms: {median:.3}\nquic_transport_rtt_raw_ms: {raw}\nquic_transport_rtt_source: median of iroh Path::rtt on the selected direct IP path after each verified fetch; QUIC smoothed estimate, not request_pack elapsed time and not STUN\n"
    )
}

fn percentile_nearest(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = ((sorted.len() as f64) * p).ceil() as usize;
    let index = rank.saturating_sub(1).min(sorted.len() - 1);
    sorted[index]
}

fn hardware_line(facts: &HostFacts) -> String {
    format!(
        "{} | {} | {} cores | MemTotal {} KiB",
        facts.uname, facts.cpu, facts.cores, facts.mem_kib
    )
}

fn host_value(facts: &HostFacts) -> Value {
    let mut map = Map::new();
    map.insert("hostname".into(), Value::String(facts.hostname.clone()));
    map.insert("machine_id".into(), Value::String(facts.machine_id.clone()));
    map.insert("uname".into(), Value::String(facts.uname.clone()));
    map.insert("cpu".into(), Value::String(facts.cpu.clone()));
    map.insert("cores".into(), Value::String(facts.cores.clone()));
    map.insert("mem_kib".into(), Value::String(facts.mem_kib.clone()));
    map.insert("commit".into(), Value::String(facts.commit.clone()));
    map.insert("rustc".into(), Value::String(facts.rustc.clone()));
    map.insert("os".into(), Value::String(facts.os.clone()));
    Value::Object(map)
}

fn host_from(object: &Map<String, Value>) -> Result<HostFacts, String> {
    Ok(HostFacts {
        hostname: required_str(object, "hostname")?,
        machine_id: required_str(object, "machine_id")?,
        uname: required_str(object, "uname")?,
        cpu: required_str(object, "cpu")?,
        cores: required_str(object, "cores")?,
        mem_kib: required_str(object, "mem_kib")?,
        commit: required_str(object, "commit")?,
        rustc: required_str(object, "rustc")?,
        os: required_str(object, "os")?,
    })
}

fn required_str(object: &Map<String, Value>, key: &str) -> Result<String, String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty() || key == "hostname" || key == "machine_id")
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("handoff is missing {key}"))
}

fn reject_private_keys(value: &Value) -> Result<(), String> {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let lower = key.to_ascii_lowercase();
                if lower.contains("secret") || lower.contains("private") {
                    return Err(
                        "handoff carries a private key; two-host exchange is public only".into(),
                    );
                }
                reject_private_keys(child)?;
            }
            Ok(())
        }
        Value::Array(items) => {
            for item in items {
                reject_private_keys(item)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn is_hex_64(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}
