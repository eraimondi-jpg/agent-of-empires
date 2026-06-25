//! Per-group shared context files: an append-only `context.md` every group
//! member reads and updates, and an outward-facing `summary.md`. Curation of
//! both is the L2 curator's job; L1 only provisions and appends.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

use super::get_profile_dir;
use super::Instance;

pub const SUMMARY_PLACEHOLDER: &str =
    "<!-- maintained by the group curator (L2); empty until then -->";

const LOCK_FILENAME: &str = ".context.lock";
const STATE_FILENAME: &str = ".curator.json";

pub struct GroupContextPaths {
    pub dir: PathBuf,
    pub context: PathBuf,
    pub summary: PathBuf,
    pub lock: PathBuf,
    pub state: PathBuf,
}

#[derive(Clone)]
pub struct Author {
    pub title: String,
    pub tool: String,
    pub session_id: String,
}

pub struct GroupSummary {
    pub group_path: String,
    pub digest: String,
}

/// Validate a group path and turn it into a relative on-disk subpath under
/// `groups/`. Rejects empty paths and any `.`/`..`/separator-bearing component.
fn group_subdir(group_path: &str) -> Result<PathBuf> {
    if group_path.trim().is_empty() {
        bail!("empty group path");
    }
    let mut out = PathBuf::new();
    for comp in group_path.split('/') {
        if comp.is_empty() || comp == "." || comp == ".." || comp.contains('\\') {
            bail!("invalid group path component: {comp:?}");
        }
        out.push(comp);
    }
    Ok(out)
}

pub fn paths_for(profile: &str, group_path: &str) -> Result<GroupContextPaths> {
    let dir = get_profile_dir(profile)?
        .join("groups")
        .join(group_subdir(group_path)?);
    Ok(GroupContextPaths {
        context: dir.join("context.md"),
        summary: dir.join("summary.md"),
        lock: dir.join(LOCK_FILENAME),
        state: dir.join(STATE_FILENAME),
        dir,
    })
}

pub fn ensure_files(profile: &str, group_path: &str) -> Result<GroupContextPaths> {
    let paths = paths_for(profile, group_path)?;
    fs::create_dir_all(&paths.dir)
        .with_context(|| format!("creating group context dir {}", paths.dir.display()))?;
    if !paths.context.exists() {
        fs::write(&paths.context, "")?;
    }
    if !paths.summary.exists() {
        fs::write(&paths.summary, format!("{SUMMARY_PLACEHOLDER}\n"))?;
    }
    Ok(paths)
}

/// RAII guard for the per-group advisory flock. Mirrors `storage.rs::StorageFlock`:
/// the kernel releases the lock on process exit, and `Drop` releases it on a
/// normal or panicking unwind.
struct ContextFlock {
    file: fs::File,
}

impl Drop for ContextFlock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

fn acquire_lock(lock_path: &Path) -> Result<ContextFlock> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(ContextFlock { file }),
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

fn short_id(id: &str) -> &str {
    let n = id.len().min(8);
    &id[..n]
}

pub fn append_entry(profile: &str, group_path: &str, author: &Author, text: &str) -> Result<()> {
    let paths = ensure_files(profile, group_path)?;
    let _guard = acquire_lock(&paths.lock)?;
    let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%MZ");
    let entry = format!(
        "\n## {ts} {title} ({tool}, {sid})\n{body}\n",
        title = author.title,
        tool = author.tool,
        sid = short_id(&author.session_id),
        body = text.trim_end(),
    );
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.context)?;
    f.write_all(entry.as_bytes())?;
    Ok(())
}

pub fn read_context(profile: &str, group_path: &str) -> Result<String> {
    let paths = paths_for(profile, group_path)?;
    Ok(fs::read_to_string(&paths.context).unwrap_or_default())
}

/// Build the delimited section prepended to a grouped session's first prompt at
/// launch (Approach 2), complementing the persistent CLAUDE.md pointer (L1) so
/// the agent reliably starts with the shared context already in its window.
/// Returns `None` when the context is empty (or whitespace only), so an empty
/// group never pollutes the first prompt.
pub fn compose_launch_injection(group_path: &str, context: &str) -> Option<String> {
    if context.trim().is_empty() {
        return None;
    }
    Some(format!(
        "# Shared group context (group: {group_path})\n\
         This is your group's shared working memory. Read it before starting; \
         record durable findings with `aoe context add`.\n\n\
         {context}\n\n---\n\n"
    ))
}

