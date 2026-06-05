//! Shared, immutable server state (cheaply cloned per request via `Arc`).

use std::path::PathBuf;
use std::sync::Arc;

use kimetsu_chat::SkillConfig;

use crate::auth::AuthConfig;

#[derive(Clone)]
pub struct AppState {
    pub data_dir: Arc<PathBuf>,
    pub auth: Arc<AuthConfig>,
    pub skills: Arc<SkillConfig>,
}

impl AppState {
    pub fn new(data_dir: PathBuf, auth: AuthConfig) -> Self {
        // Remote mode never exposes host-local skill tools, so the registry is
        // irrelevant; keep it minimal and never scan user roots on a server.
        let skills = SkillConfig {
            include_user_roots: false,
            ..SkillConfig::default()
        };
        Self {
            data_dir: Arc::new(data_dir),
            auth: Arc::new(auth),
            skills: Arc::new(skills),
        }
    }
}
