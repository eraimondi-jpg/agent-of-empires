//! Capability grants, persisted pinned to a manifest hash.
//!
//! A grant records the exact capability set the user approved and the
//! sha256 of the manifest it approved it for. A plugin update that changes
//! its declared capabilities no longer matches the stored hash, so its
//! runtime contributions deactivate until the user re-approves. Builtin
//! plugins are auto-granted and never enter this store.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use aoe_plugin_api::Capability;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const GRANTS_FILE: &str = "plugin_grants.toml";

/// sha256 over the raw manifest bytes, `sha256:<hex>`.
pub fn manifest_hash(manifest_bytes: &[u8]) -> String {
    let digest = Sha256::digest(manifest_bytes);
    format!("sha256:{}", hex_encode(&digest))
}

pub(super) fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrantRecord {
    pub manifest_hash: String,
    pub granted_at: DateTime<Utc>,
    pub capabilities: Vec<Capability>,
}

/// Grant state of one plugin against the store. Serializes to
/// `granted` / `missing` / `stale` for the shared plugin view-model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantStatus {
    /// Capabilities approved for this exact manifest hash.
    Granted,
    /// No grant on record (fresh install path never prompted, or revoked).
    Missing,
    /// A grant exists but for a different manifest hash: the plugin changed
    /// its declared capabilities (or any manifest byte) since approval.
    Stale,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct GrantsDocument {
    #[serde(default)]
    plugin_grants: BTreeMap<String, GrantRecord>,
}

/// On-disk store, `<app_dir>/plugin_grants.toml` (owner-only).
#[derive(Debug)]
pub struct GrantStore {
    path: PathBuf,
    doc: GrantsDocument,
}

impl GrantStore {
    pub fn load() -> Result<Self> {
        let path = crate::session::get_app_dir()?.join(GRANTS_FILE);
        Self::load_from(path)
    }

    pub fn load_from(path: PathBuf) -> Result<Self> {
        let doc = match std::fs::read_to_string(&path) {
            Ok(raw) => {
                toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => GrantsDocument::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Ok(Self { path, doc })
    }

    pub fn status(&self, plugin_id: &str, manifest_hash: &str) -> GrantStatus {
        match self.doc.plugin_grants.get(plugin_id) {
            None => GrantStatus::Missing,
            Some(rec) if rec.manifest_hash == manifest_hash => GrantStatus::Granted,
            Some(_) => GrantStatus::Stale,
        }
    }

    pub fn record(&self, plugin_id: &str) -> Option<&GrantRecord> {
        self.doc.plugin_grants.get(plugin_id)
    }

    pub fn grant(
        &mut self,
        plugin_id: &str,
        manifest_hash: String,
        capabilities: Vec<Capability>,
    ) -> Result<()> {
        self.doc.plugin_grants.insert(
            plugin_id.to_string(),
            GrantRecord {
                manifest_hash,
                granted_at: Utc::now(),
                capabilities,
            },
        );
        self.save()
    }

    pub fn revoke(&mut self, plugin_id: &str) -> Result<()> {
        if self.doc.plugin_grants.remove(plugin_id).is_some() {
            self.save()?;
        }
        Ok(())
    }

    fn save(&self) -> Result<()> {
        super::write_private_atomic(&self.path, &toml::to_string_pretty(&self.doc)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_hash_is_stable_and_prefixed() {
        let h = manifest_hash(b"id = \"x\"");
        assert!(h.starts_with("sha256:"));
        assert_eq!(h, manifest_hash(b"id = \"x\""));
        assert_ne!(h, manifest_hash(b"id = \"y\""));
    }

    #[test]
    fn grant_status_tracks_hash_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plugin_grants.toml");
        let mut store = GrantStore::load_from(path.clone()).unwrap();

        let h1 = manifest_hash(b"v1");
        assert_eq!(store.status("a.b", &h1), GrantStatus::Missing);

        store
            .grant("a.b", h1.clone(), vec![Capability::SessionsRead])
            .unwrap();
        assert_eq!(store.status("a.b", &h1), GrantStatus::Granted);

        let h2 = manifest_hash(b"v2-with-new-capability");
        assert_eq!(store.status("a.b", &h2), GrantStatus::Stale);

        // Reload from disk: persisted.
        let store2 = GrantStore::load_from(path).unwrap();
        assert_eq!(store2.status("a.b", &h1), GrantStatus::Granted);
        assert_eq!(
            store2.record("a.b").unwrap().capabilities,
            vec![Capability::SessionsRead]
        );
    }

    #[test]
    fn revoke_removes_grant() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = GrantStore::load_from(dir.path().join("g.toml")).unwrap();
        let h = manifest_hash(b"m");
        store.grant("a.b", h.clone(), vec![]).unwrap();
        store.revoke("a.b").unwrap();
        assert_eq!(store.status("a.b", &h), GrantStatus::Missing);
    }
}
