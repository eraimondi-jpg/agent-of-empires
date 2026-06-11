//! Plugin-contributed default overrides for CORE settings.
//!
//! A manifest `[[setting_defaults]]` whose target names a core schema field
//! (`session.auto_archive_minutes` style) changes that field's DEFAULT, not
//! the user's choice: the override applies only while the on-disk value is
//! absent or still equal to the built-in default. Saved configs are fully
//! materialized, so "still equal to the built-in default" is the practical
//! definition of "the user never chose"; an explicit non-default user value
//! always wins.
//!
//! Applied inside `Config::load` BEFORE deserialization, which is earlier
//! than the plugin registry can exist (the registry itself loads config), so
//! this module scans manifests and grants directly and caches the scan;
//! `reload_registry` invalidates it after every plugin mutation.

use std::sync::{Arc, LazyLock, RwLock};

use aoe_plugin_api::{PluginManifest, SettingDefaultOverride};

use super::grants::{manifest_hash, GrantStatus, GrantStore};
use super::registry::BUILTINS;

/// One discovered plugin reduced to what override application needs.
struct ScannedPlugin {
    id: String,
    builtin: bool,
    /// Capability grant valid for the current manifest (builtins always).
    /// Ungranted plugins fail closed: their overrides never apply.
    granted: bool,
    overrides: Vec<SettingDefaultOverride>,
}

static SCAN: RwLock<Option<Arc<Vec<ScannedPlugin>>>> = RwLock::new(None);

/// Drop the manifest scan; the next config load rescans. Called by
/// `reload_registry` after install/uninstall/enable/update.
pub fn invalidate() {
    *SCAN.write().expect("core override scan lock") = None;
}

fn scanned() -> Arc<Vec<ScannedPlugin>> {
    if let Some(scan) = SCAN.read().expect("core override scan lock").as_ref() {
        return scan.clone();
    }
    let mut slot = SCAN.write().expect("core override scan lock");
    if let Some(scan) = slot.as_ref() {
        return scan.clone();
    }
    let scan = Arc::new(scan_manifests());
    *slot = Some(scan.clone());
    scan
}

fn scan_manifests() -> Vec<ScannedPlugin> {
    let mut out = Vec::new();
    for builtin in BUILTINS {
        if let Ok(manifest) = PluginManifest::from_toml_str(builtin.manifest_toml) {
            push_scanned(&mut out, &manifest, true, true);
        }
    }
    let grants = GrantStore::load().ok();
    if let Ok(dir) = super::plugins_dir() {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let manifest_path = entry.path().join("aoe-plugin.toml");
                let Ok(raw) = std::fs::read_to_string(&manifest_path) else {
                    continue;
                };
                let Ok(manifest) = PluginManifest::from_toml_str(&raw) else {
                    continue;
                };
                let granted = grants
                    .as_ref()
                    .map(|g| {
                        g.status(manifest.id.as_str(), &manifest_hash(raw.as_bytes()))
                            == GrantStatus::Granted
                    })
                    .unwrap_or(false);
                push_scanned(&mut out, &manifest, false, granted);
            }
        }
    }
    out
}

fn push_scanned(
    out: &mut Vec<ScannedPlugin>,
    manifest: &PluginManifest,
    builtin: bool,
    granted: bool,
) {
    if manifest.setting_defaults.is_empty() || out.iter().any(|p| p.id == manifest.id.as_str()) {
        return;
    }
    out.push(ScannedPlugin {
        id: manifest.id.as_str().to_string(),
        builtin,
        granted,
        overrides: manifest.setting_defaults.clone(),
    });
}

/// Serialized `Config::default()`, the "user never chose" comparison values.
static BUILTIN_DEFAULTS: LazyLock<toml::Table> = LazyLock::new(|| {
    toml::Table::try_from(crate::session::Config::default())
        .expect("default config serializes to a table")
});

/// Core `section.field` pairs that exist in the compile-time schema; the only
/// targets a plugin may override. Anything else is a plugin-setting target
/// (resolved elsewhere) or a typo (ignored).
static CORE_FIELDS: LazyLock<std::collections::HashSet<(String, String)>> = LazyLock::new(|| {
    crate::session::settings_schema::schema()
        .into_iter()
        .map(|d| (d.section, d.field))
        .collect()
});

/// Apply active plugins' core default overrides to a raw config table, in
/// place, before it deserializes into `Config`.
pub fn apply_to_table(table: &mut toml::Table) {
    let scan = scanned();
    apply_with(table, &scan);
}

fn enabled_in(table: &toml::Table, plugin: &ScannedPlugin) -> bool {
    table
        .get("plugins")
        .and_then(|p| p.get(&plugin.id))
        .and_then(|p| p.get("enabled"))
        .and_then(|v| v.as_bool())
        // Builtins default on; installed plugins need the explicit entry the
        // install writes.
        .unwrap_or(plugin.builtin)
}

