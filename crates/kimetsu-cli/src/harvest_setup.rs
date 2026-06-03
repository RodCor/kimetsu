//! Interactive `kimetsu plugin install` wizard that configures the
//! credentialed SessionEnd distiller: collects a provider API key
//! (+ optional compatible endpoint base URL) + model, writes a
//! gitignored `.env`, and flips `[learning.distiller]` on in the
//! workspace project.toml.

use std::fs;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use kimetsu_core::config::ProjectConfig;

/// Where the wizard writes the distiller config + secret. Built by the CLI gate
/// from the install scope (workspace dir, or `~/.kimetsu`).
pub struct SetupTarget {
    pub project_toml: PathBuf,
    pub env_path: PathBuf,
    pub gitignore_dir: PathBuf,
}

#[derive(Debug, Clone, Copy)]
struct DistillerProviderChoice {
    provider: &'static str,
    api_key_env: &'static str,
    base_url_env: &'static str,
    default_model: &'static str,
    key_prompt: &'static str,
    base_url_prompt: &'static str,
}

/// Run the wizard against the given reader/writer (real stdin/stdout in
/// production; scripted in tests). Returns Ok(true) when the distiller was
/// configured, Ok(false) when the user declined or aborted.
pub fn run_harvest_setup<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    target: &SetupTarget,
    scope_label: &str,
) -> std::io::Result<bool> {
    write!(writer, "Set up the auto-harvest distiller now? [y/N]: ")?;
    writer.flush()?;
    if !read_line(reader)?.trim().eq_ignore_ascii_case("y") {
        return Ok(false);
    }

    write!(writer, "Harness [claude/codex] [claude]: ")?;
    writer.flush()?;
    let harness = read_line(reader)?.trim().to_lowercase();
    match harness.as_str() {
        "codex" | "openai-codex" | "cx" => {}
        // Blank defaults to claude; accept the common aliases.
        "" | "claude" | "claude-code" | "cc" => {}
        other => {
            writeln!(writer, "Unrecognized harness '{other}' - skipping setup.")?;
            return Ok(false);
        }
    }

    write!(
        writer,
        "Distiller provider [anthropic/openai] [anthropic]: "
    )?;
    writer.flush()?;
    let provider_input = read_line(reader)?.trim().to_string();
    let Some(choice) = resolve_distiller_provider(&provider_input) else {
        writeln!(
            writer,
            "Unrecognized distiller provider '{provider_input}' - skipping setup."
        )?;
        return Ok(false);
    };

    write!(writer, "{}", choice.key_prompt)?;
    writer.flush()?;
    let key = read_line(reader)?.trim().to_string();
    if key.is_empty() {
        writeln!(writer, "No key entered - skipping setup.")?;
        return Ok(false);
    }

    write!(writer, "{}", choice.base_url_prompt)?;
    writer.flush()?;
    let base_url = read_line(reader)?.trim().to_string();

    write!(writer, "Model [{}]: ", choice.default_model)?;
    writer.flush()?;
    let mut model = read_line(reader)?.trim().to_string();
    if model.is_empty() {
        model = choice.default_model.to_string();
    }

    apply_distiller_config(
        &target.project_toml,
        choice.provider,
        &model,
        choice.api_key_env,
        choice.base_url_env,
    )?;
    // Gitignore `.env` BEFORE writing the secret into it.
    ensure_gitignored(&target.gitignore_dir, ".env")?;
    upsert_env_var(&target.env_path, choice.api_key_env, &key)?;
    if !base_url.is_empty() {
        upsert_env_var(&target.env_path, choice.base_url_env, &base_url)?;
    }

    writeln!(
        writer,
        "\u{2713} Distiller configured for {scope_label} ({} model {model}). \
         Key stored in {} (gitignored). Note: the key was entered in plain text.",
        choice.provider,
        target.env_path.display()
    )?;
    Ok(true)
}

fn read_line<R: BufRead>(reader: &mut R) -> std::io::Result<String> {
    let mut line = String::new();
    reader.read_line(&mut line)?;
    Ok(line)
}

fn resolve_distiller_provider(input: &str) -> Option<DistillerProviderChoice> {
    match input.trim().to_ascii_lowercase().as_str() {
        "" | "anthropic" | "claude" | "claude-code" | "litellm" => Some(DistillerProviderChoice {
            provider: "anthropic",
            api_key_env: "ANTHROPIC_API_KEY",
            base_url_env: "ANTHROPIC_BASE_URL",
            default_model: "claude-haiku-4-5",
            key_prompt: "Anthropic API key (or Anthropic-compatible LiteLLM key): ",
            base_url_prompt: "ANTHROPIC_BASE_URL (optional; blank for Anthropic, set for LiteLLM): ",
        }),
        "openai" | "oai" | "gpt" => Some(DistillerProviderChoice {
            provider: "openai",
            api_key_env: "OPENAI_API_KEY",
            base_url_env: "OPENAI_BASE_URL",
            default_model: "gpt-5.4-mini",
            key_prompt: "OpenAI API key (or OpenAI-compatible endpoint key): ",
            base_url_prompt: "OPENAI_BASE_URL (optional; blank for OpenAI, accepts root or /v1): ",
        }),
        _ => None,
    }
}

