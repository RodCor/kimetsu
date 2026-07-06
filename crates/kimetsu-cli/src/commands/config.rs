//! config get/set + TOML editing helpers.
//! Split out of main.rs (v2.5.1); implementations only — the clap
//! surface stays in main.rs.

#![allow(unused_imports)]
use std::env;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use kimetsu_brain::project;
use kimetsu_core::KimetsuResult;
use kimetsu_core::memory::{MemoryKind, MemoryScope};

use crate::*;

pub(crate) fn config(command: ConfigCommand) -> KimetsuResult<()> {
    match command {
        ConfigCommand::Show => {
            print!("{}", project::config_text(&env::current_dir()?)?);
            Ok(())
        }
        ConfigCommand::Edit => {
            let cwd = env::current_dir()?;
            let paths = kimetsu_core::paths::ProjectPaths::discover(&cwd)?;
            config_edit_with(&paths.project_toml, |path| {
                // Resolve the editor: $EDITOR, then $VISUAL, then platform default.
                let editor = env::var("EDITOR")
                    .or_else(|_| env::var("VISUAL"))
                    .unwrap_or_else(|_| {
                        if cfg!(windows) {
                            "notepad".to_string()
                        } else {
                            "vi".to_string()
                        }
                    });
                let status = std::process::Command::new(&editor).arg(path).status()?;
                if status.success() {
                    Ok(())
                } else {
                    Err(std::io::Error::other(format!(
                        "editor `{editor}` exited with non-zero status: {status}"
                    )))
                }
            })
        }
        ConfigCommand::Get { key } => {
            let cwd = env::current_dir()?;
            let paths = kimetsu_core::paths::ProjectPaths::discover(&cwd)?;
            // Use the EFFECTIVE config (serde defaults filled in) so fields
            // like `embedder.enabled` show even when absent from the file.
            let cfg = project::load_config(&paths)?;
            let root: toml::Value = toml::Value::try_from(&cfg)
                .map_err(|e| format!("config get: failed to serialise config: {e}"))?;
            match get_toml_path(&root, &key) {
                Some(toml::Value::Table(t)) => {
                    // Pretty-print tables so the output is readable.
                    println!(
                        "{}",
                        toml::to_string(t)
                            .map_err(|e| format!("config get: serialise table: {e}"))?
                            .trim_end()
                    );
                }
                Some(toml::Value::Array(arr)) => {
                    println!(
                        "{}",
                        toml::to_string_pretty(&toml::Value::Array(arr.clone()))
                            .map_err(|e| format!("config get: serialise array: {e}"))?
                            .trim_end()
                    );
                }
                Some(leaf) => {
                    // Bare scalar: strip surrounding quotes for strings.
                    let rendered = toml::to_string_pretty(&toml::Value::Table({
                        let mut m = toml::map::Map::new();
                        m.insert("v".to_string(), leaf.clone());
                        m
                    }))
                    .map_err(|e| format!("config get: serialise scalar: {e}"))?;
                    // `toml::to_string_pretty` of `{v = <leaf>}` yields "v = <repr>\n".
                    // Strip the "v = " prefix and trailing newline.
                    let bare = rendered
                        .trim_end()
                        .strip_prefix("v = ")
                        .unwrap_or(rendered.trim_end());
                    println!("{bare}");
                }
                None => {
                    // Provide a helpful error listing the closest valid sub-keys.
                    let hint = closest_keys_hint(&root, &key);
                    return Err(format!("config get: key `{key}` not found.{hint}").into());
                }
            }
            Ok(())
        }
        ConfigCommand::Set { key, value } => {
            let cwd = env::current_dir()?;
            let paths = kimetsu_core::paths::ProjectPaths::discover(&cwd)?;

            let disk_text = std::fs::read_to_string(&paths.project_toml).map_err(|e| {
                format!(
                    "config set: could not read {}: {e}",
                    paths.project_toml.display()
                )
            })?;

            let (new_text, dropped_to_custom) = config_set_text(&disk_text, &key, &value)?;

            // Write — only reached when validation inside config_set_text passes.
            std::fs::write(&paths.project_toml, &new_text).map_err(|e| {
                format!(
                    "config set: failed to write {}: {e}",
                    paths.project_toml.display()
                )
            })?;

            println!("set {key} = {value}");
            if dropped_to_custom {
                println!(
                    "note: retrieval.level set to \"custom\" so this manual value is not \
                     overridden by a preset at load time."
                );
            }
            Ok(())
        }
    }
}

