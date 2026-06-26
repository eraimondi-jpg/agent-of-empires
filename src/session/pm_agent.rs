//! Per-group Project Manager (PM) agent.
//!
//! Each group gets one permanent PM: a scratch-backed session created DORMANT
//! (its record, instruction file, and permissions exist, but it is never
//! launched here) and marked non-removable. This module owns the creation and
//! idempotence; lifecycle hooks (`ensure_pm_session`) run after a group is
//! created, and the delete paths protect the PM. Launching, auto-revive, and
//! sidebar rendering land in later stages.

use std::fs;
use std::path::Path;

use anyhow::Result;
use serde_json::{json, Value};

use super::builder::{self, InstanceParams};
use super::group_context;
use super::{resolve_config_or_warn, GroupTree, Instance, Status, Storage};

/// PM instruction file body. The literal `{group}` placeholders are substituted
/// with the group path at write time via `.replace`; `format!` is unusable here
/// because the markdown contains other braces.
pub const PM_INSTRUCTIONS_TEMPLATE: &str = r#"# You are the Project Manager for the aoe group `{group}`

You report to the General Manager (the human). The other agents in this group are
your Workers. You are this group's single, permanent agent and hold three roles.

## 1. Curator, own the group's memory
- GROUP_CONTEXT.md is the group's shared working memory. Keep it clean and
  hierarchical; record durable findings with `aoe context add "<note>"`.
- Maintain summary.md (outward-facing): what the group does, key results, and a
  "who to ask" table with each Worker's stable session id and the literal
  `agent-chat ask <id> "..."`.

## 2. Contact point, answer for the group
- You are the group's agent-chat endpoint. Watch `agent-chat inbox`; answer
  questions about this group from the context. If you do not know, ask the right
  Worker (`agent-chat ask <id>`), then `agent-chat reply <msg_id>`.
- You may ask other groups' agents questions for information. You never command
  another group's agents.

## 3. Project Manager, execute the GM's directives
- Only act on a directive from the General Manager. Never start a project or spawn
  a Worker on your own initiative. With no directive, you only curate and answer.
- When the GM gives you a task, decompose it and run it to completion: spawn
  Workers into THIS group with `aoe add <dir> -t "<role>" -g {group} --tool claude --launch`
  (as many as needed; typically one to do the work and one to verify it), give each
  a clear written task, coordinate via `agent-chat`, monitor with `aoe list`, fold
  results into GROUP_CONTEXT.md, and report progress to the GM.

## Boundaries
- Stay in your group: only spawn/manage this group's Workers; never spawn into or
  direct another group's agents (ask them questions only).
- Don't destroy work: never delete the GM's or a Worker's files/sessions; check
  before anything irreversible.
- Keep the GM informed: report what you spawned, what is running, and results.
"#;

/// The group's PM, if one exists: the first instance marked `is_project_manager`
/// whose `group_path` matches.
pub fn pm_for_group<'a>(instances: &'a [Instance], group_path: &str) -> Option<&'a Instance> {
    instances
        .iter()
        .find(|i| i.is_project_manager() && i.group_path == group_path)
}

pub fn pm_exists(instances: &[Instance], group_path: &str) -> bool {
    pm_for_group(instances, group_path).is_some()
}

/// A PM that the user has started at least once (`pm_activated`) is a revive
/// candidate on startup. Liveness (is the tmux pane actually up) is checked by
/// the caller, which already holds the batched pane metadata; this predicate is
/// the PM-specific half of the gate, mirroring `recovery::is_recovery_candidate`
/// for normal sessions (which skips `Status::Stopped` PMs).
pub fn should_revive_pm(instance: &Instance) -> bool {
    instance.is_project_manager() && instance.pm_activated
}

/// Activated PMs in `instances`, in iteration order. Pure selector for the
/// startup revive pass and its unit test; the caller filters out the ones whose
/// pane is already live.
pub fn activated_pms(instances: &[Instance]) -> Vec<&Instance> {
    instances.iter().filter(|i| should_revive_pm(i)).collect()
}

