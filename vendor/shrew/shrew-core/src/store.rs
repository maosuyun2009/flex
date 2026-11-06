//! Saved servers, persisted as JSON in the app's data directory.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    pub name: String,
    /// Anything `Endpoint::parse` accepts: a URL, host:port, MagicDNS name, or socket path.
    pub address: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    #[serde(default)]
    servers: Vec<Profile>,
    #[serde(default)]
    settings: serde_json::Map<String, serde_json::Value>,
}

pub(crate) struct Store {
    path: Option<PathBuf>,
    file: File,
}

impl Store {
    pub fn open(dir: Option<&Path>) -> Self {
        let path = dir.map(|d| d.join("shrew.json"));
        let file = path.as_ref().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Self { path, file }
    }

    pub fn profiles(&self) -> &[Profile] {
        &self.file.servers
    }

    pub fn set_profiles(&mut self, p: Vec<Profile>) {
        self.file.servers = p;
        self.save();
    }

    pub fn setting(&self, key: &str) -> Option<&serde_json::Value> {
        self.file.settings.get(key)
    }

    pub fn set_setting(&mut self, key: &str, v: serde_json::Value) {
        self.file.settings.insert(key.to_owned(), v);
        self.save();
    }

    fn save(&self) {
        let Some(path) = &self.path else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // Write atomically so a crash never leaves a truncated file.
        let tmp = path.with_extension("json.tmp");
        if let Ok(bytes) = serde_json::to_vec_pretty(&self.file)
            && std::fs::write(&tmp, bytes).is_ok() {
                let _ = std::fs::rename(&tmp, path);
            }
    }
}
