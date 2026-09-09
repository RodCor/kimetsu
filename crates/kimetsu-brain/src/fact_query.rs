//! Scope-bound requests and evidence accounting for explicit configuration facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FactRequest {
    pub subject: String,
    pub environment: Option<String>,
    pub attributes: Vec<String>,
}

fn folded(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .map(|c| match c {
            'á' => 'a',
            'é' => 'e',
            'í' => 'i',
            'ó' => 'o',
            'ú' | 'ü' => 'u',
            'ñ' => 'n',
            _ => c,
        })
        .collect()
}
fn attribute(text: &str) -> Option<String> {
    Some(
        match text.trim().trim_matches('`') {
            "port" | "puerto" => "port",
            "timeout" | "tiempo de espera" => "timeout",
            "version" | "release number" => "version",
            "replicas" | "replica count" => "replicas",
            "password" | "passphrase" | "contrasena" => "password",
            "encryption key" | "clave de cifrado" => "encryption_key",
            "retries" | "retry count" | "reintentos" => "retries",
            "retention" | "retencion" => "retention",
            "memory limit" | "limite de memoria" => "memory_limit",
            key if key.contains(['.', '_'])
                && key
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)) =>
            {
                key
            }
            _ => return None,
        }
        .to_owned(),
    )
}
fn attributes(text: &str) -> Option<Vec<String>> {
    let text = text.replace(" and ", ",").replace(" y ", ",");
    let mut result = Vec::new();
    for part in text.split(',') {
        let attr = attribute(part)?;
        if !result.contains(&attr) {
            result.push(attr);
        }
    }
    if result.is_empty() || result.len() > 4 {
        None
    } else {
        Some(result)
    }
}
fn request(subject: &str, attrs: &str) -> Option<FactRequest> {
    let (subject, environment) = crate::facts::canonical_subject(subject);
    if subject.is_empty()
        || subject.split_whitespace().any(|w| {
            matches!(
                w,
                "effect"
                    | "impact"
                    | "cause"
                    | "causes"
                    | "changing"
                    | "configure"
                    | "configuration"
                    | "best"
                    | "meaning"
                    | "difference"
            )
        })
    {
        return None;
    }
    Some(FactRequest {
        subject,
        environment,
        attributes: attributes(attrs)?,
    })
}
/// Recognize direct attribute questions only, with one explicit shared subject.
/// Multi-subject, explanatory and unsupported language retain normal retrieval.
pub fn parse(query: &str) -> Option<FactRequest> {
    if query.len() > 1024 {
        return None;
    }
    let normalized = folded(query);
    let q = normalized.trim_matches(['¿', '?', ' ', '.', '\n', '\t']);
    if let Some(rest) = q.strip_prefix("what ") {
        if let Some((attrs, subject)) = rest.split_once(" does ") {
            for suffix in [" use", " require", " run", " have"] {
                if let Some(subject) = subject.strip_suffix(suffix) {
                    return request(subject, attrs);
                }
            }
            return None;
        }
    }
    let rest = [
        "what is ",
        "what are ",
        "what's ",
        "which is ",
        "which are ",
        "cual es ",
        "cuales son ",
        "que es ",
    ]
    .iter()
    .find_map(|p| q.strip_prefix(p))?;
    let rest = ["the ", "el ", "la ", "los ", "las "]
        .iter()
        .find_map(|p| rest.strip_prefix(p))
        .unwrap_or(rest);
    for separator in [" for ", " of ", " del ", " de "] {
        if let Some((attrs, subject)) = rest.split_once(separator) {
            if attributes(attrs).is_some() {
                return request(subject, attrs);
            }
        }
    }
    // Split at every word boundary: exactly one split must form a complete
    // attribute list; arbitrary suffix words cannot be silently ignored.
    for (index, ch) in rest.char_indices() {
        if ch == ' ' && attributes(&rest[index + 1..]).is_some() {
            return request(&rest[..index], &rest[index + 1..]);
        }
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SupportedFact {
    pub attribute: String,
    pub value: String,
    pub sources: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct FactAssessment {
    pub status: String,
    pub subject: String,
    pub environment: Option<String>,
    pub supported: Vec<SupportedFact>,
    pub missing: Vec<String>,
    pub conflicting: Vec<String>,
}
pub fn matches(request: &FactRequest, claim: &crate::facts::FactClaim) -> bool {
    request.subject == claim.subject
        && request.environment == claim.environment
        && request.attributes.contains(&claim.attribute)
}
pub fn assess(
    request: &FactRequest,
    claims: &[(&crate::facts::FactClaim, &str)],
) -> FactAssessment {
    use std::collections::{BTreeMap, BTreeSet};
    let mut supported = Vec::new();
    let mut missing = Vec::new();
    let mut conflicting = Vec::new();
    for attribute in &request.attributes {
        let mut values: BTreeMap<String, (&str, BTreeSet<&str>)> = BTreeMap::new();
        for (claim, source) in claims {
            if matches(request, claim) && claim.attribute == *attribute {
                values
                    .entry(crate::fact_values::equivalence_key(attribute, &claim.value))
                    .or_insert_with(|| (claim.value.as_str(), BTreeSet::new()))
                    .1
                    .insert(source);
            }
        }
        match values.len() {
            0 => missing.push(attribute.clone()),
            1 => {
                let (_, (value, sources)) = values.into_iter().next().unwrap();
                supported.push(SupportedFact {
                    attribute: attribute.clone(),
                    value: value.into(),
                    sources: sources.into_iter().map(str::to_owned).collect(),
                });
            }
            _ => conflicting.push(attribute.clone()),
        }
    }
    let status = if !conflicting.is_empty() {
        "conflicting"
    } else if supported.is_empty() {
        "missing"
    } else if missing.is_empty() {
        "supported"
    } else {
        "partial"
    };
    FactAssessment {
        status: status.into(),
        subject: request.subject.clone(),
        environment: request.environment.clone(),
        supported,
        missing,
        conflicting,
    }
}

/// Retain conflicts already observed before output-budget trimming. Sources in
/// supported claims still come exclusively from the final delivered slice.
pub fn preserve_conflicts(result: &mut FactAssessment, known: &[String]) {
    for attr in known {
        result.supported.retain(|s| &s.attribute != attr);
        result.missing.retain(|s| s != attr);
        if !result.conflicting.contains(attr) {
            result.conflicting.push(attr.clone());
        }
    }
    if !result.conflicting.is_empty() {
        result.status = "conflicting".into();
    }
}
/// Facts can support only the claim and exact excerpt that are actually visible.
pub fn visible(
    capsule: &crate::context::ContextCapsule,
    fact: &crate::fact_store::StoredFact,
) -> bool {
    capsule.expansion_handle.strip_prefix("memory:") == Some(fact.memory_id.as_str())
        && capsule.claim_revision.as_deref() == Some(fact.claim_revision.as_str())
        && !fact.claim.evidence.is_empty()
        && capsule.summary.contains(&fact.claim.evidence)
}
pub fn evaluate(
    query: &str,
    capsules: &[crate::context::ContextCapsule],
) -> Option<FactAssessment> {
    let request = parse(query)?;
    let claims: Vec<_> = capsules
        .iter()
        .flat_map(|c| {
            c.facts
                .iter()
                .filter(move |f| visible(c, f))
                .map(move |f| (&f.claim, c.expansion_handle.as_str()))
        })
        .collect();
    Some(assess(&request, &claims))
}
pub fn compress_capsule(
    query: &str,
    capsule: &crate::context::ContextCapsule,
    sentences: usize,
) -> String {
    let short =
        crate::answerability::compress_preserving_evidence(query, &capsule.summary, sentences);
    if parse(query).is_some_and(|request| {
        capsule.facts.iter().any(|f| {
            visible(capsule, f) && matches(&request, &f.claim) && !short.contains(&f.claim.evidence)
        })
    }) {
        capsule.summary.clone()
    } else {
        short
    }
}
pub fn notice(query: &str, capsules: &[crate::context::ContextCapsule]) -> Option<String> {
    notice_with_conflicts(query, capsules, &[])
}
pub fn notice_with_conflicts(
    query: &str,
    capsules: &[crate::context::ContextCapsule],
    known: &[String],
) -> Option<String> {
    let mut result = evaluate(query, capsules)?;
    preserve_conflicts(&mut result, known);
    if result.status == "supported" {
        return None;
    }
    let mut details = Vec::new();
    if !result.missing.is_empty() {
        details.push(format!(
            "no supported value for {}",
            result.missing.join(", ")
        ));
    }
    if !result.conflicting.is_empty() {
        details.push(format!(
            "conflicting values for {}",
            result.conflicting.join(", ")
        ));
    }
    Some(format!("Retrieved fact evidence: {}.", details.join("; ")))
}

#[cfg(test)]
mod tests {
    use super::*;
    pub(crate) fn capsule(text: &str, id: &str) -> crate::context::ContextCapsule {
        let mut c =
            crate::context::ContextCapsule::wire_minimal(text.into(), "memory".into(), 0.99);
        c.expansion_handle = format!("memory:{id}");
        c.claim_revision = Some(format!("baseline:{id}"));
        c.facts = crate::facts::extract(text)
            .into_iter()
            .map(|claim| crate::fact_store::StoredFact {
                memory_id: id.into(),
                claim_revision: format!("baseline:{id}"),
                source_event_id: "accepted-event".into(),
                valid_from: None,
                valid_to: None,
                claim,
            })
            .collect();
        c
    }
    #[test]
    fn only_visible_revision_bound_facts_support_an_answer() {
        let q = "What is the Orchid staging gateway port?";
        let mut c = capsule("Orchid staging gateway port is 7319.", "port");
        assert_eq!(evaluate(q, &[c.clone()]).unwrap().status, "supported");
        c.claim_revision = Some("different-revision".into());
        assert_eq!(evaluate(q, &[c.clone()]).unwrap().status, "missing");
        c.claim_revision = Some("baseline:port".into());
        c.summary = "Unrelated text.".into();
        assert_eq!(evaluate(q, &[c]).unwrap().status, "missing");
    }
    #[test]
    fn compression_preserves_each_supported_attribute() {
        let q = "What are the Orchid gateway port and timeout?";
        let c = capsule(
            "Orchid gateway port is 7319. Other setup notes. More setup notes. Orchid gateway timeout is 30 seconds.",
            "settings",
        );
        assert!(compress_capsule(q, &c, 3).contains("30 seconds"));
    }
    #[test]
    fn budget_trimming_does_not_hide_a_known_conflict() {
        use crate::context::ContextBundle;
        let q = "What is the Orchid gateway port?";
        let bundle = ContextBundle {
            stage: "localization".into(),
            budget_tokens: 6000,
            used_tokens: 0,
            capsules: vec![
                capsule("Orchid gateway port is 7319.", "a"),
                capsule("Orchid gateway port is 7320.", "b"),
            ],
            excluded: vec![],
            skipped: false,
            top_score: 0.99,
            top_abs_evidence: 0.99,
            evidence_coverage: 1.0,
            uncovered_terms: vec![],
            chronological: false,
            known_fact_conflicts: vec![],
        };
        let mut found = false;
        for budget in (300..2000).step_by(20) {
            let delivered = crate::serving::ServingPolicy {
                budget,
                explicit_fact_guard: true,
                ..Default::default()
            }
            .render_for_query(
                q,
                bundle.clone(),
                true,
                crate::serving::EVAL_EXPOSURE_ID,
            );
            if delivered.capsules.len() == 1 {
                found = true;
                assert_eq!(delivered.payload["answerability"]["status"], "conflicting");
                assert_eq!(
                    delivered.payload["answerability"]["conflicting"],
                    serde_json::json!(["port"])
                );
                break;
            }
        }
        assert!(found);
    }
    #[test]
    fn serving_metadata_tracks_the_final_budgeted_slice() {
        use crate::context::ContextBundle;
        let q = "What are the Orchid gateway port and timeout?";
        let bundle = ContextBundle {
            stage: "localization".into(),
            budget_tokens: 6000,
            used_tokens: 0,
            capsules: vec![
                capsule("Orchid gateway port is 7319.", "port"),
                capsule("Orchid gateway timeout is 30 seconds.", "timeout"),
            ],
            excluded: vec![],
            skipped: false,
            top_score: 0.99,
            top_abs_evidence: 0.99,
            evidence_coverage: 1.0,
            uncovered_terms: vec![],
            chronological: false,
            known_fact_conflicts: vec![],
        };
        let policy = crate::serving::ServingPolicy {
            explicit_fact_guard: true,
            ..Default::default()
        };
        let full =
            policy.render_for_query(q, bundle.clone(), true, crate::serving::EVAL_EXPOSURE_ID);
        assert_eq!(full.payload["answerability"]["status"], "supported");
        let mut found = false;
        for budget in (300..2000).step_by(20) {
            let delivered = crate::serving::ServingPolicy { budget, ..policy }.render_for_query(
                q,
                bundle.clone(),
                true,
                crate::serving::EVAL_EXPOSURE_ID,
            );
            if delivered.capsules.len() == 1 {
                found = true;
                assert_eq!(delivered.payload["answerability"]["status"], "partial");
                assert_eq!(
                    delivered.payload["answerability"]["missing"],
                    serde_json::json!(["timeout"])
                );
                assert!(
                    crate::context::delivery::serialized_output_tokens(&delivered.payload)
                        <= budget
                );
                break;
            }
        }
        assert!(found, "expected a budget admitting only the first capsule");
    }
    fn claim(
        subject: &str,
        environment: Option<&str>,
        attribute: &str,
        value: &str,
    ) -> crate::facts::FactClaim {
        crate::facts::FactClaim {
            subject: subject.into(),
            environment: environment.map(str::to_owned),
            attribute: attribute.into(),
            value: value.into(),
            evidence: "evidence".into(),
        }
    }
    #[test]
    fn supports_partial_answers_without_borrowing_another_scope() {
        let request = parse("What are the Orchid staging gateway port and timeout?").unwrap();
        let port = claim("orchid gateway", Some("staging"), "port", "7319");
        let wrong_env = claim(
            "orchid gateway",
            Some("production"),
            "timeout",
            "30 seconds",
        );
        let wrong_subject = claim("quartz gateway", Some("staging"), "timeout", "30 seconds");
        let result = assess(
            &request,
            &[
                (&port, "memory:port"),
                (&wrong_env, "memory:wrong-env"),
                (&wrong_subject, "memory:wrong-subject"),
            ],
        );
        assert_eq!(result.status, "partial");
        assert_eq!(result.missing, ["timeout"]);
        assert_eq!(result.supported[0].sources, ["memory:port"]);
    }
    #[test]
    fn conflicting_values_are_reported_instead_of_selecting_one() {
        let request = parse("What is the Orchid gateway port?").unwrap();
        let a = claim("orchid gateway", None, "port", "7319");
        let b = claim("orchid gateway", None, "port", "7320");
        let result = assess(&request, &[(&a, "memory:a"), (&b, "memory:b")]);
        assert_eq!(result.status, "conflicting");
        assert_eq!(result.conflicting, ["port"]);
        assert!(result.supported.is_empty());
    }
    #[test]
    fn parses_shared_subject_and_environment_for_compound_request() {
        let request = parse("What are the Orchid staging gateway port and timeout?").unwrap();
        assert_eq!(request.subject, "orchid gateway");
        assert_eq!(request.environment.as_deref(), Some("staging"));
        assert_eq!(request.attributes, ["port", "timeout"]);
    }
    #[test]
    fn parses_subject_after_attributes_and_spanish_aliases() {
        assert_eq!(
            parse("What is the port for the Quartz gateway?")
                .unwrap()
                .subject,
            "quartz gateway"
        );
        assert_eq!(
            parse("What port does the Quartz gateway use?")
                .unwrap()
                .attributes,
            ["port"]
        );
        assert_eq!(
            parse("¿Cuál es el puerto del Quartz gateway?")
                .unwrap()
                .attributes,
            ["port"]
        );
    }
    #[test]
    fn broad_or_ambiguous_questions_keep_normal_retrieval() {
        for query in [
            "What causes a version conflict?",
            "What version control system do we use?",
            "What is the effect of changing port?",
            "What does `cache.size` control?",
            "What are the gateway port and database timeout?",
            "What is the port?",
        ] {
            assert_eq!(parse(query), None, "{query}");
        }
    }
}
