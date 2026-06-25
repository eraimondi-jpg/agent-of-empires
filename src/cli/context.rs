//! `agent-of-empires context` subcommands: the universal, file-locked write
//! path into a group's shared `context.md`, plus reads and cross-group summary
//! discovery. See `src/session/group_context.rs`.

use std::path::Path;

use anyhow::{bail, Result};
use clap::{Args, Subcommand};

use crate::session::group_context::{self, wiring, Author};
use crate::session::Storage;

#[derive(Subcommand)]
pub enum ContextCommands {
    /// Append a note to the current group's shared context
    Add(AddArgs),
    /// Print a group's context.md
    Show(GroupArg),
    /// Print a group's outward-facing summary.md
    Summary(GroupArg),
    /// List all groups with a one-line summary digest
    Summaries,
    /// Print canonical file paths for a group
    Path(GroupArg),
}

#[derive(Args)]
pub struct AddArgs {
    /// The note text to append
    pub text: String,
    /// Group path; inferred from the current directory when omitted
    #[arg(short = 'g', long)]
    pub group: Option<String>,
}

#[derive(Args)]
pub struct GroupArg {
    /// Group path; inferred from the current directory when omitted
    #[arg(short = 'g', long)]
    pub group: Option<String>,
}

pub async fn run(profile: &str, command: ContextCommands) -> Result<()> {
    match command {
        ContextCommands::Add(args) => add(profile, args),
        ContextCommands::Show(a) => {
            let group = resolve_group(profile, a.group)?;
            print!("{}", group_context::read_context(profile, &group)?);
            Ok(())
        }
        ContextCommands::Summary(a) => {
            let group = resolve_group(profile, a.group)?;
            let s = group_context::read_summary(profile, &group)?;
            if s.trim().is_empty() || s.trim() == group_context::SUMMARY_PLACEHOLDER {
                println!("(no summary yet)");
            } else {
                print!("{s}");
            }
            Ok(())
        }
        ContextCommands::Summaries => {
            for s in group_context::list_summaries(profile)? {
                println!("{}\t{}", s.group_path, s.digest);
            }
            Ok(())
        }
        ContextCommands::Path(a) => {
            let group = resolve_group(profile, a.group)?;
            let p = group_context::paths_for(profile, &group)?;
            println!("{}", p.context.display());
            println!("{}", p.summary.display());
            Ok(())
        }
    }
}

fn add(profile: &str, args: AddArgs) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let (group, author) = resolve_group_and_author(profile, args.group, &cwd)?;
    group_context::append_entry(profile, &group, &author, &args.text)?;
    eprintln!("added to group `{group}` context");
    Ok(())
}

/// Resolve only the group: explicit flag, else cwd marker, else cwd to instance.
fn resolve_group(profile: &str, explicit: Option<String>) -> Result<String> {
    if let Some(g) = explicit {
        return Ok(g);
    }
    let cwd = std::env::current_dir()?;
    if let Some(m) = read_marker(&cwd) {
        return Ok(m.group_path);
    }
    if let Some((g, _)) = group_from_instance(profile, &cwd)? {
        return Ok(g);
    }
    bail!("not inside a grouped aoe session; pass --group <path>");
}

fn resolve_group_and_author(
    profile: &str,
    explicit: Option<String>,
    cwd: &Path,
) -> Result<(String, Author)> {
    // Prefer the marker: it carries the session id for attribution.
    if explicit.is_none() {
        if let Some(m) = read_marker(cwd) {
            if let Some(author) = author_for_session(profile, &m.session_id)? {
                return Ok((m.group_path, author));
            }
        }
    }
    if let Some((g, author)) = group_from_instance(profile, cwd)? {
        let group = explicit.unwrap_or(g);
        return Ok((group, author));
    }
    if let Some(g) = explicit {
        return Ok((
            g,
            Author {
                title: "unknown".into(),
                tool: "unknown".into(),
                session_id: String::new(),
            },
        ));
    }
    bail!("not inside a grouped aoe session; pass --group <path>");
}

fn read_marker(cwd: &Path) -> Option<wiring::Marker> {
    let s = std::fs::read_to_string(cwd.join(wiring::MARKER_NAME)).ok()?;
    wiring::parse_marker(&s)
}

fn author_for_session(profile: &str, session_id: &str) -> Result<Option<Author>> {
    let (instances, _) = Storage::new_unwatched(profile)?.load_with_groups()?;
    Ok(instances
        .into_iter()
        .find(|i| i.id == session_id)
        .map(|i| Author {
            title: i.title,
            tool: i.tool,
            session_id: i.id,
        }))
}

fn group_from_instance(profile: &str, cwd: &Path) -> Result<Option<(String, Author)>> {
    let (instances, _) = Storage::new_unwatched(profile)?.load_with_groups()?;
    let cwd = cwd.to_string_lossy();
    Ok(instances
        .into_iter()
        .find(|i| !i.group_path.is_empty() && cwd.starts_with(i.project_path.as_str()))
        .map(|i| {
            (
                i.group_path.clone(),
                Author {
                    title: i.title,
                    tool: i.tool,
                    session_id: i.id,
                },
            )
        }))
}
