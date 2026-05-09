use serde::{Deserialize, Serialize};
use ulid::Ulid;

pub type KimetsuId = Ulid;

pub fn new_id() -> KimetsuId {
    Ulid::new()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunId(pub KimetsuId);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventId(pub KimetsuId);

impl RunId {
    pub fn new() -> Self {
        Self(new_id())
    }
}

impl Default for RunId {
    fn default() -> Self {
        Self::new()
    }
}

impl EventId {
    pub fn new() -> Self {
        Self(new_id())
    }
}

impl Default for EventId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for RunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::fmt::Display for EventId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
