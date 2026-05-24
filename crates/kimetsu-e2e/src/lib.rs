//! Layer 1 — kimetsu e2e test harness.
//!
//! v0.5.3 introduces a layered test strategy:
//!   * Per-crate unit tests (`--lib`, ~239 tests as of v0.5.2) catch
//!     focused regressions inside one component.
//!   * This crate (`kimetsu-e2e`) drives the agent loop + brain
//!     pipeline together through a scripted ModelProvider, exercising
//!     real `ToolRuntime` calls against a real brain.db. Catches the
//!     wiring-level regressions that unit tests miss by construction.
//!   * A small `crates/kimetsu-cli/tests/cli_smoke.rs` shells out to
//!     the built binary so CLI argparse / subcommand wiring is
//!     covered too.
//!
//! Scope: one scenario per major v0.5 behavioral feature.
//!   * `tests/golden_path.rs`  — happy-path add_memory → run → blame
//!   * `tests/citations.rs`    — strong (cited) vs weak (silent passenger)
//!   * `tests/decay.rs`        — half-life ranking via the broker
//!   * `tests/conflicts.rs`    — memory_conflicts surface end-to-end
//!
//! Adding a new feature in v0.6+ MUST come with one e2e scenario here.
//!
//! ## What this crate provides
//!
//! The `prelude` module re-exports everything an integration test
//! needs in one wildcard import:
//!
//! ```rust,ignore
//! use kimetsu_e2e::prelude::*;
//!
//! #[test]
//! fn my_e2e_scenario() {
//!     let project = TempProject::init("my_scenario");
//!     let mut provider = ScriptedProvider::new()
//!         .turn(|t| t.call_tool("read_file", json!({"path": "src/main.rs"})))
//!         .turn(|t| t.done("done"))
//!         .build();
//!     // ... drive the agent loop ...
//! }
//! ```
//!
//! No API keys required. Runs in single-digit seconds under `cargo test`.

pub mod assertions;
pub mod scripted_provider;
pub mod temp_project;

pub mod prelude {
    //! One-stop wildcard import for integration tests.
    //!
    //! ```rust,ignore
    //! use kimetsu_e2e::prelude::*;
    //! ```

    pub use crate::assertions::{
        assert_citation_recorded, assert_memory_present, count_citations, count_conflicts,
        count_memories,
    };
    pub use crate::scripted_provider::ScriptedProvider;
    pub use crate::temp_project::TempProject;

    // Re-export the kimetsu surface so tests don't have to add a dep
    // on every workspace crate.
    pub use kimetsu_agent::harness::{KimetsuAgentOpts, run_model_agent};
    pub use kimetsu_agent::model::{ModelProvider, ModelResponse, ToolCall};
    pub use kimetsu_agent::tools::{ToolRuntime, ToolRuntimeConfig};
    pub use kimetsu_brain::{conflict, project, schema};
    pub use kimetsu_core::ids::RunId;
    pub use kimetsu_core::memory::{MemoryKind, MemoryScope};

    pub use serde_json::{Value, json};
}
