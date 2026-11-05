//! ADR 0001 F1 signed packs (`kimetsu_pack` 2).
//!
//! Canonical JSON profile **`kimetsu-canon-json/1`** (this is the F1 pin for
//! open question 1; it is not RFC 8785):
//!
//! 1. UTF-8.
//! 2. Objects have keys sorted by raw UTF-8 bytes, no duplicate keys, and
//!    separators `:` and `,` with no insignificant whitespace.
//! 3. Arrays keep their order.
//! 4. Strings use serde_json 1 compact escaping: `"` and `\` escaped,
//!    U+0000–U+001F as `\u00xx` lowercase hex, solidus not escaped, other
//!    non-ASCII emitted as UTF-8.
//! 5. Numbers keep the lexical form `serde_json::Number` already has. There
//!    is no ECMAScript number rewrite.
//! 6. Literals are `null`, `true`, and `false`.
//!
//! The canonical body is that encoding of the pack object **without**
//! `signer` and `signature`. BLAKE3-256 of those bytes is the content hash,
//! written as 64 lowercase hex characters.
//!
//! The Ed25519 message is the same profile applied to
//! `{"digest":"<hex>","scopes":[...]}`. `scopes` in that message is the
//! body's `scopes` array, in the same order. Inequality is a failed
//! signature. The signature is RFC 8032 Ed25519 over those raw UTF-8 bytes,
//! checked with ed25519-dalek `verify_strict` (non-canonical signatures are
//! rejected). It is not Ed25519ph and not Ed25519ctx. `signer` is the 32-byte
//! public key as lowercase hex. `signature` is the 64-byte signature as
//! lowercase hex. Gzip is outside the digest.
//!
//! Signed `scopes` are a strictly sorted unique list drawn only from `run`,
//! `session`, `repo`, `org`, `user`, and `skill`. Export never emits `run` or
//! `session`. The bus is not part of this module: no sockets, no iroh.

use std::fs;
use std::path::{Path, PathBuf};

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use kimetsu_core::memory::{MountLevel, mount_of_stored};
use serde_json::{Map, Value};

use crate::packs::MemoryExport;

/// Profile name pinned by F1. Tests and the pack bytes both follow it.
pub const CANONICAL_JSON_PROFILE: &str = "kimetsu-canon-json/1";

/// Ed25519 message profile pinned by F1.
pub const ED25519_PROFILE: &str = "ed25519-rfc8032-verify-strict";

const LEGAL_SCOPES: &[&str] = &["org", "repo", "run", "session", "skill", "user"];

/// Optional manifest fields. Absent fields are omitted from the canonical body.
#[derive(Debug, Clone, Default)]
pub struct PackManifest {
    pub name: Option<String>,
    pub version: Option<String>,
    pub description: Option<String>,
    pub exported_at: Option<String>,
}

/// A `kimetsu_pack` 2 document whose signature and scope list have been checked.
/// Entry-versus-destination checks happen in [`reject_scope_mismatch`], still
/// before any SQLite call.
#[derive(Debug, Clone)]
pub struct VerifiedPack {
    pub name: Option<String>,
    pub version: Option<String>,
    pub description: Option<String>,
    pub scopes: Vec<String>,
    pub memories: Vec<MemoryExport>,
    pub signer_hex: String,
    pub canonical_body: Vec<u8>,
    pub digest_hex: String,
}

/// Ed25519 pack signer. The secret is 32 bytes.
pub struct PackSigner {
    signing: SigningKey,
}

impl PackSigner {
    pub fn from_secret_bytes(secret: [u8; 32]) -> Self {
        Self {
            signing: SigningKey::from_bytes(&secret),
        }
    }

    pub fn from_secret_hex(hex_str: &str) -> Result<Self, String> {
        let bytes = decode_hex(hex_str.trim())?;
        let secret: [u8; 32] = bytes
            .try_into()
            .map_err(|_| "signer secret must be 32 bytes (64 hex chars)".to_string())?;
        Ok(Self::from_secret_bytes(secret))
    }

