//! Team-protocol frames.
//!
//! Legal frames are hello, announce, request_pack, pack, reject,
//! roster_sync, and roster. Any other type byte closes the connection.
//! Repository clone, fetch, checkout, and worktree transfer are not frames.
//! `roster` carries one admin-signed `kimetsu_roster` document. It is not a
//! memory pack and not a directory-sync batch.

use std::fmt;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// ALPN for the memory bus. QUIC rejects a peer that does not offer it.
pub const ALPN: &[u8] = b"kimetsu-bus/2";

/// Names of the only frames this protocol defines.
pub const LEGAL_FRAME_NAMES: &[&str] = &[
    "hello",
    "announce",
    "request_pack",
    "pack",
    "reject",
    "roster_sync",
    "roster",
];

pub const FRAME_HELLO: u8 = 1;
pub const FRAME_ANNOUNCE: u8 = 2;
pub const FRAME_REQUEST_PACK: u8 = 3;
pub const FRAME_PACK: u8 = 4;
pub const FRAME_REJECT: u8 = 5;
pub const FRAME_ROSTER_SYNC: u8 = 6;
pub const FRAME_ROSTER: u8 = 7;

const MAX_CONTROL_FRAME: usize = 16 * 1024;
/// One signed roster. Larger than a control frame, far below a pack.
pub const MAX_ROSTER_BYTES: usize = 64 * 1024;
/// Above the onboarding-sized budget, below a Remote checkpoint.
pub const MAX_PACK_BYTES: usize = 4 * 1024 * 1024;
const MAX_TTL_SECS: u64 = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    Hello,
    Announce,
    RequestPack,
    Pack,
    Reject,
    RosterSync,
    Roster,
}

impl FrameKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Hello => "hello",
            Self::Announce => "announce",
            Self::RequestPack => "request_pack",
            Self::Pack => "pack",
            Self::Reject => "reject",
            Self::RosterSync => "roster_sync",
            Self::Roster => "roster",
        }
    }

    pub fn type_byte(self) -> u8 {
        match self {
            Self::Hello => FRAME_HELLO,
            Self::Announce => FRAME_ANNOUNCE,
            Self::RequestPack => FRAME_REQUEST_PACK,
            Self::Pack => FRAME_PACK,
            Self::Reject => FRAME_REJECT,
            Self::RosterSync => FRAME_ROSTER_SYNC,
            Self::Roster => FRAME_ROSTER,
        }
    }
}

/// `None` means the type is not a team-protocol frame. Callers drop the peer.
pub fn parse_frame_kind(kind: u8) -> Option<FrameKind> {
    Some(match kind {
        FRAME_HELLO => FrameKind::Hello,
        FRAME_ANNOUNCE => FrameKind::Announce,
        FRAME_REQUEST_PACK => FrameKind::RequestPack,
        FRAME_PACK => FrameKind::Pack,
        FRAME_REJECT => FrameKind::Reject,
        FRAME_ROSTER_SYNC => FrameKind::RosterSync,
        FRAME_ROSTER => FrameKind::Roster,
        _ => return None,
    })
}

/// mDNS user-data. Endpoint id is already the discovery name. This string
/// is only the roster id, never memory text.
pub fn discovery_user_data(roster_id: &str) -> String {
    format!("r:{roster_id}")
}

