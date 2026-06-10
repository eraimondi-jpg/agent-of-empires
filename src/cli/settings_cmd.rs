//! `aoe settings`: inspect the settings surface. `explain` prints the full
//! resolution chain for a plugin setting (user value, cross-plugin default
//! override, manifest default, widget default), the CLI twin of
//! `GET /api/settings/resolved`.

use anyhow::{anyhow, Result};
use clap::Subcommand;

use crate::plugin::settings::{resolve, resolve_all, SettingSource};

#[derive(Subcommand)]
pub enum SettingsCommands {
    /// Explain where a setting's effective value comes from
    Explain {
        /// Fully qualified plugin setting key, `<plugin-id>.<key>`.
        /// Omit to list every plugin setting with its winning source.
        key: Option<String>,
    },
}

pub fn run(command: SettingsCommands) -> Result<()> {
    match command {
        SettingsCommands::Explain { key } => run_explain(key.as_deref()),
    }
}

fn source_label(source: &SettingSource) -> String {
    match source {
        SettingSource::UserConfig => "user config (config.toml)".to_string(),
        SettingSource::PluginDefault { plugin, priority } => {
            format!("default override by plugin {plugin} (priority {priority})")
        }
        SettingSource::ManifestDefault => "owning plugin's manifest default".to_string(),
        SettingSource::SchemaDefault => "widget default".to_string(),
    }
}

fn run_explain(key: Option<&str>) -> Result<()> {
    let registry = crate::plugin::registry();
    match key {
        None => {
            let resolved = resolve_all(&registry);
            if resolved.is_empty() {
                println!("No active plugin settings.");
                return Ok(());
            }
            for r in resolved {
                println!("{} = {}  [{}]", r.key, r.value, source_label(&r.source));
            }
            Ok(())
        }
        Some(key) => {
            let (plugin_id, setting_key) = key
                .rsplit_once('.')
                .ok_or_else(|| anyhow!("key must be <plugin-id>.<key>, got {key:?}"))?;
            let r = resolve(&registry, plugin_id, setting_key).ok_or_else(|| {
                anyhow!("no active plugin declares {key:?} (is the plugin enabled and granted?)")
            })?;
            println!("{} = {}", r.key, r.value);
            println!("resolved from: {}", source_label(&r.source));
            println!("\nresolution chain (first wins):");
            for (i, c) in r.candidates.iter().enumerate() {
                let marker = if i == 0 { "->" } else { "  " };
                println!("{marker} {} = {}", source_label(&c.source), c.value);
            }
            Ok(())
        }
    }
}