    /// Read a 64-hex secret or `{"kimetsu_signer":1,"secret_key":"<hex>"}`.
    pub fn load(path: &Path) -> Result<Self, String> {
        let raw =
            fs::read_to_string(path).map_err(|e| format!("sign key {}: {e}", path.display()))?;
        let trimmed = raw.trim();
        if trimmed.starts_with('{') {
            let value: Value = serde_json::from_str(trimmed)
                .map_err(|e| format!("sign key {}: {e}", path.display()))?;
            let hex_str = value
                .get("secret_key")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("sign key {} is missing secret_key", path.display()))?;
            Self::from_secret_hex(hex_str)
        } else {
            Self::from_secret_hex(trimmed)
        }
    }

    pub fn generate() -> Result<Self, String> {
        let mut secret = [0u8; 32];
        getrandom::fill(&mut secret).map_err(|e| format!("signer key generation failed: {e}"))?;
        Ok(Self::from_secret_bytes(secret))
    }

    pub fn public_key_hex(&self) -> String {
        encode_hex(self.signing.verifying_key().as_bytes())
    }

    pub fn secret_key_hex(&self) -> String {
        encode_hex(self.signing.to_bytes().as_ref())
    }

    /// Sign `memories` under `scopes` as they are given. Callers that export
    /// must pass a strictly sorted unique scope list; this function does not
    /// reorder, so a test can build an illegal pack the verifier must reject.
    pub fn seal_json(
        &self,
        manifest: &PackManifest,
        scopes: &[String],
        memories: &[MemoryExport],
    ) -> Result<String, String> {
        let body = pack_body(manifest, scopes, memories)?;
        let canonical = canonical_json(&body);
        let digest_hex = blake3::hash(&canonical).to_hex().to_string();
        let message = signed_message_bytes(&digest_hex, scopes);
        let embedded = scopes_in_canonical(&canonical)?;
        if embedded != scopes {
            return Err("body scopes != signed scopes".into());
        }
        let signature = self.signing.sign(&message);
        let mut full = match body {
            Value::Object(map) => map,
            _ => return Err("pack body must be an object".into()),
        };
        full.insert("signer".into(), Value::String(self.public_key_hex()));
        full.insert(
            "signature".into(),
            Value::String(encode_hex(signature.to_bytes().as_ref())),
        );
        let bytes = canonical_json(&Value::Object(full));
        String::from_utf8(bytes).map_err(|e| e.to_string())
    }
}

/// Operator-edited allowlist. A key is trusted only for the mounts listed.
/// First sight of a key is not trust. An empty store quarantines every signer.
#[derive(Debug, Clone, Default)]
pub struct TrustStore {
    signers: Vec<TrustedSigner>,
}

#[derive(Debug, Clone)]
struct TrustedSigner {
    public_key: [u8; 32],
    mounts: Vec<MountLevel>,
}

impl TrustStore {
    pub fn empty() -> Self {
        Self {
            signers: Vec::new(),
        }
    }

    pub fn allow(&mut self, public_key_hex: &str, mounts: &[MountLevel]) -> Result<(), String> {
        let public_key = parse_public_key(public_key_hex)?;
        if let Some(existing) = self
            .signers
            .iter_mut()
            .find(|signer| signer.public_key == public_key)
        {
            for mount in mounts {
                if !existing.mounts.contains(mount) {
                    existing.mounts.push(*mount);
                }
            }
        } else {
            self.signers.push(TrustedSigner {
                public_key,
                mounts: mounts.to_vec(),
            });
        }
        Ok(())
    }

    pub fn trusts(&self, public_key_hex: &str, mount: MountLevel) -> bool {
        let Ok(public_key) = parse_public_key(public_key_hex) else {
            return false;
        };
        self.signers
            .iter()
            .any(|signer| signer.public_key == public_key && signer.mounts.contains(&mount))
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let raw =
            fs::read_to_string(path).map_err(|e| format!("trust store {}: {e}", path.display()))?;
        Self::parse(&raw)
    }

    pub fn parse(raw: &str) -> Result<Self, String> {
        let value: Value = serde_json::from_str(raw).map_err(|e| format!("trust store: {e}"))?;
        let version = value
            .get("kimetsu_trust")
            .and_then(Value::as_u64)
            .ok_or("trust store is missing kimetsu_trust")?;
        if version != 1 {
            return Err(format!("unknown kimetsu_trust version {version}"));
        }
        let entries = value
            .get("signers")
            .and_then(Value::as_array)
            .ok_or("trust store is missing signers")?;
        let mut store = Self::empty();
        for entry in entries {
            let public_key = entry
                .get("public_key")
                .and_then(Value::as_str)
                .ok_or("trust store signer is missing public_key")?;
            let mounts = entry
                .get("mounts")
                .and_then(Value::as_array)
                .ok_or("trust store signer is missing mounts")?;
            let mut levels = Vec::new();
            for mount in mounts {
                let name = mount.as_str().ok_or("trust store mount must be a string")?;
                let level: MountLevel = name
                    .parse()
                    .map_err(|_| format!("trust store has unknown mount {name}"))?;
                levels.push(level);
            }
            store.allow(public_key, &levels)?;
        }
        Ok(store)
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("trust store {}: {e}", path.display()))?;
            }
        }
        let mut signers = Vec::new();
        for signer in &self.signers {
            let mut mounts: Vec<&str> = signer.mounts.iter().map(|level| level.as_str()).collect();
            mounts.sort_unstable();
            mounts.dedup();
            signers.push(serde_json::json!({
                "public_key": encode_hex(&signer.public_key),
                "mounts": mounts,
            }));
        }
        let body = serde_json::json!({
            "kimetsu_trust": 1,
            "signers": signers,
        });
        let text = serde_json::to_string_pretty(&body).map_err(|e| e.to_string())?;
        fs::write(path, text).map_err(|e| format!("trust store {}: {e}", path.display()))
    }
}

