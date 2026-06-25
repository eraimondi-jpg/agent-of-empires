//! Per-group shared context files: an append-only `context.md` every group
//! member reads and updates, and an outward-facing `summary.md`. Curation of
//! both is the L2 curator's job; L1 only provisions and appends.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use fs2::FileExt;

use super::get_profile_dir;

pub const SUMMARY_PLACEHOLDER: &str =
    "<!-- maintained by the group curator (L2); empty until then -->";

const LOCK_FILENAME: &str = ".context.lock";

pub struct GroupContextPaths {
    pub dir: PathBuf,
    pub context: PathBuf,
    pub summary: PathBuf,
    pub lock: PathBuf,
}

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

- Read `./{ctx}` before starting; it is the group's shared working memory.
- Record durable findings with `aoe context add \"<note>\"`. Do not hand-edit `{ctx}`; concurrent edits are lost.
- Discover other groups' summaries with `aoe context summaries`.
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
}
