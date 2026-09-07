//! Newline-delimited JSON wire protocol for the embedder daemon.

use serde::{Deserialize, Serialize};
use std::io::{self, BufRead, Write};

/// One request from the hook/client to the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Run full retrieval against the brain at `brain_root`.
    Retrieve(RetrieveArgs),
    /// Ensure the model is loaded; cheap liveness + warmth probe.
    Warm,
    /// Liveness + identity probe.
    Ping,
    /// Ask the daemon to exit (admin / version skew).
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetrieveArgs {
    pub v: u32,
    pub brain_root: String,
    pub query: String,
    #[serde(default)]
    pub stage: String,
    #[serde(default)]
    pub budget_tokens: u32,
    #[serde(default)]
    pub max_capsules: usize,
    #[serde(default)]
    pub min_score: f32,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// Daemon -> client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Response {
    /// Retrieval result: a pre-rendered capsule list ready to inject.
    Capsules {
        capsules: Vec<Capsule>,
        skipped: bool,
        top_score: f32,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        known_fact_conflicts: Vec<String>,
    },
    /// Warm/Ping identity.
    Info {
        version: String,
        model: String,
        uptime_s: u64,
        requests: u64,
        loaded_ms: u64,
    },
    /// Acknowledged (e.g. shutdown).
    Ok,
    /// Failure; the client falls back to FTS.
    Error { message: String },
}

/// A minimal capsule shape carried over the wire (subset of the brain's
/// `ContextCapsule` — only what the hook needs to render the injection).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Capsule {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub expansion_handle: String,
    #[serde(default)]
    pub claim_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub facts: Vec<kimetsu_brain::fact_store::StoredFact>,
    pub summary: String,
    pub kind: String,
    pub score: f32,
}

/// Wire version stamped into every `RetrieveArgs`.
pub const PROTOCOL_VERSION: u32 = 1;

/// Write one request/response as a single `\n`-terminated JSON line.
pub fn write_line<T: Serialize, W: Write>(w: &mut W, value: &T) -> io::Result<()> {
    let mut buf = serde_json::to_vec(value)?;
    buf.push(b'\n');
    w.write_all(&buf)?;
    w.flush()
}

/// Read exactly one `\n`-terminated JSON line into `T`. Returns
/// `UnexpectedEof` when the peer closed without sending a line.
pub fn read_line<T: for<'de> Deserialize<'de>, R: BufRead>(r: &mut R) -> io::Result<T> {
    let mut line = String::new();
    let n = r.read_line(&mut line)?;
    if n == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "peer closed"));
    }
    serde_json::from_str(line.trim_end()).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn legacy_capsule_is_explicitly_unbound() {
        let c: Capsule =
            serde_json::from_str(r#"{"summary":"legacy", "kind":"memory", "score":0.8}"#).unwrap();
        assert!(c.id.is_empty());
        assert!(c.expansion_handle.is_empty());
        assert!(c.claim_revision.is_none());
    }

    #[test]
    fn request_round_trips_through_a_line() {
        let req = Request::Retrieve(RetrieveArgs {
            v: 1,
            brain_root: "/tmp/repo".into(),
            query: "what's the idea of the repo".into(),
            stage: "localization".into(),
            budget_tokens: 2000,
            max_capsules: 2,
            min_score: 0.2,
            tags: vec!["rust".into()],
        });
        let mut buf = Vec::new();
        write_line(&mut buf, &req).unwrap();
        assert!(buf.ends_with(b"\n"), "framing must newline-terminate");

        let mut cur = Cursor::new(buf);
        let got: Request = read_line(&mut cur).unwrap();
        match got {
            Request::Retrieve(a) => {
                assert_eq!(a.query, "what's the idea of the repo");
                assert_eq!(a.max_capsules, 2);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn capsule_wire_preserves_optional_structured_evidence() {
        let wire = serde_json::json!({"id":"c", "expansion_handle":"memory:m", "claim_revision":"baseline:m", "summary":"Orchid gateway port is 7319.", "kind":"memory", "score":0.99,
            "facts":[{"memory_id":"m","claim_revision":"baseline:m","source_event_id":"accepted", "valid_from":null,"valid_to":null,
                "claim":{"subject":"orchid gateway","environment":null,"attribute":"port","value":"7319","evidence":"Orchid gateway port is 7319."}}]});
        let capsule: Capsule = serde_json::from_value(wire).unwrap();
        assert_eq!(
            serde_json::to_value(capsule).unwrap()["facts"][0]["claim"]["value"],
            "7319"
        );
    }
    #[test]
    fn response_round_trips() {
        let resp = Response::Capsules {
            capsules: vec![Capsule {
                id: "m1".into(),
                expansion_handle: "memory:m1".into(),
                claim_revision: Some("rev1".into()),
                facts: vec![],
                summary: "repo:fact - x".into(),
                kind: "memory".into(),
                score: 0.9,
            }],
            skipped: false,
            top_score: 0.9,
            known_fact_conflicts: vec![],
        };
        let mut buf = Vec::new();
        write_line(&mut buf, &resp).unwrap();
        let mut cur = Cursor::new(buf);
        let got: Response = read_line(&mut cur).unwrap();
        assert!(matches!(got, Response::Capsules { .. }));
    }

    #[test]
    fn read_line_on_empty_is_eof() {
        let mut cur = Cursor::new(Vec::new());
        let got: io::Result<Request> = read_line(&mut cur);
        assert_eq!(got.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    }
}

#[cfg(test)]
mod conflict_wire_tests {
    use super::*;
    #[test]
    fn response_preserves_conflicts_after_capsules_have_been_trimmed() {
        let json = serde_json::json!({"capsules":{"capsules":[],"skipped":true,"top_score":0.0,"known_fact_conflicts":["port"]}});
        let response: Response = serde_json::from_value(json).unwrap();
        assert_eq!(
            serde_json::to_value(response).unwrap()["capsules"]["known_fact_conflicts"],
            serde_json::json!(["port"])
        );
    }
}

#[cfg(test)]
mod legacy_conflict_wire_tests {
    use super::*;
    #[test]
    fn legacy_response_without_conflicts_still_roundtrips_without_new_fields() {
        let legacy = serde_json::json!({"capsules":{"capsules":[],"skipped":true,"top_score":0.0}});
        let response: Response = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(serde_json::to_value(response).unwrap(), legacy);
    }
}
