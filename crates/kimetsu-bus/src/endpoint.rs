//! iroh endpoint: direct QUIC, mDNS, hole-punch, encrypted relay.
//!
//! Hole-punch is iroh's path selection. It runs when a relay is configured
//! and [`BusEndpoint::online`] has observed a relay connection. The relay
//! sees ciphertext. Pack bytes are verified with the F1 verifier and then
//! imported by [`kimetsu_brain::mount::import_signed_pack`].

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::presence::PresenceStore;
use crate::protocol::{self, Announcement, FrameError, FrameKind};
use crate::roster::SignedRoster;
use iroh::endpoint::presets;
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, SecretKey};
use iroh_mdns_address_lookup::MdnsAddressLookup;
use iroh_relay::tls::CaTlsConfig;
use kimetsu_brain::mount::{self, MountGrants};
use kimetsu_brain::packs::ImportSummary;
use kimetsu_brain::signed_pack::{self, PackManifest, PackSigner, TrustStore};
use kimetsu_core::memory::MountLevel;

/// Where this endpoint sends relayed ciphertext.
///
/// [`BusRelay::Disabled`] is the stock default: no public relay list.
#[derive(Debug, Clone)]
pub enum BusRelay {
    Disabled,
    /// Operator relay map. QUIC stays encrypted end to end.
    Custom(iroh::RelayMap),
}

impl BusRelay {
    fn mode(&self) -> RelayMode {
        match self {
            Self::Disabled => RelayMode::Disabled,
            Self::Custom(map) => RelayMode::Custom(map.clone()),
        }
    }
}

#[derive(Clone)]
pub struct EndpointConfig {
    pub secret_key: SecretKey,
    pub roster: SignedRoster,
    pub relay: BusRelay,
    /// Attach mDNS. The published user-data is `r:<roster id>` only.
    pub mdns: bool,
    /// Test and operator relays that use a private certificate.
    pub insecure_relay_tls: bool,
    pub presence_path: PathBuf,
}

