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
        let args = args
            .iter()
            .map(|arg| redact_url_credentials(arg))
            .collect::<Vec<_>>()
            .join(" ");
        let stderr = redact_url_credentials(String::from_utf8_lossy(&out.stderr).trim());
        return Err(format!("git {} failed: {}", args, stderr));
    }
    Ok(())
}

fn redact_url_credentials(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    loop {
        let Some(scheme_pos) = rest.find("://") else {
            out.push_str(rest);
            break;
        };
        let scheme_end = scheme_pos + 3;
        out.push_str(&rest[..scheme_end]);
        let after_scheme = &rest[scheme_end..];
        let terminator = after_scheme
            .find(|ch: char| {
                ch.is_whitespace() || ch == '\'' || ch == '"' || ch == '<' || ch == '>'
            })
            .unwrap_or(after_scheme.len());
        let candidate = &after_scheme[..terminator];
        if let Some(at_pos) = candidate.find('@') {
            out.push_str("[redacted]@");
            out.push_str(&candidate[at_pos + 1..]);
            rest = &after_scheme[terminator..];
        } else {
            out.push_str(candidate);
            rest = &after_scheme[terminator..];
        }
    }
    out
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

#[cfg(test)]
mod tests {
    use super::redact_url_credentials;

    #[test]
    fn redacts_credentials_in_git_urls() {
        let text = "fatal: Authentication failed for 'https://user:token@example.com/org/repo.git'";
        let redacted = redact_url_credentials(text);
        assert_eq!(
            redacted,
            "fatal: Authentication failed for 'https://[redacted]@example.com/org/repo.git'"
        );
        assert!(!redacted.contains("user:token"));
    }

    #[test]
    fn leaves_plain_urls_unchanged() {
        let text = "https://github.com/org/repo.git";
        assert_eq!(redact_url_credentials(text), text);
    }
}
