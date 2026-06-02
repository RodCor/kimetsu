//! Interactive `kimetsu plugin install` wizard that configures the
//! credentialed SessionEnd distiller: collects an API key (+ optional
//! LiteLLM base URL) + model, writes a gitignored `.env`, and flips
//! `[learning.distiller]` on in the workspace project.toml.

use std::fs;
use std::io::{BufRead, Write};
use std::path::Path;

use kimetsu_brain::project;
use kimetsu_core::paths::ProjectPaths;

/// Run the wizard against the given reader/writer (real stdin/stdout in
/// production; scripted in tests). Returns Ok(true) when the distiller was
/// configured, Ok(false) when the user declined or aborted.
pub fn run_harvest_setup<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    paths: &ProjectPaths,
) -> std::io::Result<bool> {
    write!(writer, "Set up the auto-harvest distiller now? [y/N]: ")?;
    writer.flush()?;
    if !read_line(reader)?.trim().eq_ignore_ascii_case("y") {
        return Ok(false);
    }

    write!(writer, "Harness [claude/codex] (codex not yet supported): ")?;
    writer.flush()?;
    let harness = read_line(reader)?.trim().to_lowercase();
    if harness == "codex" {
        writeln!(writer, "Codex distiller is not supported yet — skipping setup.")?;
        return Ok(false);
    }

    write!(writer, "Anthropic API key (or LiteLLM key): ")?;
    writer.flush()?;
    let key = read_line(reader)?.trim().to_string();
    if key.is_empty() {
        writeln!(writer, "No key entered — skipping setup.")?;
        return Ok(false);
    }

    write!(
        writer,
        "ANTHROPIC_BASE_URL (optional; blank for Anthropic, set for LiteLLM): "
    )?;
    writer.flush()?;
    let base_url = read_line(reader)?.trim().to_string();

    write!(writer, "Model [claude-haiku-4-5]: ")?;
    writer.flush()?;
    let mut model = read_line(reader)?.trim().to_string();
    if model.is_empty() {
        model = "claude-haiku-4-5".to_string();
    }

    apply_distiller_config(paths, &model)?;
    let env_path = paths.repo_root.join(".env");
    upsert_env_var(&env_path, "ANTHROPIC_API_KEY", &key)?;
    if !base_url.is_empty() {
        upsert_env_var(&env_path, "ANTHROPIC_BASE_URL", &base_url)?;
    }
    ensure_gitignored(&paths.repo_root, ".env")?;

    writeln!(
        writer,
        "\u{2713} Distiller configured (model {model}). Key stored in .env (gitignored). \
         Note: the key was entered in plain text."
    )?;
    Ok(true)
}

fn read_line<R: BufRead>(reader: &mut R) -> std::io::Result<String> {
    let mut line = String::new();
    reader.read_line(&mut line)?;
    Ok(line)
}

/// Load (or initialize) the workspace project config and flip the
/// distiller on with the chosen model.
fn apply_distiller_config(paths: &ProjectPaths, model: &str) -> std::io::Result<()> {
    // KimetsuResult's error is Box<dyn Error + Send + Sync>; it implements
    // Display, so to_string() works without naming a concrete error type.
    let io_err =
        |e: Box<dyn std::error::Error + Send + Sync>| std::io::Error::other(e.to_string());
    if !paths.project_toml.exists() {
        project::init_project(&paths.repo_root, false).map_err(io_err)?;
    }
    let mut config = project::load_config(paths).map_err(io_err)?;
    config.learning.distiller.enabled = true;
    config.learning.distiller.provider = "anthropic".to_string();
    config.learning.distiller.model = model.to_string();
    config.learning.distiller.api_key_env = "ANTHROPIC_API_KEY".to_string();
    config.learning.distiller.base_url_env = "ANTHROPIC_BASE_URL".to_string();
    let toml = config.to_toml().map_err(io_err)?;
    fs::write(&paths.project_toml, toml)
}