/// Overwrite `context.md` wholesale under the same advisory flock `append_entry`
/// uses, so a TUI edit never races a concurrent append. The write is atomic: we
/// stage a temp file in the group dir and rename it into place.
pub fn write_context(profile: &str, group_path: &str, contents: &str) -> Result<()> {
    let paths = ensure_files(profile, group_path)?;
    let _guard = acquire_lock(&paths.lock)?;
    let tmp = paths.context.with_extension("md.tmp");
    fs::write(&tmp, contents)?;
    fs::rename(&tmp, &paths.context)?;
    Ok(())
}

/// Persisted curator bookkeeping, stored as JSON at `<group dir>/.curator.json`.
/// `last_size` is the byte length `context.md` had right after the last
/// curation, so the change-gate can tell whether agents have appended since.
#[derive(Serialize, Deserialize, Clone)]
pub struct CuratorState {
    pub last_run_at: chrono::DateTime<chrono::Utc>,
    pub last_size: u64,
}

/// Read the persisted curator state, or `None` when it has never run (missing
/// file) or the file is unreadable/unparseable. A corrupt state file degrades to
/// "treat as never curated" rather than failing the caller.
pub fn read_curator_state(profile: &str, group_path: &str) -> Result<Option<CuratorState>> {
    let paths = paths_for(profile, group_path)?;
    let Ok(raw) = fs::read_to_string(&paths.state) else {
        return Ok(None);
    };
    Ok(serde_json::from_str(&raw).ok())
}

fn write_curator_state(paths: &GroupContextPaths, state: &CuratorState) -> Result<()> {
    let tmp = paths.state.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(state)?)?;
    fs::rename(&tmp, &paths.state)?;
    Ok(())
}

/// Take a snapshot of `context.md` for curation: returns its current content and
/// byte length. The flock is held only for the read and dropped before
/// returning, because the slow LLM call happens in the caller between this and
/// `commit_curation`; the lock must never span it.
pub fn snapshot_for_curation(profile: &str, group_path: &str) -> Result<(String, u64)> {
    let paths = ensure_files(profile, group_path)?;
    let _guard = acquire_lock(&paths.lock)?;
    let content = fs::read_to_string(&paths.context).unwrap_or_default();
    let len = content.len() as u64;
    Ok((content, len))
}

/// Commit a curated rewrite of `context.md` losslessly. `snapshot_len` is the
/// byte length from the matching `snapshot_for_curation`; any bytes past it are
/// agent appends that landed during the (unlocked) LLM call and are preserved
/// verbatim by appending them after the cleaned text. Both files are written
/// atomically and the curator state is refreshed, all under the flock.
pub fn commit_curation(
    profile: &str,
    group_path: &str,
    cleaned_context: &str,
    summary: &str,
    snapshot_len: u64,
) -> Result<()> {
    let paths = ensure_files(profile, group_path)?;
    let _guard = acquire_lock(&paths.lock)?;

    let current = fs::read(&paths.context).unwrap_or_default();
    let snap = snapshot_len as usize;
    let tail: &[u8] = if current.len() > snap {
        &current[snap..]
    } else {
        &[]
    };

    let mut new_context = cleaned_context.trim_end_matches('\n').as_bytes().to_vec();
    new_context.push(b'\n');
    new_context.extend_from_slice(tail);

    let tmp_ctx = paths.context.with_extension("md.tmp");
    fs::write(&tmp_ctx, &new_context)?;
    fs::rename(&tmp_ctx, &paths.context)?;

    let tmp_sum = paths.summary.with_extension("md.tmp");
    fs::write(&tmp_sum, summary)?;
    fs::rename(&tmp_sum, &paths.summary)?;

    let state = CuratorState {
        last_run_at: chrono::Utc::now(),
        last_size: new_context.len() as u64,
    };
    write_curator_state(&paths, &state)?;
    Ok(())
}

