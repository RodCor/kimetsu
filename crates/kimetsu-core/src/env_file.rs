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

        // `export KEY=value` is shell-style dotenv. Strip a leading
        // `export` only when whitespace follows, so `export_region=...`
        // stays a key named `export_region`.
        let line = strip_optional_export(line);
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

/// Drop a leading shell `export` keyword. `exportKEY=value` is not the
/// keyword form and is returned unchanged.
fn strip_optional_export(line: &str) -> &str {
    let Some(rest) = line.strip_prefix("export") else {
        return line;
    };
    if rest.starts_with(char::is_whitespace) {
        rest.trim_start()
    } else {
        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn write_env(body: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("kimetsu-dotenv-{nanos}"));
        fs::create_dir_all(&dir).expect("temp dir");
        fs::write(dir.join(".env"), body).expect("write .env");
        dir
    }

    #[test]
    fn read_dotenv_reads_plain_quoted_and_spaced_assignments() {
        let dir = write_env(
            "\
# comment
PLAIN=plain-value
QUOTED=\"quoted value\"
SPACED = spaced-value
SINGLE='single-value'
EMPTY=
",
        );
        assert_eq!(read_dotenv(&dir, "PLAIN").as_deref(), Some("plain-value"));
        assert_eq!(read_dotenv(&dir, "QUOTED").as_deref(), Some("quoted value"));
        assert_eq!(read_dotenv(&dir, "SPACED").as_deref(), Some("spaced-value"));
        assert_eq!(read_dotenv(&dir, "SINGLE").as_deref(), Some("single-value"));
        assert_eq!(read_dotenv(&dir, "EMPTY"), None);
        assert_eq!(read_dotenv(&dir, "MISSING"), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_dotenv_reads_export_assignments() {
        let dir = write_env(
            "\
# shell-style dotenv
export KIMETSU_DOTENV_EXPORT_PROBE=\"quoted-value\"
export KIMETSU_DOTENV_EXPORT_BARE=bare-value
export KIMETSU_DOTENV_EXPORT_SPACED = spaced-value
export KIMETSU_DOTENV_EXPORT_SINGLE='single-value'
export_region=not-the-keyword
",
        );
        assert_eq!(
            read_dotenv(&dir, "KIMETSU_DOTENV_EXPORT_PROBE").as_deref(),
            Some("quoted-value"),
            "export KEY=\"value\" must not be read as unset"
        );
        assert_eq!(
            read_dotenv(&dir, "KIMETSU_DOTENV_EXPORT_BARE").as_deref(),
            Some("bare-value")
        );
        assert_eq!(
            read_dotenv(&dir, "KIMETSU_DOTENV_EXPORT_SPACED").as_deref(),
            Some("spaced-value")
        );
        assert_eq!(
            read_dotenv(&dir, "KIMETSU_DOTENV_EXPORT_SINGLE").as_deref(),
            Some("single-value")
        );
        // `export` is only a keyword when whitespace follows it.
        assert_eq!(
            read_dotenv(&dir, "export_region").as_deref(),
            Some("not-the-keyword")
        );
        assert_eq!(read_dotenv(&dir, "region"), None);
        let _ = fs::remove_dir_all(&dir);
    }
}