/// Insert or replace `NAME=value` in a `.env` file (created if missing).
fn upsert_env_var(env_path: &Path, name: &str, value: &str) -> std::io::Result<()> {
    let existing = fs::read_to_string(env_path).unwrap_or_default();
    let mut lines: Vec<String> = Vec::new();
    let mut replaced = false;
    for line in existing.lines() {
        if line
            .split_once('=')
            .map(|(k, _)| k.trim() == name)
            .unwrap_or(false)
        {
            lines.push(format!("{name}={value}"));
            replaced = true;
        } else {
            lines.push(line.to_string());
        }
    }
    if !replaced {
        lines.push(format!("{name}={value}"));
    }
    let mut body = lines.join("\n");
    body.push('\n');
    fs::write(env_path, body)
}

/// Ensure `entry` is present in the repo's `.gitignore` (created if absent).
fn ensure_gitignored(repo_root: &Path, entry: &str) -> std::io::Result<()> {
    let path = repo_root.join(".gitignore");
    let existing = fs::read_to_string(&path).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == entry) {
        return Ok(());
    }
    let mut body = existing;
    if !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }
    body.push_str(entry);
    body.push('\n');
    fs::write(&path, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    // CRITICAL: git_init_boundary makes the temp dir its own git toplevel so
    // ProjectPaths::discover doesn't climb to the real dev repo.
    fn paths_for(root: &Path) -> ProjectPaths {
        kimetsu_core::paths::git_init_boundary(root);
        ProjectPaths::discover(root).expect("discover temp paths")
    }

    #[test]
    fn wizard_writes_env_and_config() {
        let root = std::env::temp_dir().join(format!(
            "kimetsu_wizard_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join(".kimetsu")).unwrap();
        let paths = paths_for(&root);

        // y -> claude -> key -> base url (LiteLLM) -> blank model (default).
        let mut input =
            Cursor::new("y\nclaude\nsk-litellm-123\nhttp://localhost:4000\n\n".as_bytes().to_vec());
        let mut output = Vec::new();
        let configured = run_harvest_setup(&mut input, &mut output, &paths).unwrap();
        assert!(configured);

        let env = fs::read_to_string(paths.repo_root.join(".env")).unwrap();
        assert!(env.contains("ANTHROPIC_API_KEY=sk-litellm-123"));
        assert!(env.contains("ANTHROPIC_BASE_URL=http://localhost:4000"));
        let toml = fs::read_to_string(&paths.project_toml).unwrap();
        assert!(toml.contains("enabled = true"));
        assert!(toml.contains("claude-haiku-4-5"));
        assert!(
            fs::read_to_string(paths.repo_root.join(".gitignore"))
                .unwrap()
                .contains(".env")
        );

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn wizard_declined_writes_nothing() {
        let root = std::env::temp_dir().join(format!(
            "kimetsu_wizard_no_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let paths = paths_for(&root);
        let mut input = Cursor::new(b"n\n".to_vec());
        let mut output = Vec::new();
        assert!(!run_harvest_setup(&mut input, &mut output, &paths).unwrap());
        assert!(!paths.repo_root.join(".env").exists());
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn upsert_env_var_replaces_existing() {
        let dir = std::env::temp_dir().join(format!(
            "kimetsu_env_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let env = dir.join(".env");
        upsert_env_var(&env, "ANTHROPIC_API_KEY", "old").unwrap();
        upsert_env_var(&env, "OTHER", "keep").unwrap();
        upsert_env_var(&env, "ANTHROPIC_API_KEY", "new").unwrap();
        let body = fs::read_to_string(&env).unwrap();
        assert!(body.contains("ANTHROPIC_API_KEY=new"));
        assert!(!body.contains("ANTHROPIC_API_KEY=old"));
        assert!(body.contains("OTHER=keep"));
        fs::remove_dir_all(dir).ok();
    }
}
