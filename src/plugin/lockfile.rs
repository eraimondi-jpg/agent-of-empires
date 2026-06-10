//! Installed-plugin lockfile: `<app_dir>/plugins.lock`.
//!
//! Records what is installed, from where, and at which version/hash so
//! `aoe plugin list` and `aoe plugin update` work without re-reading every
//! plugin directory, and so an install can be reproduced.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::PluginSource;

const LOCK_FILE: &str = "plugins.lock";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockRecord {
    pub version: String,
    pub source: PluginSource,
    pub manifest_hash: String,
    pub installed_at: DateTime<Utc>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct LockDocument {
    #[serde(default)]
    plugins: BTreeMap<String, LockRecord>,
}

#[derive(Debug)]
pub struct Lockfile {
    path: PathBuf,
    doc: LockDocument,
}

impl Lockfile {
    pub fn load() -> Result<Self> {
        let path = crate::session::get_app_dir()?.join(LOCK_FILE);
        Self::load_from(path)
    }

    pub fn load_from(path: PathBuf) -> Result<Self> {
        let doc = match std::fs::read_to_string(&path) {
            Ok(raw) => {
                toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => LockDocument::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Ok(Self { path, doc })
    }

    pub fn get(&self, plugin_id: &str) -> Option<&LockRecord> {
        self.doc.plugins.get(plugin_id)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &LockRecord)> {
        self.doc.plugins.iter()
    }

    pub fn upsert(&mut self, plugin_id: &str, record: LockRecord) -> Result<()> {
        self.doc.plugins.insert(plugin_id.to_string(), record);
        self.save()
    }

    pub fn remove(&mut self, plugin_id: &str) -> Result<()> {
        if self.doc.plugins.remove(plugin_id).is_some() {
            self.save()?;
        }
        Ok(())
    }

    fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.path, toml::to_string_pretty(&self.doc)?)
            .with_context(|| format!("writing {}", self.path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lockfile_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plugins.lock");
        let mut lock = Lockfile::load_from(path.clone()).unwrap();
        lock.upsert(
            "a.b",
            LockRecord {
                version: "1.0.0".into(),
                source: PluginSource::GitHub {
                    slug: "owner/repo".into(),
                },
                manifest_hash: "sha256:abc".into(),
                installed_at: Utc::now(),
            },
        )
        .unwrap();

        let reloaded = Lockfile::load_from(path).unwrap();
        let rec = reloaded.get("a.b").unwrap();
        assert_eq!(rec.version, "1.0.0");
        assert_eq!(
            rec.source,
            PluginSource::GitHub {
                slug: "owner/repo".into()
            }
        );
    }
}