#[derive(Debug)]
pub enum FrameError {
    Io(String),
    Invalid(String),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(message) | Self::Invalid(message) => f.write_str(message),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hello {
    pub roster_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Announcement {
    pub endpoint_id: String,
    pub digest: String,
    pub byte_length: u64,
    pub scopes: Vec<String>,
    pub signer: String,
    pub ttl_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reject {
    pub reason: String,
}

impl Announcement {
    pub fn validate(&self) -> Result<(), String> {
        if !is_hex_64(&self.digest) {
            return Err("announcement digest is not 64 hex chars".into());
        }
        if !is_hex_64(&self.signer) {
            return Err("announcement signer is not 64 hex chars".into());
        }
        if self.endpoint_id.is_empty() || self.endpoint_id.len() > 128 {
            return Err("announcement endpoint id is empty or too long".into());
        }
        if self.scopes.is_empty() || self.scopes.len() > 6 {
            return Err("announcement scopes are empty or too long".into());
        }
        for scope in &self.scopes {
            if !matches!(
                scope.as_str(),
                "run" | "session" | "repo" | "org" | "user" | "skill"
            ) {
                return Err(format!("announcement scope {scope} is not a mount"));
            }
        }
        if self.byte_length == 0 || self.byte_length as usize > MAX_PACK_BYTES {
            return Err("announcement byte length is outside the pack budget".into());
        }
        if self.ttl_secs == 0 || self.ttl_secs > MAX_TTL_SECS {
            return Err("announcement ttl is outside 1..=300 seconds".into());
        }
        Ok(())
    }
}

pub fn encode_hello(roster_id: &str) -> Result<Vec<u8>, String> {
    if !is_hex_64(roster_id) {
        return Err("roster id is not 64 hex chars".into());
    }
    serde_json::to_vec(&Hello {
        roster_id: roster_id.to_string(),
    })
    .map_err(|err| err.to_string())
}

pub fn decode_hello(bytes: &[u8]) -> Result<Hello, String> {
    let hello: Hello = serde_json::from_slice(bytes).map_err(|err| err.to_string())?;
    if !is_hex_64(&hello.roster_id) {
        return Err("roster id is not 64 hex chars".into());
    }
    Ok(hello)
}

pub fn encode_announcements(items: &[Announcement]) -> Result<Vec<u8>, String> {
    for item in items {
        item.validate()?;
    }
    serde_json::to_vec(items).map_err(|err| err.to_string())
}

pub fn decode_announcements(bytes: &[u8]) -> Result<Vec<Announcement>, String> {
    let items: Vec<Announcement> =
        serde_json::from_slice(bytes).map_err(|err| format!("announcement rejected: {err}"))?;
    for item in &items {
        item.validate()?;
    }
    Ok(items)
}

pub fn encode_reject(reason: &str) -> Result<Vec<u8>, String> {
    let reason = reason.chars().take(180).collect::<String>();
    serde_json::to_vec(&Reject { reason }).map_err(|err| err.to_string())
}

pub fn decode_reject(bytes: &[u8]) -> Result<Reject, String> {
    serde_json::from_slice(bytes).map_err(|err| err.to_string())
}

pub fn encode_digest(digest: &str) -> Result<Vec<u8>, String> {
    if !is_hex_64(digest) {
        return Err("digest is not 64 hex chars".into());
    }
    Ok(digest.as_bytes().to_vec())
}

pub fn decode_digest(bytes: &[u8]) -> Result<String, String> {
    let digest = std::str::from_utf8(bytes).map_err(|_| "digest is not utf-8".to_string())?;
    if !is_hex_64(digest) {
        return Err("digest is not 64 hex chars".into());
    }
    Ok(digest.to_string())
}

/// Pack frame body: 64 ASCII hex chars of the digest, then the gzip bytes.
pub fn encode_pack_body(digest: &str, gzip: &[u8]) -> Result<Vec<u8>, String> {
    if gzip.len() > MAX_PACK_BYTES {
        return Err("pack exceeds the bus byte budget".into());
    }
    let mut body = encode_digest(digest)?;
    body.extend_from_slice(gzip);
    Ok(body)
}

pub fn decode_pack_body(bytes: &[u8]) -> Result<(String, Vec<u8>), String> {
    if bytes.len() < 64 {
        return Err("pack frame is short".into());
    }
    let digest = decode_digest(&bytes[..64])?;
    let gzip = bytes[64..].to_vec();
    if gzip.len() > MAX_PACK_BYTES {
        return Err("pack exceeds the bus byte budget".into());
    }
    Ok((digest, gzip))
}

pub async fn write_frame(
    send: &mut (impl AsyncWrite + Unpin),
    kind: u8,
    body: &[u8],
) -> Result<(), String> {
    let limit = body_limit(kind);
    if body.len() > limit {
        return Err("frame exceeds its limit".into());
    }
    let len = u32::try_from(body.len()).map_err(|_| "frame is too large".to_string())?;
    let mut header = [0u8; 5];
    header[0] = kind;
    header[1..5].copy_from_slice(&len.to_be_bytes());
    send.write_all(&header)
        .await
        .map_err(|err| format!("frame write: {err}"))?;
    send.write_all(body)
        .await
        .map_err(|err| format!("frame write: {err}"))?;
    Ok(())
}

pub async fn read_frame(recv: &mut (impl AsyncRead + Unpin)) -> Result<(u8, Vec<u8>), FrameError> {
    let mut header = [0u8; 5];
    recv.read_exact(&mut header)
        .await
        .map_err(|err| FrameError::Io(format!("frame read: {err}")))?;
    let kind = header[0];
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    // Unknown type: read at most a control-sized body so a huge claim
    // cannot allocate, then the caller drops the peer.
    let limit = if parse_frame_kind(kind).is_some() {
        body_limit(kind)
    } else {
        MAX_CONTROL_FRAME
    };
    if len > limit {
        return Err(FrameError::Invalid("frame exceeds its limit".into()));
    }
    let mut body = vec![0u8; len];
    if len > 0 {
        recv.read_exact(&mut body)
            .await
            .map_err(|err| FrameError::Io(format!("frame read: {err}")))?;
    }
    Ok((kind, body))
}

fn body_limit(kind: u8) -> usize {
    if kind == FRAME_PACK {
        64 + MAX_PACK_BYTES
    } else if kind == FRAME_ROSTER {
        MAX_ROSTER_BYTES
    } else {
        MAX_CONTROL_FRAME
    }
}

fn is_hex_64(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legal_frames_have_no_repository_operation() {
        let banned = [
            "git_clone",
            "git_fetch",
            "git_checkout",
            "worktree",
            "checkout",
            "clone",
            "repos_file",
        ];
        let names = LEGAL_FRAME_NAMES.join(" ");
        for word in banned {
            assert!(
                !names.contains(word),
                "frame names must not define {word}: {names}"
            );
        }
        assert_eq!(LEGAL_FRAME_NAMES.len(), 7);
        for (name, kind) in LEGAL_FRAME_NAMES.iter().zip(1u8..=7) {
            let parsed = parse_frame_kind(kind).unwrap();
            assert_eq!(parsed.name(), *name);
            assert_eq!(parsed.type_byte(), kind);
        }
        assert!(parse_frame_kind(64).is_none());
        assert!(parse_frame_kind(0).is_none());
    }

    #[test]
    fn announcement_rejects_extra_fields() {
        let ok = Announcement {
            endpoint_id: "abc".into(),
            digest: "ab".repeat(32),
            byte_length: 12,
            scopes: vec!["repo".into()],
            signer: "cd".repeat(32),
            ttl_secs: 60,
        };
        ok.validate().unwrap();
        let bytes = encode_announcements(&[ok]).unwrap();
        decode_announcements(&bytes).unwrap();

        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        value[0]["worktree"] = serde_json::json!("/tmp/tree");
        let err = decode_announcements(&serde_json::to_vec(&value).unwrap()).unwrap_err();
        assert!(err.contains("announcement rejected"), "{err}");

        value[0].as_object_mut().unwrap().remove("worktree");
        value[0]["repository_url"] = serde_json::json!("https://example.invalid/repo.git");
        assert!(decode_announcements(&serde_json::to_vec(&value).unwrap()).is_err());

        value[0].as_object_mut().unwrap().remove("repository_url");
        value[0]["checkout"] = serde_json::json!("main");
        assert!(decode_announcements(&serde_json::to_vec(&value).unwrap()).is_err());
    }

    #[test]
    fn discovery_user_data_is_the_roster_id_only() {
        let id = "ab".repeat(32);
        let data = discovery_user_data(&id);
        assert_eq!(data, format!("r:{id}"));
        assert!(data.len() <= 245);
        assert!(!data.contains("memory"));
        assert!(!data.contains("lesson"));
    }
}