/// Keys whose values `ProjectConfig::apply_retrieval_level` overwrites when a
/// non-`custom` retrieval level (`basic`/`flexible`/`deep`/`advanced`) is active.
/// Setting one of these manually must drop the level to `custom`, otherwise the
/// explicit value is silently clobbered at load time.
pub(crate) fn is_level_managed_key(key: &str) -> bool {
    matches!(key, "embedder.enabled" | "embedder.reranker")
}

/// Core of `config set` (extracted so the command and its integration test share
/// one code path). Sets `key = value` in `disk_text` surgically (comments and
/// formatting preserved via `toml_edit`). When `key` is a retrieval-level-managed
/// field AND the current level is a managed preset, it ALSO sets
/// `retrieval.level = "custom"` so the explicit value survives
/// `apply_retrieval_level` at load. Validates the result through `ProjectConfig`.
///
/// Returns `(new_toml_text, dropped_to_custom)`. Pre-levels files (no `[retrieval]`
/// table → default level `custom`) are never modified beyond the requested key, so
/// existing behavior is byte-identical.
pub(crate) fn config_set_text(
    disk_text: &str,
    key: &str,
    value: &str,
) -> KimetsuResult<(String, bool)> {
    // Resolve the existing leaf type (for coercion) from a plain value tree.
    let root_val: toml::Value = toml::from_str(disk_text)
        .map_err(|e| format!("config set: project.toml is invalid TOML: {e}"))?;
    let existing = get_toml_path(&root_val, key).cloned();
    let typed_value =
        parse_scalar(value, existing.as_ref()).map_err(|e| format!("config set: {e}"))?;

    // Surgical edit on a comment-preserving document.
    let mut doc: toml_edit::DocumentMut = disk_text
        .parse()
        .map_err(|e| format!("config set: project.toml is invalid TOML (edit): {e}"))?;
    set_toml_edit_path(&mut doc, key, &typed_value).map_err(|e| format!("config set: {e}"))?;

    // Auto-drop to "custom" when overriding a preset-managed field, so the
    // explicit value is not clobbered by apply_retrieval_level on the next load.
    let mut dropped_to_custom = false;
    if is_level_managed_key(key) {
        let cur_level = root_val
            .get("retrieval")
            .and_then(|r| r.get("level"))
            .and_then(|l| l.as_str())
            .unwrap_or("custom");
        if matches!(cur_level, "basic" | "flexible" | "deep" | "advanced") {
            let custom = toml::Value::String("custom".to_string());
            set_toml_edit_path(&mut doc, "retrieval.level", &custom)
                .map_err(|e| format!("config set: {e}"))?;
            dropped_to_custom = true;
        }
    }

    let new_text = doc.to_string();
    project::load_config_from_text(&new_text).map_err(|e| {
        format!("config set: result is not a valid config — {e}. File NOT written.")
    })?;
    Ok((new_text, dropped_to_custom))
}

/// Testable seam for `config edit`. Opens the config file at `toml_path`
/// via the `edit` closure (which is either the real editor launch or a
/// test-injected closure that mutates the file), then re-parses the
/// result to catch syntax errors before returning.
///
/// Returns `Err` with a clear message if the editor fails or if the
/// resulting TOML is invalid. Prints a confirmation on success.
pub(crate) fn config_edit_with(
    toml_path: &std::path::Path,
    edit: impl FnOnce(&std::path::Path) -> std::io::Result<()>,
) -> KimetsuResult<()> {
    edit(toml_path).map_err(|err| format!("config edit: editor failed: {err}"))?;

    // Re-parse to catch syntax errors.
    let content = std::fs::read_to_string(toml_path)
        .map_err(|err| format!("config edit: could not read {}: {err}", toml_path.display()))?;
    project::load_config_from_text(&content)
        .map_err(|err| format!("config edit: saved file has invalid TOML — {err}"))?;

    println!("config saved: {}", toml_path.display());
    Ok(())
}

