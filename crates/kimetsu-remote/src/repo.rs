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
    if s.ends_with('.') {
        return Err("repo id may not end with '.'".to_string());
    }
    if !s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
    {
        return Err("repo id may contain only letters, digits, '.', '_', '-'".to_string());
    }
    let id = s.to_ascii_lowercase();
    let stem = id.split('.').next().unwrap_or_default();
    if matches!(stem, "con" | "prn" | "aux" | "nul")
        || (stem.len() == 4
            && (stem.starts_with("com") || stem.starts_with("lpt"))
            && matches!(stem.as_bytes()[3], b'1'..=b'9'))
    {
        return Err("repo id may not use a reserved device name".to_string());
    }
    Ok(id)
}

/// Resolve a repo id to its brain root `<data_dir>/<id>`, asserting the result
/// stays inside `data_dir` (defense in depth on top of `sanitize_repo_id`).
pub fn resolve_brain_root(data_dir: &Path, repo: &str) -> Result<PathBuf, String> {
    let id = sanitize_repo_id(repo)?;
    let root = data_dir.join(&id);
    if !root.starts_with(data_dir) {
        return Err("resolved brain root escaped the data dir".to_string());
    }
    match std::fs::symlink_metadata(&root) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err("repo root must be a directory, not a link".to_string());
            }
            let expected = data_dir
                .canonicalize()
                .map_err(|e| format!("resolve data dir: {e}"))?
                .join(&id);
            let actual = root
                .canonicalize()
                .map_err(|e| format!("resolve repo root: {e}"))?;
            // Also catches junctions and aliases into a different tenant under data_dir.
            if actual != expected {
                return Err("repo root is redirected to another directory".to_string());
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("inspect repo root: {error}")),
    }
    Ok(root)
}

/// Ensure a repo's brain exists, creating + initializing it on first use. Uses
/// the no-git `init_project_at_root` so discovery never climbs out of the data
/// dir.
pub fn ensure_initialized(root: &Path) -> Result<(), String> {
    kimetsu_core::paths::ProjectPaths::at_root(root)
        .validate_state_dir()
        .map_err(|e| format!("invalid brain state paths: {e}"))?;
    if root.join(".kimetsu").join("brain.db").is_file() {
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
    fn rejects_windows_path_aliases_on_every_platform() {
        for bad in [
            "web.", "web...", "CON", "con.txt", "nul", "aux", "prn", "COM1", "lpt9.log",
        ] {
            assert!(
                sanitize_repo_id(bad).is_err(),
                "must reject filesystem alias {bad}"
            );
        }
        assert!(sanitize_repo_id("console").is_ok());
        assert!(sanitize_repo_id("com10").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_repo_symlinks_and_existing_state_symlinks() {
        use std::os::unix::fs::symlink;
        let data = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), data.path().join("escape")).unwrap();
        assert!(resolve_brain_root(data.path(), "escape").is_err());
        std::fs::create_dir(data.path().join("other")).unwrap();
        symlink(data.path().join("other"), data.path().join("alias")).unwrap();
        assert!(resolve_brain_root(data.path(), "alias").is_err());

        let root = data.path().join("safe");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(outside.path().join("brain.db"), "outside sentinel").unwrap();
        symlink(outside.path(), root.join(".kimetsu")).unwrap();
        assert!(ensure_initialized(&root).is_err());
        assert_eq!(
            std::fs::read_to_string(outside.path().join("brain.db")).unwrap(),
            "outside sentinel"
        );
    }

    #[cfg(windows)]
    #[test]
    fn rejects_repo_and_state_junctions() {
        fn junction(target: &Path, link: &Path) {
            let result = std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "create test junction: {}",
                String::from_utf8_lossy(&result.stderr)
            );
        }
        let data = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        junction(outside.path(), &data.path().join("escape"));
        assert!(resolve_brain_root(data.path(), "escape").is_err());
        let root = data.path().join("safe");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(outside.path().join("brain.db"), "outside sentinel").unwrap();
        junction(outside.path(), &root.join(".kimetsu"));
        assert!(ensure_initialized(&root).is_err());
        assert_eq!(
            std::fs::read_to_string(outside.path().join("brain.db")).unwrap(),
            "outside sentinel"
        );
    }

    #[test]
    fn resolved_root_stays_in_data_dir() {
        let data = Path::new("/srv/kbrains");
        let root = resolve_brain_root(data, "acme-api").unwrap();
        assert!(root.starts_with(data));
        assert!(resolve_brain_root(data, "../etc").is_err());
    }

    #[test]
    fn ensure_initialized_repairs_partial_state_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        std::fs::create_dir_all(root.join(".kimetsu")).expect("partial state dir");

        ensure_initialized(&root).expect("initialize");

        assert!(root.join(".kimetsu").join("brain.db").is_file());
    }
}