/// Whether `context.md` changed since the last curation: true when no state
/// exists yet, or the current byte size differs from the recorded `last_size`.
/// Feeds the auto-trigger change-gate.
pub fn context_grew_since_last_curation(profile: &str, group_path: &str) -> Result<bool> {
    let Some(state) = read_curator_state(profile, group_path)? else {
        return Ok(true);
    };
    let paths = paths_for(profile, group_path)?;
    let size = fs::metadata(&paths.context).map(|m| m.len()).unwrap_or(0);
    Ok(size != state.last_size)
}

pub fn read_summary(profile: &str, group_path: &str) -> Result<String> {
    let paths = paths_for(profile, group_path)?;
    Ok(fs::read_to_string(&paths.summary).unwrap_or_default())
}

pub fn list_summaries(profile: &str) -> Result<Vec<GroupSummary>> {
    let root = get_profile_dir(profile)?.join("groups");
    let mut out = Vec::new();
    if !root.exists() {
        return Ok(out);
    }
    let mut stack = vec![(root, String::new())];
    while let Some((dir, rel)) = stack.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            let child_rel = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            let summary = entry.path().join("summary.md");
            if summary.exists() {
                let digest = fs::read_to_string(&summary)
                    .unwrap_or_default()
                    .lines()
                    .find(|l| !l.trim().is_empty() && !l.trim_start().starts_with("<!--"))
                    .unwrap_or("(no summary yet)")
                    .trim()
                    .to_string();
                out.push(GroupSummary {
                    group_path: child_rel.clone(),
                    digest,
                });
            }
            stack.push((entry.path(), child_rel));
        }
    }
    Ok(out)
}

#[cfg(unix)]
fn make_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(not(unix))]
fn make_symlink(_target: &Path, _link: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "symlinks not supported on this platform",
    ))
}

/// Append names to `.git/info/exclude` (not the tracked `.gitignore`) so the
/// surfaced files never show up in the user's status. No-op outside a git dir.
fn add_git_excludes(cwd: &Path, names: &[&str]) {
    let exclude = cwd.join(".git").join("info").join("exclude");
    if !exclude.parent().map(|p| p.is_dir()).unwrap_or(false) {
        return;
    }
    let mut content = std::fs::read_to_string(&exclude).unwrap_or_default();
    let mut changed = false;
    for n in names {
        if !content.lines().any(|l| l.trim() == *n) {
            if !content.is_empty() && !content.ends_with('\n') {
                content.push('\n');
            }
            content.push_str(n);
            content.push('\n');
            changed = true;
        }
    }
    if changed {
        let _ = std::fs::write(&exclude, content);
    }
}

/// Surface the group's context into a session's working dir and make the agent
/// persistently aware of it (Approach 1). Best-effort: a failure here logs a
/// warning and never blocks a session launch.
pub fn attach(
    profile: &str,
    group_path: &str,
    project_path: &str,
    tool: &str,
    session_id: &str,
) -> Result<()> {
    if group_path.is_empty() {
        return Ok(());
    }
    let paths = ensure_files(profile, group_path)?;
    let cwd = Path::new(project_path);
    if !cwd.is_dir() {
        return Ok(());
    }

    // 1. Symlink context.md into cwd; never clobber a real file of that name.
    let link = cwd.join(wiring::CONTEXT_LINK_NAME);
    match std::fs::symlink_metadata(&link) {
        Ok(meta) if meta.file_type().is_symlink() => {
            let _ = std::fs::remove_file(&link);
            if let Err(e) = make_symlink(&paths.context, &link) {
                tracing::warn!("group-context: relink failed: {e}");
            }
        }
        Ok(_) => tracing::warn!(
            "group-context: {} exists and is not our symlink; leaving it",
            link.display()
        ),
        Err(_) => {
            if let Err(e) = make_symlink(&paths.context, &link) {
                tracing::warn!("group-context: symlink failed: {e}");
            }
        }
    }

    // 2. Marker (carries group + session id for `aoe context add`). Write only
    // when it changed, so repeated reconcile calls are cheap and quiet.
    let marker_path = cwd.join(wiring::MARKER_NAME);
    let marker = wiring::marker_contents(profile, group_path, session_id);
    if std::fs::read_to_string(&marker_path).ok().as_deref() != Some(marker.as_str()) {
        if let Err(e) = std::fs::write(&marker_path, marker) {
            tracing::warn!("group-context: marker write failed: {e}");
        }
    }

    // 3. Keep the surfaced files out of git status.
    add_git_excludes(cwd, &[wiring::CONTEXT_LINK_NAME, wiring::MARKER_NAME]);

    // 4. Managed block in the tool's always-loaded instruction file.
    if let Some(fname) = wiring::instruction_filename_for_tool(tool) {
        let ifile = cwd.join(fname);
        let existing = std::fs::read_to_string(&ifile).unwrap_or_default();
        let block = wiring::render_managed_block(group_path, wiring::CONTEXT_LINK_NAME);
        let updated = wiring::upsert_block(&existing, &block);
        if updated != existing {
            if let Err(e) = std::fs::write(&ifile, updated) {
                tracing::warn!("group-context: instruction-file write failed: {e}");
            }
        }
    }
    Ok(())
}

