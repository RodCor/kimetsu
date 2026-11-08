//! Contract for the two-physical-host measurement harness.
//!
//! The implementation lives next to the example so the veth path and this
//! path share one report and address policy. These tests do not open a socket
//! and they do not produce a §7 number.

#[path = "../examples/f3_hotpath/policy.rs"]
mod policy;

use std::net::Ipv4Addr;

use policy::{
    EndpointPolicy, HostFacts, PublicHandoff, ReportInput, ReportKind, build_handoff,
    parse_public_handoff, reject_false_lan_label, render_report, require_mode, validate_ip,
    validate_pair, validate_release_profile, validate_two_host_claim,
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