/// `~/.kimetsu/trust.json`, or `$KIMETSU_TRUST_STORE` when that variable is set.
/// A missing default file is an empty store. A set override that cannot be read
/// is an error.
pub fn load_default_trust_store() -> Result<TrustStore, String> {
    if let Some(path) = std::env::var_os("KIMETSU_TRUST_STORE") {
        let path = PathBuf::from(path);
        if path.as_os_str().is_empty() {
            return Ok(TrustStore::empty());
        }
        return TrustStore::load(&path);
    }
    let Some(dir) = kimetsu_core::paths::user_kimetsu_dir() else {
        return Ok(TrustStore::empty());
    };
    let path = dir.join("trust.json");
    if path.exists() {
        TrustStore::load(&path)
    } else {
        Ok(TrustStore::empty())
    }
}

/// `kimetsu-canon-json/1` encoding of `value`.
pub fn canonical_json(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    write_canon(&mut out, value);
    out
}

/// Canonical Ed25519 message for a digest and the body scope list.
pub fn signed_message_bytes(digest_hex: &str, scopes: &[String]) -> Vec<u8> {
    let mut map = Map::new();
    map.insert("digest".into(), Value::String(digest_hex.to_string()));
    map.insert(
        "scopes".into(),
        Value::Array(scopes.iter().cloned().map(Value::String).collect()),
    );
    canonical_json(&Value::Object(map))
}

pub fn verify_ed25519(public_hex: &str, message: &[u8], signature_hex: &str) -> Result<(), String> {
    let public_key = parse_public_key(public_hex)?;
    let signature_bytes =
        decode_hex(signature_hex.trim()).map_err(|_| "signature does not verify".to_string())?;
    if signature_bytes.len() != 64 {
        return Err("signature does not verify".into());
    }
    let signature = Signature::from_slice(&signature_bytes)
        .map_err(|_| "signature does not verify".to_string())?;
    let verifying = VerifyingKey::from_bytes(&public_key)
        .map_err(|_| "signature does not verify".to_string())?;
    verifying
        .verify_strict(message, &signature)
        .map_err(|_| "signature does not verify".to_string())
}

/// Gunzip when the gzip magic is present. Plain UTF-8 is accepted so a
/// canonical JSON document verifies without a compressor in the digest.
pub fn decode_pack_bytes(bytes: &[u8]) -> Result<String, String> {
    if bytes.len() >= 2 && bytes[0] == 0x1f && bytes[1] == 0x8b {
        use flate2::read::GzDecoder;
        use std::io::Read;
        let mut out = String::new();
        GzDecoder::new(bytes)
            .read_to_string(&mut out)
            .map_err(|e| format!("bad gzip: {e}"))?;
        return Ok(out);
    }
    String::from_utf8(bytes.to_vec()).map_err(|e| format!("pack is not UTF-8: {e}"))
}

pub fn encode_gzip(json: &str) -> Result<Vec<u8>, String> {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    enc.write_all(json.as_bytes())
        .map_err(|e| format!("gzip failed: {e}"))?;
    enc.finish().map_err(|e| format!("gzip failed: {e}"))
}