/// Remove the wiring `attach` added. Best-effort.
pub fn detach(project_path: &str, tool: &str) -> Result<()> {
    let cwd = Path::new(project_path);
    if !cwd.is_dir() {
        return Ok(());
    }
    let link = cwd.join(wiring::CONTEXT_LINK_NAME);
    if let Ok(meta) = std::fs::symlink_metadata(&link) {
        if meta.file_type().is_symlink() {
            let _ = std::fs::remove_file(&link);
        }
    }
    let _ = std::fs::remove_file(cwd.join(wiring::MARKER_NAME));
    if let Some(fname) = wiring::instruction_filename_for_tool(tool) {
        let ifile = cwd.join(fname);
        if let Ok(existing) = std::fs::read_to_string(&ifile) {
            let cleaned = wiring::remove_block(&existing);
            if cleaned != existing {
                let _ = std::fs::write(&ifile, cleaned);
            }
        }
    }
    Ok(())
}

pub fn attach_for_instance(profile: &str, inst: &Instance) -> Result<()> {
    attach(
        profile,
        &inst.group_path,
        &inst.project_path,
        &inst.tool,
        &inst.id,
    )
}

pub fn detach_for_instance(inst: &Instance) -> Result<()> {
    detach(&inst.project_path, &inst.tool)
}

/// Idempotently (re)attach every grouped instance to its group context. Cheap
/// to call repeatedly thanks to the write-if-changed guards in `attach`; used as
/// the TUI's post-mutation reconcile so create and move are both covered without
/// a per-operation hook.
pub fn reconcile_all(instances: &[Instance]) {
    for inst in instances {
        if inst.group_path.is_empty() {
            continue;
        }
        let _ = attach(
            &inst.source_profile,
            &inst.group_path,
            &inst.project_path,
            &inst.tool,
            &inst.id,
        );
    }
}

#[cfg(test)]
mod attach_tests {
    use super::wiring;
    use serial_test::serial;

    #[test]
    #[serial]
    fn attach_creates_wiring_and_detach_removes_it() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_CONFIG_HOME", tmp.path());
        std::env::set_var("HOME", tmp.path());
        let work = tmp.path().join("repo");
        std::fs::create_dir_all(work.join(".git").join("info")).unwrap();
        let work_s = work.to_str().unwrap();

        super::attach("default", "g1", work_s, "claude", "sess1234").unwrap();
        assert!(
            std::fs::symlink_metadata(work.join(wiring::CONTEXT_LINK_NAME))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(work.join(wiring::MARKER_NAME).exists());
        let claude = std::fs::read_to_string(work.join("CLAUDE.md")).unwrap();
        assert!(claude.contains(wiring::BLOCK_START));
        let exclude = std::fs::read_to_string(work.join(".git/info/exclude")).unwrap();
        assert!(exclude.contains(wiring::CONTEXT_LINK_NAME));

        super::detach(work_s, "claude").unwrap();
        assert!(!work.join(wiring::MARKER_NAME).exists());
        let claude2 = std::fs::read_to_string(work.join("CLAUDE.md")).unwrap();
        assert!(!claude2.contains(wiring::BLOCK_START));
    }

    #[test]
    #[serial]
    fn attach_skips_existing_real_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_CONFIG_HOME", tmp.path());
        std::env::set_var("HOME", tmp.path());
        let work = tmp.path().join("repo2");
        std::fs::create_dir_all(&work).unwrap();
        let real = work.join(wiring::CONTEXT_LINK_NAME);
        std::fs::write(&real, "i am a real file").unwrap();

