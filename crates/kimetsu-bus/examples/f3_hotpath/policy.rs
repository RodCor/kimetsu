//! Address, handoff, and report contract for the two-physical-host harness.
//!
//! The veth example calls [`reject_false_lan_label`] so a netns run cannot
//! rename itself into the §7 row. The two-host path does not take a topology
//! string: [`render_report`] picks the label from [`ReportKind`].

use std::net::Ipv4Addr;

use serde_json::{Map, Value};

pub const HANDOFF_KIND: &str = "kimetsu-f3-two-host-handoff";
pub const VETH_A: Ipv4Addr = Ipv4Addr::new(10, 200, 0, 1);
pub const VETH_B: Ipv4Addr = Ipv4Addr::new(10, 200, 0, 2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReportKind {
    TwoPhysicalHosts,
    LocalSelfTest,
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

pub fn build_handoff(handoff: &PublicHandoff) -> Result<String, String> {
    let value = Value::Object({
        let mut map = Map::new();
        map.insert("kind".into(), Value::String(HANDOFF_KIND.into()));
        map.insert("version".into(), Value::from(1));
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
    if version != 1 {
        return Err(format!("handoff version {version} is not supported"));
    }
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
    Ok(PublicHandoff {
        admin_public_hex: admin,
        roster_json,
        server_endpoint_id,
        server_ip,
        server_port: u16::try_from(server_port).map_err(|_| "handoff server port")?,
        digest,
        server: host_from(server)?,
    })
}

pub fn render_report(input: &ReportInput) -> Result<String, String> {
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
    };
    let closing = match input.kind {
        ReportKind::TwoPhysicalHosts => {
            "two physical LAN hosts: measured on the server and client blocks above\n"
        }
        ReportKind::LocalSelfTest => {
            "two physical LAN hosts: still pending. This run is not direct LAN QUIC between two LAN machines.\n"
        }
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
