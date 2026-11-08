//! Contract for the two-physical-host measurement harness.
//!
//! The implementation lives next to the example so the veth path and this
//! path share one report and address policy. These tests do not open a socket
//! and they do not produce a §7 number.

#[path = "../examples/f3_hotpath/policy.rs"]
mod policy;

use std::net::{Ipv4Addr, SocketAddrV4};

use policy::{
    EndpointPolicy, HandoffMode, HostFacts, PublicHandoff, ReportInput, ReportKind, build_handoff,
    parse_public_handoff, parse_stun_xor_mapped, reject_false_lan_label, render_report,
    require_distinct_mapping, require_fixed_udp_port, require_handoff_mode, require_mode,
    select_advertised, stun_binding_request, validate_ip, validate_pair, validate_release_profile,
    validate_two_host_claim, validate_wan_advertise, validate_wan_bind,
};

fn ip(text: &str) -> Ipv4Addr {
    text.parse().unwrap()
}

fn facts(name: &str, machine: &str) -> HostFacts {
    HostFacts {
        hostname: name.into(),
        machine_id: machine.into(),
        uname: format!("Linux {name}"),
        cpu: format!("cpu-{name}"),
        cores: "4".into(),
        mem_kib: "1024".into(),
        commit: format!("commit-{name}"),
        rustc: "rustc 1.99.1".into(),
        os: format!("os-{name}"),
    }
}

fn sample(kind: ReportKind) -> ReportInput {
    ReportInput {
        kind,
        n: 30,
        p50_ms: 1.2,
        p99_ms: 1.5,
        max_ms: 1.7,
        pack_bytes: 394,
        memories: 8,
        bind: "192.168.1.20:40000".into(),
        peer: "192.168.1.10:40001".into(),
        server: facts("pc-a", "machine-a"),
        client: facts("pc-b", "machine-b"),
        build_profile: "release".into(),
        raw_ms: Vec::new(),
        advertised: String::new(),
        client_advertised: String::new(),
        quic_transport_rtt_ms: None,
    }
}

#[test]
fn two_host_addresses_reject_loopback_veth_and_identical_ends() {
    let lan = EndpointPolicy { self_test: false };
    let err = validate_pair(ip("127.0.0.1"), ip("192.168.1.10"), lan).unwrap_err();
    assert!(err.contains("refusing loopback address 127.0.0.1"));
    let err = validate_pair(ip("10.200.0.2"), ip("192.168.1.10"), lan).unwrap_err();
    assert!(err.contains("refusing veth harness address 10.200.0.2"));
    let err = validate_pair(ip("192.168.1.10"), ip("10.200.0.1"), lan).unwrap_err();
    assert!(err.contains("refusing veth harness address 10.200.0.1"));
    let err = validate_pair(ip("0.0.0.0"), ip("192.168.1.10"), lan).unwrap_err();
    assert!(err.contains("refusing address 0.0.0.0"));
    let err = validate_pair(ip("255.255.255.255"), ip("192.168.1.10"), lan).unwrap_err();
    assert!(err.contains("refusing address 255.255.255.255"));
    let err = validate_pair(ip("224.0.0.1"), ip("192.168.1.10"), lan).unwrap_err();
    assert!(err.contains("refusing address 224.0.0.1"));
    let err = validate_pair(ip("192.168.1.10"), ip("192.168.1.10"), lan).unwrap_err();
    assert!(err.contains("refusing identical bind and peer"));
    validate_pair(ip("192.168.1.20"), ip("192.168.1.10"), lan).unwrap();
    validate_ip(ip("192.168.1.10"), lan).unwrap();
    let err = validate_ip(ip("127.0.0.2"), lan).unwrap_err();
    assert!(err.contains("refusing loopback address 127.0.0.2"));
}

#[test]
fn self_test_is_loopback_only_and_cannot_confirm() {
    let smoke = EndpointPolicy { self_test: true };
    validate_pair(ip("127.0.0.1"), ip("127.0.0.2"), smoke).unwrap();
    let err = validate_pair(ip("192.168.1.20"), ip("192.168.1.10"), smoke).unwrap_err();
    assert!(err.contains("self-test only binds loopback"));
    let err = require_mode(true, true).unwrap_err();
    assert!(err.contains("refusing --confirm-two-physical-hosts on a self-test"));
    assert!(matches!(
        require_mode(false, true).unwrap(),
        ReportKind::LocalSelfTest
    ));
    assert!(matches!(
        require_mode(true, false).unwrap(),
        ReportKind::TwoPhysicalHosts
    ));
    let err = require_mode(false, false).unwrap_err();
    assert!(err.contains("--confirm-two-physical-hosts"));
}

