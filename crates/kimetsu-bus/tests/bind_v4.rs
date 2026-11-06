//! `EndpointConfig::bind_v4` must bind that socket and advertise it.
//!
//! The default endpoint binds `0.0.0.0` and `direct_addr` rewrites that to
//! `127.0.0.1`. A peer on another interface cannot dial that rewrite.

use std::net::{IpAddr, Ipv4Addr, SocketAddrV4};
use std::time::{SystemTime, UNIX_EPOCH};

use iroh::EndpointAddr;
use kimetsu_brain::packs::MemoryExport;
use kimetsu_brain::signed_pack::{PackManifest, PackSigner};
use kimetsu_bus::{
    BusEndpoint, BusRelay, EndpointConfig, RosterAdmin, RosterEndpoint, SignedRoster,
};
use kimetsu_core::memory::MountLevel;

fn presence_path(label: &str) -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "kimetsu-bind-v4-{label}-{}-{nanos}.json",
        std::process::id()
    ))
}

fn roster_for(
    server: &iroh::SecretKey,
    client: &iroh::SecretKey,
    signer: &PackSigner,
) -> (SignedRoster, String) {
    let admin = RosterAdmin::generate().unwrap();
    let server_entry = RosterEndpoint::new(
        server.public(),
        &[MountLevel::Repo],
        [signer.public_key_hex()],
    )
    .unwrap();
    let client_entry =
        RosterEndpoint::new(client.public(), &[MountLevel::Repo], None::<&str>).unwrap();
    let document = admin
        .sign("bind-v4", &[server_entry, client_entry])
        .unwrap();
    let admin_hex = admin.public_key_hex();
    let roster = SignedRoster::open(&document, &admin_hex).unwrap();
    (roster, admin_hex)
}

async fn bind(
    secret: iroh::SecretKey,
    roster: SignedRoster,
    label: &str,
    bind_v4: Option<SocketAddrV4>,
) -> BusEndpoint {
    BusEndpoint::bind(EndpointConfig {
        secret_key: secret,
        roster,
        relay: BusRelay::Disabled,
        mdns: false,
        insecure_relay_tls: false,
        presence_path: presence_path(label),
        roster_path: None,
        bind_v4,
    })
    .await
    .unwrap_or_else(|err| panic!("bind {label}: {err}"))
}

#[tokio::test]
async fn bind_v4_advertises_the_specific_socket_not_unspecified() {
    let server_key = iroh::SecretKey::generate();
    let client_key = iroh::SecretKey::generate();
    let signer = PackSigner::generate().unwrap();
    let (roster, _) = roster_for(&server_key, &client_key, &signer);
    let endpoint = bind(
        server_key,
        roster,
        "specific",
        Some(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)),
    )
    .await;

    let bound = endpoint.bound_sockets();
    assert!(
        bound
            .iter()
            .any(|socket| socket.ip() == IpAddr::V4(Ipv4Addr::LOCALHOST)),
        "bound sockets: {bound:?}"
    );
    assert!(
        bound.iter().all(|socket| !socket.ip().is_unspecified()),
        "bind_v4 must not also bind an unspecified address: {bound:?}"
    );
    let advertised = endpoint.direct_addr().await.unwrap();
    let ips: Vec<_> = advertised.ip_addrs().copied().collect();
    assert_eq!(ips.len(), 1, "{advertised:?}");
    assert_eq!(ips[0].ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
    assert_ne!(ips[0].port(), 0);
    assert_eq!(
        ips[0].port(),
        bound.iter().find(|socket| socket.is_ipv4()).unwrap().port()
    );
    endpoint.close().await;
}

#[tokio::test]
async fn explicit_loopback_bind_still_fetches_a_verified_pack() {
    let server_key = iroh::SecretKey::generate();
    let client_key = iroh::SecretKey::generate();
    let signer = PackSigner::generate().unwrap();
    let (roster, _) = roster_for(&server_key, &client_key, &signer);
    let server = bind(
        server_key,
        roster.clone(),
        "fetch-server",
        Some(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)),
    )
    .await;
    let client = bind(
        client_key,
        roster,
        "fetch-client",
        Some(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)),
    )
    .await;
    let memories = vec![MemoryExport {
        text: "bind v4 lesson".into(),
        scope: "repo".into(),
        kind: "fact".into(),
        confidence: 0.9,
        created_at: Some("2026-01-01T00:00:00Z".into()),
    }];
    let manifest = PackManifest {
        name: Some("bind-v4".into()),
        version: Some("1".into()),
        description: Some("specific bind".into()),
        exported_at: Some("2026-01-01T00:00:00Z".into()),
    };
    let announcement = server
        .publish_memories(&manifest, &["repo".into()], &memories, &signer)
        .unwrap();
    let addr: EndpointAddr = server.direct_addr().await.unwrap();
    let dialed: Vec<_> = addr.ip_addrs().copied().collect();
    assert_eq!(dialed[0].ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
    let fetched = client
        .fetch_verified(addr, &announcement.digest)
        .await
        .unwrap();
    assert_eq!(fetched.digest_hex, announcement.digest);
    assert!(!fetched.gzip.is_empty());
    server.close().await;
    client.close().await;
}
