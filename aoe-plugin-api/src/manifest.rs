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
#[non_exhaustive]
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
    #[serde(default)]
    pub ui: Vec<UiContribution>,
    pub runtime: Option<RuntimeContribution>,
}

/// One typed contribution to a fixed UI extension point. The host renders
/// every slot with its own widgets (TUI) and components (web); the plugin's
/// worker pushes display state for its declared contributions through the
/// `ui.state.set` host RPC and never participates in the render path.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct UiContribution {
    /// Contribution id, unique within the plugin, e.g. `attention_badge`.
    pub id: String,
    pub slot: UiSlot,
    /// Shown by the host where the slot needs a heading (column header,
    /// panel title, sort mode name).
    pub title: String,
    /// Orders contributions sharing a slot; higher renders first.
    #[serde(default)]
    pub priority: i32,
}

/// The fixed extension points plugins may contribute UI to. Global slots
/// hold one state per contribution; session slots hold one per session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum UiSlot {
    /// Short global text in the TUI status bar and the web top bar.
    StatusBarSegment,
    /// Block-content card on the global overview surfaces.
    DashboardCard,
    /// Small per-session label in the session list row.
    SessionListRowBadge,
    /// Extra per-session cell with a header in the session list.
    SessionListColumn,
    /// Per-session numeric key exposed as a selectable sort mode; never
    /// overrides the default order silently.
    SessionListSortKey,
    /// Per-session facet values the list can filter on.
    SessionListFilterFacet,
    /// Small per-session annotation next to the session title.
    SessionDetailHeaderBadge,
    /// Block-content panel on the session detail surfaces.
    SessionDetailPanel,
}

impl UiSlot {
    /// Whether state for this slot is keyed per session (vs one global).
    pub fn session_scoped(self) -> bool {
        !matches!(self, UiSlot::StatusBarSegment | UiSlot::DashboardCard)
    }

    /// Kebab-case form used in manifests and API payloads.
    pub fn as_str(self) -> &'static str {
        match self {
            UiSlot::StatusBarSegment => "status-bar-segment",
            UiSlot::DashboardCard => "dashboard-card",
            UiSlot::SessionListRowBadge => "session-list-row-badge",
            UiSlot::SessionListColumn => "session-list-column",
            UiSlot::SessionListSortKey => "session-list-sort-key",
            UiSlot::SessionListFilterFacet => "session-list-filter-facet",
            UiSlot::SessionDetailHeaderBadge => "session-detail-header-badge",
            UiSlot::SessionDetailPanel => "session-detail-panel",
        }
    }
}

/// One setting rendered generically on the TUI and web settings surfaces,
/// stored under `[plugins."<id>".settings]` in config.toml.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
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
#[non_exhaustive]
pub enum SettingWidget {
    Toggle,
    Text,
    /// Integer input with optional inclusive bounds. Bounds are `i64`, matching
    /// the host's integer settings; a fractional bound (`min = 0.5`) is a loud
    /// TOML type error at parse rather than a silently truncated `0`.
    Number {
        min: Option<i64>,
        max: Option<i64>,
    },
    Select {
        options: Vec<String>,
    },
}

/// Override of another setting's default, resolved by priority.
/// The user's own config value always wins over any override.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct SettingDefaultOverride {
    /// Fully qualified target: another plugin's setting (`<plugin-id>.<key>`)
    /// or a core setting (`<section>.<field>`, e.g. `session.auto_archive`).
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
#[non_exhaustive]
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
#[non_exhaustive]
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
#[non_exhaustive]
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
#[non_exhaustive]
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
#[non_exhaustive]
pub struct ThemeContribution {
    /// Path to the theme TOML, relative to the plugin root.
    pub file: String,
}

