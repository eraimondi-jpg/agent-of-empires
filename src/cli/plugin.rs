//! `aoe plugin`: plugin management (install, list, enable, disable, update,
//! info, uninstall). Reserved for management only; commands contributed BY
//! plugins graft elsewhere into the tree (see the plugin-system design doc).

use anyhow::Result;
use clap::Subcommand;

use crate::plugin::grants::GrantStatus;
use crate::plugin::install::{InstallOutcome, InstallPrompt};
use crate::plugin::TrustLevel;

#[derive(Subcommand)]
pub enum PluginCommands {
    /// List every known plugin with trust, version, and state
    List,
    /// Show one plugin's manifest details, capabilities, and grant state
    Info {
        /// Plugin id, e.g. `aoe.status`
        id: String,
    },
    /// Install a plugin from a GitHub slug (`owner/repo`) or a local directory
    Install {
        /// `owner/repo` or a path to a directory containing aoe-plugin.toml
        source: String,
        /// Skip the interactive capability prompt and grant everything declared
        #[arg(long)]
        yes: bool,
    },
    /// Remove an installed plugin (files, grant, config entry)
    Uninstall {
        /// Plugin id
        id: String,
    },
    /// Enable a plugin's contributions
    Enable {
        /// Plugin id
        id: String,
    },
    /// Disable a plugin; its settings stay on disk for re-enabling
    Disable {
        /// Plugin id
        id: String,
    },
    /// Update an installed plugin from its recorded source
    Update {
        /// Plugin id
        id: String,
        /// Skip the capability re-prompt when the declared set changed
        #[arg(long)]
        yes: bool,
    },
}

pub fn run(command: PluginCommands) -> Result<()> {
    match command {
        PluginCommands::List => run_list(),
        PluginCommands::Info { id } => run_info(&id),
        PluginCommands::Install { source, yes } => run_install(&source, yes),
        PluginCommands::Uninstall { id } => run_uninstall(&id),
        PluginCommands::Enable { id } => run_set_enabled(&id, true),
        PluginCommands::Disable { id } => run_set_enabled(&id, false),
        PluginCommands::Update { id, yes } => run_update(&id, yes),
    }
}

fn trust_label(trust: TrustLevel) -> &'static str {
    match trust {
        TrustLevel::Builtin => "builtin",
        TrustLevel::Community => "community",
    }
}

fn state_label(plugin: &crate::plugin::LoadedPlugin) -> &'static str {
    match (plugin.enabled, plugin.grant) {
        (false, _) => "disabled",
        (true, GrantStatus::Granted) => "enabled",
        (true, GrantStatus::Missing) => "needs grant",
        (true, GrantStatus::Stale) => "needs re-grant (capabilities changed)",
    }
}

fn run_list() -> Result<()> {
    let registry = crate::plugin::registry();
    if registry.all().is_empty() {
        println!("No plugins installed.");
    } else {
        println!(
            "{:<18} {:<9} {:<10} {:<34} STATE",
            "ID", "VERSION", "TRUST", "SOURCE"
        );
        for plugin in registry.all() {
            println!(
                "{:<18} {:<9} {:<10} {:<34} {}",
                plugin.id(),
                plugin.manifest.version,
                trust_label(plugin.trust()),
                plugin.source.describe(),
                state_label(plugin),
            );
        }
    }
    for err in registry.load_errors() {
        eprintln!("warning: {err}");
    }
    Ok(())
}

fn run_info(id: &str) -> Result<()> {
    let registry = crate::plugin::registry();
    let Some(plugin) = registry.get(id) else {
        anyhow::bail!("unknown plugin {id:?}; see `aoe plugin list`");
    };
    let m = &plugin.manifest;
    println!("{} ({})", m.name, m.id);
    println!("  version:  {}", m.version);
    println!("  trust:    {}", trust_label(plugin.trust()));
    println!("  source:   {}", plugin.source.describe());
    println!("  state:    {}", state_label(plugin));
    if !m.description.is_empty() {
        println!("  about:    {}", m.description);
    }
    if m.capabilities.is_empty() {
        println!("  capabilities: none (declarative contributions only)");
    } else {
        println!("  capabilities:");
        for cap in &m.capabilities {
            println!("    - {}", cap.as_str());
        }
    }
    let contributions = [
        ("settings", m.settings.len()),
        ("commands", m.commands.len()),
        ("actions", m.actions.len()),
        ("keybinds", m.keybinds.len()),
        ("themes", m.themes.len()),
        ("status detectors", m.status_detection.len()),
    ];
    let listed: Vec<String> = contributions
        .iter()
        .filter(|(_, n)| *n > 0)
        .map(|(what, n)| format!("{n} {what}"))
        .collect();
    if !listed.is_empty() {
        println!("  contributes: {}", listed.join(", "));
    }
    if m.runtime.is_some() {
        println!("  runtime: JSON-RPC worker (Tier 1)");
    }
    Ok(())
}