/// Whether a status means the session is launched and can accept input now: a
/// running, waiting, or idle agent has a live pane. Dormant (`Stopped`),
/// mid-lifecycle (`Starting`/`Creating`/`Deleting`), or failed (`Error`)
/// sessions are not pokeable, and `Unknown` is treated as not-live so the
/// caller falls back to the headless one-shot rather than poking a corpse.
fn is_live_status(status: Status) -> bool {
    matches!(status, Status::Running | Status::Waiting | Status::Idle)
}

/// The group's PM if it exists AND its pane is live (see [`is_live_status`]).
/// A dormant `Status::Stopped` PM yields `None`, which routes periodic curation
/// to the headless one-shot instead of poking an unlaunched agent.
pub fn live_pm_for_group<'a>(instances: &'a [Instance], group_path: &str) -> Option<&'a Instance> {
    pm_for_group(instances, group_path).filter(|pm| is_live_status(pm.status))
}

/// Curate instruction the live PM receives so it, not the headless one-shot,
/// refreshes its group's memory on the periodic tick.
const PM_CURATE_POKE: &str = "Curate now: refresh GROUP_CONTEXT.md and summary.md for your group, dedupe and keep durable facts, update the who-to-ask table, and surface OPEN items.";

/// Send the live PM a fire-and-forget curate turn over its tmux pane, reusing
/// the same internal send path as `aoe send` (no shelling out). Best-effort:
/// the caller treats a missing/unstartable pane as "fall back to the headless
/// one-shot", so we do not revive here, just deliver if the pane is up.
pub fn poke_pm_to_curate(pm: &Instance) -> Result<()> {
    let session = crate::tmux::Session::new(&pm.id, &pm.title)?;
    if !session.exists() {
        anyhow::bail!("PM pane is not running");
    }
    let delay = crate::agents::send_keys_enter_delay(&pm.tool);
    session.send_keys_with_delay(PM_CURATE_POKE, delay)?;
    // Stamp the curator state now so the auto-trigger does not re-poke this PM on
    // every poll tick; the PM does the actual rewrite in its pane.
    if let Err(e) =
        group_context::mark_curation_started(&pm.effective_profile(), pm.group_path.trim())
    {
        tracing::warn!(target: "curator", group = %pm.group_path, "mark curation started failed: {e}");
    }
    Ok(())
}

/// Leaf name of a `/`-delimited group path, used for the PM's title.
fn group_leaf(group_path: &str) -> &str {
    group_path.rsplit('/').next().unwrap_or(group_path)
}

/// Merge a `permissions.allow` allowlist into the PM cwd's
/// `.claude/settings.local.json`, preserving any pre-existing JSON. Mirrors the
/// merge approach in `group_context::install_capture_hook`: read-or-default,
/// ensure the nested object/array shapes, and only add entries that are missing.
fn install_pm_permissions(cwd: &Path, allow_entries: &[&str]) -> Result<()> {
    let path = cwd.join(".claude").join("settings.local.json");
    let mut root: Value = match fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|_| json!({})),
        Err(_) => json!({}),
    };
    if !root.is_object() {
        root = json!({});
    }

    let obj = root.as_object_mut().expect("root is an object");
    let perms = obj.entry("permissions").or_insert_with(|| json!({}));
    if !perms.is_object() {
        *perms = json!({});
    }
    let perms_obj = perms.as_object_mut().expect("permissions is an object");
    let allow = perms_obj.entry("allow").or_insert_with(|| json!([]));
    if !allow.is_array() {
        *allow = json!([]);
    }
    let allow_arr = allow.as_array_mut().expect("allow is an array");
    for entry in allow_entries {
        let present = allow_arr.iter().any(|v| v.as_str() == Some(*entry));
        if !present {
            allow_arr.push(json!(entry));
        }
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, serde_json::to_string_pretty(&root)?)?;
    Ok(())
}

