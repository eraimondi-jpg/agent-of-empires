//! Plugin host: discovery, trust, registries, and lifecycle.
//!
//! The manifest and capability types live in the `aoe-plugin-api` crate (the
//! stable surface plugin authors compile against). This module is the host
//! side: it discovers installed plugins, checks capability grants, exposes
//! the enabled contributions to every surface (CLI grafting, settings
//! schema, keybinds, themes, status detection), and supervises Tier 1
//! workers. See `docs/development/internals/plugin-system.md`.

pub mod grants;
pub mod install;
pub mod lockfile;
pub mod registry;

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

    /// Human-readable origin for list/info surfaces.
    pub fn describe(&self) -> String {
        match self {
            PluginSource::Builtin => "builtin".to_string(),
            PluginSource::GitHub { slug } => format!("github:{slug}"),
            PluginSource::Path { path } => format!("path:{path}"),
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
pub fn reload_registry() -> Arc<PluginRegistry> {
    let config = crate::session::Config::load_or_warn();
    let reg = Arc::new(PluginRegistry::load(&config));
    *REGISTRY.write().expect("plugin registry lock") = Some(reg.clone());
    reg
}
