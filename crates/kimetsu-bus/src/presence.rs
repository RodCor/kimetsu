//! Light presence. The file is not `brain.db` and it is not the FTS index.
//! Recall never opens it.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::protocol::Announcement;

#[derive(Debug, Clone)]
pub struct PresenceStore {
    path: PathBuf,
    inner: std::sync::Arc<Mutex<PresenceFile>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PresenceFile {
    announcements: Vec<StoredAnnouncement>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredAnnouncement {
    announcement: Announcement,
    received_at_secs: u64,
}

impl PresenceStore {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, String> {
        let path = path.into();
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == "brain.db" || name.contains("memories_fts"))
        {
            return Err("presence must not use the memories database".into());
        }
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|err| format!("presence dir: {err}"))?;
        }
        let inner = if path.exists() {
            let raw = fs::read_to_string(&path).map_err(|err| format!("presence: {err}"))?;
            serde_json::from_str(&raw).unwrap_or_default()
        } else {
            PresenceFile::default()
        };
        Ok(Self {
            path,
            inner: std::sync::Arc::new(Mutex::new(inner)),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn record(&self, announcement: Announcement) -> Result<(), String> {
        announcement.validate()?;
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| "presence lock poisoned".to_string())?;
        guard
            .announcements
            .retain(|stored| stored.announcement.digest != announcement.digest);
        guard.announcements.push(StoredAnnouncement {
            announcement,
            received_at_secs: now_secs(),
        });
        self.flush(&guard)
    }

    pub fn list(&self) -> Result<Vec<Announcement>, String> {
        let guard = self
            .inner
            .lock()
            .map_err(|_| "presence lock poisoned".to_string())?;
        Ok(guard
            .announcements
            .iter()
            .map(|stored| stored.announcement.clone())
            .collect())
    }

    pub fn expire(&self) -> Result<(), String> {
        let now = now_secs();
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| "presence lock poisoned".to_string())?;
        guard.announcements.retain(|stored| {
            now.saturating_sub(stored.received_at_secs) < stored.announcement.ttl_secs
        });
        self.flush(&guard)
    }

    fn flush(&self, file: &PresenceFile) -> Result<(), String> {
        let body = serde_json::to_vec_pretty(file).map_err(|err| err.to_string())?;
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, body).map_err(|err| format!("presence write: {err}"))?;
        fs::rename(&tmp, &self.path).map_err(|err| format!("presence rename: {err}"))?;
        Ok(())
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}
