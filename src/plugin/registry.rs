//! Plugin discovery and the in-process registry of loaded plugins.
//!
//! Builtin plugins are compiled into the binary from `plugins/` in this
//! repository (manifest embedded, worker dispatched through the hidden
//! `aoe __plugin-worker` subcommand). Third-party plugins live under
//! `<app_dir>/plugins/<id>/` with an `aoe-plugin.toml` at the root.
//!
//! Every surface derives from the enabled set per invocation, so disabling a
//! plugin removes its contributions on the next build of that surface with
//! no residue (acceptance criterion 3 of #268).

use std::path::PathBuf;

use aoe_plugin_api::PluginManifest;
use tracing::warn;

use super::grants::{manifest_hash, GrantStatus, GrantStore};
use super::lockfile::Lockfile;
use super::{PluginSource, TrustLevel};
use crate::session::Config;

/// A plugin compiled into the aoe binary. The worker (if the manifest has a
/// `[runtime]` section) runs via `aoe __plugin-worker --id <id>` so a single
/// installed binary ships every first-party plugin.
pub struct BuiltinPlugin {
    pub manifest_toml: &'static str,
}

/// First-party plugins bundled with the binary, registered by later phases
/// (status detection, attention sort). Kept in `plugins/` in the repository
/// so they can move to their own repo without touching host code.
pub static BUILTINS: &[BuiltinPlugin] = &[
    #[cfg(feature = "default-plugins")]
    BuiltinPlugin {
        manifest_toml: include_str!("../../plugins/aoe-status/aoe-plugin.toml"),
    },
    #[cfg(feature = "default-plugins")]
    BuiltinPlugin {
        manifest_toml: include_str!("../../plugins/aoe-attention/aoe-plugin.toml"),
    },
    // The web dashboard's management marker only exists when the dashboard
    // is compiled in at all.
    #[cfg(all(feature = "serve", feature = "default-plugins"))]
    BuiltinPlugin {
        manifest_toml: include_str!("../../plugins/aoe-web/aoe-plugin.toml"),
    },
];

/// One discovered plugin with its resolved enablement and grant state.
pub struct LoadedPlugin {
    pub manifest: PluginManifest,
    pub manifest_hash: String,
    /// Install directory; `None` for builtins (assets are embedded).
    pub root: Option<PathBuf>,
    pub source: PluginSource,
    /// Resolved from `Config.plugins`; builtins default on, installed
    /// plugins default to whatever the install wrote (explicit `enabled`).
    pub enabled: bool,
    pub grant: GrantStatus,
    /// Current setting values from config (empty table when unset).
    pub settings: toml::Table,
}

impl LoadedPlugin {
    pub fn id(&self) -> &str {
        self.manifest.id.as_str()
    }

    pub fn trust(&self) -> TrustLevel {
        self.source.trust_level()
    }

    /// Whether this plugin's contributions are live: enabled by config AND
    /// its declared capabilities are granted for this exact manifest hash.
    pub fn active(&self) -> bool {
        self.enabled && self.grant == GrantStatus::Granted
    }
}

/// All discovered plugins plus any problems found while loading them.
/// Rebuilt from disk by [`super::reload_registry`] after every mutation.
pub struct PluginRegistry {
    plugins: Vec<LoadedPlugin>,
    /// Human-readable problems (parse failures, duplicate ids) surfaced by
    /// `aoe plugin list` and both management UIs instead of being silently
    /// swallowed.
    load_errors: Vec<String>,
}

impl PluginRegistry {
    pub fn load(config: &Config) -> Self {
        let mut plugins: Vec<LoadedPlugin> = Vec::new();
        let mut load_errors = Vec::new();

        let grant_store = match GrantStore::load() {
            Ok(store) => Some(store),
            Err(e) => {
                load_errors.push(format!("plugin grants unreadable: {e:#}"));
                None
            }
        };
        let lockfile = match Lockfile::load() {
            Ok(lock) => Some(lock),
            Err(e) => {
                load_errors.push(format!("plugins.lock unreadable: {e:#}"));
                None
            }
        };

        for builtin in BUILTINS {
            match PluginManifest::from_toml_str(builtin.manifest_toml) {
                Ok(manifest) => {
                    let enabled = config
                        .plugins
                        .get(manifest.id.as_str())
                        .map(|p| p.enabled)
                        .unwrap_or(true);
                    let settings = config
                        .plugins
                        .get(manifest.id.as_str())
                        .map(|p| p.settings.clone())
                        .unwrap_or_default();
                    plugins.push(LoadedPlugin {
                        manifest_hash: manifest_hash(builtin.manifest_toml.as_bytes()),
                        root: None,
                        source: PluginSource::Builtin,
                        enabled,
                        // First party, compiled in: auto-granted.
                        grant: GrantStatus::Granted,
                        settings,
                        manifest,
                    });
                }
                Err(e) => {
                    // A broken builtin manifest is a build defect; tested in CI.
                    load_errors.push(format!("builtin manifest invalid: {e}"));
                }
            }
        }

        match super::plugins_dir() {
            Ok(dir) => {
                Self::load_installed(
                    &dir,
                    config,
                    grant_store.as_ref(),
                    lockfile.as_ref(),
                    &mut plugins,
                    &mut load_errors,
                );
            }
            Err(e) => load_errors.push(format!("plugins dir unresolvable: {e:#}")),
        }

        Self {
            plugins,
            load_errors,
        }
    }

