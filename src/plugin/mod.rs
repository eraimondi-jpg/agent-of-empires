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
pub mod links;
pub mod lockfile;
pub mod panes;
pub mod registry;
pub mod runtime;
pub mod sandbox;
pub mod settings;
pub mod status;
pub mod ui;
pub mod update_check;
pub mod view;

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use anyhow::Result;
use serde::{Deserialize, Serialize};

pub use registry::{LoadedPlugin, PluginRegistry};
pub use view::{PaneView, PluginView};

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
    /// Linked to a local directory for development: the files are read live
    /// from `path` (never copied), so edits take effect on the next registry
    /// reload. Removing the link drops only the metadata, never the source.
    Linked { path: String },
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
            PluginSource::GitHub { .. }
            | PluginSource::Path { .. }
            | PluginSource::Linked { .. } => TrustLevel::Community,
        }
    }

    /// Human-readable origin for local list/info surfaces (CLI, TUI). Includes
    /// the full local path for `Path` installs; do NOT use on network surfaces.
    pub fn describe(&self) -> String {
        match self {
            PluginSource::Builtin => "builtin".to_string(),
            PluginSource::GitHub { slug } => format!("github:{slug}"),
            PluginSource::Path { path } => format!("path:{path}"),
            PluginSource::Linked { path } => format!("linked:{path}"),
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
            PluginSource::Linked { .. } => "linked".to_string(),
        }
    }
}

/// Directory holding third-party plugin installs: `<app_dir>/plugins/<id>/`.
pub fn plugins_dir() -> Result<PathBuf> {
    Ok(crate::session::get_app_dir()?.join("plugins"))
}

/// Atomically and durably write `contents` to `path` with owner-only (0600)
/// permissions. A `NamedTempFile` in the same directory (mkstemp creates it
/// 0600 on unix) is filled, fsynced, renamed over `path`, and finally the
/// parent directory is fsynced so the rename's directory entry survives an OS
/// crash. So a crash, OOM, `kill -9`, or power loss leaves either the previous
/// file or the complete new one, never a truncated or missing file, and the
/// bytes are never momentarily group/world-readable. Used by the lockfile and
/// grant stores, both of which feed security decisions (source resolution for
/// auto-update, the capability allowlist).
pub(crate) fn write_private_atomic(path: &std::path::Path, contents: &str) -> Result<()> {
    use anyhow::Context;
    use std::io::Write;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent)?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("staging write for {}", path.display()))?;
    tmp.write_all(contents.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tmp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    // Flush the data before the rename, and propagate the error: a swallowed
    // sync_all (ENOSPC/EIO/EROFS) would leave a non-durable temp.
    tmp.as_file()
        .sync_all()
        .with_context(|| format!("flushing {}", path.display()))?;
    tmp.persist(path)
        .with_context(|| format!("persisting {}", path.display()))?;
    // fsync the directory so the rename itself is durable across an OS crash;
    // best-effort (some platforms reject opening a dir for sync).
    if let Ok(dir) = std::fs::File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Lock recovery for the plugin subsystem's process-wide state. Plugin code
/// is the untrusted surface (manifest TOML, detection regex, settings JSON all
/// flow through these globals), so a panic while one of these locks is held
/// must not poison it and take the daemon (or a TUI redraw / tokio task) down
/// on the next access. Recovering the guard via `into_inner` is correct here:
/// the held data is rebuildable cache, not partial-mutation-sensitive state.
pub(crate) trait LockSafe<T> {
    fn lock_safe(&self) -> std::sync::MutexGuard<'_, T>;
}

impl<T> LockSafe<T> for std::sync::Mutex<T> {
    fn lock_safe(&self) -> std::sync::MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|e| e.into_inner())
    }
}

pub(crate) trait RwLockSafe<T> {
    fn read_safe(&self) -> std::sync::RwLockReadGuard<'_, T>;
    fn write_safe(&self) -> std::sync::RwLockWriteGuard<'_, T>;
}

impl<T> RwLockSafe<T> for RwLock<T> {
    fn read_safe(&self) -> std::sync::RwLockReadGuard<'_, T> {
        self.read().unwrap_or_else(|e| e.into_inner())
    }
    fn write_safe(&self) -> std::sync::RwLockWriteGuard<'_, T> {
        self.write().unwrap_or_else(|e| e.into_inner())
    }
}

static REGISTRY: RwLock<Option<Arc<PluginRegistry>>> = RwLock::new(None);

/// The process-wide plugin registry, loaded on first use from the global
/// config. Mutating surfaces (CLI install/enable, settings toggles, web
/// endpoints) call [`reload_registry`] after persisting their change.
pub fn registry() -> Arc<PluginRegistry> {
    if let Some(reg) = REGISTRY.read_safe().as_ref() {
        return reg.clone();
    }
    let mut slot = REGISTRY.write_safe();
    if let Some(reg) = slot.as_ref() {
        return reg.clone();
    }
    let config = crate::session::Config::load_or_warn();
    let reg = Arc::new(PluginRegistry::load(&config));
    *slot = Some(reg.clone());
    host::start_event_handlers(&reg);
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
    *REGISTRY.write_safe() = Some(reg.clone());
    status::invalidate_cache();
    links::invalidate();
    host::host().reset();
    host::start_event_handlers(&reg);
    // UI state of plugins that are no longer active must vanish with them.
    let active: std::collections::HashSet<String> =
        reg.active().map(|p| p.id().to_string()).collect();
    ui::evict_except(&active);
    panes::evict_except(&active);
    reg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_private_atomic_replaces_and_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("plugins.lock");
        write_private_atomic(&path, "first").unwrap();
        // A second write fully replaces (no append/torn content).
        write_private_atomic(&path, "second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "expected 0600, got {:o}", mode & 0o777);
        }
    }

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
        // A linked dev path is as private as a copied one: redact to the bare
        // kind, but show it verbatim on the local-only describe surface.
        let linked = PluginSource::Linked {
            path: "/Users/alice/dev/my-plugin".to_string(),
        };
        assert_eq!(linked.describe_redacted(), "linked");
        assert!(!linked.describe_redacted().contains("alice"));
        assert_eq!(linked.describe(), "linked:/Users/alice/dev/my-plugin");
        assert_eq!(linked.trust_level(), TrustLevel::Community);
    }
}