/// Winning override per core target among granted + enabled plugins:
/// highest priority, ties broken on plugin id, matching the plugin-setting
/// resolution chain.
fn winners<'a>(
    table: &toml::Table,
    scan: &'a [ScannedPlugin],
) -> std::collections::HashMap<(&'a str, &'a str), (i32, &'a str, &'a toml::Value)> {
    let mut winners: std::collections::HashMap<(&str, &str), (i32, &str, &toml::Value)> =
        std::collections::HashMap::new();
    for plugin in scan {
        if !plugin.granted || !enabled_in(table, plugin) {
            continue;
        }
        for ov in &plugin.overrides {
            let Some((section, field)) = ov.target.rsplit_once('.') else {
                continue;
            };
            if !CORE_FIELDS.contains(&(section.to_string(), field.to_string())) {
                continue;
            }
            let candidate = (ov.priority, plugin.id.as_str(), &ov.value);
            winners
                .entry((section, field))
                .and_modify(|cur| {
                    if (candidate.0, std::cmp::Reverse(candidate.1))
                        > (cur.0, std::cmp::Reverse(cur.1))
                    {
                        *cur = candidate;
                    }
                })
                .or_insert(candidate);
        }
    }
    winners
}

fn apply_with(table: &mut toml::Table, scan: &[ScannedPlugin]) {
    for ((section, field), (_, plugin_id, value)) in winners(table, scan) {
        let builtin_default = BUILTIN_DEFAULTS.get(section).and_then(|s| s.get(field));
        // Refuse type drift: a string pushed into a bool field would fail
        // the whole config deserialization.
        if let Some(default) = builtin_default {
            if std::mem::discriminant(default) != std::mem::discriminant(value) {
                tracing::warn!(
                    target: "plugin",
                    plugin = plugin_id,
                    target = format!("{section}.{field}"),
                    "core default override has mismatched type; ignored"
                );
                continue;
            }
        }
        let current = table.get(section).and_then(|s| s.get(field));
        let user_chose = match (current, builtin_default) {
            (Some(current), Some(default)) => current != default,
            (Some(_), None) => true,
            (None, _) => false,
        };
        if user_chose {
            continue;
        }
        table
            .entry(section.to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if let Some(toml::Value::Table(section_table)) = table.get_mut(section) {
            section_table.insert(field.to_string(), value.clone());
        }
    }
}

/// The inverse of [`apply_to_table`], run by `save_config` before writing:
/// any core field whose value equals a KNOWN plugin override (from any
/// scanned plugin, enabled or not) resets to the built-in default, so an
/// applied override never persists as a user choice. If the plugin is still
/// active the override re-applies on the next load; if it was just disabled
/// the field genuinely falls back to the built-in default, which is the
/// point.
pub fn strip_from_table(table: &mut toml::Table) {
    let scan = scanned();
    strip_with(table, &scan);
}

fn strip_with(table: &mut toml::Table, scan: &[ScannedPlugin]) {
    for plugin in scan {
        for ov in &plugin.overrides {
            let Some((section, field)) = ov.target.rsplit_once('.') else {
                continue;
            };
            let current = table.get(section).and_then(|s| s.get(field));
            if current != Some(&ov.value) {
                continue;
            }
            let Some(default) = BUILTIN_DEFAULTS.get(section).and_then(|s| s.get(field)) else {
                continue;
            };
            if let Some(toml::Value::Table(section_table)) = table.get_mut(section) {
                section_table.insert(field.to_string(), default.clone());
            }
        }
    }
}

/// Active plugins' core-target overrides as provenance candidates for
/// `aoe settings explain` and `GET /api/settings/resolved`, highest priority
/// first: `(plugin_id, priority, value)`.
pub fn core_override_candidates(
    registry: &super::PluginRegistry,
    section: &str,
    field: &str,
) -> Vec<(String, i32, toml::Value)> {
    let target = format!("{section}.{field}");
    let mut overrides: Vec<(String, i32, toml::Value)> = registry
        .active()
        .flat_map(|p| {
            p.manifest
                .setting_defaults
                .iter()
                .filter(|ov| ov.target == target)
                .map(|ov| (p.id().to_string(), ov.priority, ov.value.clone()))
                .collect::<Vec<_>>()
        })
        .collect();
    overrides.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    overrides
}

/// The built-in default for a core field, as TOML, if the field exists.
pub fn builtin_default(section: &str, field: &str) -> Option<toml::Value> {
    BUILTIN_DEFAULTS.get(section)?.get(field).cloned()
}

/// Whether `section.field` names a real core schema field.
pub fn is_core_field(section: &str, field: &str) -> bool {
    CORE_FIELDS.contains(&(section.to_string(), field.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plugin(
        id: &str,
        granted: bool,
        target: &str,
        value: toml::Value,
        priority: i32,
    ) -> ScannedPlugin {
        ScannedPlugin {
            id: id.to_string(),
            builtin: false,
            granted,
            overrides: vec![SettingDefaultOverride {
                target: target.to_string(),
                value,
                priority,
                reason: String::new(),
            }],
        }
    }

    fn enable(table: &mut toml::Table, id: &str) {
        let plugins = table
            .entry("plugins")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if let toml::Value::Table(plugins) = plugins {
            plugins.insert(
                id.to_string(),
                toml::Value::Table(toml::Table::from_iter([(
                    "enabled".to_string(),
                    toml::Value::Boolean(true),
                )])),
            );
        }
    }

    // session.yolo_mode_default is a real bool core field defaulting false.
    const TARGET: &str = "session.yolo_mode_default";

    #[test]
    fn override_applies_when_absent_or_still_default() {
        let scan = vec![plugin(
            "acme.a",
            true,
            TARGET,
            toml::Value::Boolean(true),
            10,
        )];

        let mut absent = toml::Table::new();
        enable(&mut absent, "acme.a");
        apply_with(&mut absent, &scan);
        assert_eq!(
            absent["session"]["yolo_mode_default"],
            toml::Value::Boolean(true)
        );

        // Materialized at the built-in default: still counts as unchosen.
        let mut at_default: toml::Table =
            toml::from_str("[session]\nyolo_mode_default = false").unwrap();
        enable(&mut at_default, "acme.a");
        apply_with(&mut at_default, &scan);
        assert_eq!(
            at_default["session"]["yolo_mode_default"],
            toml::Value::Boolean(true)
        );
    }

    #[test]
    fn explicit_user_value_wins_and_bad_states_are_ignored() {
        // User chose the non-default: untouched.
        let scan = vec![plugin(
            "acme.a",
            true,
            TARGET,
            toml::Value::Boolean(false),
            10,
        )];
        let mut user_set: toml::Table =
            toml::from_str("[session]\nyolo_mode_default = true").unwrap();
        enable(&mut user_set, "acme.a");
        apply_with(&mut user_set, &scan);
        assert_eq!(
            user_set["session"]["yolo_mode_default"],
            toml::Value::Boolean(true)
        );

        // Ungranted, disabled, type-mismatched, unknown target: all no-ops.
        for scan in [
            vec![plugin(
                "acme.a",
                false,
                TARGET,
                toml::Value::Boolean(true),
                10,
            )],
            vec![plugin(
                "acme.b",
                true,
                TARGET,
                toml::Value::Boolean(true),
                10,
            )],
            vec![plugin(
                "acme.a",
                true,
                TARGET,
                toml::Value::String("yes".into()),
                10,
            )],
            vec![plugin(
                "acme.a",
                true,
                "session.not_a_real_field",
                toml::Value::Boolean(true),
                10,
            )],
        ] {
            let mut table = toml::Table::new();
            enable(&mut table, "acme.a");
            apply_with(&mut table, &scan);
            assert!(
                table
                    .get("session")
                    .and_then(|s| s.get("yolo_mode_default"))
                    .is_none()
                    && table
                        .get("session")
                        .and_then(|s| s.get("not_a_real_field"))
                        .is_none(),
                "no override should have applied: {table:?}"
            );
        }
    }

    #[test]
    fn strip_resets_override_values_but_keeps_real_choices() {
        let scan = vec![plugin(
            "acme.a",
            true,
            TARGET,
            toml::Value::Boolean(true),
            10,
        )];

        // Baked-in override value resets to the built-in default on save,
        // even when the plugin is disabled in the table being saved.
        let mut baked: toml::Table = toml::from_str("[session]\nyolo_mode_default = true").unwrap();
        strip_with(&mut baked, &scan);
        assert_eq!(
            baked["session"]["yolo_mode_default"],
            toml::Value::Boolean(false)
        );

        // A value no override produces is a real user choice; untouched.
        let other = vec![plugin(
            "acme.a",
            true,
            TARGET,
            toml::Value::Boolean(false),
            10,
        )];
        let mut chosen: toml::Table =
            toml::from_str("[session]\nyolo_mode_default = true").unwrap();
        strip_with(&mut chosen, &other);
        assert_eq!(
            chosen["session"]["yolo_mode_default"],
            toml::Value::Boolean(true)
        );
    }

    #[test]
    fn highest_priority_wins_ties_break_on_id() {
        let scan = vec![
            plugin("acme.low", true, TARGET, toml::Value::Boolean(false), 5),
            plugin("acme.high", true, TARGET, toml::Value::Boolean(true), 50),
        ];
        let mut table = toml::Table::new();
        enable(&mut table, "acme.low");
        enable(&mut table, "acme.high");
        apply_with(&mut table, &scan);
        assert_eq!(
            table["session"]["yolo_mode_default"],
            toml::Value::Boolean(true)
        );
    }
}
