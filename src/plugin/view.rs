//! The shared plugin-manager view-model.
//!
//! One Rust description of a plugin that both surfaces render from: the web
//! dashboard serializes it over `GET /api/plugins`, and the native TUI plugin
//! manager consumes it in-process. Neither re-derives the shape, so the
//! manager has one view-model instead of two that drift.
//!
//! The mutation logic (install / update / uninstall / enable / disable /
//! discover) already lives in [`super::install`] and friends and was always
//! shared; this module closes the last duplicated piece.

use aoe_plugin_api::{Capability, LinkHandlerContribution};
use serde::{Serialize, Serializer};

use super::grants::GrantStatus;
use super::registry::LoadedPlugin;
use super::{PluginSource, TrustLevel};

/// One declared plugin pane, identity only. The host owns the command; the
/// manager only lists which panes a plugin offers.
#[derive(Debug, Clone, Serialize)]
pub struct PaneView {
    pub id: String,
    pub title: String,
}

/// Serialize a [`PluginSource`] as its REDACTED display string. A local
/// install path leaks the username and project layout over a Tunnel/Funnel
/// deploy, so the wire form only ever carries the kind (`builtin`, `github:…`,
/// `path`, `linked`). The in-process TUI reads the `source` enum directly and
/// shows the full path via [`PluginSource::describe`]; only serialization
/// redacts.
fn redacted_source<S: Serializer>(source: &PluginSource, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&source.describe_redacted())
}

/// The manager's view of one plugin. Built by [`LoadedPlugin::view`], consumed
/// directly by the TUI and serialized for the web. The serialized shape is the
/// `GET /api/plugins` contract the web TypeScript mirrors, so changing a field
/// here changes both surfaces at once.
#[derive(Debug, Clone, Serialize)]
pub struct PluginView {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    /// Structured origin. Serializes redacted (see [`redacted_source`]); the
    /// TUI reads the enum for the full local path.
    #[serde(serialize_with = "redacted_source")]
    pub source: PluginSource,
    pub trust: TrustLevel,
    pub enabled: bool,
    pub grant: GrantStatus,
    pub active: bool,
    pub capabilities: Vec<Capability>,
    pub has_runtime: bool,
    pub setting_count: usize,
    /// First-party builtin (compiled in, no install root).
    pub builtin: bool,
    pub link_handlers: Vec<LinkHandlerContribution>,
    pub panes: Vec<PaneView>,
}

impl LoadedPlugin {
    /// The manager view-model for this plugin: the single shape both UIs
    /// render from.
    pub fn view(&self) -> PluginView {
        PluginView {
            id: self.id().to_string(),
            name: self.manifest.name.clone(),
            version: self.manifest.version.clone(),
            description: self.manifest.description.clone(),
            source: self.source.clone(),
            trust: self.trust(),
            enabled: self.enabled,
            grant: self.grant,
            active: self.active(),
            capabilities: self.manifest.capabilities.clone(),
            has_runtime: self.manifest.runtime.is_some(),
            setting_count: self.manifest.settings.len(),
            builtin: self.root.is_none(),
            link_handlers: self.manifest.link_handlers.clone(),
            panes: self
                .manifest
                .panes
                .iter()
                .map(|p| PaneView {
                    id: p.id.clone(),
                    title: p.title.clone(),
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view_with_source(source: PluginSource) -> PluginView {
        PluginView {
            id: "acme.demo".into(),
            name: "Demo".into(),
            version: "1.0.0".into(),
            description: String::new(),
            source,
            trust: TrustLevel::Community,
            enabled: true,
            grant: GrantStatus::Granted,
            active: true,
            capabilities: vec![],
            has_runtime: false,
            setting_count: 0,
            builtin: false,
            link_handlers: vec![],
            panes: vec![],
        }
    }

    #[test]
    fn serialized_source_is_redacted_for_local_paths() {
        // A local path must never reach the wire: only the kind is serialized.
        let json = serde_json::to_value(view_with_source(PluginSource::Path {
            path: "/home/secret/project".into(),
        }))
        .unwrap();
        assert_eq!(json["source"], "path");
        assert!(
            !json.to_string().contains("secret"),
            "serialized view leaked a local path: {json}"
        );

        let linked = serde_json::to_value(view_with_source(PluginSource::Linked {
            path: "/home/secret/dev".into(),
        }))
        .unwrap();
        assert_eq!(linked["source"], "linked");
        assert!(!linked.to_string().contains("secret"));
    }

    #[test]
    fn serialized_source_keeps_public_github_slug() {
        let json = serde_json::to_value(view_with_source(PluginSource::GitHub {
            slug: "owner/repo".into(),
        }))
        .unwrap();
        assert_eq!(json["source"], "github:owner/repo");
    }

    #[test]
    fn grant_status_serializes_to_manager_strings() {
        assert_eq!(
            serde_json::to_value(GrantStatus::Granted).unwrap(),
            "granted"
        );
        assert_eq!(
            serde_json::to_value(GrantStatus::Missing).unwrap(),
            "missing"
        );
        assert_eq!(serde_json::to_value(GrantStatus::Stale).unwrap(), "stale");
    }
}
