//! Plugin settings: runtime schema contribution and default resolution.
//!
//! Plugin settings ride the exact same descriptor pipeline as core settings
//! (acceptance criterion 2 of #268): each active plugin's manifest settings
//! become [`FieldDescriptor`]s under a virtual section `plugin:<id>`, and the
//! TUI rows, the web schema endpoint, and the server PATCH validator all
//! consume [`runtime_schema`] instead of the compile-time `schema()`.
//!
//! The virtual section maps to the real config location
//! `plugins.<id>.settings.<key>` at the JSON read/write choke points
//! ([`nested_leaf`], `json_at` in the TUI, the PATCH transform on the
//! server). Profile overrides are not supported for plugin settings in v1,
//! so the descriptors are emitted `profile_overridable: false`.
//!
//! Default-override resolution (acceptance criterion 5): user value, then the
//! highest-priority enabled plugin's `setting_defaults` override, then the
//! owning manifest default, then the widget default. [`ResolvedSetting`]
//! carries the full losing chain for `aoe settings explain` and
//! `GET /api/settings/resolved`.

use aoe_plugin_api::{SettingContribution, SettingWidget};
use serde::Serialize;
use serde_json::{json, Value};

use super::registry::PluginRegistry;
use crate::session::settings_schema::{
    schema, FieldDescriptor, SelectOption, ValidationKind, WebWritePolicy, WidgetKind,
};

/// Prefix marking a virtual plugin section in a `FieldDescriptor`. Core
/// section names come from struct identifiers, so the colon cannot collide.
pub const VIRTUAL_PREFIX: &str = "plugin:";

/// The virtual schema section for a plugin's settings.
pub fn virtual_section(plugin_id: &str) -> String {
    format!("{VIRTUAL_PREFIX}{plugin_id}")
}

/// `Some(plugin_id)` when `section` is a virtual plugin section.
pub fn parse_virtual(section: &str) -> Option<&str> {
    section.strip_prefix(VIRTUAL_PREFIX)
}

/// Build the `{...}` JSON object that writes `section.field = leaf`. Core
/// sections produce the flat two-level shape every existing caller built
/// inline; virtual plugin sections expand to the real config nesting.
pub fn nested_leaf(section: &str, field: &str, leaf: Value) -> Value {
    match parse_virtual(section) {
        Some(id) => json!({ "plugins": { id: { "settings": { field: leaf } } } }),
        None => json!({ section: { field: leaf } }),
    }
}

/// Read the current value of `section.field` from a serialized config object,
/// resolving virtual plugin sections to their nested location.
pub fn json_at_descriptor<'a>(root: &'a Value, section: &str, field: &str) -> Option<&'a Value> {
    match parse_virtual(section) {
        Some(id) => root.get("plugins")?.get(id)?.get("settings")?.get(field),
        None => root.get(section)?.get(field),
    }
}

/// Rewrite every virtual `plugin:<id>` section of a settings PATCH body into
/// the real `plugins.<id>.settings` nesting, in place. Run after validation
/// and before `merge_json`, so the merged JSON deserializes back into the
/// typed `Config`.
pub fn expand_virtual_sections(patch: &mut Value) {
    let extracted: Vec<(String, Value)> = match patch.as_object_mut() {
        Some(obj) => {
            let keys: Vec<String> = obj
                .keys()
                .filter(|k| k.starts_with(VIRTUAL_PREFIX))
                .cloned()
                .collect();
            keys.into_iter()
                .filter_map(|k| obj.remove(&k).map(|v| (k, v)))
                .collect()
        }
        None => return,
    };
    for (section, fields) in extracted {
        let Some(fields) = fields.as_object() else {
            continue;
        };
        for (field, leaf) in fields {
            let nested = nested_leaf(&section, field, leaf.clone());
            crate::session::settings_schema::merge_json(patch, &nested);
        }
    }
}

/// The inverse of [`expand_virtual_sections`], for read responses: mirror
/// every active plugin's persisted settings table to a top-level
/// `plugin:<id>` key, resolved defaults filling unset keys. A schema-driven
/// client then finds values exactly at the paths
/// `GET /api/settings/schema` advertises.
pub fn project_virtual_sections(registry: &PluginRegistry, config_json: &mut Value) {
    let Some(root) = config_json.as_object_mut() else {
        return;
    };
    for plugin in registry.active() {
        if plugin.manifest.settings.is_empty() {
            continue;
        }
        let mut section = serde_json::Map::new();
        for setting in &plugin.manifest.settings {
            let value = plugin
                .settings
                .get(&setting.key)
                .map(toml_to_json)
                .or_else(|| {
                    resolve(registry, plugin.id(), &setting.key).map(|resolved| resolved.value)
                })
                .unwrap_or(Value::Null);
            section.insert(setting.key.clone(), value);
        }
        root.insert(virtual_section(plugin.id()), Value::Object(section));
    }
}