        super::attach("default", "g1", work.to_str().unwrap(), "claude", "s1").unwrap();
        // The real file is preserved, not replaced by a symlink.
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "i am a real file");
        assert!(!std::fs::symlink_metadata(&real)
            .unwrap()
            .file_type()
            .is_symlink());
    }
}

/// Approach-1 awareness wiring: pure helpers for the managed instruction block,
/// the per-tool instruction filename, and the cwd marker. The filesystem side
/// (`attach_for_instance`/`detach_for_instance`) builds on these.
pub mod wiring {
    use serde::{Deserialize, Serialize};

    pub const BLOCK_START: &str = "<!-- aoe:group-context:start -->";
    pub const BLOCK_END: &str = "<!-- aoe:group-context:end -->";
    pub const CONTEXT_LINK_NAME: &str = "GROUP_CONTEXT.md";
    pub const MARKER_NAME: &str = ".aoe-group";

    /// The always-loaded instruction file each agent tool reads, or `None` when
    /// the tool has no known convention (then we skip the managed block).
    pub fn instruction_filename_for_tool(tool: &str) -> Option<&'static str> {
        match tool {
            "claude" | "claude-agent-acp" | "aoe-agent" => Some("CLAUDE.md"),
            "codex" => Some("AGENTS.md"),
            "gemini" => Some("GEMINI.md"),
            _ => None,
        }
    }

    pub fn render_managed_block(group_path: &str, context_filename: &str) -> String {
        format!(
            "{start}
This session belongs to the aoe group `{group}`. The group shares a context file.

- Read `./{ctx}` before you start; it is the group's shared working memory.
- After each meaningful step or finding, immediately record a one-line note with `aoe context add \"<finding>\"`. Do this proactively, without being asked. Your session name is attached automatically, so the group and the curator know who found what.
- Never hand-edit `{ctx}`; always append via `aoe context add` (concurrent hand-edits are lost).
- To see what other groups know, run `aoe context summaries`.
{end}",
            start = BLOCK_START,
            end = BLOCK_END,
            group = group_path,
            ctx = context_filename,
        )
    }

    /// Insert the managed block, or replace an existing one in place. Idempotent:
    /// running it twice yields a single block and never touches user content
    /// outside the markers.
    pub fn upsert_block(existing: &str, block: &str) -> String {
        if let (Some(s), Some(e)) = (existing.find(BLOCK_START), existing.find(BLOCK_END)) {
            let end = e + BLOCK_END.len();
            let mut out = String::with_capacity(existing.len());
            out.push_str(&existing[..s]);
            out.push_str(block);
            out.push_str(&existing[end..]);
            return out;
        }
        let sep = if existing.is_empty() || existing.ends_with('\n') {
            ""
        } else {
            "\n"
        };
        format!("{existing}{sep}\n{block}\n")
    }

    pub fn remove_block(existing: &str) -> String {
        if let (Some(s), Some(e)) = (existing.find(BLOCK_START), existing.find(BLOCK_END)) {
            let end = e + BLOCK_END.len();
            let mut out = String::new();
            out.push_str(existing[..s].trim_end_matches('\n'));
            out.push_str(&existing[end..]);
            return out;
        }
        existing.to_string()
    }

    #[derive(Serialize, Deserialize)]
    pub struct Marker {
        pub profile: String,
        pub group_path: String,
        pub session_id: String,
    }

    pub fn marker_contents(profile: &str, group_path: &str, session_id: &str) -> String {
        serde_json::to_string_pretty(&Marker {
            profile: profile.to_string(),
            group_path: group_path.to_string(),
            session_id: session_id.to_string(),
        })
        .unwrap_or_default()
    }

    pub fn parse_marker(s: &str) -> Option<Marker> {
        serde_json::from_str(s).ok()
    }
}

#[cfg(test)]
mod wiring_tests {
    use super::wiring::*;

