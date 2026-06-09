//! Plugin manifest and capability types for the Agent of Empires plugin system.
//!
//! This crate is the stable surface a plugin author (and the in-tree host)
//! compiles against: the `aoe-plugin.toml` manifest schema, the capability
//! taxonomy, and the validation rules that gate a manifest before any of its
//! contributions are registered. The host side (registries, JSON-RPC worker
//! protocol, capability grants) lives in the main crate and lands in later
//! phases; see `docs/development/internals/plugin-system.md` for the design.

mod capability;
mod id;
mod manifest;

pub use capability::Capability;
pub use id::{InvalidPluginId, PluginId};
pub use manifest::{
    ActionContribution, CliArg, CliCommandContribution, DetectionMode, DetectionRule,
    KeybindContribution, ManifestError, PluginManifest, RuntimeContribution, SettingContribution,
    SettingDefaultOverride, SettingWidget, StatusDetectionContribution, StatusKind,
    ThemeContribution,
};

/// Version of the manifest schema and host API this crate describes.
///
/// A manifest declares the `api_version` it was written against; the host
/// refuses manifests targeting a newer version than it understands.
pub const API_VERSION: u32 = 1;
