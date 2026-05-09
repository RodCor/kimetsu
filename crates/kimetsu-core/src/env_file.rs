use std::env;
use std::fs;
use std::path::Path;

pub fn resolve_env_value(repo_root: &Path, key: &str) -> Option<String> {
    if let Ok(value) = env::var(key)
        && !value.trim().is_empty()
    {
        return Some(value);
    }

    read_dotenv(repo_root, key)
}

fn read_dotenv(repo_root: &Path, key: &str) -> Option<String> {
    let path = repo_root.join(".env");
    let content = fs::read_to_string(path).ok()?;

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        if name.trim() != key {
            continue;
        }

        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .or_else(|| {
                value
                    .strip_prefix('\'')
                    .and_then(|value| value.strip_suffix('\''))
            })
            .unwrap_or(value);
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }

    None
}