    fn load_installed(
        dir: &std::path::Path,
        config: &Config,
        grant_store: Option<&GrantStore>,
        lockfile: Option<&Lockfile>,
        plugins: &mut Vec<LoadedPlugin>,
        load_errors: &mut Vec<String>,
    ) {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            // No plugins installed yet.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                load_errors.push(format!("reading {}: {e}", dir.display()));
                return;
            }
        };
        for entry in entries.flatten() {
            let root = entry.path();
            let manifest_path = root.join("aoe-plugin.toml");
            if !manifest_path.is_file() {
                continue;
            }
            let raw = match std::fs::read_to_string(&manifest_path) {
                Ok(raw) => raw,
                Err(e) => {
                    load_errors.push(format!("reading {}: {e}", manifest_path.display()));
                    continue;
                }
            };
            let manifest = match PluginManifest::from_toml_str(&raw) {
                Ok(manifest) => manifest,
                Err(e) => {
                    load_errors.push(format!("{}: {e}", manifest_path.display()));
                    continue;
                }
            };
            if let Some(min) = manifest.min_aoe_version.as_deref() {
                let host = env!("CARGO_PKG_VERSION");
                if !host_meets_min(min, host) {
                    load_errors.push(format!(
                        "{}: requires aoe >= {min} but this build is {host}; skipped",
                        manifest_path.display()
                    ));
                    continue;
                }
            }
            let id = manifest.id.as_str().to_string();
            if plugins.iter().any(|p| p.id() == id) {
                load_errors.push(format!(
                    "{}: id {id:?} already provided by another plugin; skipped",
                    root.display()
                ));
                continue;
            }
            let dir_name = entry.file_name().to_string_lossy().to_string();
            if dir_name != id {
                warn!(target: "plugin", dir = %dir_name, id = %id, "plugin dir name differs from manifest id");
            }
            let hash = manifest_hash(raw.as_bytes());
            let grant = grant_store
                .map(|s| s.status(&id, &hash))
                .unwrap_or(GrantStatus::Missing);
            let source = lockfile
                .and_then(|l| l.get(&id))
                .map(|rec| rec.source.clone())
                .unwrap_or(PluginSource::Path {
                    path: root.display().to_string(),
                });
            let entry_cfg = config.plugins.get(&id);
            plugins.push(LoadedPlugin {
                manifest,
                manifest_hash: hash,
                root: Some(root),
                source,
                // Installed plugins are enabled only by an explicit config
                // entry (written by `aoe plugin install` / `enable`).
                enabled: entry_cfg.map(|p| p.enabled).unwrap_or(false),
                grant,
                settings: entry_cfg.map(|p| p.settings.clone()).unwrap_or_default(),
            });
        }
    }

    /// Every discovered plugin, builtin first, then installed in directory
    /// order.
    pub fn all(&self) -> &[LoadedPlugin] {
        &self.plugins
    }

    /// Plugins whose contributions are live (enabled + granted).
    pub fn active(&self) -> impl Iterator<Item = &LoadedPlugin> {
        self.plugins.iter().filter(|p| p.active())
    }

    pub fn get(&self, plugin_id: &str) -> Option<&LoadedPlugin> {
        self.plugins.iter().find(|p| p.id() == plugin_id)
    }

    pub fn load_errors(&self) -> &[String] {
        &self.load_errors
    }
}

/// Whether a host at version `host` satisfies a plugin's `min_aoe_version`.
/// Reuses the dotted-numeric comparison the update checker uses; an equal or
/// newer host passes, an older host fails.
fn host_meets_min(min: &str, host: &str) -> bool {
    !crate::update::is_newer_version(min, host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_meets_min_gates_on_version() {
        assert!(host_meets_min("0.5.0", "0.5.0"), "equal host satisfies min");
        assert!(host_meets_min("0.5.0", "0.6.0"), "newer host satisfies min");
        assert!(host_meets_min("0.5.0", "1.0.0"), "major-newer host passes");
        assert!(!host_meets_min("0.6.0", "0.5.0"), "older host fails min");
        assert!(!host_meets_min("1.0.0", "0.9.9"), "older major fails min");
    }

    #[test]
    fn builtin_manifests_parse_and_have_unique_ids() {
        let mut seen = std::collections::HashSet::new();
        for builtin in BUILTINS {
            let manifest = PluginManifest::from_toml_str(builtin.manifest_toml)
                .expect("builtin manifest must be valid");
            assert!(
                seen.insert(manifest.id.as_str().to_string()),
                "duplicate builtin id {}",
                manifest.id
            );
        }
    }
}