    #[test]
    fn tool_map_known_and_unknown() {
        assert_eq!(instruction_filename_for_tool("claude"), Some("CLAUDE.md"));
        assert_eq!(instruction_filename_for_tool("codex"), Some("AGENTS.md"));
        assert_eq!(instruction_filename_for_tool("gemini"), Some("GEMINI.md"));
        assert_eq!(instruction_filename_for_tool("totally-unknown"), None);
    }

    #[test]
    fn upsert_inserts_then_replaces_idempotently() {
        let block = render_managed_block("work/sysid", CONTEXT_LINK_NAME);
        let once = upsert_block("# My instructions\n", &block);
        assert!(once.contains(BLOCK_START) && once.contains(BLOCK_END));
        assert!(once.contains("# My instructions"));
        let twice = upsert_block(
            &once,
            &render_managed_block("work/sysid", CONTEXT_LINK_NAME),
        );
        assert_eq!(once.matches(BLOCK_START).count(), 1);
        assert_eq!(
            twice.matches(BLOCK_START).count(),
            1,
            "must not duplicate the block"
        );
    }

    #[test]
    fn remove_block_leaves_user_content_untouched() {
        let block = render_managed_block("g", CONTEXT_LINK_NAME);
        let with = upsert_block("USER ABOVE\n", &block);
        let without = remove_block(&with);
        assert!(!without.contains(BLOCK_START));
        assert!(without.contains("USER ABOVE"));
    }

