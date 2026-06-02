# SessionEnd Distiller + Interactive Setup — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an opt-in, credentialed kimetsu-side SessionEnd distiller (configured by an interactive install wizard) that distills the session transcript with a cheap model and records memories — alongside the v0.9.0 in-agent harvester.

**Architecture:** A new `[learning.distiller]` config + gitignored `.env` (set by a wizard at `kimetsu plugin install` time). A `SessionEnd` hook runs `kimetsu brain session-end-hook`, which builds an Anthropic(-compatible, base-URL-aware) provider from the config, distills 0–3 lessons, and records them via the existing confidence-gated `propose_or_merge_memory`. When the distiller is enabled, the Stop hook suppresses its end-of-session cue ("Split"); the PostToolUse resolved-failure cue stays.

**Tech Stack:** Rust; `serde`/`toml` (config), `reqwest::blocking` (Anthropic provider), the existing `ModelProvider`/`MockProvider` test seam, `kimetsu_brain::project` memory API.

**Spec:** `docs/superpowers/specs/2026-06-02-sessionend-distiller-design.md`

## File Map

- **Modify** `crates/kimetsu-core/src/config.rs` — add `DistillerSection` nested in `LearningSection`.
- **Modify** `crates/kimetsu-agent/src/anthropic.rs` — optional base URL + `for_distiller` constructor.
- **Create** `crates/kimetsu-cli/src/distiller.rs` — `Lesson`, `parse_lessons`, `distill_lessons`, `build_transcript_view`, `distill_and_record`, `run_session_end_hook`.
- **Create** `crates/kimetsu-cli/src/harvest_setup.rs` — the interactive wizard (`run_harvest_setup`) + `.env`/config/gitignore writers.
- **Modify** `crates/kimetsu-cli/src/main.rs` — `mod distiller; mod harvest_setup;`, `BrainCommand::SessionEndHook` + dispatch, the wizard TTY gate + `--no-setup`/`--setup-harvest` flags in `plugin`, Stop-cue suppression.
- **Modify** `crates/kimetsu-chat/src/bridge.rs` — install a `SessionEnd` hook group.

---

## Task 1: `DistillerSection` config

**Files:**
- Modify: `crates/kimetsu-core/src/config.rs` (the `LearningSection` block + its `Default`)
- Test: same file's `mod tests`

- [ ] **Step 1: Write the failing test**

Add to the existing `pre-v0.8 toml must load` test body (right after the `assert!(config.learning.auto_harvest);` line):

```rust
        // A pre-distiller toml has no [learning.distiller] — defaults to off,
        // anthropic, claude-haiku-4-5.
        assert!(!config.learning.distiller.enabled);
        assert_eq!(config.learning.distiller.provider, "anthropic");
        assert_eq!(config.learning.distiller.model, "claude-haiku-4-5");
        assert_eq!(config.learning.distiller.api_key_env, "ANTHROPIC_API_KEY");
        assert_eq!(config.learning.distiller.base_url_env, "ANTHROPIC_BASE_URL");
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p kimetsu-core`
Expected: FAIL — `no field distiller on type LearningSection`.

- [ ] **Step 3: Add `DistillerSection` and nest it**

In `config.rs`, replace the existing `LearningSection` struct + its `Default` impl with:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LearningSection {
    #[serde(default = "default_auto_harvest")]
    pub auto_harvest: bool,
    /// Opt-in credentialed SessionEnd distiller (configured by the install
    /// wizard). Disabled by default; `#[serde(default)]` keeps older
    /// project.toml files loading.
    #[serde(default)]
    pub distiller: DistillerSection,
}

fn default_auto_harvest() -> bool {
    true
}

impl Default for LearningSection {
    fn default() -> Self {
        Self {
            auto_harvest: default_auto_harvest(),
            distiller: DistillerSection::default(),
        }
    }
}

/// Credentialed SessionEnd distiller config. Secret values (the API key,
/// optional base URL) live in `.env` under the env-var names below; only
/// non-secret selection lives here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DistillerSection {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_distiller_provider")]
    pub provider: String,
    #[serde(default = "default_distiller_model")]
    pub model: String,
    #[serde(default = "default_distiller_api_key_env")]
    pub api_key_env: String,
    #[serde(default = "default_distiller_base_url_env")]
    pub base_url_env: String,
}

fn default_distiller_provider() -> String {
    "anthropic".to_string()
}
fn default_distiller_model() -> String {
    "claude-haiku-4-5".to_string()
}
fn default_distiller_api_key_env() -> String {
    "ANTHROPIC_API_KEY".to_string()
}
fn default_distiller_base_url_env() -> String {
    "ANTHROPIC_BASE_URL".to_string()
}

