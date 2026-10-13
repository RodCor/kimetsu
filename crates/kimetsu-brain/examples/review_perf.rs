//! Bounded, model-free performance comparison. Run in release mode with
//! KIMETSU_USER_BRAIN=0, before and after a change; compare output fingerprints.
use std::hint::black_box;
use std::time::Instant;

use kimetsu_brain::{context, schema};
use kimetsu_core::config::BrokerWeights;
use rusqlite::{Connection, params};

fn main() -> kimetsu_core::KimetsuResult<()> {
    let conn = Connection::open_in_memory()?;
    schema::initialize(&conn)?;
    let dir = tempfile::tempdir()?;
    for i in 0..1000 {
        let id = format!("m{i:04}");
        let text = format!(
            "Orchid gateway configuration bucket{} uses port {}.",
            i % 10,
            4000 + i
        );
        conn.execute("INSERT INTO memories (memory_id,scope,kind,text,normalized_text,confidence,provenance_snapshot_json,created_at,use_count,usefulness_score) VALUES (?1,'project','fact',?2,?2,0.9,'{}','2026-10-01T00:00:00Z',0,0.0)", params![id,text])?;
        conn.execute(
            "INSERT INTO memories_fts (memory_id,text,kind,scope) VALUES (?1,?2,'fact','project')",
            params![id, text],
        )?;
        for revision in 0..10 {
            conn.execute("INSERT INTO memory_revisions (memory_id,event_id,text,kind,known_at,effective_at,confidence,use_count,usefulness_score) VALUES (?1,?2,?3,'fact','2026-10-01T00:00:00Z','2026-10-01T00:00:00Z',0.9,0,0.0)", params![id,format!("{id}-r{revision}"),format!("{text} revision {revision}")])?;
        }
    }
    for query in ["Orchid gateway", "no_matching_lexical_token"] {
        let retrieve = || {
            context::retrieve_context(
                &conn,
                dir.path().to_str().unwrap(),
                &BrokerWeights::default(),
                context::ContextRequest {
                    stage: "plan".into(),
                    query: query.into(),
                    budget_tokens: 2048,
                    ..Default::default()
                },
            )
            .unwrap()
        };
        let fingerprint = |b: context::ContextBundle| {
            let evidence: Vec<_> = b
                .capsules
                .iter()
                .map(|c| (&c.summary, &c.expansion_handle, &c.claim_revision))
                .collect();
            blake3::hash(serde_json::to_string(&evidence).unwrap().as_bytes())
                .to_hex()
                .to_string()
        };
        let expected = fingerprint(retrieve());
        for _ in 0..10 {
            black_box(retrieve());
        }
        for sample in 0..5 {
            let start = Instant::now();
            for _ in 0..100 {
                black_box(retrieve());
            }
            let micros = start.elapsed().as_secs_f64() * 1e6 / 100.0;
            assert_eq!(fingerprint(retrieve()), expected);
            println!(
                "{}",
                serde_json::json!({"workload":query,"sample":sample,"microseconds":micros,"fingerprint":expected})
            );
        }
    }
    for bytes in [2048, 32 * 1024 * 1024] {
        let file = dir.path().join("large.txt");
        std::fs::write(&file, vec![b'A'; bytes])?;
        let read = || context::resolve_capsule(&conn, dir.path(), "file:large.txt").unwrap();
        let expected = read();
        for _ in 0..10 {
            black_box(read());
        }
        for sample in 0..5 {
            let start = Instant::now();
            for _ in 0..100 {
                assert_eq!(black_box(read()), expected);
            }
            println!(
                "{}",
                serde_json::json!({"workload":format!("file-{bytes}"),"sample":sample,"microseconds":start.elapsed().as_secs_f64()*1e6/100.0,"fingerprint":blake3::hash(expected.as_bytes()).to_hex().to_string()})
            );
        }
    }
    Ok(())
}
