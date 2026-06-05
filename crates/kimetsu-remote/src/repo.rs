//! Repository identity → on-disk brain location. The server hosts one brain per
//! repo under a data dir; the repo id arrives in the request URL and is
//! sanitized (path-traversal safe) before it ever touches the filesystem.

use std::path::{Path, PathBuf};

/// Sanitize a client-supplied repo id into a single, filesystem-safe path
/// segment. Rejects (does not silently strip) anything that could escape the
/// data dir. Lowercased; `[A-Za-z0-9._-]` only; ≤128 chars; no leading dot;
/// no `..`.
pub fn sanitize_repo_id(raw: &str) -> Result<String, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("repo id is empty".to_string());
    }
    if s.len() > 128 {
        return Err("repo id is too long (max 128)".to_string());
    }
    if s.starts_with('.') {
        return Err("repo id may not start with '.'".to_string());
    }
    if s.contains("..") {
        return Err("repo id may not contain '..'".to_string());
    }
    if !s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
    {
        return Err("repo id may contain only letters, digits, '.', '_', '-'".to_string());
    }
    Ok(s.to_ascii_lowercase())
}

/// Resolve a repo id to its brain root `<data_dir>/<id>`, asserting the result
/// stays inside `data_dir` (defense in depth on top of `sanitize_repo_id`).
pub fn resolve_brain_root(data_dir: &Path, repo: &str) -> Result<PathBuf, String> {
    let id = sanitize_repo_id(repo)?;
    let root = data_dir.join(&id);
    if !root.starts_with(data_dir) {
        return Err("resolved brain root escaped the data dir".to_string());
    }
    Ok(root)
}

/// Ensure a repo's brain exists, creating + initializing it on first use. Uses
/// the no-git `init_project_at_root` so discovery never climbs out of the data
/// dir.
pub fn ensure_initialized(root: &Path) -> Result<(), String> {
    if root.join(".kimetsu").exists() {
        return Ok(());
    }
    std::fs::create_dir_all(root).map_err(|e| format!("create brain dir: {e}"))?;
    kimetsu_brain::project::init_project_at_root(root, false)
        .map_err(|e| format!("init brain: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_reasonable_ids() {
        assert_eq!(sanitize_repo_id("acme-api").unwrap(), "acme-api");
        assert_eq!(sanitize_repo_id("a.b_c-1").unwrap(), "a.b_c-1");
        assert_eq!(sanitize_repo_id("MixedCase").unwrap(), "mixedcase");
    }

    #[test]
    fn rejects_traversal_and_separators() {
        for bad in [
            "", "..", "../x", "a/b", "a\\b", "/abs", "c:\\x", "c:x", ".hidden", "a..b", " ", "a b",
            "café",
        ] {
            assert!(sanitize_repo_id(bad).is_err(), "should reject {bad:?}");
        }
        assert!(sanitize_repo_id(&"x".repeat(129)).is_err());
    }

    #[test]
    fn resolved_root_stays_in_data_dir() {
        let data = Path::new("/srv/kbrains");
        let root = resolve_brain_root(data, "acme-api").unwrap();
        assert!(root.starts_with(data));
        assert!(resolve_brain_root(data, "../etc").is_err());
    }
}