/// Verify a `kimetsu_pack` 2 document. Does not open a database.
pub fn verify_pack_json(json: &str) -> Result<VerifiedPack, String> {
    let value: Value = serde_json::from_str(json).map_err(|e| format!("bad JSON: {e}"))?;
    let mut obj = match value {
        Value::Object(map) => map,
        _ => return Err("pack envelope must be a JSON object".into()),
    };
    let version = obj
        .get("kimetsu_pack")
        .and_then(Value::as_u64)
        .ok_or("missing kimetsu_pack")?;
    if version != 2 {
        return Err(format!("unknown kimetsu_pack version {version}"));
    }
    let signature = match obj.remove("signature") {
        Some(Value::String(value)) if !value.is_empty() => value,
        Some(_) => return Err("signature does not verify".into()),
        None => return Err("missing signature".into()),
    };
    let signer = match obj.remove("signer") {
        Some(Value::String(value)) if !value.is_empty() => value,
        _ => return Err("missing signer".into()),
    };
    let scopes = match obj.get("scopes") {
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| "scopes must be strings".to_string())
            })
            .collect::<Result<Vec<_>, _>>()?,
        _ => return Err("missing scopes".into()),
    };
    let canonical = canonical_json(&Value::Object(obj.clone()));
    let digest_hex = blake3::hash(&canonical).to_hex().to_string();
    let message = signed_message_bytes(&digest_hex, &scopes);
    let embedded = scopes_in_canonical(&canonical)?;
    if embedded != scopes {
        return Err("body scopes != signed scopes".into());
    }
    verify_ed25519(&signer, &message, &signature)?;
    if let Some(err) = scopes_not_canonical(&scopes) {
        return Err(err);
    }
    let memories: Vec<MemoryExport> = serde_json::from_value(
        obj.get("memories")
            .cloned()
            .ok_or_else(|| "missing memories".to_string())?,
    )
    .map_err(|e| format!("bad memories: {e}"))?;
    let count = obj
        .get("memory_count")
        .and_then(Value::as_u64)
        .ok_or("missing memory_count")?;
    if count as usize != memories.len() {
        return Err("memory_count does not match memories".into());
    }
    Ok(VerifiedPack {
        name: optional_string(&obj, "name")?,
        version: optional_string(&obj, "version")?,
        description: optional_string(&obj, "description")?,
        scopes,
        memories,
        signer_hex: encode_hex(&parse_public_key(&signer)?),
        canonical_body: canonical,
        digest_hex,
    })
}

/// Whole-pack reject when an entry mount or the destination is outside the
/// signed scope list, or an entry is not the named destination. No SQL.
pub fn reject_scope_mismatch(pack: &VerifiedPack, destination: MountLevel) -> Result<(), String> {
    let signed = signed_mounts(&pack.scopes)?;
    if !signed.contains(&destination) {
        return Err(format!(
            "scope mismatch: destination {destination} is outside signed scopes"
        ));
    }
    for entry in &pack.memories {
        let mount = mount_of_stored(&entry.scope)
            .ok_or_else(|| format!("scope mismatch: unparsable entry scope {}", entry.scope))?;
        if !signed.contains(&mount) || mount != destination {
            return Err(format!(
                "scope mismatch: entry scope {} is outside destination {destination}",
                entry.scope
            ));
        }
    }
    Ok(())
}

fn signed_mounts(scopes: &[String]) -> Result<Vec<MountLevel>, String> {
    scopes
        .iter()
        .map(|scope| {
            scope
                .parse()
                .map_err(|_| format!("illegal signed scope {scope}"))
        })
        .collect()
}

fn pack_body(
    manifest: &PackManifest,
    scopes: &[String],
    memories: &[MemoryExport],
) -> Result<Value, String> {
    let mut map = Map::new();
    map.insert("kimetsu_pack".into(), Value::from(2));
    insert_opt(&mut map, "name", &manifest.name);
    insert_opt(&mut map, "version", &manifest.version);
    insert_opt(&mut map, "description", &manifest.description);
    insert_opt(&mut map, "exported_at", &manifest.exported_at);
    map.insert(
        "scopes".into(),
        Value::Array(scopes.iter().cloned().map(Value::String).collect()),
    );
    map.insert("memory_count".into(), Value::from(memories.len()));
    map.insert(
        "memories".into(),
        serde_json::to_value(memories).map_err(|e| format!("memories: {e}"))?,
    );
    Ok(Value::Object(map))
}

fn insert_opt(map: &mut Map<String, Value>, key: &str, value: &Option<String>) {
    if let Some(value) = value {
        map.insert(key.to_string(), Value::String(value.clone()));
    }
}

fn optional_string(obj: &Map<String, Value>, key: &str) -> Result<Option<String>, String> {
    match obj.get(key) {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(format!("{key} must be a string")),
    }
}

fn scopes_in_canonical(bytes: &[u8]) -> Result<Vec<String>, String> {
    let value: Value =
        serde_json::from_slice(bytes).map_err(|e| format!("canonical body is not JSON: {e}"))?;
    match value.get("scopes") {
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| "scopes must be strings".to_string())
            })
            .collect(),
        _ => Err("canonical body is missing scopes".into()),
    }
}

fn scopes_not_canonical(scopes: &[String]) -> Option<String> {
    let mut prev: Option<&str> = None;
    for scope in scopes {
        if !LEGAL_SCOPES.contains(&scope.as_str()) {
            return Some(format!("illegal signed scope {scope}"));
        }
        if let Some(prev) = prev {
            if scope.as_str() <= prev {
                return Some("scopes are not strictly sorted".into());
            }
        }
        prev = Some(scope.as_str());
    }
    None
}