impl Default for DistillerSection {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: default_distiller_provider(),
            model: default_distiller_model(),
            api_key_env: default_distiller_api_key_env(),
            base_url_env: default_distiller_base_url_env(),
        }
    }
}
```

(Note: if a `default_auto_harvest` fn already exists above, do not duplicate it — keep the single definition.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p kimetsu-core`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/kimetsu-core/src/config.rs
git commit -m "feat: add [learning.distiller] config section"
```

---

## Task 2: Anthropic provider base URL + `for_distiller`

**Files:**
- Modify: `crates/kimetsu-agent/src/anthropic.rs`
- Test: same file's `mod tests`

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `anthropic.rs`:

```rust
#[test]
fn messages_url_uses_base_when_set() {
    assert_eq!(messages_url(&None), MESSAGES_URL);
    assert_eq!(
        messages_url(&Some("http://localhost:4000".to_string())),
        "http://localhost:4000/v1/messages"
    );
    // Trailing slash is trimmed.
    assert_eq!(
        messages_url(&Some("http://localhost:4000/".to_string())),
        "http://localhost:4000/v1/messages"
    );
}

#[test]
fn for_distiller_builds_provider_with_base_url() {
    let p = AnthropicProvider::for_distiller(
        "claude-haiku-4-5",
        "sk-test",
        Some("http://localhost:4000".to_string()),
        60,
    )
    .expect("build");
    assert_eq!(p.model_name(), "claude-haiku-4-5");
    assert_eq!(p.base_url.as_deref(), Some("http://localhost:4000"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p kimetsu-agent messages_url_uses_base_when_set for_distiller_builds_provider_with_base_url`
Expected: FAIL — `messages_url`/`for_distiller`/`base_url` not found.

- [ ] **Step 3: Add the `base_url` field, helper, and constructor**

In `anthropic.rs`, add `base_url` to the struct:

```rust
#[derive(Debug, Clone)]
pub struct AnthropicProvider {
    client: Client,
    api_key: SecretString,
    model: String,
    /// When set, requests POST to `<base_url>/v1/messages` (Anthropic-
    /// compatible endpoints such as a LiteLLM proxy). When `None`, the
    /// default Anthropic API URL is used.
    base_url: Option<String>,
}
```

In `from_config_with_key`, set `base_url: None` in the returned struct literal (the main `[model]` provider keeps the default endpoint). Then add the constructor + helper:

```rust
impl AnthropicProvider {
    /// Build a provider directly from resolved values — used by the
    /// SessionEnd distiller, which controls model/key/base independent of
    /// the project's `[model]` section. `base_url` is normalized: empty/
    /// whitespace becomes `None`.
    pub fn for_distiller(
        model: impl Into<String>,
        api_key: impl Into<String>,
        base_url: Option<String>,
        timeout_secs: u64,
    ) -> KimetsuResult<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .build()?;
        Ok(Self {
            client,
            api_key: SecretString::new(api_key.into()),
            model: model.into(),
            base_url: base_url.filter(|value| !value.trim().is_empty()),
        })
    }
}

/// Resolve the messages endpoint: `<base>/v1/messages` when a base URL is
/// configured, else the default Anthropic API URL.
fn messages_url(base_url: &Option<String>) -> String {
    match base_url {
        Some(base) => format!("{}/v1/messages", base.trim_end_matches('/')),
        None => MESSAGES_URL.to_string(),
    }
}
```

Make the test's `p.base_url` field readable from the test: the test lives in the same module (`mod tests` under `anthropic.rs`), so the private field is accessible — no visibility change needed.

- [ ] **Step 4: Route `complete()` through `messages_url`**

In `impl ModelProvider for AnthropicProvider`, change the `.post(MESSAGES_URL)` line to:

```rust
        let url = messages_url(&self.base_url);
        let response = self
            .client
            .post(&url)
            .header("x-api-key", self.api_key.expose_secret())
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()?;
```

Also update the existing `debug_format_does_not_leak_api_key` test's struct literal to add `base_url: None,` (it constructs `AnthropicProvider { … }` directly).

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p kimetsu-agent`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/kimetsu-agent/src/anthropic.rs
git commit -m "feat: optional base URL on AnthropicProvider for the distiller"
```

---

## Task 3: Distiller module (parse, distill, transcript view, record, hook)

**Files:**
- Create: `crates/kimetsu-cli/src/distiller.rs`
- Modify: `crates/kimetsu-cli/src/main.rs` (add `mod distiller;`, `BrainCommand::SessionEndHook`, dispatch)
- Test: `crates/kimetsu-cli/src/distiller.rs` `mod tests`

- [ ] **Step 1: Create `distiller.rs` with the full module**

Create `crates/kimetsu-cli/src/distiller.rs`:

```rust
//! Credentialed SessionEnd distiller: at session end, ask a cheap
//! configured model to distill the transcript into 0-3 generalizable
//! lessons and record them via the confidence-gated brain memory API.
//! Best-effort throughout — a hook must never break session shutdown.

use std::io::{BufRead, Read};
use std::path::Path;

use kimetsu_agent::anthropic::AnthropicProvider;
use kimetsu_agent::model::{
    MessageContent, MessageRole, ModelMessage, ModelProvider, ModelRequest, ToolChoice,
};
use kimetsu_brain::project;
use kimetsu_core::env_file::resolve_env_value;
use kimetsu_core::memory::{MemoryKind, MemoryScope};
use kimetsu_core::paths::ProjectPaths;
use serde::Deserialize;

/// Max characters of transcript view fed to the distiller (keeps the
/// model call cheap and bounded).
const MAX_VIEW_CHARS: usize = 12_000;

const DISTILL_SYSTEM: &str = "You are Kimetsu's memory distiller. From the session transcript, extract durable, \
generalizable lessons worth remembering across future sessions — favoring non-obvious fixes for \
commands/tools that failed and were resolved, hard-won environment quirks, and confirmed conventions or \
anti-patterns. Ignore trivia, one-liners, and anything specific to a single throwaway value.\n\n\
Reply with ONLY a JSON array (no prose, no markdown) of at most 3 objects:\n\
[{\"lesson\": \"concrete, actionable, generalized\", \"tags\": [\"2-5\", \"domain\", \"tags\"], \
\"kind\": \"semantic_operator|anti_pattern|convention\", \"confidence\": 0.0-1.0}]\n\
Use confidence 0.8 when you're sure it generalizes, lower when unsure. If nothing qualifies, reply [].";

#[derive(Debug, Deserialize, PartialEq)]
pub struct Lesson {
    pub lesson: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default = "default_kind")]
    pub kind: String,
    #[serde(default = "default_confidence")]
    pub confidence: f32,
}

