//! Managed git checkouts for server-side ingest. The clone URL always comes
//! from the operator's `--repos-file` (never the client), and we invoke git via
//! argv (no shell), so there is no arbitrary-clone or injection surface.

use std::path::{Path, PathBuf};
use std::process::Command;

fn run_git(args: &[&str]) -> Result<(), String> {
    let out = Command::new("git")
        .args(args)
        .output()
        .map_err(|e| format!("spawn git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// Ensure `<checkout_dir>/<repo_id>` is a fresh shallow checkout of `url`.
/// Clones on first use, otherwise fetches + hard-resets to the latest commit.
pub fn ensure_checkout(
    checkout_dir: &Path,
    repo_id: &str,
    url: &str,
    branch: Option<&str>,
) -> Result<PathBuf, String> {
    let dest = checkout_dir.join(repo_id);
    let dest_str = dest
        .to_str()
        .ok_or_else(|| "checkout path is not valid UTF-8".to_string())?;

    if dest.join(".git").is_dir() {
        // Refresh in place (shallow).
        let reference = branch.unwrap_or("HEAD");
        run_git(&["-C", dest_str, "fetch", "--depth", "1", "origin", reference])?;
        run_git(&["-C", dest_str, "reset", "--hard", "FETCH_HEAD"])?;
        run_git(&["-C", dest_str, "clean", "-fdq"])?;
    } else {
        std::fs::create_dir_all(checkout_dir)
            .map_err(|e| format!("create checkout dir {}: {e}", checkout_dir.display()))?;
        let mut args = vec!["clone", "--depth", "1"];
        if let Some(b) = branch {
            args.push("--branch");
            args.push(b);
        }
        args.push(url);
        args.push(dest_str);
        run_git(&args)?;
    }
    Ok(dest)
}
