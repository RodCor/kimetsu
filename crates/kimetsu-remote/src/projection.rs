//! Portable durable records; SQLite files and FTS indexes are disposable.

use flate2::{Compression, bufread::GzDecoder, write::GzEncoder};
use kimetsu_core::{KIMETSU_SCHEMA_VERSION, paths::ProjectPaths};
use rusqlite::{
    Connection, OpenFlags, params_from_iter,
    types::{Value, ValueRef},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    io::{Read, Write},
    path::Path,
};

const MAGIC: &[u8] = b"KBRN\x01";
const LOGICAL_ROOT: &str = "@kimetsu-remote-root";
const MAX_DECODED_BYTES: usize = 256 * 1024 * 1024;
const MAX_ROWS: usize = 2_000_000;
type Result<T> = std::result::Result<T, String>;

/// Temporary database paths must not accumulate global ANN/cache registrations.
pub fn release(root: &Path) {
    #[cfg(feature = "embeddings")]
    if let Ok(conn) = Connection::open_with_flags(
        ProjectPaths::at_root(root).brain_db,
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) {
        kimetsu_brain::ann::forget_projection(&conn);
    }
    #[cfg(not(feature = "embeddings"))]
    let _ = root;
}

#[derive(Serialize, Deserialize)]
struct Checkpoint {
    format: u32,
    schema: i64,
    configuration: Option<String>,
    tables: Vec<Table>,
}
#[derive(Serialize, Deserialize)]
struct Table {
    name: String,
    columns: Vec<String>,
    rows: Vec<Vec<Cell>>,
}
#[derive(Clone, Serialize, Deserialize)]
enum Cell {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(#[serde(with = "serde_bytes")] Vec<u8>),
}

fn ident(value: &str) -> Result<String> {
    if value.is_empty() || !value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err("checkpoint contains an invalid schema identifier".into());
    }
    Ok(format!("\"{value}\""))
}

fn layout(conn: &Connection) -> Result<BTreeMap<String, Vec<String>>> {
    let mut statement = conn.prepare("SELECT name FROM pragma_table_list WHERE schema='main' AND type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .map_err(|_| "cannot inspect checkpoint schema")?;
    let names = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|_| "cannot inspect checkpoint tables")?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|_| "cannot inspect checkpoint tables")?;
    let mut result = BTreeMap::new();
    for name in names {
        if matches!(name.as_str(), "schema_info" | "corpus_revision") {
            continue;
        }
        let mut statement = conn
            .prepare(&format!("PRAGMA table_info({})", ident(&name)?))
            .map_err(|_| "cannot inspect checkpoint columns")?;
        let mut columns = vec!["rowid".into()];
        columns.extend(
            statement
                .query_map([], |row| row.get::<_, String>(1))
                .map_err(|_| "cannot inspect checkpoint columns")?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|_| "cannot inspect checkpoint columns")?,
        );
        result.insert(name, columns);
    }
    Ok(result)
}

fn rebase_json(value: &mut serde_json::Value, from: &[String], to: &str) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                if matches!(key.as_str(), "repo_root" | "workdir" | "cwd")
                    && value
                        .as_str()
                        .is_some_and(|text| from.iter().any(|old| old == text))
                {
                    *value = serde_json::Value::String(to.to_string());
                } else {
                    rebase_json(value, from, to);
                }
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                rebase_json(value, from, to);
            }
        }
        _ => {}
    }
}

fn rebase(cell: &mut Cell, column: &str, from: &[String], to: &str) {
    let Cell::Text(text) = cell else { return };
    if column == "repo_root" && from.iter().any(|old| old == text) {
        *text = to.to_string();
    } else if column.ends_with("_json") {
        if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(text) {
            rebase_json(&mut value, from, to);
            // Canonicalize JSON object ordering on both sides of the round trip.
            *text = value.to_string();
        }
    }
}