fn write_canon(out: &mut Vec<u8>, value: &Value) {
    match value {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(true) => out.extend_from_slice(b"true"),
        Value::Bool(false) => out.extend_from_slice(b"false"),
        Value::Number(number) => out.extend_from_slice(number.to_string().as_bytes()),
        Value::String(text) => {
            let encoded = serde_json::to_string(text).expect("string encoding");
            out.extend_from_slice(encoded.as_bytes());
        }
        Value::Array(items) => {
            out.push(b'[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                write_canon(out, item);
            }
            out.push(b']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
            out.push(b'{');
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                let encoded = serde_json::to_string(key).expect("key encoding");
                out.extend_from_slice(encoded.as_bytes());
                out.push(b':');
                write_canon(out, &map[*key]);
            }
            out.push(b'}');
        }
    }
}

fn parse_public_key(hex_str: &str) -> Result<[u8; 32], String> {
    let bytes = decode_hex(hex_str.trim())?;
    bytes
        .try_into()
        .map_err(|_| "signer must be 32 bytes (64 hex chars)".to_string())
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

fn decode_hex(text: &str) -> Result<Vec<u8>, String> {
    if text.len() % 2 != 0 {
        return Err("odd hex length".into());
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut index = 0;
    while index < bytes.len() {
        let hi = hex_val(bytes[index])?;
        let lo = hex_val(bytes[index + 1])?;
        out.push((hi << 4) | lo);
        index += 2;
    }
    Ok(out)
}

fn hex_val(byte: u8) -> Result<u8, String> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err("invalid hex".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mount::{self, MountGrants};
    use crate::packs::{self, ImportSummary};
    use crate::project::{self, load_project_readonly};
    use crate::user_brain::with_user_brain_disabled;
    use kimetsu_core::memory::{MemoryKind, MemoryScope};
    use kimetsu_core::paths::git_init_boundary;
    use std::path::PathBuf;

    fn signer(byte: u8) -> PackSigner {
        PackSigner::from_secret_bytes([byte; 32])
    }

    fn manifest() -> PackManifest {
        PackManifest {
            name: Some("pin".into()),
            version: Some("1".into()),
            description: Some("pin".into()),
            exported_at: Some("2026-01-01T00:00:00Z".into()),
        }
    }

    fn memory(scope: &str, text: &str) -> MemoryExport {
        MemoryExport {
            text: text.into(),
            scope: scope.into(),
            kind: "fact".into(),
            confidence: 1.0,
            created_at: Some("2026-01-01T00:00:00Z".into()),
        }
    }

    fn repo_grants() -> MountGrants {
        let mut grants = MountGrants::default();
        grants.allow_write(MountLevel::Repo);
        grants.allow_write(MountLevel::User);
        grants
    }

    fn temp_project(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "kimetsu-f1-{name}-{}-{}",
            std::process::id(),
            ulid::Ulid::new()
        ));
        let _ = std::fs::remove_dir_all(&root);
        git_init_boundary(&root);
        root
    }

    fn fingerprint(root: &Path) -> (i64, i64, i64) {
        let (_, _, conn) = load_project_readonly(root).expect("readonly brain");
        let memories: i64 = conn
            .query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))
            .unwrap();
        let proposals: i64 = conn
            .query_row("SELECT COUNT(*) FROM memory_proposals", [], |row| {
                row.get(0)
            })
            .unwrap();
        let events: i64 = conn
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .unwrap();
        (memories, proposals, events)
    }

    fn text_in(root: &Path, table: &str, needle: &str) -> i64 {
        let (_, _, conn) = load_project_readonly(root).unwrap();
        let sql = format!("SELECT COUNT(*) FROM {table} WHERE text LIKE ?1");
        conn.query_row(&sql, [format!("%{needle}%")], |row| row.get(0))
            .unwrap()
    }

    fn prepared(root: &Path) {
        project::init_project(root, false).unwrap();
        project::add_memory(root, MemoryScope::Repo, MemoryKind::Fact, "SENTINEL_STAYS").unwrap();
    }

    fn assert_unchanged(root: &Path, before: (i64, i64, i64)) {
        assert_eq!(fingerprint(root), before, "reject must not write SQL");
        assert_eq!(text_in(root, "memory_proposals", "SENTINEL"), 0);
    }

    #[test]
    fn canonical_profile_is_pinned() {
        assert_eq!(CANONICAL_JSON_PROFILE, "kimetsu-canon-json/1");
        assert_eq!(ED25519_PROFILE, "ed25519-rfc8032-verify-strict");
        let sealed = signer(1)
            .seal_json(
                &manifest(),
                &["repo".into()],
                &[memory("repo", "use cargo test")],
            )
            .unwrap();
        let value: Value = serde_json::from_str(&sealed).unwrap();
        let mut body = value.as_object().unwrap().clone();
        body.remove("signature");
        body.remove("signer");
        let canonical = canonical_json(&Value::Object(body));
        let text = std::str::from_utf8(&canonical).unwrap();
        assert_eq!(
            text,
            concat!(
                r#"{"description":"pin","exported_at":"2026-01-01T00:00:00Z","kimetsu_pack":2,"#,
                r#""memories":[{"confidence":1.0,"created_at":"2026-01-01T00:00:00Z","kind":"fact","scope":"repo","text":"use cargo test"}],"#,
                r#""memory_count":1,"name":"pin","scopes":["repo"],"version":"1"}"#
            ),
            "canonical body drifted from kimetsu-canon-json/1"
        );
        let digest = blake3::hash(&canonical).to_hex().to_string();
        let message = signed_message_bytes(&digest, &["repo".into()]);
        assert_eq!(
            std::str::from_utf8(&message).unwrap(),
            format!(r#"{{"digest":"{digest}","scopes":["repo"]}}"#)
        );
        let pretty =
            serde_json::to_string_pretty(&serde_json::from_str::<Value>(&sealed).unwrap()).unwrap();
        let verified = verify_pack_json(&pretty).unwrap();
        assert_eq!(verified.canonical_body, canonical);
        assert_eq!(verified.digest_hex, digest);
        let forged = signed_message_bytes(&digest, &["user".into()]);
        let signature = value["signature"].as_str().unwrap();
        assert!(verify_ed25519(&signer(1).public_key_hex(), &forged, signature).is_err());
    }

    #[test]
    fn canonical_json_matches_serde_compact_for_sorted_keys() {
        let value = serde_json::json!({"a":1,"b":[true,null,"x / y"],"c":{"z":"quote\"slash\\"}});
        assert_eq!(canonical_json(&value), serde_json::to_vec(&value).unwrap());
    }

    #[test]
    fn f1_crate_does_not_depend_on_iroh() {
        let manifest = include_str!("../Cargo.toml");
        assert!(!manifest.contains("iroh"), "F1 must not link a bus");
    }

    #[test]
    fn tamper_scope_memory_reorder_and_strip_signature_reject_with_no_sql() {
        with_user_brain_disabled(|| {
            let root = temp_project("tamper");
            prepared(&root);
            let before = fingerprint(&root);
            let signed = signer(2)
                .seal_json(
                    &manifest(),
                    &["repo".into(), "user".into()],
                    &[memory("repo", "signed lesson")],
                )
                .unwrap();

            let cases = [
                mutate(&signed, |value| {
                    value["scopes"][0] = serde_json::json!("org");
                }),
                mutate(&signed, |value| {
                    value["memories"][0]["text"] = serde_json::json!("tampered lesson");
                }),
                mutate(&signed, |value| {
                    let scopes = value["scopes"].as_array_mut().unwrap();
                    scopes.swap(0, 1);
                }),
                mutate(&signed, |value| {
                    value.as_object_mut().unwrap().remove("signature");
                }),
            ];
            for tampered in cases {
                let err = mount::import_signed_pack(
                    &root,
                    &repo_grants(),
                    tampered.as_bytes(),
                    MountLevel::Repo,
                    false,
                    false,
                    &TrustStore::empty(),
                )
                .expect_err("tampered pack must reject");
                let message = err.to_string();
                assert!(
                    message.contains("signature") || message.contains("scope"),
                    "unexpected reject: {message}"
                );
                assert_unchanged(&root, before);
            }
            std::fs::remove_dir_all(&root).ok();
        });
    }

    #[test]
    fn user_entry_inside_repo_signed_pack_rejects_with_no_sql() {
        with_user_brain_disabled(|| {
            let root = temp_project("user-entry");
            prepared(&root);
            let before = fingerprint(&root);
            let signed = signer(3)
                .seal_json(
                    &manifest(),
                    &["repo".into()],
                    &[memory("user", "USER_ENTRY_MUST_NOT_LAND")],
                )
                .unwrap();
            verify_pack_json(&signed).expect("the malicious body is signed");
            let err = mount::import_signed_pack(
                &root,
                &repo_grants(),
                signed.as_bytes(),
                MountLevel::Repo,
                false,
                false,
                &trust_for(3, MountLevel::Repo),
            )
            .expect_err("scope mismatch");
            assert!(err.to_string().contains("scope mismatch"), "got: {err}");
            assert_unchanged(&root, before);
            assert_eq!(text_in(&root, "memories", "USER_ENTRY_MUST_NOT_LAND"), 0);
            assert_eq!(
                text_in(&root, "memory_proposals", "USER_ENTRY_MUST_NOT_LAND"),
                0
            );

            let bare = temp_project("user-entry-nosql");
            let err = mount::import_signed_pack(
                &bare,
                &repo_grants(),
                signed.as_bytes(),
                MountLevel::Repo,
                false,
                false,
                &TrustStore::empty(),
            )
            .expect_err("scope mismatch before a brain exists");
            assert!(err.to_string().contains("scope mismatch"));
            assert!(
                !bare.join(".kimetsu").exists(),
                "scope mismatch must not create a database"
            );
            std::fs::remove_dir_all(&root).ok();
            std::fs::remove_dir_all(&bare).ok();
        });
    }

    #[test]
    fn trusted_signer_merges_and_untrusted_signer_only_proposes() {
        with_user_brain_disabled(|| {
            let root = temp_project("trust");
            prepared(&root);
            let signed = signer(4)
                .seal_json(
                    &manifest(),
                    &["repo".into()],
                    &[memory("repo", "TRUSTED_LESSON")],
                )
                .unwrap();
            let merged = mount::import_signed_pack(
                &root,
                &repo_grants(),
                signed.as_bytes(),
                MountLevel::Repo,
                false,
                false,
                &trust_for(4, MountLevel::Repo),
            )
            .unwrap();
            assert_eq!(merged.imported, 1, "{merged:?}");
            assert_eq!(merged.quarantined, 0);
            assert_eq!(text_in(&root, "memories", "TRUSTED_LESSON"), 1);
            assert_eq!(text_in(&root, "memory_proposals", "TRUSTED_LESSON"), 0);

            let other = signer(5)
                .seal_json(
                    &manifest(),
                    &["repo".into()],
                    &[memory("repo", "UNTRUSTED_LESSON")],
                )
                .unwrap();
            let proposed = mount::import_signed_pack(
                &root,
                &repo_grants(),
                other.as_bytes(),
                MountLevel::Repo,
                false,
                false,
                &TrustStore::empty(),
            )
            .unwrap();
            assert_eq!(proposed.quarantined, 1, "{proposed:?}");
            assert_eq!(proposed.imported, 0);
            assert_eq!(text_in(&root, "memories", "UNTRUSTED_LESSON"), 0);
            assert_eq!(text_in(&root, "memory_proposals", "UNTRUSTED_LESSON"), 1);

            let trust_user_only = trust_for(4, MountLevel::User);
            assert!(!trust_user_only.trusts(&signer(4).public_key_hex(), MountLevel::Repo));
            std::fs::remove_dir_all(&root).ok();
        });
    }

    #[test]
    fn untrusted_replace_does_not_retire_rows() {
        with_user_brain_disabled(|| {
            let root = temp_project("replace-untrusted");
            prepared(&root);
            let before = fingerprint(&root);
            let signed = signer(6)
                .seal_json(
                    &manifest(),
                    &["repo".into()],
                    &[memory("repo", "SHOULD_NOT_REPLACE")],
                )
                .unwrap();
            let err = mount::import_signed_pack(
                &root,
                &repo_grants(),
                signed.as_bytes(),
                MountLevel::Repo,
                true,
                false,
                &TrustStore::empty(),
            )
            .expect_err("replace plus an untrusted signer is quarantine, which is forbidden");
            assert!(err.to_string().contains("replace and quarantine"));
            assert_unchanged(&root, before);
            assert_eq!(text_in(&root, "memories", "SENTINEL_STAYS"), 1);
            std::fs::remove_dir_all(&root).ok();
        });
    }

    #[test]
    fn export_strict_aborts_and_signature_covers_the_scrubbed_body() {
        with_user_brain_disabled(|| {
            let root = temp_project("strict");
            project::init_project(&root, false).unwrap();
            project::add_memory(
                &root,
                MemoryScope::Repo,
                MemoryKind::Fact,
                "contact alice@example.com about the build",
            )
            .unwrap();
            let err = mount::export_signed_pack(
                &root,
                &repo_grants(),
                MountLevel::Repo,
                false,
                false,
                &manifest(),
                true,
                &signer(7),
            )
            .expect_err("strict must abort");
            assert!(err.to_string().contains("strict"), "got: {err}");
            assert!(err.to_string().contains("email"), "got: {err}");

            let (gzip, report) = mount::export_signed_pack(
                &root,
                &repo_grants(),
                MountLevel::Repo,
                false,
                false,
                &manifest(),
                false,
                &signer(7),
            )
            .unwrap();
            assert!(report.total >= 1, "{report:?}");
            let json = decode_pack_bytes(&gzip).unwrap();
            let verified = verify_pack_json(&json).expect("scrubbed body verifies");
            let body = std::str::from_utf8(&verified.canonical_body).unwrap();
            assert!(!body.contains("alice@example.com"));
            assert!(body.contains("[REDACTED:email]"));
            assert_eq!(
                verified.memories[0].text.contains("alice@example.com"),
                false
            );
            let round = decode_pack_bytes(&encode_gzip(&json).unwrap()).unwrap();
            verify_pack_json(&round).unwrap();

            let mut tampered: Value = serde_json::from_str(&json).unwrap();
            tampered["memories"][0]["text"] =
                serde_json::json!("contact alice@example.com about the build");
            let tampered = String::from_utf8(canonical_json(&tampered)).unwrap();
            assert!(verify_pack_json(&tampered).is_err());
            std::fs::remove_dir_all(&root).ok();
        });
    }

    #[test]
    fn project_alias_in_a_repo_signed_pack_merges() {
        with_user_brain_disabled(|| {
            let root = temp_project("alias");
            prepared(&root);
            let signed = signer(8)
                .seal_json(
                    &manifest(),
                    &["repo".into()],
                    &[memory("project", "ALIAS_WINDOW_LESSON")],
                )
                .unwrap();
            let summary: ImportSummary = mount::import_signed_pack(
                &root,
                &repo_grants(),
                signed.as_bytes(),
                MountLevel::Repo,
                false,
                false,
                &trust_for(8, MountLevel::Repo),
            )
            .unwrap();
            assert_eq!(summary.imported, 1, "{summary:?}");
            assert_eq!(text_in(&root, "memories", "ALIAS_WINDOW_LESSON"), 1);
            std::fs::remove_dir_all(&root).ok();
        });
    }

    #[test]
    fn export_refuses_run_and_session() {
        with_user_brain_disabled(|| {
            let root = temp_project("no-run");
            project::init_project(&root, false).unwrap();
            for level in [MountLevel::Run, MountLevel::Session] {
                let err = mount::export_signed_pack(
                    &root,
                    &repo_grants(),
                    level,
                    false,
                    false,
                    &manifest(),
                    false,
                    &signer(9),
                )
                .expect_err("export must not emit run or session");
                assert!(err.to_string().contains("refuses"), "got: {err}");
            }
            std::fs::remove_dir_all(&root).ok();
        });
    }

    #[test]
    fn re_signed_unsorted_scopes_still_reject() {
        let signed = signer(10)
            .seal_json(
                &manifest(),
                &["user".into(), "repo".into()],
                &[memory("repo", "order")],
            )
            .unwrap();
        let err = verify_pack_json(&signed).expect_err("unsorted scopes");
        assert!(
            err.to_string().contains("not strictly sorted"),
            "got: {err}"
        );
    }

    #[test]
    fn v1_parser_refuses_a_signed_pack_and_still_reads_v1() {
        let signed = signer(11)
            .seal_json(&manifest(), &["repo".into()], &[memory("repo", "v2")])
            .unwrap();
        let err = packs::parse_pack_or_array(&signed).expect_err("v2 is not an unsigned import");
        assert!(err.to_string().contains("signed"), "got: {err}");
        let v1 = r#"{"kimetsu_pack":1,"name":"old","version":"1","memory_count":1,"memories":[{"text":"v1 lesson","scope":"repo","kind":"fact","confidence":1.0,"created_at":null}]}"#;
        let (pref, entries) = packs::parse_pack_or_array(v1).unwrap();
        assert_eq!(pref.name.as_deref(), Some("old"));
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn trust_store_round_trips_and_is_mount_specific() {
        let path = std::env::temp_dir().join(format!("kimetsu-trust-{}", ulid::Ulid::new()));
        let key = signer(12).public_key_hex();
        let mut store = TrustStore::empty();
        store.allow(&key, &[MountLevel::Repo]).unwrap();
        store.save(&path).unwrap();
        let loaded = TrustStore::load(&path).unwrap();
        assert!(loaded.trusts(&key, MountLevel::Repo));
        assert!(!loaded.trusts(&key, MountLevel::User));
        assert!(!loaded.trusts(&signer(13).public_key_hex(), MountLevel::Repo));
        std::fs::remove_file(&path).ok();
    }

    fn trust_for(byte: u8, mount: MountLevel) -> TrustStore {
        let mut store = TrustStore::empty();
        store
            .allow(&signer(byte).public_key_hex(), &[mount])
            .unwrap();
        store
    }

    fn mutate(json: &str, edit: impl FnOnce(&mut Value)) -> String {
        let mut value: Value = serde_json::from_str(json).unwrap();
        edit(&mut value);
        String::from_utf8(canonical_json(&value)).unwrap()
    }
}
