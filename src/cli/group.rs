//! `agent-of-empires group` subcommands implementation

use anyhow::{bail, Result};
use clap::{Args, Subcommand};
use serde::Serialize;

use crate::session::{GroupTree, Storage};

#[derive(Subcommand)]
pub enum GroupCommands {
    /// List all groups
    #[command(alias = "ls")]
    List(GroupListArgs),

    /// Create a new group
    Create(GroupCreateArgs),

    /// Delete a group
    Delete(GroupDeleteArgs),

    /// Move session to group
    Move(GroupMoveArgs),
}

#[derive(Args)]
pub struct GroupListArgs {
    /// Output as JSON
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
pub struct GroupCreateArgs {
    /// Group name
    name: String,

    /// Parent group for creating subgroups
    #[arg(long)]
    parent: Option<String>,
}

#[derive(Args)]
pub struct GroupDeleteArgs {
    /// Group name
    name: String,

    /// Force delete by moving sessions to default group
    #[arg(long)]
    force: bool,
}

#[derive(Args)]
pub struct GroupMoveArgs {
    /// Session ID or title
    identifier: String,

    /// Target group
    group: String,
}

#[derive(Serialize)]
struct GroupInfo {
    name: String,
    path: String,
    session_count: usize,
    children: Vec<String>,
}

#[tracing::instrument(target = "cli.session", skip_all, fields(profile = %profile))]
pub async fn run(profile: &str, command: GroupCommands) -> Result<()> {
    match command {
        GroupCommands::List(args) => list_groups(profile, args).await,
        GroupCommands::Create(args) => create_group(profile, args).await,
        GroupCommands::Delete(args) => delete_group(profile, args).await,
        GroupCommands::Move(args) => move_session(profile, args).await,
    }
}

async fn list_groups(profile: &str, args: GroupListArgs) -> Result<()> {
    let storage = Storage::new_unwatched(profile)?;
    let (instances, groups) = storage.load_with_groups()?;

    let group_tree = GroupTree::new_with_groups(&instances, &groups);

    if args.json {
        let group_list: Vec<GroupInfo> = group_tree
            .get_all_groups()
            .iter()
            .map(|g| {
                let session_count = instances.iter().filter(|i| i.group_path == g.path).count();
                GroupInfo {
                    name: g.name.clone(),
                    path: g.path.clone(),
                    session_count,
                    children: g.children.iter().map(|c| c.name.clone()).collect(),
                }
            })
            .collect();
        super::output::print_json(&group_list)?;
    } else {
        let all_groups = group_tree.get_all_groups();
        if all_groups.is_empty() {
            println!("No groups found.");
            println!("Create one with: aoe group create <name>");
            return Ok(());
        }

        println!("Groups:\n");
        for group in &all_groups {
            let session_count = instances
                .iter()
                .filter(|i| i.group_path == group.path)
                .count();
            let indent = group.path.matches('/').count();
            println!(
                "{}• {} ({} sessions)",
                "  ".repeat(indent),
                group.name,
                session_count
            );
        }
        println!("\nTotal: {} groups", all_groups.len());
    }

    Ok(())
}

async fn create_group(profile: &str, args: GroupCreateArgs) -> Result<()> {
    let storage = Storage::new_unwatched(profile)?;

    let name = args.name.trim().to_string();
    let group_path = if let Some(parent) = &args.parent {
        format!("{}/{}", parent.trim(), name)
    } else {
        name.clone()
    };

    storage.update(|instances, groups| {
        let mut group_tree = GroupTree::new_with_groups(instances, groups);
        if group_tree.group_exists(&group_path) {
            bail!("Group already exists: {}", group_path);
        }
        group_tree.create_group(&group_path);
        *groups = group_tree.get_all_groups();
        Ok(())
    })?;

    if let Err(e) = crate::session::pm_agent::ensure_pm_session(profile, &group_path) {
        tracing::warn!(target: "session.pm", "PM auto-create failed for group '{group_path}': {e}");
    }

    println!("✓ Created group: {}", group_path);
    Ok(())
}

async fn delete_group(profile: &str, args: GroupDeleteArgs) -> Result<()> {
    let storage = Storage::new_unwatched(profile)?;
    let name = args.name.trim().to_string();
    let force = args.force;

    let prefix = format!("{}/", name);
    let in_group = |g: &str| -> bool { g == name || g.starts_with(&prefix) };

    // The group is going away, so its PM is removed along with it (bypassing the
    // per-session delete refusal). Capture the scratch dirs to clean up after the
    // locked update returns.
    let mut pm_scratch_dirs: Vec<std::path::PathBuf> = Vec::new();

    let session_count = storage.update(|instances, groups| {
        let mut group_tree = GroupTree::new_with_groups(instances, groups);
        if !group_tree.group_exists(&name) {
            bail!("Group not found: {}", name);
        }

        // Worker count excludes the PM: the PM is the group's own agent, not a
        // user session, so it must not gate the --force prompt.
        let worker_count = instances
            .iter()
            .filter(|i| in_group(&i.group_path) && !i.is_project_manager())
            .count();

        if worker_count > 0 {
            if !force {
                bail!(
                    "Group '{}' contains {} sessions. Use --force to move them to default group.",
                    name,
                    worker_count
                );
            }

            for inst in instances.iter_mut() {
                if in_group(&inst.group_path) && !inst.is_project_manager() {
                    inst.group_path = String::new();
                }
            }
        }

        for pm in instances
            .iter()
            .filter(|i| in_group(&i.group_path) && i.is_project_manager())
        {
            if pm.scratch {
                pm_scratch_dirs.push(std::path::PathBuf::from(&pm.project_path));
            }
        }
        instances.retain(|i| !(in_group(&i.group_path) && i.is_project_manager()));

        group_tree.delete_group(&name);
        *groups = group_tree.get_all_groups();
        Ok(worker_count)
    })?;

    for dir in &pm_scratch_dirs {
        if crate::session::scratch::is_scratch_path(dir) {
            if let Err(e) = std::fs::remove_dir_all(dir) {
                tracing::warn!(target: "session.pm", "PM scratch cleanup failed for {}: {e}", dir.display());
            }
        }
    }

    println!("✓ Deleted group: {}", name);
    if force && session_count > 0 {
        println!("  Moved {} sessions to default group", session_count);
    }

    Ok(())
}

async fn move_session(profile: &str, args: GroupMoveArgs) -> Result<()> {
    let storage = Storage::new_unwatched(profile)?;
    let identifier = args.identifier.trim().to_string();
    let group = args.group.trim().to_string();

    let old_group = storage.update(|instances, groups| {
        let id = super::resolve_session(&identifier, instances)?.id.clone();
        let inst = instances
            .iter_mut()
            .find(|i| i.id == id)
            .expect("resolve_session returned an id that is no longer in instances");
        let old = inst.group_path.clone();
        inst.group_path = group.clone();

        if !group.is_empty() {
            let mut group_tree = GroupTree::new_with_groups(instances, groups);
            group_tree.create_group(&group);
            *groups = group_tree.get_all_groups();
        }
        Ok(old)
    })?;

    // Reconcile the moved session's group-context wiring (cwd is unchanged, so a
    // re-attach repoints it to the new group; detach when moved out of all groups).
    if let Ok((instances, _)) = storage.load_with_groups() {
        if let Ok(inst) = super::resolve_session(&identifier, &instances) {
            if inst.group_path.is_empty() {
                let _ = crate::session::group_context::detach_for_instance(inst);
            } else {
                let _ = crate::session::group_context::attach_for_instance(profile, inst);
            }
        }
    }

    if old_group.is_empty() {
        println!("✓ Moved session to group: {}", group);
    } else {
        println!("✓ Moved session from '{}' to '{}'", old_group, group);
    }

    Ok(())
}
