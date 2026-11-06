//! Signed roster (`kimetsu_roster` 1).
//!
//! The operator installs the document and pins the admin public key. `mounts`
//! is the serve and request list. `signers` is the only list of pack-signing
//! keys that endpoint may publish. An endpoint id is not consulted as a signer.
//!
//! An invitation is a newer document signed by that same admin key
//! ([`RosterAdmin::invite`]). Generation 0 omits the field, so an F2 document
//! still verifies. A higher generation replaces a lower one. The same
//! generation with a different digest is a conflict and is not merged.

use std::collections::BTreeSet;
use std::path::Path;

use ed25519_dalek::{Signer, SigningKey};
use iroh::EndpointId;
use kimetsu_brain::signed_pack::{self};
use kimetsu_core::memory::MountLevel;
use serde::Deserialize;

/// Ed25519 key that administers the roster. It is not a pack signer unless
/// a roster entry also lists it under `signers`.
pub struct RosterAdmin {
    signing: SigningKey,
}

impl RosterAdmin {
    pub fn generate() -> Result<Self, String> {
        let mut secret = [0u8; 32];
        getrandom::fill(&mut secret).map_err(|err| format!("roster admin key: {err}"))?;
        Ok(Self::from_secret_bytes(secret))
    }

    pub fn from_secret_bytes(secret: [u8; 32]) -> Self {
        Self {
            signing: SigningKey::from_bytes(&secret),
        }
    }

    pub fn public_key_hex(&self) -> String {
        encode_hex(self.signing.verifying_key().as_bytes())
    }

    pub fn sign(&self, group: &str, endpoints: &[RosterEndpoint]) -> Result<String, String> {
        self.sign_generation(group, endpoints, 0)
    }

    /// Sign `endpoints` at `generation`. Zero matches [`Self::sign`]: the
    /// field is omitted from the canonical body.
    pub fn sign_generation(
        &self,
        group: &str,
        endpoints: &[RosterEndpoint],
        generation: u64,
    ) -> Result<String, String> {
        let body = canonical_body(group, endpoints, generation)?;
        let digest = blake3::hash(&body).to_hex().to_string();
        let message = roster_message(&digest);
        let signature = self.signing.sign(&message);
        let mut wire =
            serde_json::from_slice::<serde_json::Value>(&body).map_err(|err| err.to_string())?;
        let object = wire
            .as_object_mut()
            .ok_or("roster body must be an object")?;
        object.insert(
            "signer".into(),
            serde_json::Value::String(self.public_key_hex()),
        );
        object.insert(
            "signature".into(),
            serde_json::Value::String(encode_hex(&signature.to_bytes())),
        );
        serde_json::to_string(&wire).map_err(|err| err.to_string())
    }

    /// Admin-signed roster at `current.generation() + 1` that adds `endpoint`.
    ///
    /// The same endpoint with the same mounts and signers returns the current
    /// document. A different mount or signer list is refused: that is a
    /// roster edit, which goes through [`Self::reissue`], not an invitation.
    pub fn invite(
        &self,
        current: &SignedRoster,
        endpoint: RosterEndpoint,
    ) -> Result<String, String> {
        self.require_admin(current)?;
        if let Some(existing) = current
            .endpoints
            .iter()
            .find(|entry| entry.endpoint_id == endpoint.endpoint_id)
        {
            if same_entry(existing, &endpoint) {
                return Ok(current.document.clone());
            }
            return Err(
                "invitation would change mounts or signers of an endpoint already on the roster"
                    .into(),
            );
        }
        let mut endpoints = current.endpoints.clone();
        endpoints.push(endpoint);
        self.reissue(current, &endpoints)
    }

    /// Admin-signed roster at the next generation. An identical membership
    /// returns the current document. An empty list is rejected.
    pub fn reissue(
        &self,
        current: &SignedRoster,
        endpoints: &[RosterEndpoint],
    ) -> Result<String, String> {
        self.require_admin(current)?;
        if endpoints.is_empty() {
            return Err("roster has no endpoints".into());
        }
        let body = canonical_body(current.group(), endpoints, current.generation())?;
        let digest = blake3::hash(&body).to_hex().to_string();
        if digest == current.id() {
            return Ok(current.document.clone());
        }
        let next = current
            .generation()
            .checked_add(1)
            .ok_or("roster generation is exhausted")?;
        self.sign_generation(current.group(), endpoints, next)
    }