/// Status detection for one agent: either declarative rules evaluated
/// in-core (Tier 0) or a batched RPC to the plugin worker (Tier 1).
//
// No deny_unknown_fields here: serde cannot combine it with #[serde(flatten)]
// on `mode`. CAVEAT for plugin authors: because of that, a typo in a top-level
// `[[status_detection]]` key (e.g. `agnet` for `agent`) is SILENTLY DROPPED
// rather than rejected, and you then hit a confusing downstream error. Verify
// a status_detection block with `aoe plugin info <id>` after editing. Keys
// inside `[[status_detection.rules]]` are safe: DetectionRule keeps
// deny_unknown_fields, so a rule typo still fails loudly.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct StatusDetectionContribution {
    /// Agent tool name this detector applies to, e.g. `codex`.
    pub agent: String,
    #[serde(flatten)]
    pub mode: DetectionMode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "mode")]
#[non_exhaustive]
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
#[non_exhaustive]
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
#[non_exhaustive]
pub enum StatusKind {
    Running,
    Waiting,
    Idle,
    Error,
}

/// Tier 1 worker definition: the executable the host spawns and supervises.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct RuntimeContribution {
    /// Executable path relative to the plugin root.
    pub entrypoint: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ManifestError {
    #[error("manifest is not valid TOML: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("manifest targets api_version {found} but this host supports 1..={max}; upgrade aoe")]
    UnsupportedApiVersion { found: u64, max: u32 },
    #[error("manifest is invalid:\n{}", .0.join("\n"))]
    Invalid(Vec<String>),
}

impl PluginManifest {
    /// Parse and validate an `aoe-plugin.toml` document.
    pub fn from_toml_str(input: &str) -> Result<Self, ManifestError> {
        // Pre-parse api_version permissively first. A manifest targeting a
        // newer host introduces capability/widget variants this host's strict
        // enums do not know, so a plain `toml::from_str::<Self>` would fail
        // with a confusing "unknown variant" error. Surfacing the version
        // mismatch first tells the author the real problem (upgrade aoe), not
        // that their capability name is wrong.
        if let Some(found) = toml::from_str::<toml::Value>(input)
            .ok()
            .and_then(|doc| doc.get("api_version").and_then(toml::Value::as_integer))
        {
            if found > API_VERSION as i64 {
                return Err(ManifestError::UnsupportedApiVersion {
                    found: found as u64,
                    max: API_VERSION,
                });
            }
        }
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

        let mut ui_ids = std::collections::HashSet::new();
        for ui in &self.ui {
            check(
                is_key(&ui.id),
                format!("ui contribution id {:?} must be snake_case", ui.id),
            );
            check(
                ui_ids.insert(ui.id.as_str()),
                format!("duplicate ui contribution id {:?}", ui.id),
            );
            check(
                !ui.title.is_empty(),
                format!("ui contribution {:?} needs a title", ui.id),
            );
        }

        let needs_runtime = !self.commands.is_empty()
            || !self.actions.is_empty()
            || !self.ui.is_empty()
            || self
                .status_detection
                .iter()
                .any(|d| matches!(d.mode, DetectionMode::Rpc { .. }));
        check(
            !needs_runtime || self.runtime.is_some(),
            "commands, actions, ui contributions, and rpc status detection require a [runtime] section"
                .into(),
        );

        if let Some(runtime) = &self.runtime {
            check(
                is_safe_relative_plugin_path(&runtime.entrypoint),
                format!(
                    "runtime.entrypoint {:?} must be a relative path inside the plugin (no leading \"/\", drive prefix, or \"..\")",
                    runtime.entrypoint
                ),
            );
        }

        for theme in &self.themes {
            check(
                is_safe_relative_plugin_path(&theme.file),
                format!(
                    "theme file {:?} must be a relative path inside the plugin (no leading \"/\", drive prefix, or \"..\")",
                    theme.file
                ),
            );
        }

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

/// A non-empty path the installer may safely join onto the plugin root: at
/// least one normal component, and no `..`, leading `/`, or drive prefix that
/// would escape the directory. Used for both `runtime.entrypoint` and theme
/// files, which the host copies/spawns relative to the plugin root.
fn is_safe_relative_plugin_path(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    let mut has_normal = false;
    for component in std::path::Path::new(value).components() {
        match component {
            std::path::Component::Normal(_) => has_normal = true,
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir
            | std::path::Component::RootDir
            | std::path::Component::Prefix(_) => return false,
        }
    }
    has_normal
}