fn default_kind() -> String {
    "semantic_operator".to_string()
}
fn default_confidence() -> f32 {
    0.7
}

/// Extract the first JSON array from `text` and parse it into lessons.
/// Tolerant: returns empty on any parse failure; drops empty lessons;
/// caps at 3.
pub fn parse_lessons(text: &str) -> Vec<Lesson> {
    let (Some(start), Some(end)) = (text.find('['), text.rfind(']')) else {
        return Vec::new();
    };
    if end < start {
        return Vec::new();
    }
    serde_json::from_str::<Vec<Lesson>>(&text[start..=end])
        .unwrap_or_default()
        .into_iter()
        .filter(|l| !l.lesson.trim().is_empty())
        .take(3)
        .collect()
}

/// Ask the model to distill lessons from a transcript view. Returns empty
/// on any model/parse error.
pub fn distill_lessons(transcript_view: &str, provider: &mut dyn ModelProvider) -> Vec<Lesson> {
    let request = ModelRequest {
        messages: vec![
            ModelMessage {
                role: MessageRole::System,
                content: vec![MessageContent::Text {
                    text: DISTILL_SYSTEM.to_string(),
                }],
            },
            ModelMessage::user_text(transcript_view),
        ],
        tools: Vec::new(),
        tool_choice: ToolChoice::None,
        max_output_tokens: 1024,
        temperature: 0.2,
        metadata: serde_json::Value::Null,
    };
    match provider.complete(request) {
        Ok(response) => parse_lessons(response.text.as_deref().unwrap_or("")),
        Err(_) => Vec::new(),
    }
}

/// Stream a transcript JSONL into a compact, character-bounded view of the
/// user/assistant text (most-recent tail kept). Best-effort.
pub fn build_transcript_view(path: &str, max_chars: usize) -> String {
    let Ok(file) = std::fs::File::open(path) else {
        return String::new();
    };
    let mut out = String::new();
    for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
        let line = line.trim().trim_start_matches('\u{feff}');
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let Some(snippet) = extract_snippet(&value) {
            out.push_str(&snippet);
            out.push('\n');
            // Keep the working buffer bounded as we stream.
            if out.chars().count() > max_chars * 2 {
                out = tail_chars(&out, max_chars);
            }
        }
    }
    tail_chars(&out, max_chars)
}

/// Pull a `role: text` snippet from one transcript message (text content
/// blocks only; tool blocks are noted briefly). Returns None when there's
/// no human-readable text.
fn extract_snippet(value: &serde_json::Value) -> Option<String> {
    let message = value.get("message").unwrap_or(value);
    let role = message
        .get("role")
        .and_then(|r| r.as_str())
        .or_else(|| value.get("type").and_then(|t| t.as_str()))
        .unwrap_or("msg");
    let content = message.get("content").or_else(|| value.get("content"))?;
    let mut parts = Vec::new();
    if let Some(text) = content.as_str() {
        parts.push(text.to_string());
    } else if let Some(blocks) = content.as_array() {
        for block in blocks {
            if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                parts.push(text.to_string());
            } else if let Some(name) = block.get("name").and_then(|n| n.as_str()) {
                parts.push(format!("[tool {name}]"));
            }
        }
    }
    let body = parts.join(" ").trim().to_string();
    if body.is_empty() {
        None
    } else {
        Some(format!("{role}: {body}"))
    }
}

/// Char-boundary-safe tail of at most `n` chars.
fn tail_chars(s: &str, n: usize) -> String {
    let count = s.chars().count();
    if count <= n {
        s.to_string()
    } else {
        s.chars().skip(count - n).collect()
    }
}