// ── config get/set pure helpers ──────────────────────────────────────────────

/// Navigate a dotted key path (`a.b.c`) through `root` and return a reference
/// to the leaf value, or `None` if any segment is missing.
pub(crate) fn get_toml_path<'a>(root: &'a toml::Value, key: &str) -> Option<&'a toml::Value> {
    let mut current = root;
    for segment in key.split('.') {
        current = current.get(segment)?;
    }
    Some(current)
}

/// Navigate/create a dotted key path (`a.b.c`) in `root` (a `toml::Value::Table`)
/// and set the leaf to `value`. Intermediate segments are created as empty tables
/// when absent. Returns `Err` if an intermediate segment exists but is not a table.
///
/// NOTE: this function is kept for unit tests only.  Production config writes use
/// `set_toml_edit_path` which preserves TOML comments.
#[cfg(test)]
pub(crate) fn set_toml_path(
    root: &mut toml::Value,
    key: &str,
    value: toml::Value,
) -> Result<(), String> {
    let segments: Vec<&str> = key.split('.').collect();
    let (leaf_key, parents) = segments
        .split_last()
        .ok_or_else(|| "key must not be empty".to_string())?;

    let mut current = root;
    for seg in parents {
        // Ensure the current node is a table.
        if !current.is_table() {
            return Err(format!(
                "cannot set `{key}`: `{seg}` is `{}`, not a table",
                current.type_str()
            ));
        }
        // Navigate into the segment, creating an empty table if absent.
        if current.get(seg).is_none() {
            current
                .as_table_mut()
                .unwrap()
                .insert(seg.to_string(), toml::Value::Table(toml::map::Map::new()));
        }
        current = current.get_mut(seg).unwrap();
    }
    if !current.is_table() {
        return Err(format!(
            "cannot set `{key}`: parent is `{}`, not a table",
            current.type_str()
        ));
    }
    current
        .as_table_mut()
        .unwrap()
        .insert(leaf_key.to_string(), value);
    Ok(())
}

/// S4.2 — Surgical, comment-preserving write via `toml_edit`.
///
/// Navigate/create a dotted key path (`a.b.c`) inside a `toml_edit::DocumentMut`
/// and overwrite the leaf with `value` (a `toml::Value` for type information).
/// Intermediate tables are created when absent. Returns `Err` when an
/// intermediate segment is not a table.
///
/// This preserves all TOML comments, whitespace, and unknown keys because
/// `toml_edit` operates on the concrete syntax tree rather than a typed struct.
pub(crate) fn set_toml_edit_path(
    doc: &mut toml_edit::DocumentMut,
    key: &str,
    value: &toml::Value,
) -> Result<(), String> {
    let segments: Vec<&str> = key.split('.').collect();
    let (leaf_key, parents) = segments
        .split_last()
        .ok_or_else(|| "key must not be empty".to_string())?;

    // Navigate into parent tables, creating inline tables when absent.
    let mut current: &mut toml_edit::Item = doc.as_item_mut();
    for seg in parents {
        // If the segment doesn't exist yet, insert an empty table.
        if current.get(seg).is_none() {
            if let Some(tbl) = current.as_table_mut() {
                tbl.insert(seg, toml_edit::Item::Table(toml_edit::Table::new()));
            } else {
                return Err(format!("cannot set `{key}`: `{seg}` is not a table"));
            }
        }
        current = current
            .get_mut(seg)
            .ok_or_else(|| format!("cannot set `{key}`: `{seg}` not found after insert"))?;
        if !current.is_table() && !current.is_inline_table() {
            return Err(format!("cannot set `{key}`: `{seg}` is not a table"));
        }
    }

    // Convert the toml::Value leaf into a toml_edit::Value.
    let edit_val: toml_edit::Value = match value {
        toml::Value::Boolean(b) => toml_edit::Value::from(*b),
        toml::Value::Integer(n) => toml_edit::Value::from(*n),
        toml::Value::Float(f) => toml_edit::Value::from(*f),
        toml::Value::String(s) => toml_edit::Value::from(s.as_str()),
        other => {
            // Fallback: round-trip through TOML text for complex types.
            let text = toml::to_string(other)
                .map_err(|e| format!("cannot serialise value for `{key}`: {e}"))?;
            text.trim()
                .parse::<toml_edit::Value>()
                .map_err(|e| format!("cannot parse serialised value for `{key}`: {e}"))?
        }
    };

    if let Some(tbl) = current.as_table_mut() {
        tbl.insert(leaf_key, toml_edit::Item::Value(edit_val));
    } else {
        return Err(format!("cannot set `{key}`: parent segment is not a table"));
    }

    Ok(())
}