/// Interactive confirmation, honest about what capability gating is and is
/// not. Used by install and capability-changing updates.
fn prompt_for_capabilities(prompt: &InstallPrompt) -> bool {
    println!(
        "{} {} v{} [{}]",
        if prompt.previous_capabilities.is_some() {
            "Updating"
        } else {
            "Installing"
        },
        prompt.name,
        prompt.version,
        prompt.source.describe()
    );
    if !prompt.description.is_empty() {
        println!("  {}", prompt.description);
    }
    if let Some(prev) = &prompt.previous_capabilities {
        println!("\nDeclared capabilities CHANGED since you granted them:");
        let prev_strs: Vec<&str> = prev.iter().map(|c| c.as_str()).collect();
        for cap in &prompt.capabilities {
            let marker = if prev_strs.contains(&cap.as_str()) {
                " "
            } else {
                "+"
            };
            println!("  {marker} {}", cap.as_str());
        }
    } else if prompt.capabilities.is_empty() {
        println!("\nNo runtime capabilities requested (declarative contributions only).");
    } else {
        println!("\nThis plugin requests:");
        for cap in &prompt.capabilities {
            println!("  - {}", cap.as_str());
        }
    }
    if prompt.trust == TrustLevel::Community {
        println!(
            "\nNote: capability grants gate this plugin's access to aoe's APIs.\n\
             They are NOT an OS sandbox; its worker process runs with your user's\n\
             permissions. Only install plugins you trust."
        );
    }
    print!("\nProceed? [y/N] ");
    use std::io::Write;
    std::io::stdout().flush().ok();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim(), "y" | "Y" | "yes")
}

fn run_install(source: &str, yes: bool) -> Result<()> {
    let source = crate::plugin::install::parse_source(source)?;
    let mut confirm = |prompt: &InstallPrompt| yes || prompt_for_capabilities(prompt);
    match crate::plugin::install::install(source, &mut confirm)? {
        InstallOutcome::Installed { id, version } => {
            println!("Installed {id} v{version} (enabled).");
        }
        InstallOutcome::Declined => println!("Aborted; nothing installed."),
        outcome => unreachable!("install returned {outcome:?}"),
    }
    Ok(())
}

fn run_uninstall(id: &str) -> Result<()> {
    crate::plugin::install::uninstall(id)?;
    println!("Uninstalled {id}. Per-session plugin data was kept.");
    Ok(())
}

fn run_set_enabled(id: &str, enabled: bool) -> Result<()> {
    crate::plugin::install::set_enabled(id, enabled)?;
    println!("{} {id}.", if enabled { "Enabled" } else { "Disabled" });
    if enabled {
        let registry = crate::plugin::registry();
        if let Some(plugin) = registry.get(id) {
            if plugin.grant != GrantStatus::Granted {
                println!(
                    "Capabilities are not granted for the current manifest; run \
                     `aoe plugin update {id}` or reinstall to re-approve."
                );
            }
        }
    }
    Ok(())
}

fn run_update(id: &str, yes: bool) -> Result<()> {
    let mut confirm = |prompt: &InstallPrompt| yes || prompt_for_capabilities(prompt);
    match crate::plugin::install::update(id, &mut confirm)? {
        InstallOutcome::Updated { id, version } => println!("Updated {id} to v{version}."),
        InstallOutcome::UpToDate { id, version } => {
            println!("{id} is up to date (v{version}).")
        }
        InstallOutcome::Declined => println!("Aborted; the installed version is unchanged."),
        outcome => unreachable!("update returned {outcome:?}"),
    }
    Ok(())
}
