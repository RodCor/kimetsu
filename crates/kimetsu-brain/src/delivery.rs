//! Final serving boundary. Retrieval/reranking decides relevance; this module
//! admits only whole chosen capsules that fit the serialized delivery budget.
use super::{ContextCapsule, memory_revision_bindings};
use serde_json::{Value, json};

pub struct Delivery {
    pub payload: Value,
    pub capsules: Vec<ContextCapsule>,
}

/// Conservative tokenizer-independent bound: one token per UTF-8 byte, including
/// the MCP content envelope and both layers of JSON escaping. This is an upper
/// bound for byte-based tokenizers, not a measured model tokenizer count. JSON-RPC
/// request IDs/framing are transport-only and are not included.
pub fn serialized_output_tokens(payload: &Value) -> u32 {
    let bytes = json!({"content": [{"type": "text", "text": payload.to_string()}]})
        .to_string()
        .len();
    u32::try_from(bytes).unwrap_or(u32::MAX)
}

fn account(payload: &mut Value) -> u32 {
    payload["used_tokens"] = json!(0);
    loop {
        let bound = serialized_output_tokens(payload);
        if payload["used_tokens"].as_u64() == Some(u64::from(bound)) {
            return bound;
        }
        payload["used_tokens"] = json!(bound);
    }
}

pub fn compact_capsules(capsules: &[ContextCapsule]) -> Vec<Value> {
    capsules
        .iter()
        .map(|c| {
            json!({
                "id": c.id, "kind": c.kind, "summary": c.summary,
                "expansion_handle": c.expansion_handle, "score": c.score,
            })
        })
        .collect()
}

/// `render` must rebuild all text and counts from this slice, including duplicated
/// playbook text. Rejected candidates never enter the render callback. If even an
/// empty envelope cannot fit, return an explicit error with its true bound (which
/// can exceed the requested tiny budget); never report success or delivered IDs.
pub fn fit_json(
    mut capsules: Vec<ContextCapsule>,
    budget: u32,
    render: impl Fn(&[ContextCapsule]) -> Value,
) -> Delivery {
    loop {
        let mut payload = render(&capsules);
        payload["budget_tokens"] = json!(budget);
        payload["token_accounting"] = json!("utf8_byte_upper_bound");
        if account(&mut payload) <= budget {
            return Delivery { payload, capsules };
        }
        if capsules.pop().is_none() {
            let mut payload = json!({"ok": false, "error": "budget_too_small",
                "budget_tokens": budget, "capsules": [], "capsule_count": 0,
                "token_accounting": "utf8_byte_upper_bound"});
            account(&mut payload);
            return Delivery { payload, capsules };
        }
    }
}

/// Add optional framing only if the complete payload still fits. Evidence wins
/// over warm-start hints; callers must invoke this before logging the exposure.
pub fn add_optional_field(delivery: &mut Delivery, key: &str, value: Value, budget: u32) {
    let mut payload = delivery.payload.clone();
    payload[key] = value;
    if account(&mut payload) <= budget {
        delivery.payload = payload;
    }
}

/// Exposure is built from the final delivered slice, never by re-reading current
/// claims. An empty revision map explicitly means unbound, not legacy attribution.
pub fn injected_payload(capsules: &[ContextCapsule], used_tokens: u32) -> Value {
    json!({
        "memory_ids": capsules.iter().filter_map(|c| c.expansion_handle.strip_prefix("memory:")).collect::<Vec<_>>(),
        "memory_revisions": memory_revision_bindings(capsules),
        "capsule_handles": capsules.iter().map(|c| c.expansion_handle.as_str()).collect::<Vec<_>>(),
        "capsule_count": capsules.len(), "used_tokens": used_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capsule(id: &str, text: &str) -> ContextCapsule {
        let mut c = ContextCapsule::wire_minimal(text.into(), "memory".into(), 0.9);
        c.id = id.into();
        c.expansion_handle = format!("memory:{id}");
        c.claim_revision = Some(format!("revision-{id}"));
        c
    }
    fn render(c: &[ContextCapsule]) -> Value {
        json!({"ok":true,"capsules":compact_capsules(c),"capsule_count":c.len()})
    }

    #[test]
    fn final_serialization_bounds_unicode_identifiers_and_escaping() {
        for text in [
            "字".repeat(1000),
            "no_space_identifier".repeat(1000),
            "\"\\\n".repeat(1000),
        ] {
            let delivery = fit_json(
                vec![capsule("kept", "short"), capsule("dropped", &text)],
                800,
                render,
            );
            assert_eq!(delivery.capsules.len(), 1);
            assert_eq!(
                delivery.payload["used_tokens"].as_u64(),
                Some(u64::from(serialized_output_tokens(&delivery.payload)))
            );
            assert!(serialized_output_tokens(&delivery.payload) <= 800);
            let event = injected_payload(&delivery.capsules, 0);
            assert_eq!(event["memory_ids"], json!(["kept"]));
            assert_eq!(event["memory_revisions"], json!({"kept":"revision-kept"}));
        }
    }

    #[test]
    fn tiny_budget_reports_actual_error_cost_and_no_exposure() {
        let delivery = fit_json(vec![capsule("secret", "secret")], 1, render);
        assert_eq!(delivery.payload["error"], "budget_too_small");
        assert!(delivery.payload["used_tokens"].as_u64().unwrap() > 1);
        assert!(delivery.capsules.is_empty());
        assert_eq!(injected_payload(&[], 0)["memory_revisions"], json!({}));
    }

    #[test]
    fn repeated_playbook_text_is_included_in_final_bound() {
        let delivery = fit_json(
            vec![capsule("large", &"x".repeat(500))],
            900,
            |c| json!({"capsules":compact_capsules(c),"playbook_markdown":c.iter().map(|c|c.summary.as_str()).collect::<Vec<_>>().join("\n")}),
        );
        assert!(delivery.capsules.is_empty());
        assert_eq!(delivery.payload["playbook_markdown"], "");
        assert!(serialized_output_tokens(&delivery.payload) <= 900);
    }
}
