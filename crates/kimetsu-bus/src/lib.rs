//! ADR 0001 F2 memory bus.
//!
//! iroh carries announcements and `kimetsu_pack` 2 bytes. The store path is
//! [`kimetsu_sdk::import_signed_pack`], the same function a local file uses.
//! This crate does not reimplement merge, quarantine, scrub, or supersede.
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
//!   key is pinned by the operator. There is no invitation flow.

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
pub use roster::{RosterAdmin, RosterEndpoint, SignedRoster};
