use std::fmt::{Display, Formatter};
use std::str::FromStr;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScope {
    GlobalUser,
    Project,
    Repo,
    Run,
}

impl Display for MemoryScope {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::GlobalUser => "global_user",
            Self::Project => "project",
            Self::Repo => "repo",
            Self::Run => "run",
        };
        f.write_str(value)
    }
}

impl FromStr for MemoryScope {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "global_user" | "global-user" | "user" => Ok(Self::GlobalUser),
            "project" => Ok(Self::Project),
            "repo" | "repository" => Ok(Self::Repo),
            "run" => Ok(Self::Run),
            _ => Err(format!("unknown memory scope: {value}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    Preference,
    Convention,
    Command,
    FailurePattern,
    Fact,
}

impl Display for MemoryKind {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::Preference => "preference",
            Self::Convention => "convention",
            Self::Command => "command",
            Self::FailurePattern => "failure_pattern",
            Self::Fact => "fact",
        };
        f.write_str(value)
    }
}

impl FromStr for MemoryKind {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "preference" => Ok(Self::Preference),
            "convention" => Ok(Self::Convention),
            "command" => Ok(Self::Command),
            "failure_pattern" | "failure-pattern" => Ok(Self::FailurePattern),
            "fact" => Ok(Self::Fact),
            _ => Err(format!("unknown memory kind: {value}")),
        }
    }
}

pub fn normalize_memory_text(value: &str) -> String {
    value
        .trim()
        .trim_end_matches(['.', '!', '?', ';', ':'])
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}