/// Allowlist entries the PM needs to drive `aoe` and `agent-chat` without
/// per-call approval prompts. Claude Code's `Bash(<prefix>:*)` shape.
const PM_ALLOW_ENTRIES: &[&str] = &["Bash(aoe:*)", "Bash(agent-chat:*)"];

/// Best-effort filesystem wiring for a freshly built PM instance: instruction
/// file, permissions allowlist, and group-context attach. Failures warn and are
/// swallowed so a fs hiccup never fails the group create.
fn wire_pm_cwd(profile: &str, instance: &Instance, group_path: &str) {
    let cwd = Path::new(&instance.project_path);
    let claude_md = cwd.join("CLAUDE.md");
    let body = PM_INSTRUCTIONS_TEMPLATE.replace("{group}", group_path);
    if let Err(e) = fs::write(&claude_md, body) {
        tracing::warn!(target: "session.pm", "PM CLAUDE.md write failed: {e}");
    }
    if let Err(e) = install_pm_permissions(cwd, PM_ALLOW_ENTRIES) {
        tracing::warn!(target: "session.pm", "PM permissions write failed: {e}");
    }
    if let Err(e) = group_context::attach_for_instance(profile, instance) {
        tracing::warn!(target: "session.pm", "PM group-context attach failed: {e}");
    }
}

/// Ensure the group has its permanent PM session, creating it dormant if absent.
/// Idempotent: a second call on a group that already has a PM is a no-op.
/// Returns the new PM's id when one was created, `None` when the feature is
/// disabled or a PM already exists.
pub fn ensure_pm_session(profile: &str, group_path: &str) -> Result<Option<String>> {
    if !resolve_config_or_warn(profile).project_manager.enabled {
        return Ok(None);
    }

    let storage = Storage::new_unwatched(profile)?;
    let (instances, _groups) = storage.load_with_groups()?;
    if pm_exists(&instances, group_path) {
        return Ok(None);
    }

    let params = InstanceParams {
        title: format!("PM - {}", group_leaf(group_path)),
        path: String::new(),
        group: group_path.to_string(),
        tool: "claude".to_string(),
        worktree_enabled: false,
        worktree_branch: None,
        create_new_branch: false,
        base_branch: None,
        sandbox: false,
        sandbox_image: String::new(),
        yolo_mode: false,
        extra_env: Vec::new(),
        extra_args: String::new(),
        command_override: String::new(),
        extra_repo_paths: Vec::new(),
        scratch: true,
    };

    let title_refs: Vec<&str> = instances.iter().map(|i| i.title.as_str()).collect();
    let mut instance = builder::build_instance(params, &title_refs, &[], profile)?.instance;
    instance.is_project_manager = true;
    instance.source_profile = profile.to_string();
    // Dormant: the record exists but the agent is not launched here.
    instance.status = Status::Stopped;

    let id = instance.id.clone();
    storage.update(|all_instances, groups| {
        all_instances.push(instance.clone());
        let mut tree = GroupTree::new_with_groups(all_instances, groups);
        tree.create_group(group_path);
        *groups = tree.get_all_groups();
        Ok(())
    })?;

    wire_pm_cwd(profile, &instance, group_path);
    Ok(Some(id))
}

/// Ensure every group in `profile` has a PM. Backfills groups created before the
/// PM feature existed (the create-time hook does not fire for already-existing
/// groups). Idempotent: `ensure_pm_session` skips groups that already have one,
/// and the whole thing is a no-op when `project_manager.enabled` is false.
pub fn backfill_pms(profile: &str) -> Result<()> {
    let (_instances, groups) = Storage::new_unwatched(profile)?.load_with_groups()?;
    for g in &groups {
        if g.path.trim().is_empty() {
            continue;
        }
        if let Err(e) = ensure_pm_session(profile, &g.path) {
            tracing::warn!(target: "pm", "backfill_pms: {}: {e}", g.path);
        }
    }
    Ok(())
}