pub fn capture(root: &Path) -> Result<Vec<u8>> {
    let paths = ProjectPaths::at_root(root);
    let conn = Connection::open_with_flags(&paths.brain_db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|_| "cannot open projection for capture")?;
    kimetsu_brain::schema::validate(&conn).map_err(|_| "projection has an incompatible schema")?;
    let transaction = conn
        .unchecked_transaction()
        .map_err(|_| "cannot read a consistent projection")?;
    let mut aliases = vec![root.to_string_lossy().into_owned()];
    if let Ok(canonical) = root.canonicalize() {
        aliases.push(canonical.to_string_lossy().into_owned());
        aliases.push(kimetsu_core::paths::display_path(&canonical));
    }
    let mut tables = Vec::new();
    let mut total_rows = 0usize;
    let mut approximate_bytes = 0usize;
    for (name, columns) in layout(&transaction)? {
        let query = format!(
            "SELECT {} FROM {} ORDER BY rowid",
            columns
                .iter()
                .map(|c| ident(c))
                .collect::<Result<Vec<_>>>()?
                .join(","),
            ident(&name)?
        );
        let mut statement = transaction
            .prepare(&query)
            .map_err(|_| "cannot read checkpoint records")?;
        let mut cursor = statement
            .query([])
            .map_err(|_| "cannot read checkpoint records")?;
        let mut rows = Vec::new();
        while let Some(row) = cursor.next().map_err(|_| "cannot read checkpoint row")? {
            total_rows += 1;
            if total_rows > MAX_ROWS {
                return Err("checkpoint has too many records".into());
            }
            let mut cells = Vec::new();
            for (idx, column) in columns.iter().enumerate() {
                let mut cell = match row
                    .get_ref(idx)
                    .map_err(|_| "cannot read checkpoint value")?
                {
                    ValueRef::Null => Cell::Null,
                    ValueRef::Integer(value) => Cell::Integer(value),
                    ValueRef::Real(value) if value.is_finite() => Cell::Real(value),
                    ValueRef::Real(_) => {
                        return Err("checkpoint contains a non-finite number".into());
                    }
                    ValueRef::Text(value) => Cell::Text(
                        std::str::from_utf8(value)
                            .map_err(|_| "checkpoint contains invalid UTF-8")?
                            .to_string(),
                    ),
                    ValueRef::Blob(value) => Cell::Blob(value.to_vec()),
                };
                approximate_bytes = approximate_bytes.saturating_add(match &cell {
                    Cell::Text(value) => value.len(),
                    Cell::Blob(value) => value.len(),
                    _ => 16,
                });
                if approximate_bytes > MAX_DECODED_BYTES {
                    return Err("checkpoint exceeds decoded size limit".into());
                }
                rebase(&mut cell, column, &aliases, LOGICAL_ROOT);
                cells.push(cell);
            }
            rows.push(cells);
        }
        tables.push(Table {
            name,
            columns,
            rows,
        });
    }
    transaction
        .commit()
        .map_err(|_| "cannot finish checkpoint read")?;
    let configuration = match std::fs::read_to_string(&paths.project_toml) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return Err("cannot read projection configuration".into()),
    };
    let encoded = rmp_serde::to_vec(&Checkpoint {
        format: 1,
        schema: KIMETSU_SCHEMA_VERSION,
        configuration,
        tables,
    })
    .map_err(|_| "cannot encode checkpoint")?;
    if encoded.len() > MAX_DECODED_BYTES {
        return Err("checkpoint exceeds decoded size limit".into());
    }
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder
        .write_all(&encoded)
        .map_err(|_| "cannot compress checkpoint")?;
    let compressed = encoder
        .finish()
        .map_err(|_| "cannot finish checkpoint compression")?;
    if compressed.len().saturating_add(MAGIC.len()) > crate::storage::MAX_STATE_BYTES {
        return Err("checkpoint exceeds compressed size limit".into());
    }
    let mut result = MAGIC.to_vec();
    result.extend(compressed);
    Ok(result)
}