#[test]
fn confirm_requires_two_machine_ids_and_a_release_build() {
    validate_two_host_claim(false, true, "same", "same").unwrap();
    let err = validate_two_host_claim(true, false, "same", "same").unwrap_err();
    assert!(err.contains("machine ids match"));
    let err = validate_two_host_claim(true, false, "", "other").unwrap_err();
    assert!(err.contains("machine id is missing"));
    let err = validate_two_host_claim(true, false, "a", "").unwrap_err();
    assert!(err.contains("hostname is missing") || err.contains("machine id is missing"));
    validate_two_host_claim(true, false, "machine-a", "machine-b").unwrap();
    let err = validate_release_profile(true, true).unwrap_err();
    assert!(err.contains("debug build"));
    validate_release_profile(true, false).unwrap();
    validate_release_profile(false, true).unwrap();
}

#[test]
fn veth_label_still_cannot_claim_two_lan_hosts() {
    let err = reject_false_lan_label("two LAN hosts on a real switch").unwrap_err();
    assert!(err.contains("no second physical host"));
    let err = reject_false_lan_label("between two LAN machines").unwrap_err();
    assert!(err.contains("no second physical host"));
    reject_false_lan_label("two Linux netns connected by a veth pair on one physical host")
        .unwrap();
}

#[test]
fn handoff_is_public_and_rejects_a_carried_secret() {
    let handoff = PublicHandoff {
        admin_public_hex: "ab".repeat(32),
        roster_json: r#"{"kimetsu_roster":1,"group":"f3-two-host"}"#.into(),
        server_endpoint_id: "endpoint-a".into(),
        server_ip: "192.168.1.10".into(),
        server_port: 43111,
        digest: "cd".repeat(32),
        server: facts("pc-a", "machine-a"),
        mode: policy::HandoffMode::Lan,
        bind_ip: "192.168.1.10".into(),
        bind_port: 43111,
    };
    let text = build_handoff(&handoff).unwrap();
    assert!(!text.to_ascii_lowercase().contains("secret"));
    assert!(!text.to_ascii_lowercase().contains("private"));
    let parsed = parse_public_handoff(&text).unwrap();
    assert_eq!(parsed.server_ip, "192.168.1.10");
    assert_eq!(parsed.server_port, 43111);
    assert_eq!(parsed.digest, "cd".repeat(32));
    assert_eq!(parsed.server.machine_id, "machine-a");
    assert_eq!(parsed.server.hostname, "pc-a");

    let mut smuggled = serde_json::from_str::<serde_json::Value>(&text).unwrap();
    smuggled["client_secret_hex"] = serde_json::Value::String("ff".repeat(32));
    let err = parse_public_handoff(&smuggled.to_string()).unwrap_err();
    assert!(err.contains("private key"));

    let mut relay = serde_json::from_str::<serde_json::Value>(&text).unwrap();
    relay["relay"] = serde_json::Value::String("custom".into());
    let err = parse_public_handoff(&relay.to_string()).unwrap_err();
    assert!(err.contains("relay"));
}

