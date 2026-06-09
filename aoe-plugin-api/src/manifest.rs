use serde::{Deserialize, Serialize};

use crate::{Capability, PluginId, API_VERSION};

/// Parsed `aoe-plugin.toml`.
///
/// Tier 0 contributions (settings, keybinds, themes, declarative status
/// rules) are usable without any plugin code running. Contributions that need
/// a handler (commands, actions, RPC status detection) require a `runtime`
/// section and are dispatched to the plugin's JSON-RPC worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginManifest {
    pub id: PluginId,
    /// Human-readable display name.
    pub name: String,
    pub version: String,
    /// Manifest schema / host API version this manifest targets.
    pub api_version: u32,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    #[serde(default)]
    pub settings: Vec<SettingContribution>,
    #[serde(default)]
    pub setting_defaults: Vec<SettingDefaultOverride>,
    #[serde(default)]
    pub commands: Vec<CliCommandContribution>,
    #[serde(default)]
    pub actions: Vec<ActionContribution>,
    #[serde(default)]
    pub keybinds: Vec<KeybindContribution>,
    #[serde(default)]
    pub themes: Vec<ThemeContribution>,
    #[serde(default)]
    pub status_detection: Vec<StatusDetectionContribution>,
    pub runtime: Option<RuntimeContribution>,
}

/// One setting rendered generically on the TUI and web settings surfaces,
/// stored under `[plugins."<id>".settings]` in config.toml.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingContribution {
    /// Field name within the plugin's settings table, e.g. `poll_interval_ms`.
    pub key: String,
    pub label: String,
    #[serde(default)]
    pub description: String,
    pub widget: SettingWidget,
    pub default: Option<toml::Value>,
}

/// Widget kinds the generic settings renderers support for plugin fields.
/// A deliberate subset of the core schema's widgets; custom widgets stay
/// core-only because they need bespoke UI code on both surfaces.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum SettingWidget {
    Toggle,
    Text,
    Number { min: Option<f64>, max: Option<f64> },
    Select { options: Vec<String> },
}

/// Override of another plugin's setting default, resolved by priority.
/// The user's own config value always wins over any override.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingDefaultOverride {
    /// Fully qualified target, `<plugin-id>.<key>`.
    pub target: String,
    pub value: toml::Value,
    pub priority: i32,
    #[serde(default)]
    pub reason: String,
}

/// A CLI command grafted into the `aoe` command tree at runtime and
/// dispatched to the plugin worker over JSON-RPC.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CliCommandContribution {
    /// Position in the command tree: `["review"]` is top level (requires the
    /// `cli-top-level` capability), `["session", "archive"]` slots under an
    /// existing group. Core-owned paths always win; collisions are rejected
    /// at manifest load.
    pub path: Vec<String>,
    pub about: String,
    #[serde(default)]
    pub args: Vec<CliArg>,
    /// JSON-RPC method invoked on the plugin worker.
    pub rpc_method: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CliArg {
    pub name: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub help: String,
}

/// A named action dispatchable from keybinds and the command palette,
/// canonically `plugin.<id>.<name>`, handled by the plugin worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionContribution {
    pub name: String,
    pub label: String,
    /// JSON-RPC method invoked on the plugin worker.
    pub rpc_method: String,
}

/// Default chord for a contributed action. Core bindings always shadow
/// plugin bindings; conflicts between plugins resolve by priority and are
/// inspectable, never silent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeybindContribution {
    /// Name of an action declared in this manifest.
    pub action: String,
    /// Chord in the keymap syntax, e.g. `ctrl+r`.
    pub chord: String,
    #[serde(default)]
    pub priority: i32,
}

/// A theme file shipped with the plugin, copied into the theme search path.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThemeContribution {
    /// Path to the theme TOML, relative to the plugin root.
    pub file: String,
}

/// Status detection for one agent: either declarative rules evaluated
/// in-core (Tier 0) or a batched RPC to the plugin worker (Tier 1).
// No deny_unknown_fields here: serde cannot combine it with #[serde(flatten)].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusDetectionContribution {
    /// Agent tool name this detector applies to, e.g. `codex`.
    pub agent: String,
    #[serde(flatten)]
    pub mode: DetectionMode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "mode")]
pub enum DetectionMode {
    /// Ordered marker/regex rules evaluated in-core; no plugin code runs.
    Declarative { rules: Vec<DetectionRule> },
    /// Pane snapshots are batched per poll tick into one `method` call on the
    /// plugin worker. Requires `runtime` and the `pane-read` capability.
    Rpc { method: String },
}

