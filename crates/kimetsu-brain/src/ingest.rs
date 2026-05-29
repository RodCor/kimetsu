use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

use ignore::{DirEntry, WalkBuilder};
use kimetsu_core::KimetsuResult;
use kimetsu_core::config::ProjectConfig;
use kimetsu_core::paths::ProjectPaths;
use rusqlite::{Connection, params};
use time::OffsetDateTime;

#[derive(Debug, Clone, Default)]
pub struct RepoIngestSummary {
    pub repo_root: PathBuf,
    pub indexed_files: usize,
    pub skipped_files: usize,
    pub manifests: usize,
}

#[derive(Debug, Clone)]
struct IndexedFile {
    path: String,
    hash: String,
    size: u64,
    mtime: String,
    language_guess: String,
    snippet: String,
    manifest: Option<ManifestRecord>,
}

#[derive(Debug, Clone)]
struct ManifestRecord {
    path: String,
    kind: String,
    parsed_summary_json: String,
    hash: String,
    mtime: String,
}

pub fn ingest_repo(
    conn: &Connection,
    paths: &ProjectPaths,
    config: &ProjectConfig,
) -> KimetsuResult<RepoIngestSummary> {
    let repo_root = paths.repo_root.canonicalize()?;
    let skip_dirs = skip_dirs(config);
    let mut builder = WalkBuilder::new(&repo_root);
    builder
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .filter_entry(move |entry| should_descend(entry, &skip_dirs));

    let mut indexed = Vec::new();
    let mut skipped = 0usize;

    for result in builder.build() {
        let entry = match result {
            Ok(entry) => entry,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };

        let path = entry.path();
        if path == repo_root {
            continue;
        }

        let Some(file_type) = entry.file_type() else {
            skipped += 1;
            continue;
        };
        if !file_type.is_file() {
            continue;
        }

        match index_file(&repo_root, path, config.ingestion.max_file_bytes) {
            Ok(Some(file)) => indexed.push(file),
            Ok(None) => skipped += 1,
            Err(_) => skipped += 1,
        }

        if indexed.len() >= config.ingestion.max_total_files as usize {
            break;
        }
    }

    let tx = conn.unchecked_transaction()?;
    let repo_root_text = repo_root.to_string_lossy().to_string();
    tx.execute(
        "DELETE FROM repo_files WHERE repo_root = ?1",
        params![repo_root_text],
    )?;
    tx.execute(
        "DELETE FROM repo_files_fts WHERE repo_root = ?1",
        params![repo_root_text],
    )?;
    tx.execute(
        "DELETE FROM repo_manifests WHERE repo_root = ?1",
        params![repo_root_text],
    )?;
    tx.execute(
        "DELETE FROM repo_manifests_fts WHERE repo_root = ?1",
        params![repo_root_text],
    )?;

    let mut manifests = 0usize;
    for file in &indexed {
        tx.execute(
            "
            INSERT INTO repo_files (
                repo_root, path, hash, size, mtime, language_guess, snippet
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            ",
            params![
                repo_root_text,
                file.path,
                file.hash,
                file.size,
                file.mtime,
                file.language_guess,
                file.snippet
            ],
        )?;
        tx.execute(
            "
            INSERT INTO repo_files_fts (repo_root, path, snippet, language_guess)
            VALUES (?1, ?2, ?3, ?4)
            ",
            params![repo_root_text, file.path, file.snippet, file.language_guess],
        )?;

        if let Some(manifest) = &file.manifest {
            manifests += 1;
            tx.execute(
                "
                INSERT INTO repo_manifests (
                    repo_root, manifest_path, manifest_kind,
                    parsed_summary_json, hash, mtime
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                ",
                params![
                    repo_root_text,
                    manifest.path,
                    manifest.kind,
                    manifest.parsed_summary_json,
                    manifest.hash,
                    manifest.mtime
                ],
            )?;
            tx.execute(
                "
                INSERT INTO repo_manifests_fts (
                    repo_root, manifest_path, manifest_kind, parsed_summary_json
                )
                VALUES (?1, ?2, ?3, ?4)
                ",
                params![
                    repo_root_text,
                    manifest.path,
                    manifest.kind,
                    manifest.parsed_summary_json
                ],
            )?;
        }
    }

    tx.commit()?;

    Ok(RepoIngestSummary {
        repo_root,
        indexed_files: indexed.len(),
        skipped_files: skipped,
        manifests,
    })
}

fn skip_dirs(config: &ProjectConfig) -> HashSet<String> {
    let mut skip = [
        ".git",
        ".kimetsu",
        "node_modules",
        "target",
        "dist",
        "build",
        ".next",
        "vendor",
        ".venv",
        "__pycache__",
    ]
    .into_iter()
    .map(str::to_string)
    .collect::<HashSet<_>>();

    for extra in &config.ingestion.extra_skip_dirs {
        skip.insert(extra.clone());
    }
    skip
}

fn should_descend(entry: &DirEntry, skip_dirs: &HashSet<String>) -> bool {
    if let Some(name) = entry.file_name().to_str() {
        return !skip_dirs.contains(name);
    }
    true
}

fn index_file(
    repo_root: &Path,
    path: &Path,
    max_file_bytes: u64,
) -> KimetsuResult<Option<IndexedFile>> {
    let rel = repo_relative_path(repo_root, path)?;
    if is_secret_path(&rel) || is_binary_extension(&rel) {
        return Ok(None);
    }

    let metadata = fs::metadata(path)?;
    if metadata.len() > max_file_bytes {
        return Ok(None);
    }

    let bytes = fs::read(path)?;
    if looks_binary(&bytes) {
        return Ok(None);
    }

    let hash = blake3::hash(&bytes).to_hex().to_string();
    let snippet_len = bytes.len().min(4096);
    let raw_snippet = String::from_utf8_lossy(&bytes[..snippet_len]).to_string();
    // Redact secrets before the snippet lands in brain.db. `is_secret_path`
    // only filters by filename, so an API key hardcoded inside an ordinary
    // source file (config.ts, settings.py, ...) would otherwise be stored
    // verbatim and surface in every future retrieval capsule. This mirrors the
    // redaction applied on the memory/proposal write paths in `project.rs`.
    let snippet = crate::redact::redact_secrets(&raw_snippet).text;
    let mtime = metadata
        .modified()
        .ok()
        .map(OffsetDateTime::from)
        .unwrap_or_else(OffsetDateTime::now_utc)
        .format(&time::format_description::well_known::Rfc3339)?;
    let language_guess = language_guess(&rel).to_string();
    let manifest = manifest_record(&rel, &snippet, &hash, &mtime);

    Ok(Some(IndexedFile {
        path: rel,
        hash,
        size: metadata.len(),
        mtime,
        language_guess,
        snippet,
        manifest,
    }))
}

fn repo_relative_path(repo_root: &Path, path: &Path) -> KimetsuResult<String> {
    let rel = path.strip_prefix(repo_root)?;
    let mut parts = Vec::new();
    for component in rel.components() {
        match component {
            Component::Normal(part) => {
                let Some(part) = part.to_str() else {
                    return Err("repo path is not valid UTF-8".into());
                };
                if part.is_empty() || part == "." || part == ".." {
                    return Err(format!("invalid repo path component: {part}").into());
                }
                parts.push(part.to_string());
            }
            _ => return Err("repo path contains unsupported component".into()),
        }
    }
    Ok(parts.join("/"))
}

fn is_secret_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let file_name = lower.rsplit('/').next().unwrap_or(&lower);
    file_name == ".env"
        || file_name.starts_with(".env.")
        || file_name.ends_with(".pem")
        || file_name.ends_with(".key")
        || file_name.starts_with("id_rsa")
}

fn is_binary_extension(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    matches!(
        lower.rsplit('.').next(),
        Some(
            "png"
                | "jpg"
                | "jpeg"
                | "gif"
                | "webp"
                | "ico"
                | "pdf"
                | "zip"
                | "gz"
                | "xz"
                | "7z"
                | "rar"
                | "exe"
                | "dll"
                | "pdb"
                | "wasm"
                | "mp3"
                | "mp4"
                | "mov"
                | "avi"
                | "woff"
                | "woff2"
                | "ttf"
                | "otf"
        )
    )
}

fn looks_binary(bytes: &[u8]) -> bool {
    let scan_len = bytes.len().min(8192);
    if bytes[..scan_len].contains(&0) {
        return true;
    }

    bytes.starts_with(b"\x7fELF")
        || bytes.starts_with(b"MZ")
        || bytes.starts_with(b"%PDF")
        || bytes.starts_with(b"PK\x03\x04")
        || bytes.starts_with(b"\x89PNG")
        || bytes.starts_with(b"\xff\xd8\xff")
}

fn language_guess(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "rs" => "rust",
        "toml" => "toml",
        "json" => "json",
        "js" | "mjs" | "cjs" => "javascript",
        "ts" | "tsx" => "typescript",
        "jsx" => "javascript",
        "py" => "python",
        "go" => "go",
        "md" | "mdx" => "markdown",
        "yml" | "yaml" => "yaml",
        "html" => "html",
        "css" => "css",
        "sql" => "sql",
        _ => "unknown",
    }
}

