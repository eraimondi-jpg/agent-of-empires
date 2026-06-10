//! Install, uninstall, and update third-party plugins.
//!
//! Sources: a GitHub slug (`owner/repo`, fetched with `git clone --depth 1`,
//! the same git binary every aoe install already requires) or a local
//! directory. Installation is: stage, validate the manifest, show the
//! declared capabilities once, then copy into `<app_dir>/plugins/<id>/`,
//! record the lockfile entry, persist the grant pinned to the manifest hash,
//! and enable the plugin in config.
//!
//! The capability prompt is a callback so the CLI, the TUI, and the web
//! endpoint reuse the identical decision flow with their own UI.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use aoe_plugin_api::{Capability, PluginManifest};
use chrono::Utc;

use super::grants::{manifest_hash, GrantStore};
use super::lockfile::{LockRecord, Lockfile};
use super::{PluginSource, TrustLevel};
use crate::session::{save_config, Config, PluginConfig};

/// Everything the user must see before approving an install or a
/// capability-changing update.
#[derive(Debug)]
pub struct InstallPrompt {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub capabilities: Vec<Capability>,
    pub trust: TrustLevel,
    pub source: PluginSource,
    /// Set on update when the previously granted capability set differs.
    pub previous_capabilities: Option<Vec<Capability>>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum InstallOutcome {
    Installed {
        id: String,
        version: String,
    },
    Updated {
        id: String,
        version: String,
    },
    UpToDate {
        id: String,
        version: String,
    },
    /// The user declined the capability prompt; nothing was written.
    Declined,
}

/// Parse a user-supplied source string: an existing directory wins, then
/// `owner/repo`.
pub fn parse_source(input: &str) -> Result<PluginSource> {
    let as_path = Path::new(input);
    if as_path.is_dir() {
        let canonical = as_path
            .canonicalize()
            .with_context(|| format!("resolving {input}"))?;
        return Ok(PluginSource::Path {
            path: canonical.display().to_string(),
        });
    }
    let mut parts = input.split('/');
    if let (Some(owner), Some(repo), None) = (parts.next(), parts.next(), parts.next()) {
        let valid = |s: &str| {
            !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        };
        if valid(owner) && valid(repo) {
            return Ok(PluginSource::GitHub {
                slug: format!("{owner}/{repo}"),
            });
        }
    }
    bail!("{input:?} is neither an existing directory nor a GitHub owner/repo slug")
}

/// A staged (not yet installed) plugin: its files on disk plus the parsed,
/// validated manifest.
struct Staged {
    /// Directory containing aoe-plugin.toml. Owned tempdir when cloned.
    root: PathBuf,
    _tempdir: Option<tempfile::TempDir>,
    manifest: PluginManifest,
    manifest_raw: String,
}

fn stage(source: &PluginSource) -> Result<Staged> {
    let (root, tempdir) = match source {
        PluginSource::Path { path } => (PathBuf::from(path), None),
        PluginSource::GitHub { slug } => {
            let tmp = tempfile::tempdir().context("creating staging dir")?;
            let url = format!("https://github.com/{slug}.git");
            let dest = tmp.path().join("plugin");
            let output = std::process::Command::new("git")
                .args(["clone", "--depth", "1", &url])
                .arg(&dest)
                .output()
                .context("running git clone")?;
            if !output.status.success() {
                bail!(
                    "git clone of {url} failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            (dest, Some(tmp))
        }
        PluginSource::Builtin => bail!("builtin plugins are part of the aoe binary"),
    };
    let manifest_path = root.join("aoe-plugin.toml");
    let manifest_raw = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("no aoe-plugin.toml at {}", root.display()))?;
    let manifest = PluginManifest::from_toml_str(&manifest_raw)?;
    Ok(Staged {
        root,
        _tempdir: tempdir,
        manifest,
        manifest_raw,
    })
}

/// Install from a parsed source. `confirm` is called exactly once with the
/// declared capability set; returning false aborts with nothing written.
pub fn install(
    source: PluginSource,
    confirm: &mut dyn FnMut(&InstallPrompt) -> bool,
) -> Result<InstallOutcome> {
    let staged = stage(&source)?;
    let id = staged.manifest.id.as_str().to_string();

    let registry = super::registry();
    if let Some(existing) = registry.get(&id) {
        match existing.source {
            PluginSource::Builtin => bail!("{id} is a builtin plugin and already bundled"),
            _ => bail!("{id} is already installed; use `aoe plugin update {id}`"),
        }
    }

    let prompt = InstallPrompt {
        id: id.clone(),
        name: staged.manifest.name.clone(),
        version: staged.manifest.version.clone(),
        description: staged.manifest.description.clone(),
        capabilities: staged.manifest.capabilities.clone(),
        trust: source.trust_level(),
        source: source.clone(),
        previous_capabilities: None,
    };
    if !confirm(&prompt) {
        return Ok(InstallOutcome::Declined);
    }

    let dest = super::plugins_dir()?.join(&id);
    copy_plugin_tree(&staged.root, &dest)?;

    let hash = manifest_hash(staged.manifest_raw.as_bytes());
    GrantStore::load()?.grant(&id, hash.clone(), staged.manifest.capabilities.clone())?;
    Lockfile::load()?.upsert(
        &id,
        LockRecord {
            version: staged.manifest.version.clone(),
            source,
            manifest_hash: hash,
            installed_at: Utc::now(),
        },
    )?;
    enable_in_config(&id, true)?;
    super::reload_registry();
    Ok(InstallOutcome::Installed {
        id,
        version: staged.manifest.version,
    })
}

/// Update one installed plugin from its recorded source. Re-prompts via
/// `confirm` only when the new manifest's capability set differs from the
/// granted one; an unchanged set keeps the grant (re-pinned to the new
/// manifest hash).
pub fn update(
    plugin_id: &str,
    confirm: &mut dyn FnMut(&InstallPrompt) -> bool,
) -> Result<InstallOutcome> {
    let mut lockfile = Lockfile::load()?;
    let record = lockfile
        .get(plugin_id)
        .ok_or_else(|| anyhow!("{plugin_id} is not installed"))?
        .clone();
    if record.source == PluginSource::Builtin {
        bail!("{plugin_id} is builtin; it updates with the aoe binary");
    }

    let staged = stage(&record.source)?;
    if staged.manifest.id.as_str() != plugin_id {
        bail!(
            "source now serves id {:?}, expected {plugin_id:?}; uninstall and reinstall instead",
            staged.manifest.id.as_str()
        );
    }
    let new_hash = manifest_hash(staged.manifest_raw.as_bytes());
    if new_hash == record.manifest_hash {
        return Ok(InstallOutcome::UpToDate {
            id: plugin_id.to_string(),
            version: record.version,
        });
    }

    let mut grants = GrantStore::load()?;
    let granted_caps = grants
        .record(plugin_id)
        .map(|r| r.capabilities.clone())
        .unwrap_or_default();
    let mut new_caps = staged.manifest.capabilities.clone();
    let mut old_caps = granted_caps.clone();
    new_caps.sort();
    old_caps.sort();
    if new_caps != old_caps {
        let prompt = InstallPrompt {
            id: plugin_id.to_string(),
            name: staged.manifest.name.clone(),
            version: staged.manifest.version.clone(),
            description: staged.manifest.description.clone(),
            capabilities: staged.manifest.capabilities.clone(),
            trust: record.source.trust_level(),
            source: record.source.clone(),
            previous_capabilities: Some(granted_caps),
        };
        if !confirm(&prompt) {
            return Ok(InstallOutcome::Declined);
        }
    }

    let dest = super::plugins_dir()?.join(plugin_id);
    let backup = dest.with_extension("updating");
    if backup.exists() {
        std::fs::remove_dir_all(&backup)?;
    }
    if dest.exists() {
        std::fs::rename(&dest, &backup)?;
    }
    match copy_plugin_tree(&staged.root, &dest) {
        Ok(()) => {
            if backup.exists() {
                std::fs::remove_dir_all(&backup).ok();
            }
        }
        Err(e) => {
            // Roll the old tree back so a failed update never leaves a hole.
            std::fs::remove_dir_all(&dest).ok();
            if backup.exists() {
                std::fs::rename(&backup, &dest).ok();
            }
            return Err(e);
        }
    }

    grants.grant(
        plugin_id,
        new_hash.clone(),
        staged.manifest.capabilities.clone(),
    )?;
    lockfile.upsert(
        plugin_id,
        LockRecord {
            version: staged.manifest.version.clone(),
            source: record.source,
            manifest_hash: new_hash,
            installed_at: Utc::now(),
        },
    )?;
    super::reload_registry();
    Ok(InstallOutcome::Updated {
        id: plugin_id.to_string(),
        version: staged.manifest.version,
    })
}

/// Remove an installed plugin: files, lockfile entry, grant, and config
/// entry. Per-session `plugin_meta` is retained (cheap, and a reinstall
/// picks it back up).
pub fn uninstall(plugin_id: &str) -> Result<()> {
    let registry = super::registry();
    let plugin = registry
        .get(plugin_id)
        .ok_or_else(|| anyhow!("{plugin_id} is not installed"))?;
    let root = match (&plugin.source, &plugin.root) {
        (PluginSource::Builtin, _) => {
            bail!("{plugin_id} is builtin; disable it instead: `aoe plugin disable {plugin_id}`")
        }
        (_, Some(root)) => root.clone(),
        (_, None) => bail!("{plugin_id} has no install directory on record"),
    };
    std::fs::remove_dir_all(&root).with_context(|| format!("removing {}", root.display()))?;
    Lockfile::load()?.remove(plugin_id)?;
    GrantStore::load()?.revoke(plugin_id)?;
    let mut config = Config::load()?;
    config.plugins.remove(plugin_id);
    save_config(&config)?;
    super::reload_registry();
    Ok(())
}

/// Set the enabled flag for a known plugin id in the global config,
/// preserving any stored settings.
pub fn set_enabled(plugin_id: &str, enabled: bool) -> Result<()> {
    let registry = super::registry();
    if registry.get(plugin_id).is_none() {
        bail!("unknown plugin {plugin_id:?}; see `aoe plugin list`");
    }
    enable_in_config(plugin_id, enabled)?;
    super::reload_registry();
    Ok(())
}

fn enable_in_config(plugin_id: &str, enabled: bool) -> Result<()> {
    let mut config = Config::load()?;
    config
        .plugins
        .entry(plugin_id.to_string())
        .or_insert_with(PluginConfig::default)
        .enabled = enabled;
    save_config(&config)
}

/// Copy the staged plugin tree, skipping VCS internals.
fn copy_plugin_tree(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        if entry.file_type()?.is_dir() {
            copy_plugin_tree(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_source_distinguishes_paths_and_slugs() {
        let dir = tempfile::tempdir().unwrap();
        let parsed = parse_source(dir.path().to_str().unwrap()).unwrap();
        assert!(matches!(parsed, PluginSource::Path { .. }));

        assert_eq!(
            parse_source("owner/repo").unwrap(),
            PluginSource::GitHub {
                slug: "owner/repo".into()
            }
        );
        assert!(parse_source("not a source").is_err());
        assert!(parse_source("a/b/c").is_err());
    }

    #[test]
    fn copy_plugin_tree_skips_git_dir() {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("aoe-plugin.toml"), "x").unwrap();
        std::fs::create_dir_all(src.path().join(".git")).unwrap();
        std::fs::write(src.path().join(".git/HEAD"), "ref").unwrap();
        std::fs::create_dir_all(src.path().join("themes")).unwrap();
        std::fs::write(src.path().join("themes/t.toml"), "y").unwrap();

        let dst = tempfile::tempdir().unwrap();
        let dest = dst.path().join("plugin");
        copy_plugin_tree(src.path(), &dest).unwrap();
        assert!(dest.join("aoe-plugin.toml").is_file());
        assert!(dest.join("themes/t.toml").is_file());
        assert!(!dest.join(".git").exists());
    }
}