/// One declarative rule; the highest-priority matching rule wins.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DetectionRule {
    pub status: StatusKind,
    #[serde(default)]
    pub priority: i32,
    /// Matches when every literal appears in the pane text.
    #[serde(default)]
    pub contains: Vec<String>,
    /// Matches when the regex matches the pane text.
    pub regex: Option<String>,
    /// Fallback when no other rule matches; at most one per agent.
    #[serde(default)]
    pub default: bool,
}

/// Session statuses a detection rule may report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusKind {
    Running,
    Waiting,
    Idle,
    Error,
}

/// Tier 1 worker definition: the executable the host spawns and supervises.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeContribution {
    /// Executable path relative to the plugin root.
    pub entrypoint: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("manifest is not valid TOML: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("manifest is invalid:\n{}", .0.join("\n"))]
    Invalid(Vec<String>),
}

impl PluginManifest {
    /// Parse and validate an `aoe-plugin.toml` document.
    pub fn from_toml_str(input: &str) -> Result<Self, ManifestError> {
        let manifest: Self = toml::from_str(input)?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Structural validation; collects every problem instead of stopping at
    /// the first so a plugin author sees the full list in one pass.
    pub fn validate(&self) -> Result<(), ManifestError> {
        let mut errors = Vec::new();
        let mut check = |ok: bool, msg: String| {
            if !ok {
                errors.push(msg);
            }
        };

        check(
            (1..=API_VERSION).contains(&self.api_version),
            format!(
                "api_version {} is not supported (host supports 1..={API_VERSION})",
                self.api_version
            ),
        );
        check(!self.version.is_empty(), "version must not be empty".into());
        check(!self.name.is_empty(), "name must not be empty".into());

        let mut seen = std::collections::HashSet::new();
        for setting in &self.settings {
            check(
                is_key(&setting.key),
                format!("setting key {:?} must be snake_case", setting.key),
            );
            check(
                seen.insert(&setting.key),
                format!("duplicate setting key {:?}", setting.key),
            );
        }

        for ov in &self.setting_defaults {
            let well_formed = match ov.target.rsplit_once('.') {
                Some((plugin, key)) => PluginId::new(plugin).is_ok() && is_key(key),
                None => false,
            };
            check(
                well_formed,
                format!(
                    "setting_defaults target {:?} must be \"<plugin-id>.<key>\"",
                    ov.target
                ),
            );
        }

        let mut action_names = std::collections::HashSet::new();
        for action in &self.actions {
            check(
                is_key(&action.name),
                format!("action name {:?} must be snake_case", action.name),
            );
            check(
                action_names.insert(action.name.as_str()),
                format!("duplicate action {:?}", action.name),
            );
        }
        for keybind in &self.keybinds {
            check(
                action_names.contains(keybind.action.as_str()),
                format!("keybind references undeclared action {:?}", keybind.action),
            );
        }

        let mut paths = std::collections::HashSet::new();
        for command in &self.commands {
            check(
                !command.path.is_empty(),
                "command path must not be empty".into(),
            );
            check(
                command.path.iter().all(|s| !s.is_empty()),
                format!("command path {:?} has an empty segment", command.path),
            );
            check(
                paths.insert(command.path.clone()),
                format!("duplicate command path {:?}", command.path),
            );
            check(
                command.path.len() > 1 || self.capabilities.contains(&Capability::CliTopLevel),
                format!(
                    "top-level command {:?} requires the cli-top-level capability",
                    command.path
                ),
            );
        }

        for detection in &self.status_detection {
            match &detection.mode {
                DetectionMode::Declarative { rules } => {
                    check(
                        !rules.is_empty(),
                        format!("agent {:?} declares no detection rules", detection.agent),
                    );
                    check(
                        rules.iter().filter(|r| r.default).count() <= 1,
                        format!(
                            "agent {:?} declares more than one default rule",
                            detection.agent
                        ),
                    );
                    for rule in rules {
                        check(
                            rule.default || !rule.contains.is_empty() || rule.regex.is_some(),
                            format!(
                                "agent {:?} has a rule with no contains/regex and default = false",
                                detection.agent
                            ),
                        );
                    }
                }
                DetectionMode::Rpc { .. } => {
                    check(
                        self.capabilities.contains(&Capability::PaneRead),
                        format!(
                            "agent {:?} uses rpc detection without the pane-read capability",
                            detection.agent
                        ),
                    );
                }
            }
        }

        let needs_runtime = !self.commands.is_empty()
            || !self.actions.is_empty()
            || self
                .status_detection
                .iter()
                .any(|d| matches!(d.mode, DetectionMode::Rpc { .. }));
        check(
            !needs_runtime || self.runtime.is_some(),
            "commands, actions, and rpc status detection require a [runtime] section".into(),
        );

        if errors.is_empty() {
            Ok(())
        } else {
            Err(ManifestError::Invalid(errors))
        }
    }
}

fn is_key(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}