/// Core schema plus the descriptors contributed by every active plugin.
pub fn runtime_schema(registry: &PluginRegistry) -> Vec<FieldDescriptor> {
    let mut all = schema();
    for plugin in registry.active() {
        for setting in &plugin.manifest.settings {
            all.push(descriptor_for(plugin.id(), setting));
        }
    }
    all
}

fn descriptor_for(plugin_id: &str, setting: &SettingContribution) -> FieldDescriptor {
    FieldDescriptor {
        section: virtual_section(plugin_id),
        field: setting.key.clone(),
        category: "Plugins".to_string(),
        label: setting.label.clone(),
        description: setting.description.clone(),
        widget: widget_for(&setting.widget),
        web_write: WebWritePolicy::Allow,
        profile_overridable: false,
        validation: ValidationKind::None,
        advanced: false,
    }
}

fn widget_for(widget: &SettingWidget) -> WidgetKind {
    match widget {
        SettingWidget::Toggle => WidgetKind::Toggle,
        SettingWidget::Text => WidgetKind::Text {
            multiline: false,
            mono: false,
        },
        SettingWidget::Number { min, max } => WidgetKind::Number {
            min: min.map(|v| v as i64),
            max: max.map(|v| v as i64),
        },
        SettingWidget::Select { options } => WidgetKind::Select {
            options: options
                .iter()
                .map(|value| SelectOption::new(value, value))
                .collect(),
        },
    }
}

/// Where a resolved plugin-setting value came from.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum SettingSource {
    /// Explicit value in config.toml; always wins.
    UserConfig,
    /// Another plugin's `setting_defaults` override.
    PluginDefault { plugin: String, priority: i32 },
    /// The owning plugin's manifest default.
    ManifestDefault,
    /// The widget's zero value; nothing else supplied one.
    SchemaDefault,
    /// The built-in default of a core setting (`Config::default()`).
    CoreDefault,
}

/// One candidate in the resolution chain, winners and losers alike.
#[derive(Debug, Clone, Serialize)]
pub struct SettingCandidate {
    #[serde(flatten)]
    pub source: SettingSource,
    pub value: Value,
}

/// A fully resolved plugin setting with provenance, for
/// `aoe settings explain <key>` and `GET /api/settings/resolved`.
#[derive(Debug, Clone, Serialize)]
pub struct ResolvedSetting {
    /// Fully qualified key, `<plugin-id>.<key>`.
    pub key: String,
    pub value: Value,
    #[serde(flatten)]
    pub source: SettingSource,
    /// The full chain in precedence order; `candidates[0]` is the winner.
    pub candidates: Vec<SettingCandidate>,
}

fn widget_default(widget: &SettingWidget) -> Value {
    match widget {
        SettingWidget::Toggle => json!(false),
        SettingWidget::Text => json!(""),
        SettingWidget::Number { min, .. } => json!(min.unwrap_or(0.0)),
        SettingWidget::Select { options } => json!(options.first().cloned().unwrap_or_default()),
    }
}

fn toml_to_json(value: &toml::Value) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

/// Resolve every setting of every active plugin, plus every CORE setting an
/// active plugin overrides the default of.
pub fn resolve_all(registry: &PluginRegistry) -> Vec<ResolvedSetting> {
    let mut all: Vec<ResolvedSetting> = registry
        .active()
        .flat_map(|p| {
            p.manifest
                .settings
                .iter()
                .map(|s| resolve_one(registry, p.id(), s))
                .collect::<Vec<_>>()
        })
        .collect();
    let mut core_targets: Vec<(String, String)> = registry
        .active()
        .flat_map(|p| p.manifest.setting_defaults.iter())
        .filter_map(|ov| {
            let (section, field) = ov.target.rsplit_once('.')?;
            super::core_overrides::is_core_field(section, field)
                .then(|| (section.to_string(), field.to_string()))
        })
        .collect();
    core_targets.sort();
    core_targets.dedup();
    for (section, field) in core_targets {
        if let Some(resolved) = resolve_core(registry, &section, &field) {
            all.push(resolved);
        }
    }
    all
}