pub fn restore(root: &Path, bytes: &[u8]) -> Result<()> {
    if bytes.len() > crate::storage::MAX_STATE_BYTES || !bytes.starts_with(MAGIC) {
        return Err("invalid checkpoint format or compressed size".into());
    }
    let mut decoded = Vec::new();
    let mut decoder = GzDecoder::new(&bytes[MAGIC.len()..]);
    decoder
        .by_ref()
        .take(MAX_DECODED_BYTES as u64 + 1)
        .read_to_end(&mut decoded)
        .map_err(|_| "checkpoint compression is corrupt")?;
    if decoded.len() > MAX_DECODED_BYTES {
        return Err("checkpoint exceeds decoded size limit".into());
    }
    if !decoder.get_ref().is_empty() {
        return Err("checkpoint contains trailing compressed data".into());
    }
    let mut records = rmp_serde::Deserializer::new(std::io::Cursor::new(&decoded));
    let checkpoint =
        Checkpoint::deserialize(&mut records).map_err(|_| "checkpoint records are corrupt")?;
    if records.into_inner().position() != decoded.len() as u64 {
        return Err("checkpoint contains trailing records".into());
    }
    if checkpoint.format != 1 || checkpoint.schema != KIMETSU_SCHEMA_VERSION {
        return Err("checkpoint format/schema requires an explicit migration".into());
    }
    let paths = ProjectPaths::at_root(root);
    if paths.brain_db.exists() {
        return Err("restore target already contains a brain".into());
    }
    let configuration = checkpoint.configuration.unwrap_or_else(|| {
        kimetsu_core::config::ProjectConfig::default_for_project("remote")
            .to_toml()
            .unwrap_or_default()
    });
    kimetsu_core::config::ProjectConfig::from_toml(&configuration)
        .map_err(|_| "checkpoint configuration is invalid")?;
    let schema = Connection::open_in_memory().map_err(|_| "cannot validate checkpoint schema")?;
    kimetsu_brain::schema::initialize(&schema)
        .map_err(|_| "cannot initialize validation schema")?;
    let expected = layout(&schema)?;
    let mut names = HashSet::new();
    let mut count = 0usize;
    for table in &checkpoint.tables {
        let Some(columns) = expected.get(&table.name) else {
            return Err("checkpoint contains an unknown table".into());
        };
        if !names.insert(&table.name) || &table.columns != columns {
            return Err("checkpoint contains invalid table columns".into());
        }
        count = count.saturating_add(table.rows.len());
        if count > MAX_ROWS {
            return Err("checkpoint has too many records".into());
        }
        if table.rows.iter().any(|row| row.len() != columns.len()) {
            return Err("checkpoint row has the wrong number of columns".into());
        }
    }
    if names.len() != expected.len() {
        return Err("checkpoint is missing required tables".into());
    }
    std::fs::create_dir_all(&paths.kimetsu_dir)
        .map_err(|_| "cannot create temporary projection")?;
    let temporary = paths
        .kimetsu_dir
        .join(format!("restore-{}.db", ulid::Ulid::new()));
    let mut conn = Connection::open(&temporary).map_err(|_| "cannot create temporary database")?;
    kimetsu_brain::schema::initialize(&conn).map_err(|_| "cannot initialize temporary schema")?;
    conn.pragma_update(None, "foreign_keys", false)
        .map_err(|_| "cannot initialize import")?;
    let transaction = conn
        .transaction()
        .map_err(|_| "cannot begin checkpoint import")?;
    for name in expected.keys() {
        transaction
            .execute(&format!("DELETE FROM {}", ident(name)?), [])
            .map_err(|_| "cannot reset temporary table")?;
    }
    // Ingest and query handlers filter by the canonical root (including the
    // Windows extended-path prefix), so restored rows must use that identity.
    let destination = root
        .canonicalize()
        .map_err(|_| "cannot resolve projection root")?
        .to_string_lossy()
        .into_owned();
    for table in checkpoint.tables {
        let query = format!(
            "INSERT INTO {} ({}) VALUES ({})",
            ident(&table.name)?,
            table
                .columns
                .iter()
                .map(|c| ident(c))
                .collect::<Result<Vec<_>>>()?
                .join(","),
            (1..=table.columns.len())
                .map(|idx| format!("?{idx}"))
                .collect::<Vec<_>>()
                .join(",")
        );
        let mut statement = transaction
            .prepare(&query)
            .map_err(|_| "invalid checkpoint insert")?;
        for row in table.rows {
            let mut values = Vec::new();
            for (mut cell, column) in row.into_iter().zip(&table.columns) {
                rebase(&mut cell, column, &[LOGICAL_ROOT.to_string()], &destination);
                values.push(match cell {
                    Cell::Null => Value::Null,
                    Cell::Integer(value) => Value::Integer(value),
                    Cell::Real(value) if value.is_finite() => Value::Real(value),
                    Cell::Real(_) => return Err("checkpoint contains a non-finite number".into()),
                    Cell::Text(value) => Value::Text(value),
                    Cell::Blob(value) => Value::Blob(value),
                });
            }
            statement
                .execute(params_from_iter(values))
                .map_err(|_| "checkpoint violates brain data constraints")?;
        }
    }
    transaction.execute_batch(
        "DELETE FROM memories_fts;
         INSERT INTO memories_fts(memory_id,text,kind,scope) SELECT memory_id,text,kind,scope FROM memories WHERE invalidated_at IS NULL AND superseded_by IS NULL;
         DELETE FROM repo_files_fts;
         INSERT INTO repo_files_fts(repo_root,path,snippet,language_guess) SELECT repo_root,path,snippet,language_guess FROM repo_files;
         DELETE FROM repo_manifests_fts;
         INSERT INTO repo_manifests_fts(repo_root,manifest_path,manifest_kind,parsed_summary_json) SELECT repo_root,manifest_path,manifest_kind,parsed_summary_json FROM repo_manifests;"
    ).map_err(|_| "cannot rebuild projection search indexes")?;
    let violations: i64 = transaction
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })
        .map_err(|_| "cannot validate imported relationships")?;
    if violations != 0 {
        return Err("checkpoint contains invalid relationships".into());
    }
    transaction
        .commit()
        .map_err(|_| "cannot commit temporary projection")?;
    let latest: Option<String> = conn
        .query_row("SELECT MAX(hlc) FROM events", [], |row| row.get(0))
        .map_err(|_| "cannot restore event clock")?;
    if let Some(clock) = latest.and_then(|value| kimetsu_core::clock::Hlc::parse(&value)) {
        kimetsu_core::clock::observe(&clock);
    }
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE;")
        .map_err(|_| "cannot finalize temporary projection")?;
    conn.close()
        .map_err(|_| "cannot close temporary projection")?;
    std::fs::rename(temporary, &paths.brain_db)
        .map_err(|_| "cannot publish temporary projection")?;
    std::fs::write(&paths.project_toml, configuration)
        .map_err(|_| "cannot restore project configuration")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kimetsu_brain::project;
    use kimetsu_core::{
        memory::{MemoryKind, MemoryScope},
        paths::{ProjectPaths, UserBrainScope},
    };
    use rusqlite::params;

    fn empty_checkpoint() -> Checkpoint {
        let conn = Connection::open_in_memory().unwrap();
        kimetsu_brain::schema::initialize(&conn).unwrap();
        Checkpoint {
            format: 1,
            schema: KIMETSU_SCHEMA_VERSION,
            configuration: None,
            tables: layout(&conn)
                .unwrap()
                .into_iter()
                .map(|(name, columns)| Table {
                    name,
                    columns,
                    rows: vec![],
                })
                .collect(),
        }
    }

    fn encode_raw(encoded: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(encoded).unwrap();
        let mut bytes = MAGIC.to_vec();
        bytes.extend(encoder.finish().unwrap());
        bytes
    }

    #[test]
    fn checkpoint_rejects_incompatible_and_injected_schemas_before_publication() {
        for mutation in 0..5 {
            let mut checkpoint = empty_checkpoint();
            match mutation {
                0 => checkpoint.schema += 1,
                1 => checkpoint.tables[0].name = "memories; DROP TABLE events".into(),
                2 => checkpoint.tables[0].columns[0] = "text".into(),
                3 => {
                    checkpoint.tables.pop();
                }
                _ => checkpoint.tables[0].rows.push(vec![]),
            }
            let destination = tempfile::tempdir().unwrap();
            assert!(
                restore(
                    destination.path(),
                    &encode_raw(&rmp_serde::to_vec(&checkpoint).unwrap())
                )
                .is_err()
            );
            assert!(!ProjectPaths::at_root(destination.path()).brain_db.exists());
        }
        let destination = tempfile::tempdir().unwrap();
        let oversized = vec![0u8; crate::storage::MAX_STATE_BYTES + 1];
        assert!(
            restore(destination.path(), &oversized)
                .unwrap_err()
                .contains("compressed size")
        );
    }

    #[test]
    fn checkpoint_rejects_trailing_records_and_compressed_members() {
        let encoded = rmp_serde::to_vec(&empty_checkpoint()).unwrap();
        let mut trailing_records = encoded.clone();
        trailing_records.push(0xc0); // A second MessagePack value is never part of a checkpoint.
        let mut trailing_gzip = encode_raw(&encoded);
        trailing_gzip.extend(encode_raw(b"unrelated").into_iter().skip(MAGIC.len()));
        for bytes in [encode_raw(&trailing_records), trailing_gzip] {
            let destination = tempfile::tempdir().unwrap();
            assert!(restore(destination.path(), &bytes).is_err());
            assert!(!ProjectPaths::at_root(destination.path()).brain_db.exists());
        }
    }

    #[test]
    fn checkpoint_restores_records_embeddings_fts_and_logical_roots() {
        let _scope = UserBrainScope::new(None);
        let source = tempfile::tempdir().unwrap();
        kimetsu_core::paths::git_init_boundary(source.path());
        project::init_project_at_root(source.path(), false).unwrap();
        let paths = ProjectPaths::at_root(source.path());
        let mut cfg = project::load_config(&paths).unwrap();
        cfg.embedder.enabled = false;
        std::fs::write(&paths.project_toml, cfg.to_toml().unwrap()).unwrap();
        let id = project::add_memory(
            source.path(),
            MemoryScope::Project,
            MemoryKind::Convention,
            "Orchid gateway uses port 7319",
        )
        .unwrap();
        let conn = rusqlite::Connection::open(&paths.brain_db).unwrap();
        conn.execute("UPDATE memories SET embedding=?2, embedding_model='test-vector', use_count=7 WHERE memory_id=?1",
            params![id, vec![0u8, 1, 2, 255]]).unwrap();
        conn.execute("INSERT INTO repo_files(repo_root,path,hash,size,mtime,language_guess,snippet) VALUES(?1,'src/orchid.rs','h',1,'now','rust','Orchid gateway')",
            [source.path().to_string_lossy().as_ref()]).unwrap();
        drop(conn);
        let first = capture(source.path()).unwrap();
        let destination = tempfile::tempdir().unwrap();
        restore(destination.path(), &first).unwrap();
        let conn =
            rusqlite::Connection::open(ProjectPaths::at_root(destination.path()).brain_db).unwrap();
        let values: (i64, Vec<u8>) = conn
            .query_row(
                "SELECT use_count,embedding FROM memories WHERE memory_id=?1",
                [&id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(values, (7, vec![0, 1, 2, 255]));
        let matched: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memories_fts WHERE memories_fts MATCH 'orchid'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(matched, 1);
        let root: String = conn
            .query_row(
                "SELECT repo_root FROM repo_files WHERE path='src/orchid.rs'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            root,
            destination.path().canonicalize().unwrap().to_string_lossy()
        );
        drop(conn);
        kimetsu_core::paths::pin_discover_to_root();
        assert!(
            !project::search_files(destination.path(), "orchid", 5)
                .unwrap()
                .is_empty(),
            "cold retrieval must find ingested files using the canonical projection root"
        );
        assert_eq!(
            first,
            capture(destination.path()).unwrap(),
            "physical paths and FTS reconstruction must not create a new durable state"
        );
        assert!(
            restore(destination.path(), &first).is_err(),
            "restore must not overwrite a live brain"
        );
    }

    #[test]
    fn checkpoint_rejects_corruption_and_does_not_create_a_brain() {
        let destination = tempfile::tempdir().unwrap();
        for data in [&b""[..], &b"not a checkpoint"[..], &b"KBRN\x01bad gzip"[..]] {
            assert!(restore(destination.path(), data).is_err());
            assert!(!ProjectPaths::at_root(destination.path()).brain_db.exists());
        }
    }
}