pub struct PackRequest {
    pub peer: EndpointAddr,
    pub digest: String,
    pub destination: MountLevel,
    pub grants: MountGrants,
    pub trust: TrustStore,
    pub brain: PathBuf,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct BusStats {
    pub non_member_drops: u64,
    pub protocol_drops: u64,
}

struct StagedPack {
    gzip: Vec<u8>,
    announcement: Announcement,
}

struct Inner {
    roster: SignedRoster,
    packs: Mutex<BTreeMap<String, StagedPack>>,
    presence: PresenceStore,
    non_member_drops: AtomicU64,
    protocol_drops: AtomicU64,
    endpoint_id: EndpointId,
}

#[derive(Clone)]
struct Handler {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Handler {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("kimetsu bus handler")
    }
}

pub struct BusEndpoint {
    router: Router,
    inner: Arc<Inner>,
    presence_task: tokio::task::JoinHandle<()>,
}

impl BusEndpoint {
    pub async fn bind(config: EndpointConfig) -> Result<Self, String> {
        if !config.roster.allows(config.secret_key.public()) {
            return Err("local endpoint is not on the roster".into());
        }
        let user_data = protocol::discovery_user_data(config.roster.id());
        if user_data.len() > iroh::address_lookup::UserData::MAX_LENGTH {
            return Err("discovery user-data exceeds the mDNS budget".into());
        }
        let mut builder = Endpoint::builder(presets::Minimal)
            .secret_key(config.secret_key.clone())
            .relay_mode(config.relay.mode())
            .user_data_for_address_lookup(
                iroh::address_lookup::UserData::from_str(&user_data)
                    .map_err(|err| format!("discovery user-data: {err}"))?,
            );
        if config.mdns {
            builder =
                builder.address_lookup(MdnsAddressLookup::builder().service_name("kimetsuv1"));
        }
        if config.insecure_relay_tls {
            builder = builder.ca_tls_config(CaTlsConfig::insecure_skip_verify());
        }
        let endpoint = builder
            .bind()
            .await
            .map_err(|err| format!("bind endpoint: {err}"))?;
        let presence = PresenceStore::open(config.presence_path)?;
        let inner = Arc::new(Inner {
            roster: config.roster,
            packs: Mutex::new(BTreeMap::new()),
            presence: presence.clone(),
            non_member_drops: AtomicU64::new(0),
            protocol_drops: AtomicU64::new(0),
            endpoint_id: endpoint.id(),
        });
        let router = Router::builder(endpoint)
            .accept(
                protocol::ALPN,
                Handler {
                    inner: Arc::clone(&inner),
                },
            )
            .spawn();
        let expirer = presence.clone();
        let presence_task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(15)).await;
                let _ = expirer.expire();
            }
        });
        Ok(Self {
            router,
            inner,
            presence_task,
        })
    }

    pub fn id(&self) -> EndpointId {
        self.router.endpoint().id()
    }

    pub fn stats(&self) -> BusStats {
        BusStats {
            non_member_drops: self.inner.non_member_drops.load(Ordering::Relaxed),
            protocol_drops: self.inner.protocol_drops.load(Ordering::Relaxed),
        }
    }

    pub fn presence(&self) -> &PresenceStore {
        &self.inner.presence
    }

    /// Direct socket addresses for a same-machine or LAN dial.
    pub async fn direct_addr(&self) -> Result<EndpointAddr, String> {
        for _ in 0..50 {
            if let Ok(addr) = direct_addr(self.router.endpoint()) {
                return Ok(addr);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        direct_addr(self.router.endpoint())
    }

    /// Relay-only address. iroh hole-punches after the relay path is up.
    pub fn relay_addr(&self, relay: impl Into<iroh::RelayUrl>) -> EndpointAddr {
        EndpointAddr::new(self.id()).with_relay_url(relay.into())
    }

    /// Resolves when a configured relay is connected. Hole-punch needs that.
    /// Do not call this on [`BusRelay::Disabled`]: there is no relay to wait for,
    /// and the local store must not wait either.
    pub async fn online(&self) {
        self.router.endpoint().online().await;
    }

    pub fn address_lookup_enabled(&self) -> bool {
        self.router.endpoint().address_lookup().is_ok()
    }

    /// Publish a verified v2 pack. v1 and unsigned bytes are refused, so
    /// this process does not put them on the wire.
    pub fn publish_pack(&self, gzip: &[u8], signer: &PackSigner) -> Result<Announcement, String> {
        let json = signed_pack::decode_pack_bytes(gzip)?;
        let verified = signed_pack::verify_pack_json(&json)?;
        if verified.signer_hex != signer.public_key_hex() {
            return Err("pack signer does not match the publishing key".into());
        }
        if !self
            .inner
            .roster
            .may_publish_signer(self.inner.endpoint_id, &verified.signer_hex)
        {
            return Err("roster does not let this endpoint publish that signer".into());
        }
        let announcement = Announcement {
            endpoint_id: self.inner.endpoint_id.to_string(),
            digest: verified.digest_hex.clone(),
            byte_length: u64::try_from(gzip.len()).unwrap_or(u64::MAX),
            scopes: verified.scopes.clone(),
            signer: verified.signer_hex,
            ttl_secs: 60,
        };
        announcement.validate()?;
        self.inner.presence.record(announcement.clone())?;
        self.inner
            .packs
            .lock()
            .map_err(|_| "pack lock poisoned".to_string())?
            .insert(
                verified.digest_hex,
                StagedPack {
                    gzip: gzip.to_vec(),
                    announcement: announcement.clone(),
                },
            );
        Ok(announcement)
    }

    /// Build a v2 pack from rows and publish it. Signing happens before the
    /// bytes are eligible for the wire.
    pub fn publish_memories(
        &self,
        manifest: &PackManifest,
        scopes: &[String],
        memories: &[kimetsu_brain::packs::MemoryExport],
        signer: &PackSigner,
    ) -> Result<Announcement, String> {
        let json = signer.seal_json(manifest, scopes, memories)?;
        let gzip = signed_pack::encode_gzip(&json)?;
        self.publish_pack(&gzip, signer)
    }

    /// Hello plus announcements. Does not import.
    pub async fn exchange_announcements(
        &self,
        peer: EndpointAddr,
    ) -> Result<Vec<Announcement>, String> {
        self.ensure_member(peer.id)?;
        let dialed = self.round_trip(peer, None).await?;
        Ok(dialed.announcements)
    }

    /// Dial a roster member, require an announcement for `digest`, verify,
    /// then [`mount::import_signed_pack`].
    pub async fn request_pack(&self, request: PackRequest) -> Result<ImportSummary, String> {
        self.ensure_member(request.peer.id)?;
        if !self
            .inner
            .roster
            .may_use_mount(self.inner.endpoint_id, request.destination)
        {
            return Err(format!(
                "roster does not let this endpoint request {}",
                request.destination
            ));
        }
        if !self
            .inner
            .roster
            .may_use_mount(request.peer.id, request.destination)
        {
            return Err(format!(
                "roster does not let the peer serve {}",
                request.destination
            ));
        }
        if !request.grants.can_write(request.destination) {
            return Err(format!("import requires write on {}", request.destination));
        }
        let digest = request.digest.trim().to_ascii_lowercase();
        if digest.len() != 64 {
            return Err("digest is not 64 hex chars".into());
        }
        let peer_id = request.peer.id;
        let dialed = self.round_trip(request.peer, Some(digest.clone())).await?;
        let announced = dialed
            .announcements
            .iter()
            .find(|item| item.digest == digest);
        let Some(announced) = announced else {
            return Err("peer did not announce that digest".into());
        };
        let Some(gzip) = dialed.pack else {
            return Err("peer sent no pack".into());
        };
        // Verify before any store call. A signer the roster did not publish
        // still does not reach import_signed_pack (that would quarantine).
        let json = signed_pack::decode_pack_bytes(&gzip)?;
        let verified = signed_pack::verify_pack_json(&json)?;
        if verified.digest_hex != digest {
            return Err("pack digest does not match the announcement".into());
        }
        if verified.signer_hex != announced.signer.to_ascii_lowercase() {
            return Err("pack signer does not match the announcement".into());
        }
        if !self
            .inner
            .roster
            .may_publish_signer(peer_id, &verified.signer_hex)
        {
            return Err("pack signer is not published by that endpoint".into());
        }
        // Signature and scope checks finish inside import_signed_pack, before SQL.
        mount::import_signed_pack(
            &request.brain,
            &request.grants,
            &gzip,
            request.destination,
            false,
            false,
            &request.trust,
        )
        .map_err(|err| err.to_string())
    }

    pub async fn close(self) {
        self.presence_task.abort();
        let _ = self.router.shutdown().await;
        self.router.endpoint().close().await;
    }

    fn ensure_member(&self, endpoint_id: EndpointId) -> Result<(), String> {
        if !self.inner.roster.allows(endpoint_id) {
            return Err("non-member handshake refused before dial".into());
        }
        Ok(())
    }

    async fn round_trip(
        &self,
        peer: EndpointAddr,
        digest: Option<String>,
    ) -> Result<Dialed, String> {
        let endpoint = self.router.endpoint();
        let connection = endpoint
            .connect(peer, protocol::ALPN)
            .await
            .map_err(|err| format!("dial: {err}"))?;
        let (mut send, mut recv) = connection
            .open_bi()
            .await
            .map_err(|err| format!("open stream: {err}"))?;
        let hello = protocol::encode_hello(self.inner.roster.id())?;
        protocol::write_frame(&mut send, protocol::FRAME_HELLO, &hello).await?;

        let (kind, body) = read_peer(&mut recv).await?;
        if kind != FrameKind::Hello {
            connection.close(2u32.into(), b"dropped");
            return Err("peer did not hello".into());
        }
        let remote_hello = protocol::decode_hello(&body)?;
        if remote_hello.roster_id != self.inner.roster.id() {
            connection.close(1u32.into(), b"non-member");
            return Err("peer roster id does not match".into());
        }

        let (kind, body) = read_peer(&mut recv).await?;
        if kind != FrameKind::Announce {
            connection.close(2u32.into(), b"dropped");
            return Err("peer did not announce".into());
        }
        let announcements = protocol::decode_announcements(&body).inspect_err(|_| {
            connection.close(2u32.into(), b"dropped");
        })?;
        for announcement in &announcements {
            // Presence only. No import.
            self.inner.presence.record(announcement.clone())?;
        }

        let mut payload = None;
        if let Some(digest) = digest {
            let request = protocol::encode_digest(&digest)?;
            protocol::write_frame(&mut send, protocol::FRAME_REQUEST_PACK, &request).await?;
            let (kind, body) = read_peer(&mut recv).await?;
            match kind {
                FrameKind::Pack => {
                    let (got, gzip) = protocol::decode_pack_body(&body)?;
                    if got.to_ascii_lowercase() != digest {
                        return Err("pack frame digest does not match the request".into());
                    }
                    payload = Some(gzip);
                }
                FrameKind::Reject => {
                    let reject = protocol::decode_reject(&body)?;
                    return Err(format!("peer rejected request_pack: {}", reject.reason));
                }
                _ => {
                    connection.close(2u32.into(), b"dropped");
                    return Err("peer response is not a pack".into());
                }
            }
        }
        send.finish().map_err(|err| format!("finish: {err}"))?;
        Ok(Dialed {
            announcements,
            pack: payload,
        })
    }
}

struct Dialed {
    announcements: Vec<Announcement>,
    pack: Option<Vec<u8>>,
}

fn direct_addr(endpoint: &Endpoint) -> Result<EndpointAddr, String> {
    let sockets = endpoint.bound_sockets();
    let ip = sockets
        .iter()
        .copied()
        .find(|socket| socket.is_ipv4())
        .or_else(|| sockets.first().copied())
        .ok_or_else(|| "endpoint has no bound socket".to_string())?;
    Ok(EndpointAddr::new(endpoint.id()).with_ip_addr(dialable(ip)))
}

/// `0.0.0.0` / `::` is a bind address. Dial loopback with the same port.
fn dialable(socket: std::net::SocketAddr) -> std::net::SocketAddr {
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
    match socket {
        SocketAddr::V4(addr) if addr.ip().is_unspecified() => {
            SocketAddr::from((Ipv4Addr::LOCALHOST, addr.port()))
        }
        SocketAddr::V6(addr) if addr.ip().is_unspecified() => {
            SocketAddr::from((Ipv6Addr::LOCALHOST, addr.port()))
        }
        other => other,
    }
}

async fn read_peer(
    recv: &mut (impl tokio::io::AsyncRead + Unpin),
) -> Result<(FrameKind, Vec<u8>), String> {
    let (kind, body) = protocol::read_frame(recv)
        .await
        .map_err(|err| err.to_string())?;
    let Some(kind) = protocol::parse_frame_kind(kind) else {
        return Err("peer sent a frame this protocol does not define".into());
    };
    Ok((kind, body))
}

impl ProtocolHandler for Handler {
    async fn accept(&self, connection: iroh::endpoint::Connection) -> Result<(), AcceptError> {
        if let Err(class) = self.serve(&connection).await {
            match class {
                ServeClass::NonMember => {
                    self.inner.non_member_drops.fetch_add(1, Ordering::Relaxed);
                    connection.close(1u32.into(), b"non-member");
                }
                ServeClass::Protocol => {
                    self.inner.protocol_drops.fetch_add(1, Ordering::Relaxed);
                    connection.close(2u32.into(), b"dropped");
                }
                ServeClass::Done => {}
            }
        }
        Ok(())
    }
}

enum ServeClass {
    NonMember,
    Protocol,
    Done,
}

impl Handler {
    async fn serve(&self, connection: &iroh::endpoint::Connection) -> Result<(), ServeClass> {
        let remote = connection.remote_id();
        if !self.inner.roster.allows(remote) {
            return Err(ServeClass::NonMember);
        }
        let (mut send, mut recv) = connection.accept_bi().await.map_err(|_| ServeClass::Done)?;
        let (kind, body) = match protocol::read_frame(&mut recv).await {
            Ok(frame) => frame,
            Err(FrameError::Io(_)) => return Err(ServeClass::Done),
            Err(FrameError::Invalid(_)) => return Err(ServeClass::Protocol),
        };
        let Some(kind) = protocol::parse_frame_kind(kind) else {
            return Err(ServeClass::Protocol);
        };
        if kind != FrameKind::Hello {
            return Err(ServeClass::Protocol);
        }
        let hello = protocol::decode_hello(&body).map_err(|_| ServeClass::Protocol)?;
        if hello.roster_id != self.inner.roster.id() {
            return Err(ServeClass::NonMember);
        }
        let response =
            protocol::encode_hello(self.inner.roster.id()).map_err(|_| ServeClass::Done)?;
        protocol::write_frame(&mut send, protocol::FRAME_HELLO, &response)
            .await
            .map_err(|_| ServeClass::Done)?;

        let announcements = {
            let guard = self.inner.packs.lock().map_err(|_| ServeClass::Done)?;
            guard
                .values()
                .map(|staged| staged.announcement.clone())
                .collect::<Vec<_>>()
        };
        let encoded =
            protocol::encode_announcements(&announcements).map_err(|_| ServeClass::Done)?;
        protocol::write_frame(&mut send, protocol::FRAME_ANNOUNCE, &encoded)
            .await
            .map_err(|_| ServeClass::Done)?;

        let (kind, body) = match protocol::read_frame(&mut recv).await {
            Ok(frame) => frame,
            Err(FrameError::Io(_)) => return Ok(()),
            Err(FrameError::Invalid(_)) => return Err(ServeClass::Protocol),
        };
        let Some(kind) = protocol::parse_frame_kind(kind) else {
            return Err(ServeClass::Protocol);
        };
        if kind != FrameKind::RequestPack {
            return Err(ServeClass::Protocol);
        }
        let digest = protocol::decode_digest(&body).map_err(|_| ServeClass::Protocol)?;
        let gzip = {
            let guard = self.inner.packs.lock().map_err(|_| ServeClass::Done)?;
            guard.get(&digest).map(|staged| staged.gzip.clone())
        };
        if let Some(gzip) = gzip {
            // Refuse to transmit anything publish_pack did not verify as v2.
            if signed_pack::decode_pack_bytes(&gzip)
                .ok()
                .and_then(|json| signed_pack::verify_pack_json(&json).ok())
                .is_none()
            {
                let reject = protocol::encode_reject("only kimetsu_pack 2 is accepted on the bus")
                    .map_err(|_| ServeClass::Done)?;
                protocol::write_frame(&mut send, protocol::FRAME_REJECT, &reject)
                    .await
                    .map_err(|_| ServeClass::Done)?;
                return Ok(());
            }
            let body = protocol::encode_pack_body(&digest, &gzip).map_err(|_| ServeClass::Done)?;
            protocol::write_frame(&mut send, protocol::FRAME_PACK, &body)
                .await
                .map_err(|_| ServeClass::Done)?;
        } else {
            let reject =
                protocol::encode_reject("digest is not announced").map_err(|_| ServeClass::Done)?;
            protocol::write_frame(&mut send, protocol::FRAME_REJECT, &reject)
                .await
                .map_err(|_| ServeClass::Done)?;
        }
        let _ = send.finish();
        let _ = recv.read_to_end(1).await;
        Ok(())
    }
}