fn manifest_record(path: &str, snippet: &str, hash: &str, mtime: &str) -> Option<ManifestRecord> {
    let kind = match path.rsplit('/').next()? {
        "Cargo.toml" => "cargo",
        "package.json" => "package_json",
        "pyproject.toml" => "pyproject",
        "go.mod" => "go_mod",
        _ => return None,
    };

    let parsed_summary_json = serde_json::json!({
        "kind": kind,
        "path": path,
        "preview": snippet.lines().take(12).collect::<Vec<_>>().join("\n"),
    })
    .to_string();

    Some(ManifestRecord {
        path: path.to_string(),
        kind: kind.to_string(),
        parsed_summary_json,
        hash: hash.to_string(),
        mtime: mtime.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn index_file_redacts_secrets_in_snippet() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let repo_root = std::env::temp_dir().join(format!("kimetsu_ingest_redact_{nanos}"));
        fs::create_dir_all(&repo_root).expect("repo root");
        // A secret hardcoded in an ordinary (non-secret-named) source file.
        let secret = "sk-ant-api03-AbCdEfGhIjKlMnOpQrStUv0123456789AbCdEf";
        let file = repo_root.join("config.ts");
        fs::write(&file, format!("export const KEY = \"{secret}\";\n")).expect("write file");

        let indexed = index_file(&repo_root, &file, 1_000_000)
            .expect("index_file")
            .expect("file should be indexed");

        assert!(
            indexed.snippet.contains("[REDACTED:anthropic_oauth]"),
            "snippet not redacted: {}",
            indexed.snippet
        );
        assert!(
            !indexed.snippet.contains(secret),
            "raw secret leaked into snippet"
        );

        fs::remove_dir_all(&repo_root).ok();
    }
}
