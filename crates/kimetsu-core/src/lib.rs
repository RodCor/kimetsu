pub mod config;
pub mod env_file;
pub mod event;
pub mod ids;
pub mod memory;
pub mod paths;
pub mod secret;

pub const KIMETSU_SCHEMA_VERSION: i64 = 1;
pub const EVENT_SCHEMA_VERSION: u32 = 1;

pub type KimetsuResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
