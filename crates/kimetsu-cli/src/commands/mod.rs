//! Command implementations, one module per CLI group (v2.5.1 split).
//! The clap surface (arg structs + enums) lives in main.rs; these modules
//! hold the handlers and their tests.
pub(crate) mod bench;
pub(crate) mod brain;
pub(crate) mod chat;
pub(crate) mod config;
pub(crate) mod hooks;
pub(crate) mod hosts;
pub(crate) mod integrations;
pub(crate) mod lifecycle;
pub(crate) mod memory;
pub(crate) mod runs;