#[test]
fn reports_keep_self_test_out_of_the_section_7_row() {
    let confirmed = render_report(&sample(ReportKind::TwoPhysicalHosts)).unwrap();
    assert!(confirmed.contains("topology: two physical LAN hosts\n"));
    assert!(confirmed.contains(
        "request_pack direct QUIC until verified in memory: two physical LAN hosts. n=30 p50=1.200 ms p99=1.500 ms max=1.700 ms\n"
    ));
    assert!(confirmed.contains("pack: 394 gzip bytes, 8 memories, relay disabled\n"));
    assert!(confirmed.contains("bind: 192.168.1.20:40000\n"));
    assert!(confirmed.contains("peer: 192.168.1.10:40001\n"));
    assert!(confirmed.contains("server_hostname: pc-a\n"));
    assert!(confirmed.contains("client_hostname: pc-b\n"));
    assert!(confirmed.contains("server_machine_id: machine-a\n"));
    assert!(confirmed.contains("client_machine_id: machine-b\n"));
    assert!(confirmed.contains("server_commit: commit-pc-a\n"));
    assert!(confirmed.contains("client_commit: commit-pc-b\n"));
    assert!(confirmed.contains("server_rustc: rustc 1.99.1\n"));
    assert!(confirmed.contains("client_rustc: rustc 1.99.1\n"));
    assert!(confirmed.contains("server_os: os-pc-a\n"));
    assert!(confirmed.contains("client_os: os-pc-b\n"));
    assert!(
        confirmed
            .contains("server_hardware: Linux pc-a | cpu-pc-a | 4 cores | MemTotal 1024 KiB\n")
    );
    assert!(
        confirmed
            .contains("client_hardware: Linux pc-b | cpu-pc-b | 4 cores | MemTotal 1024 KiB\n")
    );
    assert!(confirmed.contains("build_profile: release\n"));
    assert!(confirmed.contains("embeddings: off\n"));
    assert!(confirmed.contains("import and FTS rebuild: not included in the request_pack timer\n"));
    assert!(
        confirmed
            .contains("dialed address is 192.168.1.10:40001; local bind is 192.168.1.20:40000\n")
    );
    assert!(
        confirmed
            .contains("two physical LAN hosts: measured on the server and client blocks above\n")
    );
    assert!(!confirmed.contains("still pending"));
    assert!(!confirmed.contains("10.200.0."));
    assert!(!confirmed.contains("127.0.0.1"));
    assert!(!confirmed.contains("internet/WAN"));

    let smoke = render_report(&sample(ReportKind::LocalSelfTest)).unwrap();
    assert!(smoke.contains("topology: local self-test on one host, not two physical LAN hosts\n"));
    assert!(smoke.contains(
        "two physical LAN hosts: still pending. This run is not direct LAN QUIC between two LAN machines.\n"
    ));
    assert!(!smoke.contains("topology: two physical LAN hosts\n"));
    assert!(!smoke.contains("two physical LAN hosts: measured"));
    assert!(smoke.contains("relay disabled"));
    assert!(smoke.contains("n=30 p50=1.200 ms p99=1.500 ms max=1.700 ms"));

    let mut matching = sample(ReportKind::TwoPhysicalHosts);
    matching.client.machine_id = matching.server.machine_id.clone();
    let err = render_report(&matching).unwrap_err();
    assert!(err.contains("machine ids match"));

    let mut unnamed = sample(ReportKind::TwoPhysicalHosts);
    unnamed.server.hostname.clear();
    let err = render_report(&unnamed).unwrap_err();
    assert!(err.contains("hostname is missing"));
}

#[test]
fn wan_advertise_is_explicit_and_never_taken_from_stun() {
    validate_wan_bind(ip("192.168.1.29")).unwrap();
    validate_wan_bind(ip("172.30.0.2")).unwrap();
    let err = validate_wan_bind(ip("0.0.0.0")).unwrap_err();
    assert!(err.contains("refusing address 0.0.0.0"));
    let err = validate_wan_bind(ip("127.0.0.1")).unwrap_err();
    assert!(err.contains("loopback"));
    let err = validate_wan_bind(ip("10.200.0.1")).unwrap_err();
    assert!(err.contains("veth"));
    validate_wan_advertise(ip("1.1.1.1"), 44144).unwrap();
    validate_wan_advertise(ip("8.8.8.8"), 443).unwrap();
    validate_wan_advertise(ip("203.0.114.1"), 44144).unwrap();
    validate_wan_advertise(ip("192.0.0.9"), 443).unwrap();
    validate_wan_advertise(ip("192.0.0.10"), 3478).unwrap();
    validate_wan_advertise(ip("192.31.196.1"), 443).unwrap();
    validate_wan_advertise(ip("192.52.193.1"), 443).unwrap();
    for rejected in [
        "0.1.2.3",
        "10.1.1.1",
        "100.64.0.1",
        "127.0.0.1",
        "169.254.1.1",
        "172.16.0.1",
        "192.0.0.1",
        "192.0.0.8",
        "192.0.2.1",
        "192.88.99.1",
        "192.88.99.2",
        "192.168.1.29",
        "198.18.0.1",
        "198.51.100.1",
        "203.0.113.10",
        "224.0.0.1",
        "240.0.0.1",
        "255.255.255.255",
    ] {
        let err = validate_wan_advertise(ip(rejected), 44144).unwrap_err();
        assert!(
            err.contains("global unicast") || err.contains("refusing advertised address"),
            "{rejected}: {err}"
        );
    }
    let err = policy::parse_advertised_endpoint("[2001:db8::1]:443").unwrap_err();
    assert!(err.contains("IPv6 is not accepted"));
    let err = policy::parse_advertised_endpoint("203.0.113.10:44144").unwrap_err();
    assert!(err.contains("global unicast"));
    policy::parse_advertised_endpoint("1.1.1.1:44144").unwrap();
    assert!(require_fixed_udp_port(None).is_err());
    assert!(require_fixed_udp_port(Some(0)).is_err());
    assert_eq!(require_fixed_udp_port(Some(44144)).unwrap(), 44144);
    require_distinct_mapping(ip("172.30.0.2"), 44144, ip("1.1.1.1"), 44144).unwrap();
    require_distinct_mapping(ip("1.1.1.1"), 44144, ip("1.1.1.1"), 37749).unwrap();
    let err = require_distinct_mapping(ip("1.1.1.1"), 44144, ip("1.1.1.1"), 44144).unwrap_err();
    assert!(err.contains("local bind"));

    let stun = SocketAddrV4::new(ip("198.18.0.1"), 37749);
    let explicit = SocketAddrV4::new(ip("1.1.1.1"), 44144);
    let chosen = select_advertised(Some(explicit), Some(stun)).unwrap();
    assert_eq!(chosen, explicit);
    let err = select_advertised(None, Some(stun)).unwrap_err();
    assert!(err.contains("refusing to advertise a STUN mapping"));
}