/// Distill lessons from `view` and record each via the confidence-gated
/// brain API. Returns the count recorded (added/merged/proposed all count).
pub fn distill_and_record(start: &Path, view: &str, provider: &mut dyn ModelProvider) -> usize {
    let mut recorded = 0;
    for lesson in distill_lessons(view, provider) {
        let kind = match lesson.kind.as_str() {
            "anti_pattern" => MemoryKind::FailurePattern,
            "convention" => MemoryKind::Convention,
            _ => MemoryKind::Fact,
        };
        let confidence = lesson.confidence.clamp(0.0, 1.0);
        if project::propose_or_merge_memory(
            start,
            MemoryScope::Project,
            kind,
            lesson.lesson.trim(),
            confidence,
            "auto-harvested at session end",
        )
        .is_ok()
        {
            recorded += 1;
        }
    }
    recorded
}

/// `kimetsu brain session-end-hook` entry. Reads the SessionEnd payload
/// from stdin, and if the distiller is enabled + credentialed, distills
/// the transcript and records lessons. Silent no-op otherwise.
pub fn run_session_end_hook(workspace: &Path) {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).ok();
    let payload: serde_json::Value =
        serde_json::from_str(input.trim()).unwrap_or(serde_json::Value::Null);

    let Ok(paths) = ProjectPaths::discover(workspace) else {
        return;
    };
    let Ok(config) = project::load_config(&paths) else {
        return;
    };
    let distiller = &config.learning.distiller;
    if !distiller.enabled || distiller.provider != "anthropic" {
        return;
    }
    let Some(api_key) = resolve_env_value(&paths.repo_root, &distiller.api_key_env) else {
        return;
    };
    let base_url = resolve_env_value(&paths.repo_root, &distiller.base_url_env);
    let Some(transcript_path) = payload
        .get("transcript_path")
        .and_then(|v| v.as_str())
        .filter(|p| !p.trim().is_empty())
    else {
        return;
    };
    let view = build_transcript_view(transcript_path, MAX_VIEW_CHARS);
    if view.trim().is_empty() {
        return;
    }
    let Ok(mut provider) = AnthropicProvider::for_distiller(
        &distiller.model,
        api_key,
        base_url,
        config.model.request_timeout_secs,
    ) else {
        return;
    };
    let recorded = distill_and_record(&paths.repo_root, &view, &mut provider);
    if recorded > 0 {
        println!(
            "[Kimetsu] distilled {recorded} lesson{} at session end.",
            if recorded == 1 { "" } else { "s" }
        );
    }
}
```

- [ ] **Step 2: Wire the module + command in `main.rs`**

Add near the other `mod` declarations at the top of `main.rs`:

```rust
mod distiller;
mod harvest_setup;
```

(Adding `mod harvest_setup;` now is fine even though Task 6 creates the file — but to keep this task compiling on its own, add ONLY `mod distiller;` here and add `mod harvest_setup;` in Task 6.)

So for THIS task add only:

```rust
mod distiller;
```

Add a `BrainCommand` variant (in the `enum BrainCommand { … }`):

```rust
    /// Claude Code SessionEnd hook — runs the credentialed distiller.
    #[command(name = "session-end-hook")]
    SessionEndHook(StopHookArgs),
```

In the `brain` dispatch `match` (where `BrainCommand::StopHook(args) => …` is handled), add:

```rust
        BrainCommand::SessionEndHook(args) => {
            let workspace = args
                .workspace
                .unwrap_or_else(|| env::current_dir().unwrap_or_default());
            distiller::run_session_end_hook(&workspace);
            Ok(())
        }
