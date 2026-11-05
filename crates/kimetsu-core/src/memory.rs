use std::fmt::{Display, Formatter};
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Grant and contamination boundary. `project` is not a level: it is an
/// input alias of [`MountLevel::Repo`] and stays a distinct stored string
/// until a durable migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MountLevel {
    Run,
    Session,
    Repo,
    Org,
    User,
    Skill,
}

impl MountLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::Session => "session",
            Self::Repo => "repo",
            Self::Org => "org",
            Self::User => "user",
            Self::Skill => "skill",
        }
    }
}

impl Display for MountLevel {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for MountLevel {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "run" => Ok(Self::Run),
            "session" => Ok(Self::Session),
            "repo" | "repository" | "project" => Ok(Self::Repo),
            "org" => Ok(Self::Org),
            "user" | "global_user" | "global-user" => Ok(Self::User),
            "skill" => Ok(Self::Skill),
            _ => Err(format!("unknown mount: {value}")),
        }
    }
}

/// Stored scope strings that belong to one mount.
///
/// Repo includes the alias-window strings `project` and `repository`.
/// Those rows stay distinct in the projection; this list is only for
/// grants, retrieval, and duplicate detection.
pub fn mount_stored_scopes(level: MountLevel) -> &'static [&'static str] {
    match level {
        MountLevel::Run => &["run"],
        MountLevel::Session => &["session"],
        MountLevel::Repo => &["repo", "project", "repository"],
        MountLevel::Org => &["org"],
        MountLevel::User => &["global_user", "user", "global-user"],
        MountLevel::Skill => &["skill"],
    }
}

/// Label a stored scope for retrieval. Alias-window `project` reads as `repo`.
pub fn retrieval_label(stored: &str) -> &str {
    match stored {
        "project" | "repository" => "repo",
        other => other,
    }
}

pub fn mount_of_stored(stored: &str) -> Option<MountLevel> {
    match stored {
        "run" => Some(MountLevel::Run),
        "session" => Some(MountLevel::Session),
        "repo" | "project" | "repository" => Some(MountLevel::Repo),
        "org" => Some(MountLevel::Org),
        "global_user" | "global-user" | "user" => Some(MountLevel::User),
        "skill" => Some(MountLevel::Skill),
        _ => None,
    }
}

/// Same mount for grants and contamination. Exact stored strings can still differ
/// (`project` vs `repo`); similarity merge must not treat that as a license to
/// collapse the rows.
pub fn same_mount_stored(a: &str, b: &str) -> bool {
    match (mount_of_stored(a), mount_of_stored(b)) {
        (Some(left), Some(right)) => left == right,
        _ => a == b,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScope {
    GlobalUser,
    Project,
    Repo,
    Run,
    Session,
    Org,
    Skill,
}

impl MemoryScope {
    pub fn mount(self) -> MountLevel {
        match self {
            Self::Run => MountLevel::Run,
            Self::Session => MountLevel::Session,
            Self::Project | Self::Repo => MountLevel::Repo,
            Self::Org => MountLevel::Org,
            Self::GlobalUser => MountLevel::User,
            Self::Skill => MountLevel::Skill,
        }
    }

    /// Scope string written on a new durable event.
    ///
    /// `project` is an input alias: new events store `repo`, never a second
    /// level named `project`. Existing `project` rows are left untouched.
    pub fn stored_level(self) -> &'static str {
        match self {
            Self::Project | Self::Repo => "repo",
            Self::GlobalUser => "global_user",
            Self::Run => "run",
            Self::Session => "session",
            Self::Org => "org",
            Self::Skill => "skill",
        }
    }
}

impl Display for MemoryScope {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::GlobalUser => "global_user",
            Self::Project => "project",
            Self::Repo => "repo",
            Self::Run => "run",
            Self::Session => "session",
            Self::Org => "org",
            Self::Skill => "skill",
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
            "session" => Ok(Self::Session),
            "org" => Ok(Self::Org),
            "skill" => Ok(Self::Skill),
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
