//! Plugin host: discovery, trust, registries, and lifecycle.
//!
//! The manifest and capability types live in the `aoe-plugin-api` crate (the
//! stable surface plugin authors compile against). This module is the host
//! side: it discovers installed plugins, checks capability grants, exposes
//! the enabled contributions to every surface (CLI grafting, settings
//! schema, keybinds, themes, status detection), and supervises Tier 1
//! workers. See `docs/development/internals/plugin-system.md`.

pub mod cli_graft;
pub mod core_overrides;
pub mod discover;
pub mod featured;
pub mod grants;
pub mod host;
pub mod install;
pub mod integrity;
pub mod lockfile;
pub mod registry;
pub mod runtime;
pub mod sandbox;
pub mod settings;
pub mod status;
pub mod ui;
pub mod update_check;

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use anyhow::Result;
use serde::{Deserialize, Serialize};

pub use registry::{LoadedPlugin, PluginRegistry};

/// Where a plugin came from; drives its trust level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum PluginSource {
    /// Compiled into the aoe binary from `plugins/` in this repository.
    Builtin,
    /// Installed from a GitHub repository (`owner/repo`).
    GitHub { slug: String },
    /// Installed from a local directory.
    Path { path: String },
}

/// Trust levels shown on every management surface and used to word the
/// capability prompt. Capability gating at the host API boundary stops
/// cooperative plugins from drifting beyond their manifest; it is NOT an
/// OS sandbox, and the prompt for non-builtin plugins says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustLevel {
    /// First party, ships inside the aoe binary; capabilities auto-granted.
    Builtin,
    /// Anything installed from outside the binary: a GitHub slug or a local
    /// path. Requires an explicit one-time capability grant pinned to the
    /// manifest hash; re-prompts whenever the declared set changes.
    Community,
}

impl PluginSource {
    pub fn trust_level(&self) -> TrustLevel {
        match self {
            PluginSource::Builtin => TrustLevel::Builtin,
            PluginSource::GitHub { .. } | PluginSource::Path { .. } => TrustLevel::Community,
        }
    }

    /// Human-readable origin for local list/info surfaces (CLI, TUI). Includes
    /// the full local path for `Path` installs; do NOT use on network surfaces.
    pub fn describe(&self) -> String {
        match self {
            PluginSource::Builtin => "builtin".to_string(),
            PluginSource::GitHub { slug } => format!("github:{slug}"),
            PluginSource::Path { path } => format!("path:{path}"),
        }
    }

    /// Origin for network surfaces (REST `/api/plugins`, install-prompt JSON).
    /// A `github` slug is a public repo, but a local `Path` is a privacy leak
    /// (username, project layout) over a Tunnel/Funnel deployment, so it
    /// collapses to the bare kind.
    pub fn describe_redacted(&self) -> String {
        match self {
            PluginSource::Builtin => "builtin".to_string(),
            PluginSource::GitHub { slug } => format!("github:{slug}"),
            PluginSource::Path { .. } => "path".to_string(),
        }
    }
}

/// Directory holding third-party plugin installs: `<app_dir>/plugins/<id>/`.
pub fn plugins_dir() -> Result<PathBuf> {
    Ok(crate::session::get_app_dir()?.join("plugins"))
}

static REGISTRY: RwLock<Option<Arc<PluginRegistry>>> = RwLock::new(None);

/// The process-wide plugin registry, loaded on first use from the global
/// config. Mutating surfaces (CLI install/enable, settings toggles, web
/// endpoints) call [`reload_registry`] after persisting their change.
pub fn registry() -> Arc<PluginRegistry> {
    if let Some(reg) = REGISTRY.read().expect("plugin registry lock").as_ref() {
        return reg.clone();
    }
    let mut slot = REGISTRY.write().expect("plugin registry lock");
    if let Some(reg) = slot.as_ref() {
        return reg.clone();
    }
    let config = crate::session::Config::load_or_warn();
    let reg = Arc::new(PluginRegistry::load(&config));
    *slot = Some(reg.clone());
    reg
}

/// Rebuild the registry from the current on-disk config and install state.
/// Tears down every running worker (and resets respawn budgets) so a
/// disabled, uninstalled, or capability-changed plugin cannot keep its old
/// worker and grant set alive; active plugins respawn on their next call.
pub fn reload_registry() -> Arc<PluginRegistry> {
    core_overrides::invalidate();
    let config = crate::session::Config::load_or_warn();
    let reg = Arc::new(PluginRegistry::load(&config));
    *REGISTRY.write().expect("plugin registry lock") = Some(reg.clone());
    status::invalidate_cache();
    host::host().reset();
    // UI state of plugins that are no longer active must vanish with them.
    let active: std::collections::HashSet<String> =
        reg.active().map(|p| p.id().to_string()).collect();
    ui::evict_except(&active);
    reg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacted_describe_hides_local_path() {
        let p = PluginSource::Path {
            path: "/Users/alice/secret-project/plugin".to_string(),
        };
        assert_eq!(p.describe_redacted(), "path");
        assert!(!p.describe_redacted().contains("alice"));
        // github slugs and builtin are public, kept verbatim.
        assert_eq!(
            PluginSource::GitHub {
                slug: "owner/repo".to_string()
            }
            .describe_redacted(),
            "github:owner/repo"
        );
        assert_eq!(PluginSource::Builtin.describe_redacted(), "builtin");
    }
}