    #[test]
    fn marker_roundtrips() {
        let s = marker_contents("default", "work/sysid", "7f3a2b1c");
        let m = parse_marker(&s).unwrap();
        assert_eq!(m.profile, "default");
        assert_eq!(m.group_path, "work/sysid");
        assert_eq!(m.session_id, "7f3a2b1c");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn with_temp_profile() -> (tempfile::TempDir, String) {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_CONFIG_HOME", tmp.path());
        std::env::set_var("HOME", tmp.path());
        (tmp, "default".to_string())
    }

    #[test]
    #[serial]
    fn paths_mirror_group_hierarchy() {
        let (_t, p) = with_temp_profile();
        let paths = paths_for(&p, "work/sysid").unwrap();
        assert!(paths.context.ends_with("groups/work/sysid/context.md"));
        assert!(paths.summary.ends_with("groups/work/sysid/summary.md"));
        assert!(paths.lock.ends_with("groups/work/sysid/.context.lock"));
    }

    #[test]
    #[serial]
    fn rejects_dotdot_components() {
        let (_t, p) = with_temp_profile();
        assert!(paths_for(&p, "../escape").is_err());
        assert!(paths_for(&p, "a/../b").is_err());
    }

    #[test]
    #[serial]
    fn ensure_files_creates_empty_context_and_placeholder_summary() {
        let (_t, p) = with_temp_profile();
        let paths = ensure_files(&p, "g1").unwrap();
        assert!(paths.context.exists());
        assert_eq!(std::fs::read_to_string(&paths.context).unwrap(), "");
        assert_eq!(
            std::fs::read_to_string(&paths.summary).unwrap().trim(),
            SUMMARY_PLACEHOLDER
        );
    }

    #[test]
    #[serial]
    fn append_entry_writes_attributed_header_and_text() {
        let (_t, p) = with_temp_profile();
        let a = Author {
            title: "fit".into(),
            tool: "claude".into(),
            session_id: "7f3a2b1cdead".into(),
        };
        append_entry(&p, "g1", &a, "net B best, r2=0.97").unwrap();
        let body = read_context(&p, "g1").unwrap();
        assert!(
            body.contains("fit (claude, 7f3a2b1c)"),
            "header missing: {body}"
        );
        assert!(body.contains("net B best, r2=0.97"));
        assert!(body.starts_with("\n## "));
    }

    #[test]
    #[serial]
    fn append_entry_is_additive() {
        let (_t, p) = with_temp_profile();
        let a = Author {
            title: "x".into(),
            tool: "claude".into(),
            session_id: "abcd1234efgh".into(),
        };
        append_entry(&p, "g1", &a, "first").unwrap();
        append_entry(&p, "g1", &a, "second").unwrap();
        let body = read_context(&p, "g1").unwrap();
        assert!(body.find("first").unwrap() < body.find("second").unwrap());
    }

    #[test]
    #[serial]
    fn write_context_round_trips_and_overwrites() {
        let (_t, p) = with_temp_profile();
        write_context(&p, "g1", "first body\n").unwrap();
        assert_eq!(read_context(&p, "g1").unwrap(), "first body\n");
        write_context(&p, "g1", "replaced\n").unwrap();
        assert_eq!(read_context(&p, "g1").unwrap(), "replaced\n");
    }

    #[test]
    fn compose_launch_injection_wraps_non_empty_context() {
        let section = compose_launch_injection("work/sysid", "net B best, r2=0.97")
            .expect("non-empty context must produce a section");
        assert!(section.contains("group: work/sysid"));
        assert!(section.contains("net B best, r2=0.97"));
        assert!(section.contains("aoe context add"));
        assert!(section.ends_with("---\n\n"));
    }

    #[test]
    fn compose_launch_injection_returns_none_for_empty_context() {
        assert!(compose_launch_injection("g1", "").is_none());
        assert!(compose_launch_injection("g1", "   \n\t").is_none());
    }

    #[test]
    #[serial]
    fn list_summaries_reports_groups_with_files() {
        let (_t, p) = with_temp_profile();
        ensure_files(&p, "alpha").unwrap();
        ensure_files(&p, "beta/child").unwrap();
        let mut got: Vec<String> = list_summaries(&p)
            .unwrap()
            .into_iter()
            .map(|s| s.group_path)
            .collect();
        got.sort();
        assert_eq!(got, vec!["alpha".to_string(), "beta/child".to_string()]);
    }

    #[test]
    #[serial]
    fn snapshot_returns_content_and_byte_length() {
        let (_t, p) = with_temp_profile();
        write_context(&p, "g1", "hello world\n").unwrap();
        let (content, len) = snapshot_for_curation(&p, "g1").unwrap();
        assert_eq!(content, "hello world\n");
        assert_eq!(len, content.len() as u64);
    }

    #[test]
    #[serial]
    fn commit_without_concurrent_append_replaces_both_files() {
        let (_t, p) = with_temp_profile();
        write_context(&p, "g1", "raw notes\nmore raw\n").unwrap();
        let (_content, len) = snapshot_for_curation(&p, "g1").unwrap();
        commit_curation(&p, "g1", "clean summary line", "outward digest", len).unwrap();

        assert_eq!(read_context(&p, "g1").unwrap(), "clean summary line\n");
        assert_eq!(read_summary(&p, "g1").unwrap(), "outward digest");

        let state = read_curator_state(&p, "g1").unwrap().unwrap();
        assert_eq!(state.last_size, "clean summary line\n".len() as u64);
    }

    #[test]
    #[serial]
    fn commit_preserves_concurrent_append_tail() {
        let (_t, p) = with_temp_profile();
        write_context(&p, "g1", "original body\n").unwrap();
        let (_content, len) = snapshot_for_curation(&p, "g1").unwrap();

        // An agent append lands during the (simulated) LLM call.
        let a = Author {
            title: "late".into(),
            tool: "claude".into(),
            session_id: "deadbeef0000".into(),
        };
        append_entry(&p, "g1", &a, "appended after snapshot").unwrap();

        commit_curation(&p, "g1", "cleaned text", "digest", len).unwrap();
        let body = read_context(&p, "g1").unwrap();
        let clean_at = body.find("cleaned text").expect("cleaned text present");
        let tail_at = body
            .find("appended after snapshot")
            .expect("appended note preserved");
        assert!(clean_at < tail_at, "cleaned text must precede the tail");
    }

    #[test]
    #[serial]
    fn grew_tracks_curation_and_subsequent_appends() {
        let (_t, p) = with_temp_profile();
        write_context(&p, "g1", "seed\n").unwrap();
        assert!(context_grew_since_last_curation(&p, "g1").unwrap());

        let (_content, len) = snapshot_for_curation(&p, "g1").unwrap();
        commit_curation(&p, "g1", "curated", "digest", len).unwrap();
        assert!(!context_grew_since_last_curation(&p, "g1").unwrap());

        let a = Author {
            title: "x".into(),
            tool: "claude".into(),
            session_id: "abcd1234efgh".into(),
        };
        append_entry(&p, "g1", &a, "new note").unwrap();
        assert!(context_grew_since_last_curation(&p, "g1").unwrap());
    }
}