#[test]
fn stun_mapped_address_parses_and_stays_out_of_the_handoff() {
    let transaction_id = [9u8; 12];
    let request = stun_binding_request(&transaction_id);
    assert_eq!(request.len(), 20);
    let mapped = SocketAddrV4::new(ip("1.1.1.1"), 44144);
    let response = xor_mapped_response(mapped, &transaction_id);
    assert_eq!(
        parse_stun_xor_mapped(&response, &transaction_id).unwrap(),
        mapped
    );
    let err = parse_stun_xor_mapped(&response, &[1u8; 12]).unwrap_err();
    assert!(err.contains("transaction id"));

    let mut handoff = lan_handoff();
    handoff.mode = HandoffMode::InternetWan;
    handoff.bind_ip = "172.30.0.2".into();
    handoff.bind_port = 44144;
    handoff.server_ip = "1.1.1.1".into();
    handoff.server_port = 44144;
    let text = build_handoff(&handoff).unwrap();
    assert!(!text.to_ascii_lowercase().contains("stun"));
    assert!(!text.to_ascii_lowercase().contains("secret"));
    let parsed = parse_public_handoff(&text).unwrap();
    assert_eq!(parsed.mode, HandoffMode::InternetWan);
    assert_eq!(parsed.server_ip, "1.1.1.1");
    assert_eq!(parsed.bind_ip, "172.30.0.2");
    assert_eq!(parsed.bind_port, 44144);
    require_handoff_mode(HandoffMode::InternetWan, parsed.mode).unwrap();
    let err = require_handoff_mode(HandoffMode::Lan, parsed.mode).unwrap_err();
    assert!(err.contains("does not match"));

    let lan = parse_public_handoff(&build_handoff(&lan_handoff()).unwrap()).unwrap();
    assert_eq!(lan.mode, HandoffMode::Lan);
    let err = require_handoff_mode(HandoffMode::InternetWan, lan.mode).unwrap_err();
    assert!(err.contains("does not match"));
}