/// Best-effort PM backfill across all profiles, run once at startup so existing
/// groups get their PM without needing a fresh group-create.
pub fn backfill_pms_all_profiles() {
    let Ok(profiles) = super::list_profiles() else {
        return;
    };
    for p in profiles {
        let _ = backfill_pms(&p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn temp_home() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", tmp.path());
        std::env::set_var("XDG_CONFIG_HOME", tmp.path().join(".config"));
        tmp
    }

    fn write_config(disabled: bool) {
        let app_dir = super::super::get_app_dir().unwrap();
        std::fs::create_dir_all(&app_dir).unwrap();
        if disabled {
            std::fs::write(
                app_dir.join("config.toml"),
                "[project_manager]\nenabled = false\n",
            )
            .unwrap();
        }
    }

    #[test]
    #[serial]
    fn creates_one_pm_and_is_idempotent() {
        let _tmp = temp_home();
        write_config(false);

        let first = ensure_pm_session("default", "work/sysid").unwrap();
        assert!(first.is_some(), "first call must create a PM");

        let second = ensure_pm_session("default", "work/sysid").unwrap();
        assert!(second.is_none(), "second call must be a no-op");

        let storage = Storage::new_unwatched("default").unwrap();
        let (instances, _g) = storage.load_with_groups().unwrap();
        let pms: Vec<_> = instances
            .iter()
            .filter(|i| i.is_project_manager() && i.group_path == "work/sysid")
            .collect();
        assert_eq!(pms.len(), 1, "exactly one PM per group");
        let pm = pms[0];
        assert_eq!(pm.status, Status::Stopped, "PM is created dormant");
        assert!(pm.scratch, "PM is scratch-backed");
        assert!(pm.title.contains("sysid"));
    }

    #[test]
    #[serial]
    fn pm_cwd_has_instructions_and_permissions() {
        let _tmp = temp_home();
        write_config(false);

        ensure_pm_session("default", "alpha").unwrap();
        let storage = Storage::new_unwatched("default").unwrap();
        let (instances, _g) = storage.load_with_groups().unwrap();
        let pm = pm_for_group(&instances, "alpha").expect("PM exists");

        let cwd = Path::new(&pm.project_path);
        let claude = std::fs::read_to_string(cwd.join("CLAUDE.md")).unwrap();
        assert!(claude.contains("Project Manager for the aoe group `alpha`"));
        assert!(claude.contains("aoe add <dir>"));

        let settings: Value = serde_json::from_str(
            &std::fs::read_to_string(cwd.join(".claude").join("settings.local.json")).unwrap(),
        )
        .unwrap();
        let allow = settings["permissions"]["allow"].as_array().unwrap();
        let allow: Vec<&str> = allow.iter().filter_map(|v| v.as_str()).collect();
        assert!(allow.contains(&"Bash(aoe:*)"));
        assert!(allow.contains(&"Bash(agent-chat:*)"));
    }

    #[test]
    #[serial]
    fn disabled_config_creates_no_pm() {
        let _tmp = temp_home();
        write_config(true);

        let created = ensure_pm_session("default", "g1").unwrap();
        assert!(created.is_none(), "disabled config must not create a PM");

        let storage = Storage::new_unwatched("default").unwrap();
        let (instances, _g) = storage.load_with_groups().unwrap();
        assert!(!pm_exists(&instances, "g1"));
    }

    #[test]
    fn revive_selector_picks_only_activated_pms() {
        let mut normal = Instance::new("worker", "/tmp/w");
        normal.group_path = "g".to_string();

        let mut dormant_pm = Instance::new("PM - g", "/tmp/pm1");
        dormant_pm.group_path = "g".to_string();
        dormant_pm.is_project_manager = true;
        // pm_activated stays false: never started, so it must NOT be revived.

        let mut activated_pm = Instance::new("PM - h", "/tmp/pm2");
        activated_pm.group_path = "h".to_string();
        activated_pm.is_project_manager = true;
        activated_pm.pm_activated = true;

        let instances = vec![normal, dormant_pm, activated_pm.clone()];
        let revivable = activated_pms(&instances);
        assert_eq!(revivable.len(), 1, "only the activated PM is revivable");
        assert_eq!(revivable[0].id, activated_pm.id);
        assert!(should_revive_pm(&activated_pm));
    }

    #[test]
    fn live_pm_selector_requires_a_live_status() {
        let mut worker = Instance::new("worker", "/tmp/w");
        worker.group_path = "g".to_string();
        worker.status = Status::Idle;

        let mut dormant_pm = Instance::new("PM - g", "/tmp/pm");
        dormant_pm.group_path = "g".to_string();
        dormant_pm.is_project_manager = true;
        dormant_pm.status = Status::Stopped;

        // A dormant (Stopped) PM is not live: route to the headless one-shot.
        let instances = vec![worker.clone(), dormant_pm.clone()];
        assert!(live_pm_for_group(&instances, "g").is_none());

        // Flip the PM to a live status and it is selected.
        for live in [Status::Idle, Status::Running, Status::Waiting] {
            let mut live_pm = dormant_pm.clone();
            live_pm.status = live;
            let instances = vec![worker.clone(), live_pm.clone()];
            let picked = live_pm_for_group(&instances, "g").expect("live PM is selected");
            assert_eq!(picked.id, live_pm.id);
            assert!(is_live_status(live));
        }

        // Mid-lifecycle / failed statuses are not pokeable.
        for not_live in [
            Status::Starting,
            Status::Creating,
            Status::Error,
            Status::Unknown,
        ] {
            let mut pm = dormant_pm.clone();
            pm.status = not_live;
            let instances = vec![pm];
            assert!(
                live_pm_for_group(&instances, "g").is_none(),
                "{not_live:?} must not be live"
            );
            assert!(!is_live_status(not_live));
        }
    }

    #[test]
    fn live_pm_routes_to_poke_only_when_a_live_pm_exists() {
        // Group with a live PM is routed to poke (Some); a group whose only PM
        // is dormant falls through to the headless one-shot (None). This mirrors
        // the branch both auto-curate tick sites take.
        let mut live_pm = Instance::new("PM - has", "/tmp/p1");
        live_pm.group_path = "has".to_string();
        live_pm.is_project_manager = true;
        live_pm.status = Status::Running;

        let mut dormant_pm = Instance::new("PM - none", "/tmp/p2");
        dormant_pm.group_path = "none".to_string();
        dormant_pm.is_project_manager = true;
        dormant_pm.status = Status::Stopped;

        let instances = vec![live_pm, dormant_pm];
        assert!(
            live_pm_for_group(&instances, "has").is_some(),
            "group with a live PM is poked"
        );
        assert!(
            live_pm_for_group(&instances, "none").is_none(),
            "group with only a dormant PM uses the headless one-shot"
        );
    }

    #[test]
    fn install_permissions_merges_and_dedupes() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path();
        std::fs::create_dir_all(cwd.join(".claude")).unwrap();
        std::fs::write(
            cwd.join(".claude").join("settings.local.json"),
            r#"{"permissions":{"allow":["Bash(aoe:*)"]},"otherKey":1}"#,
        )
        .unwrap();

        install_pm_permissions(cwd, PM_ALLOW_ENTRIES).unwrap();
        let v: Value = serde_json::from_str(
            &std::fs::read_to_string(cwd.join(".claude").join("settings.local.json")).unwrap(),
        )
        .unwrap();
        let allow: Vec<&str> = v["permissions"]["allow"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e.as_str())
            .collect();
        assert_eq!(allow.iter().filter(|e| **e == "Bash(aoe:*)").count(), 1);
        assert!(allow.contains(&"Bash(agent-chat:*)"));
        assert_eq!(v["otherKey"], json!(1));
    }
}
