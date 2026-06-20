pub mod config;
pub mod env_file;
pub mod event;
pub mod ids;
pub mod memory;
pub mod paths;
pub mod secret;

pub const KIMETSU_SCHEMA_VERSION: i64 = 4;
/// The `project.toml` config-file format version. Deliberately decoupled
/// from `KIMETSU_SCHEMA_VERSION` (the brain.db schema): the DB schema can
/// advance via migrations without forcing every project.toml to be rewritten.
pub const KIMETSU_CONFIG_VERSION: i64 = 1;
pub const EVENT_SCHEMA_VERSION: u32 = 1;

pub type KimetsuResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