    fn require_admin(&self, current: &SignedRoster) -> Result<(), String> {
        if current.admin_hex() != self.public_key_hex() {
            return Err("roster admin does not match the signing key".into());
        }
        Ok(())
    }
}

/// One machine. `signers` holds lowercase hex Ed25519 pack keys.
#[derive(Debug, Clone)]
pub struct RosterEndpoint {
    pub endpoint_id: EndpointId,
    pub mounts: Vec<MountLevel>,
    pub signers: BTreeSet<String>,
}

impl RosterEndpoint {
    pub fn new(
        endpoint_id: EndpointId,
        mounts: &[MountLevel],
        signers: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Result<Self, String> {
        if mounts.is_empty() {
            return Err("roster endpoint needs at least one mount".into());
        }
        let mut normalized = BTreeSet::new();
        for signer in signers {
            let signer = signer.as_ref().trim().to_ascii_lowercase();
            if signer.len() != 64 || !signer.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err("roster signer must be 64 hex chars".into());
            }
            normalized.insert(signer);
        }
        Ok(Self {
            endpoint_id,
            mounts: mounts.to_vec(),
            signers: normalized,
        })
    }
}

/// Which signed document wins. Same generation and a different digest is
/// [`RosterSupersede::Conflict`]: the documents are not merged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RosterSupersede {
    IncomingWins,
    CurrentWins,
    Same,
    Conflict,
    ForeignGroup,
}

/// A roster whose admin signature has been checked.
#[derive(Debug, Clone)]
pub struct SignedRoster {
    group: String,
    roster_id: String,
    admin_hex: String,
    generation: u64,
    endpoints: Vec<RosterEndpoint>,
    document: String,
}

impl SignedRoster {
    pub fn open(json: &str, admin_public_key_hex: &str) -> Result<Self, String> {
        let wire: Wire = serde_json::from_str(json).map_err(|err| format!("roster: {err}"))?;
        if wire.kimetsu_roster != 1 {
            return Err(format!(
                "unknown kimetsu_roster version {}",
                wire.kimetsu_roster
            ));
        }
        let admin = admin_public_key_hex.trim().to_ascii_lowercase();
        if wire.signer.to_ascii_lowercase() != admin {
            return Err("roster signer is not the pinned admin key".into());
        }
        if wire.group.trim().is_empty() || wire.group.len() > 64 {
            return Err("roster group name is empty or too long".into());
        }
        if wire.endpoints.is_empty() {
            return Err("roster has no endpoints".into());
        }
        let mut endpoints = Vec::with_capacity(wire.endpoints.len());
        for entry in &wire.endpoints {
            let endpoint_id: EndpointId = entry
                .endpoint_id
                .parse()
                .map_err(|err| format!("roster endpoint id: {err}"))?;
            let mut mounts = Vec::new();
            for mount in &entry.mounts {
                mounts.push(
                    mount
                        .parse::<MountLevel>()
                        .map_err(|_| format!("roster mount {mount}"))?,
                );
            }
            endpoints.push(RosterEndpoint::new(
                endpoint_id,
                &mounts,
                entry.signers.iter(),
            )?);
        }
        let body = canonical_body(&wire.group, &endpoints, wire.generation)?;
        let digest = blake3::hash(&body).to_hex().to_string();
        let message = roster_message(&digest);
        signed_pack::verify_ed25519(&wire.signer, &message, &wire.signature)?;
        Ok(Self {
            group: wire.group,
            roster_id: digest,
            admin_hex: admin,
            generation: wire.generation,
            endpoints,
            document: json.to_string(),
        })
    }

    pub fn load(path: &Path, admin_public_key_hex: &str) -> Result<Self, String> {
        let raw = std::fs::read_to_string(path)
            .map_err(|err| format!("roster {}: {err}", path.display()))?;
        Self::open(&raw, admin_public_key_hex)
    }

