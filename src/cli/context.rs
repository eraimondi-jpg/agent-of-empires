//! `agent-of-empires context` subcommands: the universal, file-locked write
//! path into a group's shared `context.md`, plus reads and cross-group summary
//! discovery. See `src/session/group_context.rs`.

use std::path::Path;

use anyhow::{bail, Result};
use clap::{Args, Subcommand};

use crate::session::group_context::{self, wiring, Author};
use crate::session::{Instance, Storage};

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

/// Identify the writing session as reliably as possible. `AOE_INSTANCE_ID` is
/// set per agent and stays unique even when several sessions share one working
/// directory, so it is preferred; the cwd marker is the fallback.
fn writer(instances: &[Instance], cwd: &Path) -> Option<(String, Author)> {
    if let Ok(id) = std::env::var("AOE_INSTANCE_ID") {
        if !id.is_empty() {
            if let Some(i) = instances.iter().find(|i| i.id == id) {
                return Some(to_group_author(i));
            }
        }
    }
    let m = read_marker(cwd)?;
    instances
        .iter()
        .find(|i| i.id == m.session_id)
        .map(to_group_author)
}

/// A grouped session whose project path contains `cwd`. Ambiguous when several
/// sessions share a directory, so it is only a fallback after `writer`.
fn cwd_match(instances: &[Instance], cwd: &Path) -> Option<(String, Author)> {
    let cwd = cwd.to_string_lossy();
    instances
        .iter()
        .find(|i| !i.group_path.is_empty() && cwd.starts_with(i.project_path.as_str()))
        .map(to_group_author)
}

fn to_group_author(i: &Instance) -> (String, Author) {
    (
        i.group_path.clone(),
        Author {
            title: i.title.clone(),
            tool: i.tool.clone(),
            session_id: i.id.clone(),
        },
    )
}

/// Resolve only the group: explicit flag, else the writing session, else a cwd
/// match, else the marker's group.
pub fn resolve_group(profile: &str, explicit: Option<String>) -> Result<String> {
    if let Some(g) = explicit {
        return Ok(g);
    }
    let (instances, _) = Storage::new_unwatched(profile)?.load_with_groups()?;
    let cwd = std::env::current_dir()?;
    if let Some((g, _)) = writer(&instances, &cwd) {
        if !g.is_empty() {
            return Ok(g);
        }
    }
    if let Some((g, _)) = cwd_match(&instances, &cwd) {
        return Ok(g);
    }
    if let Some(m) = read_marker(&cwd) {
        if !m.group_path.is_empty() {
            return Ok(m.group_path);
        }
    }
    bail!("not inside a grouped aoe session; pass --group <path>");
}

fn resolve_group_and_author(
    profile: &str,
    explicit: Option<String>,
    cwd: &Path,
) -> Result<(String, Author)> {
    let (instances, _) = Storage::new_unwatched(profile)?.load_with_groups()?;
    let me = writer(&instances, cwd);
    let by_cwd = cwd_match(&instances, cwd);

    let author = me
        .as_ref()
        .map(|(_, a)| a.clone())
        .or_else(|| by_cwd.as_ref().map(|(_, a)| a.clone()));

    let group = explicit
        .or_else(|| {
            me.as_ref()
                .map(|(g, _)| g.clone())
                .filter(|g| !g.is_empty())
        })
        .or_else(|| by_cwd.as_ref().map(|(g, _)| g.clone()))
        .or_else(|| {
            read_marker(cwd)
                .map(|m| m.group_path)
                .filter(|g| !g.is_empty())
        });

    match group {
        Some(group) => Ok((
            group,
            author.unwrap_or_else(|| Author {
                title: "unknown".into(),
                tool: "unknown".into(),
                session_id: String::new(),
            }),
        )),
        None => bail!("not inside a grouped aoe session; pass --group <path>"),
    }
}

fn read_marker(cwd: &Path) -> Option<wiring::Marker> {
    let s = std::fs::read_to_string(cwd.join(wiring::MARKER_NAME)).ok()?;
    wiring::parse_marker(&s)
}
