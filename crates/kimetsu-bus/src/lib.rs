//! ADR 0001 F2 memory bus.
//!
//! iroh carries announcements and `kimetsu_pack` 2 bytes. The store path is
//! [`kimetsu_brain::mount::import_signed_pack`], the same function a local
//! file uses through `kimetsu_sdk::import_signed_pack`.
//! Directory sync stays an offline JSONL replica (`kimetsu brain sync`).
//! This crate has no frame for that batch and does not call `import_events`.
//! This crate does not reimplement merge, quarantine, scrub, or supersede.
//! A v2 pack may carry supersede links. [`import_signed_pack`] applies them.
//! This crate does not choose a survivor and does not open a second store.
//!
//! Presence is a side file and a side task. [`kimetsu_brain::mount::context`]
//! and [`kimetsu_brain::mount::search`] do not call into this crate. Callers
//! reach those reads through `kimetsu-sdk`.
//!
//! # Open-question picks
//!
//! The ADR leaves these unset. F2 pins them here, not by editing the ADR.
//!
//! - **Default relay.** [`BusRelay::Disabled`]. A stock endpoint does not
//!   know n0's public relays. LAN discovery is mDNS plus direct QUIC.
//!   Operators pass [`BusRelay::Custom`] with their own relay URLs. The
//!   relay carries QUIC ciphertext. Hole-punch is iroh's path selection,
//!   ready once a relay is configured and [`BusEndpoint::online`] returns.
//! - **Roster shape.** `kimetsu_roster` 1. One `mounts` list per endpoint
//!   covers both serve and request. `signers` is a separate list, so an
//!   endpoint key is not a pack author unless it is named there. The admin
//!   key is pinned by the operator. An invitation is a newer roster signed
//!   by that key. Peers adopt the higher generation when they connect.
//!   The same generation with a different digest is not merged.

#![allow(clippy::result_large_err)]

mod endpoint;
mod presence;
pub mod protocol;
mod roster;

pub use endpoint::{BusEndpoint, BusRelay, BusStats, EndpointConfig, FetchedPack, PackRequest};
pub use presence::PresenceStore;
pub use protocol::{
    ALPN, Announcement, FrameKind, LEGAL_FRAME_NAMES, discovery_user_data, parse_frame_kind,
};
pub use roster::{RosterAdmin, RosterEndpoint, RosterSupersede, SignedRoster};
