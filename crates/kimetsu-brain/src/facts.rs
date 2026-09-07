//! Bounded deterministic extraction of explicitly scoped configuration evidence.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FactClaim {
    pub subject: String,
    pub attribute: String,
    pub value: String,
    pub environment: Option<String>,
    pub evidence: String,
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

/// Canonicalize a bounded, explicit noun phrase. Invalid/ambiguous scope is empty.
pub fn canonical_subject(text: &str) -> (String, Option<String>) {
    let normalized = folded(text).replace('’', "'");
    let mut words = Vec::new();
    let mut environment = None;
    for word in normalized.split_whitespace() {
        let word = word.strip_suffix("'s").unwrap_or(word);
        if matches!(word, "the" | "a" | "an" | "el" | "la" | "los" | "las") {
            continue;
        }
        if matches!(word, "production" | "staging" | "development" | "test") {
            if environment.is_some() {
                return (String::new(), None);
            }
            environment = Some(word.to_owned());
            continue;
        }
        if word.len() > 48
            || !word
                .chars()
                .all(|c| c.is_alphanumeric() || "_-".contains(c))
            || matches!(
                word,
                "and"
                    | "or"
                    | "but"
                    | "while"
                    | "if"
                    | "when"
                    | "for"
                    | "of"
                    | "de"
                    | "del"
                    | "changing"
                    | "causes"
                    | "is"
                    | "are"
                    | "was"
                    | "were"
                    | "has"
                    | "uses"
                    | "stores"
                    | "requires"
                    | "should"
                    | "could"
                    | "would"
                    | "may"
                    | "might"
                    | "not"
                    | "no"
                    | "never"
                    | "without"
                    | "sin"
                    | "nunca"
                    | "example"
                    | "ejemplo"
                    | "hypothetical"
                    | "unknown"
                    | "redacted"
                    | "this"
                    | "that"
                    | "it"
                    | "we"
                    | "you"
            )
        {
            return (String::new(), None);
        }
        words.push(word);
    }
    if words.is_empty() || words.len() > 8 {
        return (String::new(), None);
    }
    (words.join(" "), environment)
}

fn patterns() -> &'static [(String, regex::Regex)] {
    static PATTERNS: std::sync::OnceLock<Vec<(String, regex::Regex)>> = std::sync::OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            ("port", "port|puerto", r"\d{1,5}"),
            ("timeout", "timeout|tiempo de espera", r"\d+(?:\.\d+)?\s*(?:ms|s|seconds?|segundos?|minutes?|minutos?)"),
            ("version", "version|release", r"v?\d+(?:\.\d+){0,5}"),
            ("retries", "retries|retry count|reintentos", r"\d{1,9}"),
            ("replicas", "replicas?|replica count", r"\d{1,9}"),
            ("memory_limit", "memory limit|limite de memoria", r"\d+\s*(?:kib|mib|gib|kb|mb|gb|bytes)"),
            ("retention", "retention|retencion", r"\d+\s*(?:seconds?|minutes?|hours?|days?|weeks?|months?|years?|segundos?|minutos?|horas?|dias?|semanas?|meses|anos?)"),
            ("password", "password|passphrase|contrasena", r#"[a-z0-9_+./-]{1,128}"#),
            ("encryption_key", "encryption key|clave de cifrado", r#"[a-z0-9_+./-]{1,128}"#),
            ("literal", r"(?P<key>[a-z_][a-z0-9_-]*(?:[._][a-z0-9_-]+)+)", r#"[a-z0-9_+./-]{1,128}"#),
        ].into_iter().map(|(name, attr, value)| {
            let assignment = r"\s*(?:is\s*|are\s+|es\s*|son\s+|=\s*|:\s*)";
            let binder = if matches!(name, "password" | "encryption_key" | "literal") {
                assignment.to_owned()
            } else {
                format!(r"(?:{assignment}|\s+)")
            };
            let pattern = format!(r#"(?i)^(?P<subject>.+?)\s+`?(?:{attr})`?{binder}[`"']?(?P<value>{value})[`"']?$"#);
            (name.to_owned(), regex::Regex::new(&pattern).expect("constant fact grammar"))
        }).collect()
    })
}