```

- [ ] **Step 3: Add the module tests**

Append to `distiller.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use kimetsu_agent::model::{MockProvider, ModelResponse, StopReason, TokenUsage};

    fn text_response(text: &str) -> ModelResponse {
        ModelResponse {
            text: Some(text.to_string()),
            tool_calls: Vec::new(),
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        }
    }

    #[test]
    fn parse_lessons_extracts_array_and_defaults() {
        let lessons = parse_lessons(
            "Sure! Here you go:\n[{\"lesson\":\"Use X not Y\",\"tags\":[\"a\"]}, \
             {\"lesson\":\"  \"}]\nthanks",
        );
        assert_eq!(lessons.len(), 1, "blank lesson dropped");
        assert_eq!(lessons[0].lesson, "Use X not Y");
        assert_eq!(lessons[0].kind, "semantic_operator");
        assert_eq!(lessons[0].confidence, 0.7);
    }

    #[test]
    fn parse_lessons_tolerates_garbage() {
        assert!(parse_lessons("no json here").is_empty());
        assert!(parse_lessons("[not valid json}").is_empty());
        assert!(parse_lessons("[]").is_empty());
    }

    #[test]
    fn distill_lessons_uses_model_text() {
        let mut provider = MockProvider::new([text_response(
            "[{\"lesson\":\"Pin the linker\",\"tags\":[\"rust\",\"windows\"],\"kind\":\"convention\",\"confidence\":0.9}]",
        )]);
        let lessons = distill_lessons("user: it failed\nuser: fixed it", &mut provider);
        assert_eq!(lessons.len(), 1);
        assert_eq!(lessons[0].kind, "convention");
        // The provider was asked exactly once, with a system + user message.
        assert_eq!(provider.requests.len(), 1);
        assert_eq!(provider.requests[0].messages.len(), 2);
    }

    #[test]
    fn distill_lessons_empty_on_model_error() {
        // MockProvider with no queued response returns Err on complete().
        let mut provider = MockProvider::new([]);
        assert!(distill_lessons("anything", &mut provider).is_empty());
    }

    #[test]
    fn build_transcript_view_streams_text_and_bounds() {
        let dir = std::env::temp_dir().join(format!(
            "kimetsu_view_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        std::fs::write(
            &path,
            "\u{feff}{\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"hello\"}]}}\n\
             {\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"name\":\"Bash\"}]}}\n\
             garbage line\n\
             {\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"bye\"}]}}\n",
        )
        .unwrap();

        let view = build_transcript_view(path.to_str().unwrap(), 10_000);
        assert!(view.contains("user: hello"));
        assert!(view.contains("assistant: [tool Bash]"));
        assert!(view.contains("assistant: bye"));

        // Bounded: a tiny cap keeps only the tail.
        let tiny = build_transcript_view(path.to_str().unwrap(), 5);
        assert!(tiny.chars().count() <= 5);

        // Missing file → empty.
        assert!(build_transcript_view("/no/such.jsonl", 100).is_empty());

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn distill_and_record_writes_to_a_temp_brain() {
        // A real (temp) brain so propose_or_merge_memory persists.
        let root = std::env::temp_dir().join(format!(
            "kimetsu_distill_brain_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        // Isolate from any developer/user brain.
        // SAFETY: single-threaded test setup.
        unsafe {
            std::env::set_var("KIMETSU_USER_BRAIN", "0");
        }
        kimetsu_brain::project::init_project(&root, true).expect("init brain");

        let mut provider = MockProvider::new([text_response(
            "[{\"lesson\":\"Set USERPROFILE for global installs\",\"tags\":[\"cargo\",\"windows\"],\"confidence\":0.9}]",
        )]);
        let n = distill_and_record(&root, "user: a\nuser: b", &mut provider);
        assert_eq!(n, 1);
        let memories = kimetsu_brain::project::list_memories(&root).expect("list");
        assert!(
            memories.iter().any(|m| m.text.contains("USERPROFILE")),
            "distilled lesson was recorded"
        );

        std::fs::remove_dir_all(root).ok();
    }
}
```

- [ ] **Step 4: Run tests + build**

Run: `cargo test -p kimetsu-cli distiller`
Expected: PASS (5 tests).

Run: `cargo build -p kimetsu-cli`
Expected: builds (the `session-end-hook` command is wired).

If `ProjectConfig` import is unused, remove it and the guard line; rebuild.

- [ ] **Step 5: Commit**

```bash
git add crates/kimetsu-cli/src/distiller.rs crates/kimetsu-cli/src/main.rs
git commit -m "feat: SessionEnd distiller (parse/distill/record) + brain session-end-hook"
```

---

## Task 4: Install a `SessionEnd` hook

**Files:**
- Modify: `crates/kimetsu-chat/src/bridge.rs` (`write_claude_hooks`)
- Test: `crates/kimetsu-chat/src/bridge.rs` `mod tests`

- [ ] **Step 1: Write the failing test**

Add to `mod tests`:

```rust
#[test]
fn claude_hooks_install_session_end() {
    let root = temp_root("claude_session_end");
    let claude = root.join(".claude");
    fs::create_dir_all(&claude).unwrap();
    let settings = claude.join("settings.json");
    write_claude_hooks(&settings, true).unwrap();

    let value: serde_json::Value =
        serde_json::from_str(strip_bom(&fs::read_to_string(&settings).unwrap())).unwrap();
    assert_eq!(
        value["hooks"]["SessionEnd"][0]["hooks"][0]["command"],
        "kimetsu brain session-end-hook"
    );
    fs::remove_dir_all(root).ok();
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p kimetsu-chat claude_hooks_install_session_end`
Expected: FAIL — `SessionEnd` is null.

- [ ] **Step 3: Add the SessionEnd upsert**

In `write_claude_hooks`, after the `Stop` `upsert_kimetsu_hook(...)` call, add:

```rust
    upsert_kimetsu_hook(
        hooks_obj,
        "SessionEnd",
        serde_json::json!({
            "matcher": "",
            "hooks": [{ "type": "command", "command": "kimetsu brain session-end-hook" }]
        }),
    );
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p kimetsu-chat`
Expected: PASS (existing Claude hook tests still green; the new event sits alongside).

- [ ] **Step 5: Commit**

```bash
git add crates/kimetsu-chat/src/bridge.rs
git commit -m "feat: install a SessionEnd hook for the distiller"
```

---

## Task 5: Suppress the Stop end-of-session cue when the distiller is enabled

**Files:**
- Modify: `crates/kimetsu-cli/src/main.rs` (`brain_stop_hook`)
- Test: covered by behavior; add a focused unit test on the gate predicate.

- [ ] **Step 1: Write the failing test**

The cue gate becomes a small predicate. Add a free function + test. Add to `main.rs` near `brain_stop_hook`:

```rust
/// The end-of-session harvest cue fires only when auto-harvest is on AND
/// the credentialed distiller is not handling end-of-session itself.
fn should_emit_stop_harvest_cue(auto_harvest: bool, distiller_enabled: bool) -> bool {
    auto_harvest && !distiller_enabled
}
```

Add to `main.rs` `mod tests`:

```rust
    #[test]
    fn stop_cue_suppressed_when_distiller_enabled() {
        assert!(should_emit_stop_harvest_cue(true, false));
        assert!(!should_emit_stop_harvest_cue(true, true));
        assert!(!should_emit_stop_harvest_cue(false, false));
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p kimetsu-cli stop_cue_suppressed_when_distiller_enabled`
Expected: FAIL — `should_emit_stop_harvest_cue` not found.

- [ ] **Step 3: Use the predicate in `brain_stop_hook`**

In `brain_stop_hook`, the config is currently loaded as:

```rust
    let auto_harvest = paths
        .as_ref()
        .and_then(|p| project::load_config(p).ok())
        .map(|c| c.learning.auto_harvest)
        .unwrap_or(true);
```

Replace it with a load of both flags:

```rust
    let (auto_harvest, distiller_enabled) = paths
        .as_ref()
        .and_then(|p| project::load_config(p).ok())
        .map(|c| (c.learning.auto_harvest, c.learning.distiller.enabled))
        .unwrap_or((true, false));
```

Then change the cue guard from `if auto_harvest && !stop_active && let Some(paths) = …` to use the predicate:

```rust
    if should_emit_stop_harvest_cue(auto_harvest, distiller_enabled)
        && !stop_active
        && let Some(paths) = paths.as_ref()
    {
```

(The record-count banner and the plain `[Kimetsu] No lessons recorded…` fallback are unchanged.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p kimetsu-cli`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/kimetsu-cli/src/main.rs
git commit -m "feat: suppress Stop harvest cue when the distiller is enabled"
```

---

## Task 6: Interactive setup wizard

**Files:**
- Create: `crates/kimetsu-cli/src/harvest_setup.rs`
- Modify: `crates/kimetsu-cli/src/main.rs` (`mod harvest_setup;`, `PluginInstallArgs` flags, wizard call in `plugin`)
- Test: `crates/kimetsu-cli/src/harvest_setup.rs` `mod tests`

- [ ] **Step 1: Create `harvest_setup.rs`**

Create `crates/kimetsu-cli/src/harvest_setup.rs`:

```rust
//! Interactive `kimetsu plugin install` wizard that configures the
//! credentialed SessionEnd distiller: collects an API key (+ optional
//! LiteLLM base URL) + model, writes a gitignored `.env`, and flips
//! `[learning.distiller]` on in the workspace project.toml.

use std::fs;
use std::io::{BufRead, Write};
use std::path::Path;

use kimetsu_brain::project;
use kimetsu_core::paths::ProjectPaths;

/// Run the wizard against the given reader/writer (real stdin/stdout in
/// production; scripted in tests). Returns Ok(true) when the distiller was
/// configured, Ok(false) when the user declined or aborted.
pub fn run_harvest_setup<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    paths: &ProjectPaths,
) -> std::io::Result<bool> {
    write!(writer, "Set up the auto-harvest distiller now? [y/N]: ")?;
    writer.flush()?;
    if !read_line(reader)?.trim().eq_ignore_ascii_case("y") {
        return Ok(false);
    }

    write!(writer, "Harness [claude/codex] (codex not yet supported): ")?;
    writer.flush()?;
    let harness = read_line(reader)?.trim().to_lowercase();
    if harness == "codex" {
        writeln!(writer, "Codex distiller is not supported yet — skipping setup.")?;
        return Ok(false);
    }

    write!(writer, "Anthropic API key (or LiteLLM key): ")?;
    writer.flush()?;
    let key = read_line(reader)?.trim().to_string();
    if key.is_empty() {
        writeln!(writer, "No key entered — skipping setup.")?;
        return Ok(false);
    }

    write!(
        writer,
        "ANTHROPIC_BASE_URL (optional; blank for Anthropic, set for LiteLLM): "
    )?;
    writer.flush()?;
    let base_url = read_line(reader)?.trim().to_string();

    write!(writer, "Model [claude-haiku-4-5]: ")?;
    writer.flush()?;
    let mut model = read_line(reader)?.trim().to_string();
    if model.is_empty() {
        model = "claude-haiku-4-5".to_string();
    }

    apply_distiller_config(paths, &model)?;
    let env_path = paths.repo_root.join(".env");
    upsert_env_var(&env_path, "ANTHROPIC_API_KEY", &key)?;
    if !base_url.is_empty() {
        upsert_env_var(&env_path, "ANTHROPIC_BASE_URL", &base_url)?;
    }
    ensure_gitignored(&paths.repo_root, ".env")?;

    writeln!(
        writer,
        "✓ Distiller configured (model {model}). Key stored in .env (gitignored). \
         Note: the key was entered in plain text."
    )?;
    Ok(true)
}

fn read_line<R: BufRead>(reader: &mut R) -> std::io::Result<String> {
    let mut line = String::new();
    reader.read_line(&mut line)?;
    Ok(line)
}

/// Load (or initialize) the workspace project config and flip the
/// distiller on with the chosen model.
fn apply_distiller_config(paths: &ProjectPaths, model: &str) -> std::io::Result<()> {
    // KimetsuResult's error is Box<dyn Error + Send + Sync>; it implements
    // Display, so to_string() works without naming a concrete error type.
    let io_err = |e: Box<dyn std::error::Error + Send + Sync>| std::io::Error::other(e.to_string());
    if !paths.project_toml.exists() {
        project::init_project(&paths.repo_root, false).map_err(io_err)?;
    }
    let mut config = project::load_config(paths).map_err(io_err)?;
    config.learning.distiller.enabled = true;
    config.learning.distiller.provider = "anthropic".to_string();
    config.learning.distiller.model = model.to_string();
    config.learning.distiller.api_key_env = "ANTHROPIC_API_KEY".to_string();
    config.learning.distiller.base_url_env = "ANTHROPIC_BASE_URL".to_string();
    let toml = config.to_toml().map_err(io_err)?;
    fs::write(&paths.project_toml, toml)
}

/// Insert or replace `NAME=value` in a `.env` file (created if missing).
fn upsert_env_var(env_path: &Path, name: &str, value: &str) -> std::io::Result<()> {
    let existing = fs::read_to_string(env_path).unwrap_or_default();
    let mut lines: Vec<String> = Vec::new();
    let mut replaced = false;
    for line in existing.lines() {
        if line.split_once('=').map(|(k, _)| k.trim() == name).unwrap_or(false) {
            lines.push(format!("{name}={value}"));
            replaced = true;
        } else {
            lines.push(line.to_string());
        }
    }
    if !replaced {
        lines.push(format!("{name}={value}"));
    }
    let mut body = lines.join("\n");
    body.push('\n');
    fs::write(env_path, body)
}

/// Ensure `entry` is present in the repo's `.gitignore` (created if absent).
fn ensure_gitignored(repo_root: &Path, entry: &str) -> std::io::Result<()> {
    let path = repo_root.join(".gitignore");
    let existing = fs::read_to_string(&path).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == entry) {
        return Ok(());
    }
    let mut body = existing;
    if !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }
    body.push_str(entry);
    body.push('\n');
    fs::write(&path, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    // ProjectPaths has 7 fields; build it via `discover` (temp dirs live
    // outside the kimetsu git tree, so repo_root resolves to `root` itself).
    fn paths_for(root: &Path) -> ProjectPaths {
        ProjectPaths::discover(root).expect("discover temp paths")
    }

    #[test]
    fn wizard_writes_env_and_config() {
        let root = std::env::temp_dir().join(format!(
            "kimetsu_wizard_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join(".kimetsu")).unwrap();
        let paths = paths_for(&root);

        // y → claude → key → base url (LiteLLM) → blank model (default).
        let mut input = Cursor::new(
            "y\nclaude\nsk-litellm-123\nhttp://localhost:4000\n\n".as_bytes().to_vec(),
        );
        let mut output = Vec::new();
        let configured = run_harvest_setup(&mut input, &mut output, &paths).unwrap();
        assert!(configured);

        // Read via `paths` (canonicalized) to match where the wizard wrote.
        let env = fs::read_to_string(paths.repo_root.join(".env")).unwrap();
        assert!(env.contains("ANTHROPIC_API_KEY=sk-litellm-123"));
        assert!(env.contains("ANTHROPIC_BASE_URL=http://localhost:4000"));
        let toml = fs::read_to_string(&paths.project_toml).unwrap();
        assert!(toml.contains("enabled = true"));
        assert!(toml.contains("claude-haiku-4-5"));
        assert!(
            fs::read_to_string(paths.repo_root.join(".gitignore"))
                .unwrap()
                .contains(".env")
        );

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn wizard_declined_writes_nothing() {
        let root = std::env::temp_dir().join(format!(
            "kimetsu_wizard_no_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let paths = paths_for(&root);
        let mut input = Cursor::new(b"n\n".to_vec());
        let mut output = Vec::new();
        assert!(!run_harvest_setup(&mut input, &mut output, &paths).unwrap());
        assert!(!root.join(".env").exists());
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn upsert_env_var_replaces_existing() {
        let dir = std::env::temp_dir().join(format!(
            "kimetsu_env_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let env = dir.join(".env");
        upsert_env_var(&env, "ANTHROPIC_API_KEY", "old").unwrap();
        upsert_env_var(&env, "OTHER", "keep").unwrap();
        upsert_env_var(&env, "ANTHROPIC_API_KEY", "new").unwrap();
        let body = fs::read_to_string(&env).unwrap();
        assert!(body.contains("ANTHROPIC_API_KEY=new"));
        assert!(!body.contains("ANTHROPIC_API_KEY=old"));
        assert!(body.contains("OTHER=keep"));
        fs::remove_dir_all(dir).ok();
    }
}
```

(Note: `std::io::Error::other` requires Rust ≥1.74; the workspace MSRV is 1.85, so it's available. `KimetsuError` is the error type behind `KimetsuResult`; confirm the exact name via `kimetsu_core` and adjust the `io_err` closure if it differs, e.g. `Box<dyn Error>` → `std::io::Error::other(e.to_string())` works for any `Display`.)

- [ ] **Step 2: Run the module tests**

Run: `cargo test -p kimetsu-cli` — but first the module must be declared. Add `mod harvest_setup;` to `main.rs` (next to `mod distiller;`). Then:

Run: `cargo test -p kimetsu-cli harvest_setup`
Expected: PASS (3 tests).

- [ ] **Step 3: Add CLI flags + the TTY-gated call**

In `PluginInstallArgs` (in `main.rs`), add after `no_proactive`:

```rust
    /// Skip the interactive auto-harvest distiller setup prompt.
    #[arg(long)]
    no_setup: bool,
    /// Force the auto-harvest distiller setup prompt even off a TTY.
    #[arg(long)]
    setup_harvest: bool,
```

In the `plugin` fn, after the install report is printed (the `for file in report.files { … }` loop), add — still inside the `PluginCommand::Install(args)` arm:

```rust
            // Offer interactive distiller setup for Claude Code on a TTY.
            use std::io::IsTerminal;
            let interactive = args.setup_harvest
                || (std::io::stdin().is_terminal() && std::io::stdout().is_terminal());
            if matches!(target, BridgeTarget::ClaudeCode) && !args.no_setup && interactive {
                if let Ok(paths) = kimetsu_core::paths::ProjectPaths::discover(&workspace) {
                    let stdin = std::io::stdin();
                    let mut reader = stdin.lock();
                    let mut stdout = std::io::stdout();
                    if let Err(err) =
                        harvest_setup::run_harvest_setup(&mut reader, &mut stdout, &paths)
                    {
                        eprintln!("kimetsu plugin install: distiller setup skipped: {err}");
                    }
                }
            }
```

(`target` is in scope from the parse above. `ProjectPaths::discover(&workspace)` does NOT require a `.kimetsu` dir — it resolves `repo_root` via git-root/canonicalize and derives the paths — so it succeeds on a freshly-installed workspace; the wizard's `apply_distiller_config` then calls `init_project` to create `.kimetsu/project.toml`.)

- [ ] **Step 4: Build + test the whole crate**

Run: `cargo build -p kimetsu-cli`
Expected: builds.

Run: `cargo test -p kimetsu-cli`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/kimetsu-cli/src/harvest_setup.rs crates/kimetsu-cli/src/main.rs
git commit -m "feat: interactive distiller setup wizard at plugin install"
```

---

## Task 7: Workspace verification + docs

**Files:**
- Verify only; CHANGELOG/README touch.

- [ ] **Step 1: Full build, test, fmt**

Run: `cargo build --workspace` → clean.
Run: `cargo test --workspace` → all green.
Run: `cargo fmt --all && cargo fmt --all --check` → clean.

- [ ] **Step 2: Manual wizard smoke (interactive)**

```bash
d=$(mktemp -d)
target/debug/kimetsu.exe plugin install claude-code --workspace "$(cygpath -m "$d")"
# answer: y / claude / sk-test / (blank) / (blank)
cat "$d/.env"                       # ANTHROPIC_API_KEY=sk-test
cat "$d/.kimetsu/project.toml"      # [learning.distiller] enabled = true, model claude-haiku-4-5
grep '"SessionEnd"' "$d/.claude/settings.json"
rm -rf "$d"
```

Expected: `.env` has the key, project.toml has the distiller enabled, settings.json has the SessionEnd hook.

- [ ] **Step 3: Manual session-end-hook smoke (disabled → silent; enabled needs a live key)**

```bash
echo '{"session_id":"s","transcript_path":"/no/file"}' | target/debug/kimetsu.exe brain session-end-hook --workspace .
# prints nothing (distiller disabled in this repo) — confirms graceful no-op
```

- [ ] **Step 4: Update CHANGELOG + README**

Add an `Unreleased` ADDED entry describing the opt-in credentialed SessionEnd distiller + the install wizard (Anthropic/LiteLLM, `claude-haiku-4-5` default, `[learning.distiller]`, `.env` storage, "Split" with the in-agent cue). Add a short README note under the auto-harvest paragraph.

- [ ] **Step 5: Commit**

```bash
git add CHANGELOG.md README.md
git commit -m "docs: document the SessionEnd distiller + setup wizard"
```

---

## Done

Hooks now offer both memory-generation paths: the no-setup in-agent harvester (v0.9.0) and, when configured via the wizard, a credentialed SessionEnd distiller (Anthropic or LiteLLM) that records lessons at session end — with the Stop cue suppressed so they don't double up.
