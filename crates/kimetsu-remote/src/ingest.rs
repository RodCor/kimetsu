//! Server-side ingest state: the operator-supplied repo registry (id → git URL)
//! and the managed checkout dir. Enabled only when `--repos-file` is given.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;
use tokio::sync::Mutex;

#[derive(Debug, Clone)]
pub struct RepoSpec {
    pub url: String,
    pub branch: Option<String>,
}

/// One entry in the repos file — either a bare URL string or a `{ url, branch }`
/// table.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RepoSpecRaw {
    Url(String),
    Full { url: String, branch: Option<String> },
}

impl From<RepoSpecRaw> for RepoSpec {
    fn from(raw: RepoSpecRaw) -> Self {
        match raw {
            RepoSpecRaw::Url(url) => RepoSpec { url, branch: None },
            RepoSpecRaw::Full { url, branch } => RepoSpec { url, branch },
        }
    }
}

#[derive(Debug, Deserialize)]
struct ReposFile {
    #[serde(default)]
    repos: HashMap<String, RepoSpecRaw>,
}

/// Parse a `--repos-file` TOML into a repo-id → spec map.
///
/// ```toml
/// [repos]
/// my-api = { url = "https://github.com/org/api.git", branch = "main" }
/// my-web = "https://github.com/org/web.git"
/// ```
pub fn load_repos_file(text: &str) -> Result<HashMap<String, RepoSpec>, String> {
    let parsed: ReposFile = toml::from_str(text).map_err(|e| format!("parse repos file: {e}"))?;
    Ok(parsed
        .repos
        .into_iter()
        .map(|(k, v)| (k, v.into()))
        .collect())
}

pub struct IngestState {
    pub checkout_dir: PathBuf,
    pub repos: HashMap<String, RepoSpec>,
    /// Serializes ingests so concurrent calls don't race on a checkout.
    pub lock: Mutex<()>,
}

impl IngestState {
    pub fn new(checkout_dir: PathBuf, repos: HashMap<String, RepoSpec>) -> Self {
        Self {
            checkout_dir,
            repos,
            lock: Mutex::new(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_forms() {
        let toml = r#"
[repos]
api = { url = "https://github.com/org/api.git", branch = "main" }
web = "https://github.com/org/web.git"
"#;
        let repos = load_repos_file(toml).unwrap();
        assert_eq!(repos["api"].url, "https://github.com/org/api.git");
        assert_eq!(repos["api"].branch.as_deref(), Some("main"));
        assert_eq!(repos["web"].url, "https://github.com/org/web.git");
        assert_eq!(repos["web"].branch, None);
    }

    #[test]
    fn empty_file_is_ok() {
        assert!(load_repos_file("").unwrap().is_empty());
    }
}
