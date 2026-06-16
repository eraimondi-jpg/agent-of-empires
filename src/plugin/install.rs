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

use super::featured::FeaturedValidation;
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
    /// Whether the staged tree matches the curated featured index (a hash
    /// mismatch never reaches the prompt; it fails the install outright).
    pub featured: FeaturedValidation,
    /// Hash of the exact staged manifest this prompt describes. Two-phase
    /// surfaces (the web 409 flow) echo it back on confirm so approval
    /// binds to what the user reviewed, not whatever the source serves on
    /// the second fetch.
    pub manifest_hash: String,
    /// Core setting defaults this plugin will redirect, as `section.field =
    /// value` strings. Surfaced on every prompt because redirecting a core
    /// default needs no capability, so it would otherwise be invisible.
    pub core_default_overrides: Vec<String>,
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

/// Cross-process plugin mutation lock (`<app_dir>/plugin.lock`). Install,
/// update, uninstall, and enable/disable each run load-modify-save cycles
/// over three whole-file stores (grants, lockfile, config); two concurrent
/// mutations starting from the same snapshots would drop each other's
/// writes (reachable from the dashboard, where every REST mutation runs in
/// its own blocking task). Held for the WRITE phase only, never across an
/// interactive capability prompt; released on drop.
fn mutation_lock() -> Result<std::fs::File> {
    use fs2::FileExt;
    let path = crate::session::get_app_dir()?.join("plugin.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    file.lock_exclusive()
        .with_context(|| format!("locking {}", path.display()))?;
    Ok(file)
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
    tree_hash: String,
    /// HEAD of the clone for GitHub sources; None for local paths.
    commit: Option<String>,
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
    let tree_hash = super::integrity::tree_hash(&root)?;
    let commit = match source {
        PluginSource::GitHub { .. } => {
            let output = std::process::Command::new("git")
                .args(["-C"])
                .arg(&root)
                .args(["rev-parse", "HEAD"])
                .output()
                .context("running git rev-parse")?;
            output
                .status
                .success()
                .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
        }
        _ => None,
    };
    Ok(Staged {
        root,
        _tempdir: tempdir,
        manifest,
        manifest_raw,
        tree_hash,
        commit,
    })
}

/// Check a staged tree against the curated featured index. Local-path
/// installs are never featured; a pinned-hash mismatch is a hard error.
fn featured_validation(source: &PluginSource, staged: &Staged) -> Result<FeaturedValidation> {
    match source {
        PluginSource::GitHub { slug } => super::featured::index().validate(
            slug,
            staged.manifest.id.as_str(),
            &staged.manifest.version,
            &staged.tree_hash,
        ),
        _ => Ok(FeaturedValidation::NotFeatured),
    }
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

    let featured = featured_validation(&source, &staged)?;
    // The aoe.* and agent-of-empires.* namespaces are first-party. A community
    // install may use one only when the source is featured (the index pins
    // that slug to that id, so it is the genuine official plugin, e.g.
    // agent-of-empires.github from agent-of-empires/plugin-github); otherwise
    // a third party could squat the builtin/official id and usurp the
    // telemetry allowlist and plugin_meta namespace.
    if staged.manifest.id.is_reserved_namespace() && featured == FeaturedValidation::NotFeatured {
        bail!(
            "plugin id {id:?} uses a reserved namespace (aoe.* / agent-of-empires.*); \
             only first-party builtins and featured plugins may use it"
        );
    }
    let staged_hash = manifest_hash(staged.manifest_raw.as_bytes());
    let prompt = InstallPrompt {
        id: id.clone(),
        name: staged.manifest.name.clone(),
        version: staged.manifest.version.clone(),
        description: staged.manifest.description.clone(),
        capabilities: staged.manifest.capabilities.clone(),
        trust: source.trust_level(),
        source: source.clone(),
        previous_capabilities: None,
        featured,
        manifest_hash: staged_hash.clone(),
        core_default_overrides: super::core_overrides::declared_core_overrides(&staged.manifest),
    };
    if !confirm(&prompt) {
        return Ok(InstallOutcome::Declined);
    }

    let _lock = mutation_lock()?;
    // Re-check under the lock: the duplicate guard above ran pre-lock (it has
    // to, the prompt is interactive and the lock is never held across it), so
    // two concurrent dashboard installs of the same id could both pass it and
    // both prompt. Only one wins the lock; the loser must not overwrite the
    // winner's freshly installed tree, grant, and lockfile entry.
    if super::registry().get(&id).is_some() {
        bail!("{id} is already installed; use `aoe plugin update {id}`");
    }
    let dest = super::plugins_dir()?.join(&id);
    copy_plugin_tree(&staged.root, &dest)?;

    // The tree swap and the metadata writes are one unit: if any post-copy
    // write fails, remove the tree so a half-installed plugin never lingers
    // behind missing grant/lockfile/config state.
    let metadata = (|| -> Result<()> {
        let hash = staged_hash;
        GrantStore::load()?.grant(&id, hash.clone(), staged.manifest.capabilities.clone())?;
        Lockfile::load()?.upsert(
            &id,
            LockRecord {
                version: staged.manifest.version.clone(),
                source,
                manifest_hash: hash,
                tree_hash: staged.tree_hash.clone(),
                commit: staged.commit.clone(),
                installed_at: Utc::now(),
            },
        )?;
        enable_in_config(&id, true)
    })();
    if let Err(e) = metadata {
        std::fs::remove_dir_all(&dest).ok();
        return Err(e);
    }
    super::reload_registry();
    Ok(InstallOutcome::Installed {
        id,
        version: staged.manifest.version,
    })
}

/// Whether `update` must re-prompt the user. Beyond a capability-set change,
/// a featured slug serving a version absent from the index (`UnknownVersion`)
/// forces the prompt: that is the version-bump hijack vector, and a
/// capability-only gate would let it through silently (auto-update declines
/// the forced prompt, so it fail-closes to NeedsApproval).
fn update_needs_prompt(caps_changed: bool, featured: FeaturedValidation) -> bool {
    caps_changed || featured == FeaturedValidation::UnknownVersion
}

/// Update one installed plugin from its recorded source. Re-prompts via
/// `confirm` when the capability set differs from the granted one, or when a
/// featured slug serves an unverified (unlisted) version; an unchanged set on
/// a verified/non-featured release keeps the grant (re-pinned to the new
/// manifest hash).
pub fn update(
    plugin_id: &str,
    confirm: &mut dyn FnMut(&InstallPrompt) -> bool,
) -> Result<InstallOutcome> {
    let record = Lockfile::load()?
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
    // Up-to-date means the whole TREE is unchanged, not just the manifest:
    // a code-only release with an untouched manifest must still install.
    if staged.tree_hash == record.tree_hash {
        return Ok(InstallOutcome::UpToDate {
            id: plugin_id.to_string(),
            version: record.version,
        });
    }
    let featured = featured_validation(&record.source, &staged)?;
    let new_hash = manifest_hash(staged.manifest_raw.as_bytes());

    let granted_caps = GrantStore::load()?
        .record(plugin_id)
        .map(|r| r.capabilities.clone())
        .unwrap_or_default();
    let mut new_caps = staged.manifest.capabilities.clone();
    let mut old_caps = granted_caps.clone();
    new_caps.sort();
    old_caps.sort();
    if update_needs_prompt(new_caps != old_caps, featured) {
        let prompt = InstallPrompt {
            id: plugin_id.to_string(),
            name: staged.manifest.name.clone(),
            version: staged.manifest.version.clone(),
            description: staged.manifest.description.clone(),
            capabilities: staged.manifest.capabilities.clone(),
            trust: record.source.trust_level(),
            source: record.source.clone(),
            previous_capabilities: Some(granted_caps),
            featured,
            manifest_hash: new_hash.clone(),
            core_default_overrides: super::core_overrides::declared_core_overrides(
                &staged.manifest,
            ),
        };
        if !confirm(&prompt) {
            return Ok(InstallOutcome::Declined);
        }
    }

    let _lock = mutation_lock()?;
    // Re-load the grant and lockfile stores under the lock: the snapshots
    // above were read before the (lock-free) interactive prompt, so a
    // concurrent mutation could have superseded them. The load-modify-save
    // writes below must build on current disk state, not the stale snapshot.
    let mut grants = GrantStore::load()?;
    let mut lockfile = Lockfile::load()?;
    let dest = super::plugins_dir()?.join(plugin_id);
    let backup = dest.with_extension("updating");
    if backup.exists() {
        std::fs::remove_dir_all(&backup)?;
    }
    if dest.exists() {
        std::fs::rename(&dest, &backup)?;
    }
    // Tree swap + metadata writes are one unit: the backup is restored on
    // ANY failure up to and including the grant/lockfile writes, so new
    // plugin bytes can never sit behind an old manifest hash.
    let swap = (|| -> Result<()> {
        copy_plugin_tree(&staged.root, &dest)?;
        grants.grant(
            plugin_id,
            new_hash.clone(),
            staged.manifest.capabilities.clone(),
        )?;
        lockfile.upsert(
            plugin_id,
            LockRecord {
                version: staged.manifest.version.clone(),
                source: record.source.clone(),
                manifest_hash: new_hash.clone(),
                tree_hash: staged.tree_hash.clone(),
                commit: staged.commit.clone(),
                installed_at: Utc::now(),
            },
        )
    })();
    match swap {
        Ok(()) => {
            if backup.exists() {
                std::fs::remove_dir_all(&backup).ok();
            }
        }
        Err(e) => {
            // Roll the old tree back so a failed update never leaves a hole
            // or a half-updated install.
            std::fs::remove_dir_all(&dest).ok();
            if backup.exists() {
                std::fs::rename(&backup, &dest).ok();
            }
            return Err(e);
        }
    }
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
    let _lock = mutation_lock()?;
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
    let _lock = mutation_lock()?;
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
        if super::integrity::dir_or_reject(&entry.file_type()?, &from)? {
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
    fn update_prompts_on_unverified_featured_version_even_with_unchanged_caps() {
        // The hijack: caps unchanged, but a featured slug now serves a version
        // not in the index. Must still prompt (so auto-update fail-closes).
        assert!(update_needs_prompt(
            false,
            FeaturedValidation::UnknownVersion
        ));
        // Capability change always prompts, regardless of featured status.
        assert!(update_needs_prompt(true, FeaturedValidation::Verified));
        assert!(update_needs_prompt(true, FeaturedValidation::NotFeatured));
        // The only silent path: unchanged caps on a verified or ordinary
        // community release.
        assert!(!update_needs_prompt(false, FeaturedValidation::Verified));
        assert!(!update_needs_prompt(false, FeaturedValidation::NotFeatured));
    }

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
