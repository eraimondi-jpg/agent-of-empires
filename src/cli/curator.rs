//! `agent-of-empires curator` subcommands: manually run the headless group
//! curator over a group's shared `context.md`, and inspect its bookkeeping. The
//! engine lives in `src/session/curator.rs`; group resolution is shared with the
//! sibling `context` CLI.

use anyhow::Result;
use clap::Subcommand;

use crate::cli::context::{resolve_group, GroupArg};
use crate::session::curator::{self, CurateOutcome};
use crate::session::group_context;

/// Default tool for a manual curate when `--agent` is not given.
const DEFAULT_TOOL: &str = "claude";

#[derive(Subcommand)]
pub enum CuratorCommands {
    /// Curate a group's context.md now (forces past the change-gate)
    Run {
        #[command(flatten)]
        group: GroupArg,
        /// One-shot agent to run the curate with (defaults to `claude`)
        #[arg(long)]
        agent: Option<String>,
    },
    /// Show a group's curator state and whether a curate is pending
    Status(GroupArg),
}

pub async fn run(profile: &str, command: CuratorCommands) -> Result<()> {
    match command {
        CuratorCommands::Run { group, agent } => {
            let group = resolve_group(profile, group.group)?;
            let tool = agent.unwrap_or_else(|| DEFAULT_TOOL.to_string());
            // A manual run forces past the change-gate: the user asked for it.
            let outcome = curator::curate(profile, &group, &tool, true).await?;
            print_outcome(&group, &outcome);
            Ok(())
        }
        CuratorCommands::Status(a) => {
            let group = resolve_group(profile, a.group)?;
            print_status(profile, &group)
        }
    }
}

fn print_outcome(group: &str, outcome: &CurateOutcome) {
    match outcome {
        CurateOutcome::Curated {
            context_bytes,
            summary_bytes,
        } => println!(
            "Curated {group}: context {context_bytes} bytes, summary {summary_bytes} bytes"
        ),
        CurateOutcome::SkippedNoChange => println!("No changes since last run for {group}"),
        CurateOutcome::SkippedNoAgent(tool) => {
            println!("No one-shot-capable agent '{tool}' for {group}")
        }
        CurateOutcome::Failed(msg) => println!("Curation failed for {group}: {msg}"),
    }
}

fn print_status(profile: &str, group: &str) -> Result<()> {
    match group_context::read_curator_state(profile, group)? {
        Some(state) => {
            println!("Group: {group}");
            println!("Last run: {}", state.last_run_at.to_rfc3339());
            println!("Last size: {} bytes", state.last_size);
        }
        None => {
            println!("Group: {group}");
            println!("Last run: never curated");
        }
    }
    if group_context::context_grew_since_last_curation(profile, group)? {
        println!("Status: pending changes");
    } else {
        println!("Status: up to date");
    }
    Ok(())
}