/// Flip the distiller on in the project config, writing to `project_toml`
/// directly. Loads an existing `project.toml` if present, else starts from a
/// default — it does NOT call `init_project` (which would climb to an
/// enclosing git repo and open the brain DB), so the config + secret never
/// land in a parent repository.
fn apply_distiller_config(
    project_toml: &Path,
    provider: &str,
    model: &str,
    api_key_env: &str,
    base_url_env: &str,
) -> std::io::Result<()> {
    let io_err = |e: Box<dyn std::error::Error + Send + Sync>| std::io::Error::other(e.to_string());
    let mut config = if project_toml.exists() {
        ProjectConfig::from_toml(&fs::read_to_string(project_toml)?).map_err(io_err)?
    } else {
        // <root>/.kimetsu/project.toml -> project_id from <root>.
        let project_id = project_toml
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("workspace")
            .to_string();
        ProjectConfig::default_for_project(project_id)
    };
    config.learning.distiller.enabled = true;
    config.learning.distiller.provider = provider.to_string();
    config.learning.distiller.model = model.to_string();
    config.learning.distiller.api_key_env = api_key_env.to_string();
    config.learning.distiller.base_url_env = base_url_env.to_string();
    if let Some(parent) = project_toml.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(project_toml, config.to_toml().map_err(io_err)?)
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

    fn target_at(dir: &Path) -> SetupTarget {
        SetupTarget {
            project_toml: dir.join(".kimetsu").join("project.toml"),
            env_path: dir.join(".env"),
            gitignore_dir: dir.to_path_buf(),
        }
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
        let mut input = Cursor::new(
            "y\nclaude\n\nsk-litellm-123\nhttp://localhost:4000\n\n"
                .as_bytes()
                .to_vec(),
        );
        let mut output = Vec::new();
        let configured =
            run_harvest_setup(&mut input, &mut output, &target_at(&root), "this workspace")
                .unwrap();
        assert!(configured);

        let env = fs::read_to_string(root.join(".env")).unwrap();
        assert!(env.contains("ANTHROPIC_API_KEY=sk-litellm-123"));
        assert!(env.contains("ANTHROPIC_BASE_URL=http://localhost:4000"));
        let toml = fs::read_to_string(root.join(".kimetsu").join("project.toml")).unwrap();
        assert!(toml.contains("enabled = true"));
        assert!(toml.contains("provider = \"anthropic\""));
        assert!(toml.contains("claude-haiku-4-5"));
        assert!(
            fs::read_to_string(root.join(".gitignore"))
                .unwrap()
                .contains(".env")
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn wizard_accepts_codex_harness() {
        let root = std::env::temp_dir().join(format!(
            "kimetsu_wizard_codex_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join(".kimetsu")).unwrap();
        let mut input = Cursor::new(
            "y\ncodex\nopenai\nsk-openai-codex\nhttp://localhost:4000/v1\n\n"
                .as_bytes()
                .to_vec(),
        );
        let mut output = Vec::new();
        let configured =
            run_harvest_setup(&mut input, &mut output, &target_at(&root), "this workspace")
                .unwrap();
        assert!(configured);

        let out = String::from_utf8(output).unwrap();
        assert!(!out.contains("not supported"));
        let env = fs::read_to_string(root.join(".env")).unwrap();
        assert!(env.contains("OPENAI_API_KEY=sk-openai-codex"));
        assert!(env.contains("OPENAI_BASE_URL=http://localhost:4000/v1"));
        assert!(!env.contains("ANTHROPIC_API_KEY"));
        let toml = fs::read_to_string(root.join(".kimetsu").join("project.toml")).unwrap();
        assert!(toml.contains("enabled = true"));
        assert!(toml.contains("provider = \"openai\""));
        assert!(toml.contains("gpt-5.4-mini"));
        assert!(toml.contains("api_key_env = \"OPENAI_API_KEY\""));
        assert!(toml.contains("base_url_env = \"OPENAI_BASE_URL\""));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn wizard_accepts_openai_custom_model() {
        let root = std::env::temp_dir().join(format!(
            "kimetsu_wizard_openai_model_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join(".kimetsu")).unwrap();
        let mut input = Cursor::new(
            "y\ncodex\nopenai\nsk-openai-custom\n\nmy-distiller-model\n"
                .as_bytes()
                .to_vec(),
        );
        let mut output = Vec::new();
        let configured =
            run_harvest_setup(&mut input, &mut output, &target_at(&root), "this workspace")
                .unwrap();
        assert!(configured);

        let toml = fs::read_to_string(root.join(".kimetsu").join("project.toml")).unwrap();
        assert!(toml.contains("provider = \"openai\""));
        assert!(toml.contains("model = \"my-distiller-model\""));
        assert!(!toml.contains("model = \"gpt-5.4-mini\""));
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
        let paths = target_at(&root);
        let mut input = Cursor::new(b"n\n".to_vec());
        let mut output = Vec::new();
        assert!(!run_harvest_setup(&mut input, &mut output, &paths, "this workspace").unwrap());
        assert!(!root.join(".env").exists());
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn wizard_unrecognized_harness_aborts() {
        let root = std::env::temp_dir().join(format!(
            "kimetsu_wizard_bad_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let paths = target_at(&root);
        let mut input = Cursor::new(b"y\ngemini\n".to_vec());
        let mut output = Vec::new();
        assert!(!run_harvest_setup(&mut input, &mut output, &paths, "this workspace").unwrap());
        assert!(!root.join(".env").exists());
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn wizard_unrecognized_provider_aborts() {
        let root = std::env::temp_dir().join(format!(
            "kimetsu_wizard_bad_provider_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let paths = target_at(&root);
        let mut input = Cursor::new(b"y\nclaude\ngemini\n".to_vec());
        let mut output = Vec::new();
        assert!(!run_harvest_setup(&mut input, &mut output, &paths, "this workspace").unwrap());
        assert!(!root.join(".env").exists());
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