#[test]
fn wan_report_is_not_a_lan_row_and_has_no_invented_rtt() {
    let mut input = sample(ReportKind::InternetWan);
    input.raw_ms = vec![1.0, 1.2, 1.5];
    input.n = 3;
    input.p50_ms = 1.2;
    input.p99_ms = 1.5;
    input.max_ms = 1.5;
    input.advertised = "1.1.1.1:44144".into();
    input.client_advertised = "8.8.8.8:44144".into();
    input.quic_transport_rtt_ms = Some(vec![]);
    input.bind = "192.168.1.29:44144".into();
    input.peer = "1.1.1.1:44144".into();
    let report = render_report(&input).unwrap();
    assert!(report.contains("topology: internet/WAN, not a physical LAN\n"));
    assert!(report.contains("stun: not a route and not advertised\n"));
    assert!(report.contains(
        "request_pack_elapsed_metric: median client-side elapsed time of verified request_pack, from send through in-memory signature verification; not a QUIC transport RTT and not STUN\n"
    ));
    assert!(report.contains("request_pack_elapsed_p50_ms: 1.200\n"));
    assert!(report.contains("request_pack_elapsed_raw_ms: 1.000,1.200,1.500\n"));
    assert!(report.contains("quic_transport_rtt: unavailable\n"));
    assert!(!report.contains("peer_rtt"));
    assert!(report.contains("advertised: 1.1.1.1:44144\n"));
    assert!(report.contains("pack: 394 gzip bytes, 8 memories, relay disabled\n"));
    assert!(report.contains("physical LAN section 7 row: not this run"));
    assert!(!report.contains("topology: two physical LAN hosts\n"));
    assert!(!report.contains("two physical LAN hosts: measured"));

    input.raw_ms.clear();
    input.n = 0;
    let err = render_report(&input).unwrap_err();
    assert!(err.contains("refusing to invent"));

    input.n = 3;
    input.raw_ms = vec![1.0, 1.2, 1.5];
    input.quic_transport_rtt_ms = Some(vec![0.4, 0.6]);
    let with_rtt = render_report(&input).unwrap();
    assert!(
        with_rtt.contains("quic_transport_rtt_ms: 0.400\n")
            || with_rtt.contains("quic_transport_rtt_ms: 0.500\n")
            || with_rtt.contains("quic_transport_rtt_ms: 0.600\n")
    );
    assert!(with_rtt.contains("quic_transport_rtt_source: median of iroh Path::rtt"));
    assert!(with_rtt.contains("not request_pack elapsed time and not STUN"));
}

#[test]
fn tampered_roster_is_rejected_and_a_stranger_is_not_a_member() {
    let admin = kimetsu_bus::RosterAdmin::generate().unwrap();
    let server = iroh::SecretKey::generate();
    let client = iroh::SecretKey::generate();
    let stranger = iroh::SecretKey::generate();
    let server_entry = kimetsu_bus::RosterEndpoint::new(
        server.public(),
        &[kimetsu_core::memory::MountLevel::Repo],
        None::<&str>,
    )
    .unwrap();
    let client_entry = kimetsu_bus::RosterEndpoint::new(
        client.public(),
        &[kimetsu_core::memory::MountLevel::Repo],
        None::<&str>,
    )
    .unwrap();
    let document = admin.sign("f3-wan", &[server_entry, client_entry]).unwrap();
    let roster = kimetsu_bus::SignedRoster::open(&document, &admin.public_key_hex()).unwrap();
    assert!(roster.allows(client.public()));
    assert!(!roster.allows(stranger.public()));
    let mut tampered: serde_json::Value = serde_json::from_str(&document).unwrap();
    tampered["signature"] = serde_json::Value::String("00".repeat(64));
    let err = kimetsu_bus::SignedRoster::open(&tampered.to_string(), &admin.public_key_hex())
        .unwrap_err();
    assert!(!err.is_empty());
}

fn lan_handoff() -> PublicHandoff {
    PublicHandoff {
        admin_public_hex: "ab".repeat(32),
        roster_json: r#"{"kimetsu_roster":1,"group":"f3-two-host"}"#.into(),
        server_endpoint_id: "endpoint-a".into(),
        server_ip: "192.168.1.10".into(),
        server_port: 43111,
        digest: "cd".repeat(32),
        server: facts("pc-a", "machine-a"),
        mode: HandoffMode::Lan,
        bind_ip: "192.168.1.10".into(),
        bind_port: 43111,
    }
}

fn xor_mapped_response(mapped: SocketAddrV4, transaction_id: &[u8; 12]) -> Vec<u8> {
    let magic = 0x2112_A442u32;
    let xport = mapped.port() ^ ((magic >> 16) as u16);
    let xaddr = u32::from(*mapped.ip()) ^ magic;
    let mut attr = vec![0x00, 0x20, 0x00, 0x08, 0x00, 0x01];
    attr.extend_from_slice(&xport.to_be_bytes());
    attr.extend_from_slice(&xaddr.to_be_bytes());
    let mut msg = vec![0x01, 0x01];
    msg.extend_from_slice(&(attr.len() as u16).to_be_bytes());
    msg.extend_from_slice(&magic.to_be_bytes());
    msg.extend_from_slice(transaction_id);
    msg.extend_from_slice(&attr);
    msg
}