    pub fn group(&self) -> &str {
        &self.group
    }

    pub fn id(&self) -> &str {
        &self.roster_id
    }

    pub fn admin_hex(&self) -> &str {
        &self.admin_hex
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The signed JSON that [`Self::open`] verified.
    pub fn document(&self) -> &str {
        &self.document
    }

    pub fn endpoints(&self) -> &[RosterEndpoint] {
        &self.endpoints
    }

    /// `incoming` is compared with this roster. A foreign group never wins.
    pub fn compare(&self, incoming: &SignedRoster) -> RosterSupersede {
        if self.admin_hex != incoming.admin_hex || self.group != incoming.group {
            return RosterSupersede::ForeignGroup;
        }
        match self.generation.cmp(&incoming.generation) {
            std::cmp::Ordering::Equal if self.roster_id == incoming.roster_id => {
                RosterSupersede::Same
            }
            std::cmp::Ordering::Equal => RosterSupersede::Conflict,
            std::cmp::Ordering::Greater => RosterSupersede::CurrentWins,
            std::cmp::Ordering::Less => RosterSupersede::IncomingWins,
        }
    }

    pub fn allows(&self, endpoint_id: EndpointId) -> bool {
        self.endpoints
            .iter()
            .any(|entry| entry.endpoint_id == endpoint_id)
    }

    pub fn may_use_mount(&self, endpoint_id: EndpointId, mount: MountLevel) -> bool {
        self.endpoints
            .iter()
            .any(|entry| entry.endpoint_id == endpoint_id && entry.mounts.contains(&mount))
    }

    /// Pack authors this endpoint is allowed to publish. The endpoint id
    /// itself is not a member of this set unless the operator listed it.
    pub fn may_publish_signer(&self, endpoint_id: EndpointId, signer_hex: &str) -> bool {
        let signer = signer_hex.trim().to_ascii_lowercase();
        self.endpoints
            .iter()
            .any(|entry| entry.endpoint_id == endpoint_id && entry.signers.contains(&signer))
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    kimetsu_roster: u64,
    group: String,
    #[serde(default)]
    generation: u64,
    endpoints: Vec<WireEndpoint>,
    signer: String,
    signature: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireEndpoint {
    endpoint_id: String,
    mounts: Vec<String>,
    signers: Vec<String>,
}

fn same_entry(left: &RosterEndpoint, right: &RosterEndpoint) -> bool {
    if left.endpoint_id != right.endpoint_id || left.signers != right.signers {
        return false;
    }
    let mut left_mounts: Vec<&str> = left.mounts.iter().map(|mount| mount.as_str()).collect();
    let mut right_mounts: Vec<&str> = right.mounts.iter().map(|mount| mount.as_str()).collect();
    left_mounts.sort_unstable();
    left_mounts.dedup();
    right_mounts.sort_unstable();
    right_mounts.dedup();
    left_mounts == right_mounts
}

fn canonical_body(
    group: &str,
    endpoints: &[RosterEndpoint],
    generation: u64,
) -> Result<Vec<u8>, String> {
    let mut entries: Vec<&RosterEndpoint> = endpoints.iter().collect();
    entries.sort_by_key(|entry| entry.endpoint_id);
    let mut seen = BTreeSet::new();
    let mut encoded = Vec::with_capacity(entries.len());
    for entry in entries {
        if !seen.insert(entry.endpoint_id) {
            return Err("roster lists an endpoint twice".into());
        }
        let mut mounts: Vec<&str> = entry.mounts.iter().map(|mount| mount.as_str()).collect();
        mounts.sort_unstable();
        mounts.dedup();
        let signers: Vec<&str> = entry.signers.iter().map(String::as_str).collect();
        encoded.push(serde_json::json!({
            "endpoint_id": entry.endpoint_id.to_string(),
            "mounts": mounts,
            "signers": signers,
        }));
    }
    let mut value = serde_json::json!({
        "endpoints": encoded,
        "group": group,
        "kimetsu_roster": 1,
    });
    if generation > 0 {
        value["generation"] = serde_json::json!(generation);
    }
    Ok(signed_pack::canonical_json(&value))
}

fn roster_message(digest_hex: &str) -> Vec<u8> {
    let value = serde_json::json!({
        "digest": digest_hex,
        "kimetsu_roster": 1,
    });
    signed_pack::canonical_json(&value)
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tamper_and_wrong_admin_fail_and_roles_stay_split() {
        // SecretKey lives in iroh; generate two so the ids parse.
        let secret_a = iroh::SecretKey::generate();
        let secret_b = iroh::SecretKey::generate();
        let admin = RosterAdmin::generate().unwrap();
        let pack_signer = "ab".repeat(32);
        let member = RosterEndpoint::new(
            secret_a.public(),
            &[MountLevel::Repo],
            [pack_signer.as_str()],
        )
        .unwrap();
        let relay_only =
            RosterEndpoint::new(secret_b.public(), &[MountLevel::Repo], None::<&str>).unwrap();
        let signed = admin.sign("lab", &[member, relay_only]).unwrap();
        let roster = SignedRoster::open(&signed, &admin.public_key_hex()).unwrap();
        assert!(roster.allows(secret_a.public()));
        assert!(roster.may_publish_signer(secret_a.public(), &pack_signer));
        assert!(
            !roster.may_publish_signer(secret_b.public(), &pack_signer),
            "an endpoint with an empty signer list publishes nothing"
        );
        assert!(!roster.allows(iroh::SecretKey::generate().public()));
        assert!(!roster.may_use_mount(secret_a.public(), MountLevel::Org));

        let mut tampered: serde_json::Value = serde_json::from_str(&signed).unwrap();
        tampered["endpoints"][0]["mounts"][0] = serde_json::json!("org");
        let err = SignedRoster::open(
            &serde_json::to_string(&tampered).unwrap(),
            &admin.public_key_hex(),
        )
        .unwrap_err();
        assert!(
            err.contains("does not verify") || err.contains("signature"),
            "{err}"
        );

        let other = RosterAdmin::generate().unwrap();
        let err = SignedRoster::open(&signed, &other.public_key_hex()).unwrap_err();
        assert!(err.contains("pinned admin"), "{err}");

        tampered["endpoints"][0]["mounts"][0] = serde_json::json!("repo");
        tampered["worktree"] = serde_json::json!("/tmp/tree");
        assert!(
            SignedRoster::open(
                &serde_json::to_string(&tampered).unwrap(),
                &admin.public_key_hex()
            )
            .is_err()
        );
    }

    #[test]
    fn generation_zero_omits_the_field_and_still_opens() {
        let secret = iroh::SecretKey::generate();
        let admin = RosterAdmin::generate().unwrap();
        let member =
            RosterEndpoint::new(secret.public(), &[MountLevel::Repo], ["ab".repeat(32)]).unwrap();
        let signed = admin.sign("lab", &[member]).unwrap();
        assert!(
            !signed.contains("generation"),
            "generation 0 must stay the F2 canonical body: {signed}"
        );
        let roster = SignedRoster::open(&signed, &admin.public_key_hex()).unwrap();
        assert_eq!(roster.generation(), 0);
        assert_eq!(roster.document(), signed);
    }

    #[test]
    fn invite_adds_an_endpoint_and_a_tampered_mount_does_not_verify() {
        let alice = iroh::SecretKey::generate();
        let bob = iroh::SecretKey::generate();
        let admin = RosterAdmin::generate().unwrap();
        let signer = "cd".repeat(32);
        let founder =
            RosterEndpoint::new(alice.public(), &[MountLevel::Repo], None::<&str>).unwrap();
        let base = SignedRoster::open(
            &admin.sign("lab", &[founder]).unwrap(),
            &admin.public_key_hex(),
        )
        .unwrap();
        let invitee =
            RosterEndpoint::new(bob.public(), &[MountLevel::Repo], [signer.as_str()]).unwrap();
        let invited = admin.invite(&base, invitee).unwrap();
        let roster = SignedRoster::open(&invited, &admin.public_key_hex()).unwrap();
        assert_eq!(roster.generation(), 1);
        assert!(roster.allows(alice.public()));
        assert!(roster.allows(bob.public()));
        assert!(roster.may_use_mount(bob.public(), MountLevel::Repo));
        assert!(roster.may_publish_signer(bob.public(), &signer));
        assert!(
            !roster.may_publish_signer(alice.public(), &signer),
            "the invitation must not turn the founder into the pack author"
        );
        assert_ne!(roster.id(), base.id());
        assert!(matches!(
            base.compare(&roster),
            RosterSupersede::IncomingWins
        ));
        assert!(matches!(
            roster.compare(&base),
            RosterSupersede::CurrentWins
        ));

        let again =
            RosterEndpoint::new(bob.public(), &[MountLevel::Repo], [signer.as_str()]).unwrap();
        let second = admin.invite(&roster, again).unwrap();
        assert_eq!(second, roster.document());

        let widened =
            RosterEndpoint::new(bob.public(), &[MountLevel::Org], [signer.as_str()]).unwrap();
        let err = admin.invite(&roster, widened).unwrap_err();
        assert!(err.contains("mounts") || err.contains("signers"), "{err}");

        let mut tampered: serde_json::Value = serde_json::from_str(&invited).unwrap();
        tampered["endpoints"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|entry| entry["endpoint_id"] == bob.public().to_string())
            .unwrap()["mounts"][0] = serde_json::json!("org");
        let err = SignedRoster::open(
            &serde_json::to_string(&tampered).unwrap(),
            &admin.public_key_hex(),
        )
        .unwrap_err();
        assert!(err.contains("does not verify"), "{err}");

        let other = RosterAdmin::generate().unwrap();
        assert!(SignedRoster::open(&invited, &other.public_key_hex()).is_err());
    }

    #[test]
    fn reissue_removes_an_endpoint_and_a_same_generation_fork_does_not_merge() {
        let alice = iroh::SecretKey::generate();
        let bob = iroh::SecretKey::generate();
        let admin = RosterAdmin::generate().unwrap();
        let founder =
            RosterEndpoint::new(alice.public(), &[MountLevel::Repo], None::<&str>).unwrap();
        let base = SignedRoster::open(
            &admin.sign("lab", std::slice::from_ref(&founder)).unwrap(),
            &admin.public_key_hex(),
        )
        .unwrap();
        let invitee = RosterEndpoint::new(bob.public(), &[MountLevel::Repo], None::<&str>).unwrap();
        let invited = SignedRoster::open(
            &admin.invite(&base, invitee).unwrap(),
            &admin.public_key_hex(),
        )
        .unwrap();
        let removed = SignedRoster::open(
            &admin.reissue(&invited, &[founder]).unwrap(),
            &admin.public_key_hex(),
        )
        .unwrap();
        assert_eq!(removed.generation(), 2);
        assert!(removed.allows(alice.public()));
        assert!(!removed.allows(bob.public()));
        assert!(matches!(
            invited.compare(&removed),
            RosterSupersede::IncomingWins
        ));

        let fork = SignedRoster::open(
            &admin
                .sign_generation(
                    "lab",
                    &[
                        RosterEndpoint::new(bob.public(), &[MountLevel::Org], None::<&str>)
                            .unwrap(),
                    ],
                    2,
                )
                .unwrap(),
            &admin.public_key_hex(),
        )
        .unwrap();
        assert!(matches!(removed.compare(&fork), RosterSupersede::Conflict));
        assert_eq!(removed.generation(), fork.generation());
        assert_ne!(removed.id(), fork.id());

        let other_group = SignedRoster::open(
            &admin
                .sign_generation(
                    "other",
                    &[
                        RosterEndpoint::new(alice.public(), &[MountLevel::Repo], None::<&str>)
                            .unwrap(),
                    ],
                    3,
                )
                .unwrap(),
            &admin.public_key_hex(),
        )
        .unwrap();
        assert!(matches!(
            removed.compare(&other_group),
            RosterSupersede::ForeignGroup
        ));
    }
}
