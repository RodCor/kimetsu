//! A bounded, local check for explicit configuration facts, not an entailment model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactEvidence {
    Unrecognized,
    ValuePresent,
    MissingValue,
}
struct Rule {
    query: regex::Regex,
    value: regex::Regex,
    secret: bool,
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
fn rules() -> &'static [Rule] {
    static RULES: std::sync::OnceLock<Vec<Rule>> = std::sync::OnceLock::new();
    RULES.get_or_init(|| {
        [
            (r"\b(?:password|passphrase|contrasena)\b", r#"\b(?:password|passphrase|contrasena)\s*(?:is|es|=|:)\s*[`"']?(?P<value>[a-z0-9_+./-]+)"#, true),
            (r"\b(?:encryption key|clave de cifrado|clave de encriptacion)\b", r#"\b(?:encryption key|clave de cifrado|clave de encriptacion)\s*(?:is|es|=|:)\s*[`"']?(?P<value>[a-z0-9_+./-]+)"#, true),
            (r"\b(?:version|release number)\b", r"\b(?:version|release)\s*(?:is\s+|es\s+|=\s*|:\s*)?v?\d+(?:\.\d+)*\b", false),
            (r"\b(?:replica|replicas)\b", r"\b(?:\d+\s+(?:production\s+)?replicas?|replicas?\s*(?:count\s*)?(?:is\s+|are\s+|son\s+|=\s*|:\s*)?\d+)\b", false),
            (r"\b(?:retention|retencion)\b|\b(?:days|dias|weeks|semanas)\b.*\b(?:keep|kept|retain\w*|conserv\w*)\b|\bhow long\b.*\b(?:keep|kept|retain\w*)\b", r"\b(?:retention|retencion|retained|retain|keep|kept|conserv\w*)\b[^.;\n]{0,64}\b\d+\s*(?:seconds?|segundos?|minutes?|minutos?|hours?|horas?|days?|dias?|weeks?|semanas?|months?|meses|years?|anos?)\b", false),
            (r"\b(?:port|puerto)\b", r"\b(?:port|puerto)\s*(?:is\s+|es\s+|=\s*|:\s*)?\d{1,5}\b", false),
            (r"\b(?:timeout|tiempo de espera)\b", r"\b(?:timeout|tiempo de espera)\s*(?:is\s+|es\s+|=\s*|:\s*)?\d+(?:\.\d+)?\s*(?:ms|s|seconds?|segundos?|minutes?|minutos?)\b", false),
            (r"\b(?:retries|retry count|reintentos)\b", r"\b(?:retries|retry count|reintentos)\s*(?:is\s+|are\s+|son\s+|=\s*|:\s*)?\d+\b|\b\d+\s+(?:retries|reintentos)\b", false),
            (r"\b(?:memory limit|limite de memoria)\b", r"\b(?:memory limit|limite de memoria)\s*(?:is\s+|es\s+|=\s*|:\s*)?\d+\s*(?:kib|mib|gib|kb|mb|gb|bytes)\b", false),
        ].into_iter().map(|(query,value,secret)| Rule {
            query: regex::Regex::new(query).expect("constant query pattern"),
            value: regex::Regex::new(value).expect("constant evidence pattern"), secret,
        }).collect()
    })
}
fn concrete(value: &str) -> bool {
    !matches!(
        value.trim_end_matches('.'),
        "unknown"
            | "redacted"
            | "missing"
            | "unavailable"
            | "not"
            | "stored"
            | "configured"
            | "required"
            | "managed"
            | "generated"
            | "hidden"
            | "provided"
            | "set"
            | "secret"
            | "desconocida"
            | "desconocido"
            | "configurada"
            | "configurado"
    )
}
fn clause_supports(query: &str, body: &str, start: usize, end: usize) -> bool {
    static ENTITIES: std::sync::OnceLock<Vec<regex::Regex>> = std::sync::OnceLock::new();
    let entities = ENTITIES.get_or_init(|| {
        [
            r"\b(?:database|db|base de datos|sqlite|postgresql|postgres)\b",
            r"\b(?:gateway|puerta de enlace)\b",
            r"\b(?:client|cliente)\b",
            r"\b(?:server|servidor|listener)\b",
            r"\b(?:logs?|registros)\b",
            r"\b(?:backups?|copias de seguridad)\b",
            r"\b(?:workers?|trabajadores)\b",
            r"\bsqlite\b",
            r"\b(?:postgres|postgresql)\b",
            r"\bopenssl\b",
            r"\bredis\b",
            r"\bpython\b",
            r"\bnode\b",
            r"\brust\b",
        ]
        .into_iter()
        .map(|p| regex::Regex::new(p).unwrap())
        .collect()
    });
    let boundary = |i: usize, c: char| {
        c == ';'
            || c == ','
            || c == '\n'
            || (c == '.' && body[i + 1..].starts_with(char::is_whitespace))
    };
    let left = body[..start]
        .char_indices()
        .filter(|(i, c)| boundary(*i, *c))
        .map(|(i, _)| i + 1)
        .last()
        .unwrap_or(0);
    let right = body[end..]
        .char_indices()
        .find(|(i, c)| boundary(end + *i, *c))
        .map(|(i, _)| end + i)
        .unwrap_or(body.len());
    let clause = &body[left..right];
    if [
        "example",
        "ejemplo",
        "hypothetical",
        "hipotetic",
        "unknown",
        "redacted",
        "not configured",
        "not installed",
        "no longer",
    ]
    .iter()
    .any(|word| clause.contains(word))
    {
        return false;
    }
    // Explicit absence is handled separately; ordinary values under negation
    // are not affirmative evidence. Keep this conservative within one clause.
    static NEGATED: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let negated = NEGATED.get_or_init(|| {
        regex::Regex::new(r"\b(?:not|never|no|nunca|sin|isn't|isnt|don't|doesn't)\b").unwrap()
    });
    let matched = &body[start..end];
    let explicit_absence = matched.contains("no password")
        || matched.contains("not required")
        || matched.contains("requiere contrasena");
    if negated.is_match(clause) && !explicit_absence {
        return false;
    }
    entities
        .iter()
        .all(|entity| !entity.is_match(query) || entity.is_match(clause))
}
/// Check explicit configuration questions only. Unrecognized questions retain
/// normal retrieval. ValuePresent means syntactic evidence, not verified truth,
/// entity identity, freshness or general entailment; other retrieval gates remain.
pub fn assess(query: &str, text: &str) -> FactEvidence {
    let q = folded(query);
    let q = q.trim_start_matches(['¿', ' ', '\t', '\n']);
    if ![
        "what ",
        "what's ",
        "which ",
        "how many ",
        "how much ",
        "how long ",
        "que ",
        "cual ",
        "cuantos ",
        "cuantas ",
        "cuanto ",
        "tell me ",
        "dime ",
    ]
    .iter()
    .any(|prefix| q.starts_with(prefix))
    {
        return FactEvidence::Unrecognized;
    }
    if q.split_whitespace()
        .any(|w| matches!(w, "should" | "could" | "would" | "deberia" | "debo"))
    {
        return FactEvidence::Unrecognized;
    }
    let normalized = folded(text);
    let mut body = normalized.as_str();
    if let Some((prefix, rest)) = body.split_once(" - ") {
        if prefix.contains(':') && !prefix.contains(' ') {
            body = rest;
        }
    }
    while body.starts_with('[') {
        if let Some((_, rest)) = body.split_once(']') {
            body = rest.trim_start();
        } else {
            break;
        }
    }
    // Literal keys are only gated for direct value questions with one target.
    // Explanatory and coordinated key questions retain normal retrieval.
    if q.contains('`')
        && (q.matches('`').count() != 2
            || !["what is ", "what's ", "cual es ", "que valor "]
                .iter()
                .any(|p| q.starts_with(p)))
    {
        return FactEvidence::Unrecognized;
    }
    // Exact literal config keys generalize beyond the curated natural-language
    // attributes. Do not treat a different key's value as an answer.
    if let Some(key) = q.split('`').nth(1).filter(|key| {
        key.contains(['.', '_'])
            && key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
    }) {
        static ASSIGNMENT: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let pattern = ASSIGNMENT.get_or_init(|| {
            regex::Regex::new(
                r#"\b(?P<key>[a-z_][a-z0-9_.-]*)`?\s*(?:=|:)\s*[`"']?(?P<value>[a-z0-9_+./-]+)"#,
            )
            .unwrap()
        });
        return if pattern.captures_iter(body).any(|c| {
            &c["key"] == key
                && concrete(&c["value"])
                && clause_supports(q, body, c.get(0).unwrap().start(), c.get(0).unwrap().end())
        }) {
            FactEvidence::ValuePresent
        } else {
            FactEvidence::MissingValue
        };
    }
    // A single-attribute guard cannot adjudicate compound questions. Leave
    // those to normal retrieval so complementary capsules remain available.
    let matches: Vec<_> = rules()
        .iter()
        .filter_map(|r| r.query.find(q).map(|m| (m.start(), r)))
        .collect();
    if matches.len() != 1 {
        return FactEvidence::Unrecognized;
    }
    let (offset, rule) = matches[0];
    // Only accept a constrained noun phrase before the attribute. Unknown
    // wording passes through rather than suppressing troubleshooting evidence.
    let attribute = rule.query.find(q).unwrap();
    let suffix = q[attribute.end()..].trim_start();
    if [
        "control",
        "hashing",
        "policy",
        "rotation",
        "conflict",
        "file",
        "algorithm",
        "management",
    ]
    .iter()
    .any(|word| suffix.starts_with(word))
    {
        return FactEvidence::Unrecognized;
    }
    let prefix = &q[..offset];
    let proper_names: Vec<_> = query
        .split_whitespace()
        .filter(|w| w.chars().next().is_some_and(char::is_uppercase))
        .map(folded)
        .collect();
    if !prefix.split_whitespace().all(|word| {
        matches!(
            word,
            "what"
                | "what's"
                | "which"
                | "is"
                | "are"
                | "the"
                | "a"
                | "an"
                | "how"
                | "many"
                | "much"
                | "long"
                | "que"
                | "cual"
                | "es"
                | "la"
                | "el"
                | "cuantos"
                | "cuantas"
                | "cuanto"
                | "tell"
                | "me"
                | "dime"
                | "current"
                | "configured"
                | "required"
                | "authentication"
                | "request"
                | "tcp"
                | "http"
                | "database"
                | "db"
                | "sqlite"
                | "postgresql"
                | "postgres"
                | "gateway"
                | "client"
                | "server"
                | "listener"
                | "worker"
                | "memory"
                | "production"
                | "staging"
                | "backup"
                | "log"
                | "logs"
                | "base"
                | "de"
                | "datos"
                | "del"
        ) || proper_names.iter().any(|name| name == word)
    }) {
        return FactEvidence::Unrecognized;
    }
    if rule.secret {
        static ABSENT: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let absent = ABSENT.get_or_init(|| regex::Regex::new(r"\b(?:no password (?:is )?required|password (?:is )?not required|no (?:se )?requiere contrasena)\b").unwrap());
        if rule.query.as_str().contains("password")
            && absent
                .find_iter(body)
                .any(|m| clause_supports(q, body, m.start(), m.end()))
        {
            return FactEvidence::ValuePresent;
        }
    }
    if rule.value.captures_iter(body).any(|c| {
        (!rule.secret || concrete(&c["value"]))
            && clause_supports(q, body, c.get(0).unwrap().start(), c.get(0).unwrap().end())
    }) {
        FactEvidence::ValuePresent
    } else {
        FactEvidence::MissingValue
    }
}

/// Apply the same explicit-fact policy to MCP and the lightweight hook.
pub fn filter_bundle(query: &str, bundle: &mut crate::context::ContextBundle) {
    let request = crate::fact_query::parse(query);
    for capsule in std::mem::take(&mut bundle.capsules) {
        let rejected = if let Some(request) = request.as_ref().filter(|_| !capsule.facts.is_empty()) {
            !capsule.facts.iter().any(|fact| crate::fact_query::visible(&capsule, fact) && crate::fact_query::matches(request, &fact.claim))
        } else { assess(query, &capsule.summary) == FactEvidence::MissingValue };
        if rejected {
            bundle.excluded.push(capsule);
        } else {
            bundle.capsules.push(capsule);
        }
    }
    bundle.used_tokens = bundle.capsules.iter().map(|c| c.token_estimate).sum();
    bundle.top_score = bundle
        .capsules
        .iter()
        .map(|c| c.score)
        .fold(0.0_f32, f32::max);
    if bundle.capsules.is_empty() {
        bundle.skipped = true;
        bundle.evidence_coverage = 0.0;
    }
}
/// Compression must not hide the value used for admission. Final byte budgeting
/// still decides whether the complete evidence fits.
pub fn compress_preserving_evidence(query: &str, summary: &str, sentences: usize) -> String {
    let short = crate::context::compress_for_render(summary, sentences);
    if assess(query, summary) == FactEvidence::ValuePresent
        && assess(query, &short) == FactEvidence::MissingValue
    {
        summary.to_owned()
    } else {
        short
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn related_topics_do_not_supply_missing_configuration_values() {
        for (q, text) in [
            (
                "What password does the staging listener require?",
                "The staging listener binds port 6319.",
            ),
            (
                "¿Qué versión de SQLite requiere el proyecto?",
                "The project uses SQLite WAL mode.",
            ),
            (
                "How many replicas run in production?",
                "Production runs in eu-north-1.",
            ),
            (
                "¿Cuántos días se conservan las copias?",
                "Backups run daily at 02:40 UTC.",
            ),
            (
                "What is the encryption key?",
                "Encryption is enabled for the database.",
            ),
            (
                "What is the request timeout?",
                "Requests are logged every 30 seconds.",
            ),
            ("What is `cache.max_entries`?", "cache.min_entries = 200"),
        ] {
            assert_eq!(assess(q, text), FactEvidence::MissingValue, "{q}");
        }
    }
    #[test]
    fn explicit_values_survive_in_both_languages() {
        for (q, text) in [
            (
                "What password is configured?",
                "password = `sample-only-value`",
            ),
            (
                "¿Qué versión de SQLite requiere el proyecto?",
                "SQLite version 3.46 is required.",
            ),
            (
                "How many replicas run in production?",
                "Production runs 4 replicas.",
            ),
            (
                "¿Cuántos días se conservan las copias?",
                "Backups are retained for 21 days.",
            ),
            (
                "What is the encryption key?",
                "encryption key = `sample-only-key`",
            ),
            (
                "What is the request timeout?",
                "The request timeout is 30 seconds.",
            ),
            ("What is `cache.max_entries`?", "cache.max_entries = 200"),
            (
                "Which TCP port is configured?",
                "The listener binds TCP port 6319.",
            ),
        ] {
            assert_eq!(assess(q, text), FactEvidence::ValuePresent, "{q}");
        }
    }
    #[test]
    fn mentions_and_unknown_values_are_not_answers() {
        for text in [
            "The password is unknown.",
            "Set a password before starting.",
            "password = [REDACTED]",
            "[tags: password] The listener port is 6319.",
        ] {
            assert_eq!(
                assess("What is the password?", text),
                FactEvidence::MissingValue,
                "{text}"
            );
        }
    }
    #[test]
    fn explicit_absence_and_short_retention_are_useful_answers() {
        assert_eq!(
            assess(
                "What password is required?",
                "No password is required for this listener."
            ),
            FactEvidence::ValuePresent
        );
        assert_eq!(
            assess("How long are logs kept?", "Logs are kept for 12 hours."),
            FactEvidence::ValuePresent
        );
        assert_eq!(
            assess(
                "What should I do about a version conflict?",
                "Read the migration notes."
            ),
            FactEvidence::Unrecognized
        );
    }
    #[test]
    fn a_value_for_another_component_is_not_an_answer() {
        for (q, text) in [
            (
                "What is the database timeout?",
                "Gateway timeout is 30 seconds. The database stores state.",
            ),
            (
                "What is the SQLite version?",
                "PostgreSQL version 16 is installed.",
            ),
            (
                "¿Qué contraseña requiere la base de datos?",
                "The gateway password is `example-value-only`.",
            ),
            (
                "How long are logs retained?",
                "Backups are retained for 21 days.",
            ),
            (
                "What is the password?",
                "Example: password = `demo-only-value`",
            ),
        ] {
            assert_eq!(assess(q, text), FactEvidence::MissingValue, "{q}");
        }
    }
    #[test]
    fn review_scope_regressions() {
        for q in [
            "What causes a version conflict?",
            "What version control system do we use?",
            "What password hashing algorithm do we use?",
            "What does `cache.max_entries` control?",
            "What are `cache.max_entries` and `cache.ttl`?",
            "Which files configure the port?",
            "What are the database port and password?",
        ] {
            assert_eq!(
                assess(q, "See configuration notes."),
                FactEvidence::Unrecognized,
                "{q}"
            );
        }
    }
    #[test]
    fn review_negated_values() {
        for text in [
            "We do not use SQLite version 3.45.",
            "No usamos SQLite version 3.45.",
        ] {
            assert_eq!(
                assess("What is the SQLite version?", text),
                FactEvidence::MissingValue,
                "{text}"
            );
        }
    }
    #[test]
    fn review_competing_clause() {
        assert_eq!(
            assess(
                "What is the database timeout?",
                "Gateway timeout is 30 seconds, while the database stores state."
            ),
            FactEvidence::MissingValue
        );
    }
    #[test]
    fn broad_tasks_remain_outside_this_bounded_guard() {
        for q in [
            "How do I configure passwords safely?",
            "Fix the SQLite migration failure",
            "Explain memory retrieval",
            "Why does my test hang?",
        ] {
            assert_eq!(
                assess(q, "Relevant troubleshooting advice."),
                FactEvidence::Unrecognized
            );
        }
    }
}