/// Resolve a CORE setting's default chain: explicit user value (an on-disk
/// value differing from the built-in default), then plugin default overrides
/// by priority, then the built-in default. `None` when the key is not a core
/// schema field.
pub fn resolve_core(
    registry: &PluginRegistry,
    section: &str,
    field: &str,
) -> Option<ResolvedSetting> {
    if !super::core_overrides::is_core_field(section, field) {
        return None;
    }
    let builtin = super::core_overrides::builtin_default(section, field);
    let mut candidates: Vec<SettingCandidate> = Vec::new();

    let on_disk = crate::session::config::config_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|raw| toml::from_str::<toml::Table>(&raw).ok())
        .and_then(|t| t.get(section)?.get(field).cloned());
    if let Some(value) = on_disk {
        if builtin.as_ref() != Some(&value) {
            candidates.push(SettingCandidate {
                source: SettingSource::UserConfig,
                value: toml_to_json(&value),
            });
        }
    }

    for (plugin, priority, value) in
        super::core_overrides::core_override_candidates(registry, section, field)
    {
        candidates.push(SettingCandidate {
            source: SettingSource::PluginDefault { plugin, priority },
            value: toml_to_json(&value),
        });
    }

    candidates.push(SettingCandidate {
        source: SettingSource::CoreDefault,
        value: builtin.as_ref().map(toml_to_json).unwrap_or(Value::Null),
    });

    let winner = &candidates[0];
    Some(ResolvedSetting {
        key: format!("{section}.{field}"),
        value: winner.value.clone(),
        source: winner.source.clone(),
        candidates,
    })
}

/// Resolve a single `<plugin-id>.<key>`, or `None` if no active plugin
/// declares it.
pub fn resolve(registry: &PluginRegistry, plugin_id: &str, key: &str) -> Option<ResolvedSetting> {
    let plugin = registry.get(plugin_id).filter(|p| p.active())?;
    let setting = plugin.manifest.settings.iter().find(|s| s.key == key)?;
    Some(resolve_one(registry, plugin_id, setting))
}

fn resolve_one(
    registry: &PluginRegistry,
    plugin_id: &str,
    setting: &SettingContribution,
) -> ResolvedSetting {
    let target = format!("{plugin_id}.{}", setting.key);
    let mut candidates: Vec<SettingCandidate> = Vec::new();

    if let Some(value) = registry
        .get(plugin_id)
        .and_then(|p| p.settings.get(&setting.key))
    {
        candidates.push(SettingCandidate {
            source: SettingSource::UserConfig,
            value: toml_to_json(value),
        });
    }

    // Default overrides from every active plugin, highest priority first.
    // The owning plugin may not override its own setting this way; its
    // channel is the manifest default.
    let mut overrides: Vec<(&str, i32, &toml::Value)> = registry
        .active()
        .filter(|p| p.id() != plugin_id)
        .flat_map(|p| {
            p.manifest
                .setting_defaults
                .iter()
                .filter(|ov| ov.target == target)
                .map(|ov| (p.id(), ov.priority, &ov.value))
                .collect::<Vec<_>>()
        })
        .collect();
    overrides.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    for (plugin, priority, value) in overrides {
        candidates.push(SettingCandidate {
            source: SettingSource::PluginDefault {
                plugin: plugin.to_string(),
                priority,
            },
            value: toml_to_json(value),
        });
    }

    if let Some(default) = &setting.default {
        candidates.push(SettingCandidate {
            source: SettingSource::ManifestDefault,
            value: toml_to_json(default),
        });
    }

    candidates.push(SettingCandidate {
        source: SettingSource::SchemaDefault,
        value: widget_default(&setting.widget),
    });

    let winner = &candidates[0];
    ResolvedSetting {
        key: target,
        value: winner.value.clone(),
        source: winner.source.clone(),
        candidates,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_section_round_trips() {
        let section = virtual_section("aoe-status");
        assert_eq!(parse_virtual(&section), Some("aoe-status"));
        assert_eq!(parse_virtual("session"), None);
    }

    #[test]
    fn nested_leaf_expands_virtual_sections() {
        let leaf = nested_leaf("plugin:aoe-status", "poll_interval_ms", json!(500));
        assert_eq!(
            leaf,
            json!({ "plugins": { "aoe-status": { "settings": { "poll_interval_ms": 500 } } } })
        );
        let core = nested_leaf("session", "yolo_mode_default", json!(true));
        assert_eq!(core, json!({ "session": { "yolo_mode_default": true } }));
    }

    #[test]
    fn json_at_descriptor_reads_both_shapes() {
        let root = json!({
            "session": { "yolo_mode_default": true },
            "plugins": { "aoe-status": { "enabled": true, "settings": { "poll_interval_ms": 500 } } },
        });
        assert_eq!(
            json_at_descriptor(&root, "session", "yolo_mode_default"),
            Some(&json!(true))
        );
        assert_eq!(
            json_at_descriptor(&root, "plugin:aoe-status", "poll_interval_ms"),
            Some(&json!(500))
        );
        assert_eq!(json_at_descriptor(&root, "plugin:missing", "x"), None);
    }
}