fn parse_clause(evidence: &str) -> Option<FactClaim> {
    if evidence.len() > 512 {
        return None;
    }
    let body = evidence.trim_end_matches(['.', ',', ';']).trim();
    let lower = folded(body);
    // Unknown/redacted/provisional language is never converted to a value.
    if [
        "redacted",
        "unknown",
        "example",
        "ejemplo",
        "hypothetical",
        "not configured",
        "no longer",
        "unavailable",
        "hidden",
    ]
    .iter()
    .any(|w| lower.contains(w))
    {
        return None;
    }
    let absent_subject = lower
        .strip_prefix("no password is required for ")
        .or_else(|| lower.strip_prefix("no password required for "))
        .or_else(|| lower.strip_suffix(" no password required"))
        .or_else(|| lower.strip_suffix(" no password is required"))
        .or_else(|| lower.strip_suffix(" password is not required"));
    if let Some(subject) = absent_subject {
        let (subject, environment) = canonical_subject(subject);
        if subject.is_empty() {
            return None;
        }
        return Some(FactClaim {
            subject,
            environment,
            attribute: "password".into(),
            value: "not required".into(),
            evidence: evidence.into(),
        });
    }
    for (attribute, pattern) in patterns() {
        let Some(captures) = pattern.captures(body) else {
            continue;
        };
        let (subject, environment) = canonical_subject(&captures["subject"]);
        if subject.is_empty() {
            continue;
        }
        let value = captures["value"].to_owned();
        if matches!(
            folded(&value).as_str(),
            "missing"
                | "not"
                | "stored"
                | "configured"
                | "required"
                | "managed"
                | "generated"
                | "provided"
                | "set"
                | "secret"
                | "none"
                | "null"
        ) {
            continue;
        }
        if attribute == "port" && value.parse::<u16>().is_err() {
            continue;
        }
        return Some(FactClaim {
            subject,
            environment,
            attribute: if attribute == "literal" {
                folded(&captures["key"])
            } else {
                attribute.clone()
            },
            value: if matches!(
                attribute.as_str(),
                "password" | "encryption_key" | "literal"
            ) {
                value
            } else {
                folded(&value)
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            },
            evidence: evidence.into(),
        });
    }
    None
}