/// Parse `input` into a typed `toml::Value`.
///
/// Type-resolution order:
/// 1. If `existing` is `Some`, coerce to its type (bool, integer, float, string).
///    Returns `Err` if coercion to integer or float fails so callers can surface a clear message.
/// 2. Otherwise infer from the literal:
///    - `"true"` / `"false"` → `Bool`
///    - All-digit string (optionally leading `-`) → `Integer`
///    - Parseable as `f64` → `Float`
///    - Anything else → `String`
pub(crate) fn parse_scalar(
    input: &str,
    existing: Option<&toml::Value>,
) -> Result<toml::Value, String> {
    match existing {
        Some(toml::Value::Boolean(_)) => {
            Ok(toml::Value::Boolean(input.eq_ignore_ascii_case("true")))
        }
        Some(toml::Value::Integer(_)) => {
            input.parse::<i64>().map(toml::Value::Integer).map_err(|_| {
                format!("cannot coerce `{input}` to integer (existing field is an integer)")
            })
        }
        Some(toml::Value::Float(_)) => input
            .parse::<f64>()
            .map(toml::Value::Float)
            .map_err(|_| format!("cannot coerce `{input}` to float (existing field is a float)")),
        Some(toml::Value::String(_)) => Ok(toml::Value::String(input.to_string())),
        // Array / table / datetime: fall through to literal inference.
        _ => Ok(infer_scalar(input)),
    }
}

/// Infer a `toml::Value` type from a bare string literal.
pub(crate) fn infer_scalar(input: &str) -> toml::Value {
    if input.eq_ignore_ascii_case("true") {
        return toml::Value::Boolean(true);
    }
    if input.eq_ignore_ascii_case("false") {
        return toml::Value::Boolean(false);
    }
    // Integer: optional leading `-`, then all digits.
    let digit_part = input.strip_prefix('-').unwrap_or(input);
    if !digit_part.is_empty() && digit_part.bytes().all(|b| b.is_ascii_digit()) {
        if let Ok(n) = input.parse::<i64>() {
            return toml::Value::Integer(n);
        }
    }
    if let Ok(f) = input.parse::<f64>() {
        // Distinguish "1.0" (float) from "1" (already caught as integer above).
        if input.contains('.') || input.contains('e') || input.contains('E') {
            return toml::Value::Float(f);
        }
    }
    toml::Value::String(input.to_string())
}

/// Build a human-readable hint listing the closest valid keys when `get` fails.
pub(crate) fn closest_keys_hint(root: &toml::Value, key: &str) -> String {
    // Walk as far as we can, then show the available keys at the stuck level.
    let segments: Vec<&str> = key.split('.').collect();
    let mut current = root;
    let mut walked = Vec::new();
    for seg in &segments {
        match current.get(seg) {
            Some(next) => {
                walked.push(*seg);
                current = next;
            }
            None => {
                // Show available keys at this level.
                if let Some(table) = current.as_table() {
                    let keys: Vec<&str> = table.keys().map(|k| k.as_str()).collect();
                    let prefix = if walked.is_empty() {
                        String::new()
                    } else {
                        format!(" Under `{}`:", walked.join("."))
                    };
                    return format!("{prefix} available keys: [{}]", keys.join(", "));
                }
                return String::new();
            }
        }
    }
    String::new()
}