/// Parse complete clauses only: explicit subject + attribute + is/=/colon + value.
/// Numeric attributes also accept whitespace instead of an assignment binder.
/// No inherited scope, free-form entailment, model calls, or secret recovery.
/// Sentences containing commas are unsupported: a comma can introduce scope,
/// qualification or a numeric separator. Never discard that context or truncate
/// its number. Within comma-free sentences, semicolons separate explicit clauses.
/// Explicit example/hypothetical markers reject the entire input, since a header
/// may qualify following sentences without repeating its provisional status.
/// One leading `[tags: ...] ` record metadata prefix (at most 512 bytes, no nested
/// brackets or newlines) is ignored without contributing subject or environment.
/// Limits: 64 KiB input (oversize rejected), 256 clauses, 512 bytes/clause, 32 facts.
pub fn extract(text: &str) -> Vec<FactClaim> {
    if text.len() > 65_536 {
        return Vec::new();
    }
    let normalized = folded(text);
    if ["example", "ejemplo", "hypothetical"]
        .iter()
        .any(|marker| normalized.contains(marker))
    {
        return Vec::new();
    }
    let text = if let Some(tagged) = text.strip_prefix("[tags: ") {
        let Some((tags, body)) = tagged.split_once("] ") else {
            return Vec::new();
        };
        if tags.len() > 503 || tags.contains(['[', ']', '\n', '\r']) || body.starts_with("[tags:") {
            return Vec::new();
        }
        body
    } else {
        text
    };
    let mut facts = Vec::new();
    let mut start = 0;
    let mut clauses = 0;
    let sentence_ends = text.char_indices().filter_map(|(offset, c)| {
        let end = offset + c.len_utf8();
        (c == '\n'
            || (c == '.' && (end == text.len() || text[end..].starts_with(char::is_whitespace))))
        .then_some(end)
    });
    for end in sentence_ends.chain(std::iter::once(text.len())) {
        let sentence = &text[start..end];
        start = end;
        if sentence.contains(',') {
            continue;
        }
        for clause in sentence.split_inclusive(';') {
            let clause = clause.trim();
            if let Some(fact) = parse_clause(clause) {
                facts.push(fact);
            }
            clauses += 1;
            if facts.len() == 32 || clauses == 256 {
                return facts;
            }
        }
    }
    facts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_environment_and_exact_evidence() {
        let text = "Orchid staging gateway port is 7319.";
        let facts = extract(text);
        assert_eq!(facts.len(), 1);
        assert_eq!(
            facts[0],
            FactClaim {
                subject: "orchid gateway".into(),
                attribute: "port".into(),
                value: "7319".into(),
                environment: Some("staging".into()),
                evidence: text.into()
            }
        );
        assert_eq!(
            extract("The Orchid gateway timeout is45seconds.")[0].value,
            "45seconds"
        );
    }

    #[test]
    fn clauses_do_not_inherit_scope() {
        let facts =
            extract("Orchid gateway port is7319; the database stores state. timeout is 45seconds.");
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].subject, "orchid gateway");
        assert!(extract("Orchid gateway stores state and database port is 5432.").is_empty());
        assert!(extract("Orchid gateway port is7319, while the database stores state.").is_empty());
    }

    #[test]
    fn rejects_negated_hypothetical_and_hidden_values() {
        for text in [
            "Orchid gateway port is not 7319.",
            "Example: Orchid gateway port is 7319.",
            "Orchid gateway password = [REDACTED]",
            "Orchid gateway password is unknown.",
            "Orchid gateway port is 7319 or 7320.",
            "If Orchid gateway port is 7319.",
            "Orchid gateway port is 7319?",
            "Orchid gateway port is 73190abc.",
        ] {
            assert!(extract(text).is_empty(), "{text}");
        }
    }

    #[test]
    fn recognizes_scoped_absence_and_attributes() {
        for text in [
            "Orchid gateway no password required.",
            "No password is required for the Orchid gateway.",
        ] {
            let f = extract(text);
            assert_eq!(f.len(), 1, "{text}");
            assert_eq!(f[0].subject, "orchid gateway");
            assert_eq!(f[0].value, "not required");
        }
        for (attribute, value) in [
            ("version", "3.46"),
            ("retries", "3"),
            ("memory limit", "512 MiB"),
            ("retention", "21 days"),
            ("encryption key", "demo-key"),
            ("cache.max_entries", "200"),
        ] {
            let f = extract(&format!("Orchid gateway {attribute} = {value}."));
            assert_eq!(f.len(), 1, "{attribute}");
            assert_eq!(f[0].attribute, attribute.replace(' ', "_"));
        }
    }

    #[test]
    fn output_and_input_are_bounded() {
        assert_eq!(
            extract(&"Orchid gateway port is 7319.\n".repeat(100)).len(),
            32
        );
        assert!(extract(&format!("{} port is 7319.", "a".repeat(600))).is_empty());
    }

    #[test]
    fn canonical_scope_keeps_identity_and_rejects_mixed_environments() {
        assert_eq!(
            canonical_subject("The Orchid’s staging gateway"),
            ("orchid gateway".into(), Some("staging".into()))
        );
        assert!(canonical_subject("Orchid staging production gateway")
            .0
            .is_empty());
        assert!(canonical_subject("the production").0.is_empty());
        assert_eq!(
            extract("Orchid gateway `cache.max_entries` = 200.")[0].attribute,
            "cache.max_entries"
        );
        assert_eq!(
            extract("Orchid gateway password = `AbC-123`.")[0].value,
            "AbC-123"
        );
    }

    #[test]
    fn numeric_attributes_allow_bare_values_and_replica_counts() {
        assert_eq!(extract("Orchid gateway port 7319.")[0].value, "7319");
        let facts = extract("Orchid production gateway replicas are 4.");
        assert_eq!(facts[0].attribute, "replicas");
        assert_eq!(facts[0].value, "4");
        assert_eq!(facts[0].environment.as_deref(), Some("production"));
    }

    #[test]
    fn rejects_relational_and_action_subjects() {
        for subject in [
            "effect of changing Orchid gateway",
            "Orchid causes",
            "puerto del gateway",
            "gateway de Orchid",
        ] {
            assert!(canonical_subject(subject).0.is_empty(), "{subject}");
        }
    }

    #[test]
    fn comma_qualifiers_cannot_be_discarded() {
        for text in [
            "If deployment succeeds, Orchid gateway port is 7319.",
            "In staging, Orchid gateway port is 7319.",
            "For example, Orchid gateway port is 7319.",
            "Assuming approval, Orchid gateway port is 7319.",
        ] {
            assert!(extract(text).is_empty(), "{text}");
        }
        let facts = extract(
            "If deployment succeeds, Orchid gateway port is 7319. Orchid database port is 5432.",
        );
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].subject, "orchid database");
    }

    #[test]
    fn comma_numbers_cannot_be_truncated_into_facts() {
        for text in [
            "Orchid gateway port is 7,319.",
            "Orchid gateway replicas are 1,000.",
            "Orchid gateway timeout is 0,5 seconds.",
        ] {
            assert!(extract(text).is_empty(), "{text}");
        }
    }

    #[test]
    fn provisional_headers_cannot_be_discarded_at_clause_boundaries() {
        for text in [
            "Example configuration:\nOrchid gateway port is 7319.",
            "Hypothetical configuration; Orchid gateway port is 7319.",
            "EJEMPLO de configuración:\nOrchid gateway port is 7319.",
            "Hypothetical configuration. Orchid gateway port is 7319.",
        ] {
            assert!(extract(text).is_empty(), "{text}");
        }
    }

    #[test]
    fn record_tag_metadata_does_not_become_fact_scope() {
        let text = "[tags: network gateway production] Orchid staging gateway port is 7319.";
        let facts = extract(text);
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].subject, "orchid gateway");
        assert_eq!(facts[0].environment.as_deref(), Some("staging"));
        assert_eq!(facts[0].attribute, "port");
        assert_eq!(facts[0].value, "7319");
        assert!(text.contains(&facts[0].evidence));
        assert_eq!(facts[0].evidence, "Orchid staging gateway port is 7319.");
        assert!(extract("[tags: production] port is 7319.").is_empty());
        assert!(extract("[tags: example] Orchid gateway port is 7319.").is_empty());
        assert!(extract("[tags: production] [tags: gateway] Orchid port is 7319.").is_empty());
    }
}
