//! Headless one-shot group curator (L2).
//!
//! A group's `context.md` is append-only: every member writes attributed
//! entries via `aoe context add`, so it grows monotonically and accretes
//! duplication and chatter. The curator runs the configured agent once in
//! non-interactive one-shot mode (e.g. `claude -p`) over a snapshot of that
//! file, asking it to return a cleaned, deduplicated, hierarchical rewrite plus
//! a short outward-facing `summary.md`. Both are committed losslessly through
//! the L1 primitives in [`super::group_context`], which preserve any agent
//! appends that landed during the (unlocked) LLM call.
//!
//! This works with or without the `serve` feature: it loads instances directly
//! from storage rather than the live in-memory dashboard state, and runs its own
//! one-shot spawn helper.

use crate::agents;
use crate::session::group_context;

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// Hard cap on how long a single curate one-shot may run before it is killed.
/// The curator competes with live agent turns for the provider API, so it gets
/// the same generous budget as smart-rename; the child is killed on drop, so a
/// timed-out call leaves no orphan.
const ONESHOT_TIMEOUT: Duration = Duration::from_secs(120);

/// Section markers the agent must wrap each output in. Chosen to be unambiguous
/// and vanishingly unlikely to occur in real prose, so parsing is a plain
/// substring split rather than a fragile heuristic.
const CONTEXT_BEGIN: &str = "===AOE_CONTEXT_BEGIN===";
const CONTEXT_END: &str = "===AOE_CONTEXT_END===";
const SUMMARY_BEGIN: &str = "===AOE_SUMMARY_BEGIN===";
const SUMMARY_END: &str = "===AOE_SUMMARY_END===";

/// A group member as it appears in the "who to ask for what" roster: the
/// session title and the tool that ran it.
pub struct RosterMember {
    pub title: String,
    pub tool: String,
}

/// Outcome of a curate attempt, surfaced to the CLI and auto-trigger callers.
pub enum CurateOutcome {
    /// The rewrite was produced and committed. Byte counts are the cleaned
    /// `context.md` body and the `summary.md` body that were written.
    Curated {
        context_bytes: usize,
        summary_bytes: usize,
    },
    /// `context.md` has not grown since the last curation and `force` was not
    /// set, so nothing was run.
    SkippedNoChange,
    /// The resolved tool has no one-shot mode (carries the tool name).
    SkippedNoAgent(String),
    /// A curate was attempted but did not produce a committed result (spawn
    /// error, timeout, non-zero exit, parse miss, an already-running curate for
    /// the same group, or a storage error). Carries a short reason.
    Failed(String),
}

/// Per-group inflight set so only one curate runs per group at a time. A second
/// trigger for the same group while one is running returns `Failed` rather than
/// spawning a concurrent agent over the same file.
fn inflight() -> &'static Mutex<HashSet<String>> {
    static INFLIGHT: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    INFLIGHT.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Marks a group as having a curate in flight; removed on drop so every exit
/// path (including early returns and panics) releases it.
struct InflightGuard {
    group: String,
}

impl InflightGuard {
    fn acquire(group: &str) -> Option<Self> {
        let mut guard = inflight().lock().expect("curator inflight poisoned");
        if !guard.insert(group.to_string()) {
            return None;
        }
        Some(Self {
            group: group.to_string(),
        })
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        if let Ok(mut guard) = inflight().lock() {
            guard.remove(&self.group);
        }
    }
}

/// Resolve the curate agent from a tool name, returning it only if it has a
/// one-shot mode. The default caller passes `"claude"`. An unknown tool, or a
/// known tool without a `oneshot_flag`, yields `None`.
fn resolve_curator_agent(tool: &str) -> Option<&'static agents::AgentDef> {
    let agent = agents::get_agent(tool)?;
    agent.oneshot_flag?;
    Some(agent)
}

/// Build the roster of members for a group from loaded instances: every
/// instance whose `group_path` is the group itself or a descendant of it.
fn roster_for_group(group_path: &str, instances: &[crate::session::Instance]) -> Vec<RosterMember> {
    let prefix = format!("{group_path}/");
    instances
        .iter()
        .filter(|i| i.group_path == group_path || i.group_path.starts_with(&prefix))
        .map(|i| RosterMember {
            title: i.title.clone(),
            tool: i.tool.clone(),
        })
        .collect()
}

/// Render the roster as a markdown bullet list the prompt embeds verbatim.
fn render_roster(roster: &[RosterMember]) -> String {
    if roster.is_empty() {
        return "(no members recorded)".to_string();
    }
    let mut out = String::new();
    for m in roster {
        out.push_str(&format!("- {} ({})\n", m.title, m.tool));
    }
    out
}

/// Build the curate prompt: the instructions, the member roster, the two
/// required output sections with their markers, and the current `context.md`
/// body. Pure so it can be unit-tested without an agent.
pub fn build_curate_prompt(group_path: &str, context: &str, roster: &[RosterMember]) -> String {
    let roster_md = render_roster(roster);
    format!(
        "You are the curator for the shared working memory of the agent group `{group_path}`.\n\
         You are given the group's current `context.md` and its member roster. Rewrite the\n\
         context and author an outward-facing summary. Do NOT invent facts: use only what is\n\
         present in the context below.\n\
         \n\
         Return EXACTLY two sections, each wrapped in its markers and nothing outside them:\n\
         \n\
         {CONTEXT_BEGIN}\n\
         <the rewritten context.md body>\n\
         {CONTEXT_END}\n\
         {SUMMARY_BEGIN}\n\
         <the summary.md body>\n\
         {SUMMARY_END}\n\
         \n\
         Rules for the context section:\n\
         - Produce an updated, deduplicated, hierarchical document grouped by topic.\n\
         - Keep durable facts (decisions, results, configurations, file paths); drop chatter,\n\
         greetings, and transient status.\n\
         - Preserve attribution where it matters (who established a fact).\n\
         - Record unresolved questions as lines starting with `OPEN:`.\n\
         - Merge entries about the same topic; do not lose any durable information.\n\
         \n\
         Rules for the summary section:\n\
         - 3 to 6 lines on what the group is doing and its key durable results.\n\
         - Then a `who to ask for what` markdown table with columns: Member, Tool, Topics.\n\
         Build one row per roster member; infer each member's topics from the attribution\n\
         headers in the context (header format `## <ts> <title> (<tool>, <sid>)`).\n\
         \n\
         Member roster:\n\
         {roster_md}\n\
         \n\
         Current context.md:\n\
         {context}\n"
    )
}

/// Extract the cleaned context and summary bodies from the agent's raw output.
/// Returns `None` if either marker pair is missing, the markers are out of
/// order, a body is empty after trimming, or the output reads as a refusal.
/// ANSI escapes are stripped first; multi-line bodies are allowed (unlike
/// smart-rename, which expects a single line).
pub fn parse_curator_response(raw: &str) -> Option<(String, String)> {
    let cleaned = strip_ansi(raw);
    let context = extract_between(&cleaned, CONTEXT_BEGIN, CONTEXT_END)?;
    let summary = extract_between(&cleaned, SUMMARY_BEGIN, SUMMARY_END)?;
    let context = context.trim().to_string();
    let summary = summary.trim().to_string();
    if context.is_empty() || summary.is_empty() {
        return None;
    }
    if is_refusal(&context) || is_refusal(&summary) {
        return None;
    }
    Some((context, summary))
}

/// Return the substring strictly between the first `begin` and the first `end`
/// that follows it. `None` if either marker is absent or `end` precedes `begin`.
fn extract_between(s: &str, begin: &str, end: &str) -> Option<String> {
    let start = s.find(begin)? + begin.len();
    let rest = &s[start..];
    let stop = rest.find(end)?;
    Some(rest[..stop].to_string())
}

/// Whether a body is an obvious model refusal rather than real content. Mirrors
/// smart-rename's philosophy but checks the first non-empty line, since the
/// curator output is multi-line.
fn is_refusal(body: &str) -> bool {
    let Some(first) = body.lines().map(str::trim).find(|l| !l.is_empty()) else {
        return true;
    };
    let lc = first.to_lowercase();
    const PREFIXES: &[&str] = &[
        "i cannot",
        "i can't",
        "i can not",
        "i am unable",
        "i'm unable",
        "i won't",
        "i will not",
        "unable to",
        "sorry",
        "as an ai",
    ];
    PREFIXES.iter().any(|p| lc.starts_with(p)) || lc.contains("cannot determine")
}

/// Remove ANSI/CSI escape sequences (color codes etc.) that CLI agents emit.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
            }
            for n in chars.by_ref() {
                if n.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Run the agent one-shot in `cwd`, capturing stdout. Returns `None` on spawn
/// error, non-zero exit, or timeout. The child is killed on drop, so a
/// timed-out call leaves no orphan. Independent of the `serve` feature.
async fn run_oneshot(argv: &[String], cwd: &str, timeout: Duration) -> Option<String> {
    use tokio::process::Command;
    if argv.is_empty() {
        return None;
    }
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    if !cwd.is_empty() {
        cmd.current_dir(cwd);
    }
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!(target: "curator", "one-shot spawn failed: {e}");
            return None;
        }
    };
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) if out.status.success() => {
            Some(String::from_utf8_lossy(&out.stdout).into_owned())
        }
        Ok(Ok(out)) => {
            tracing::debug!(target: "curator", code = ?out.status.code(), "one-shot exited non-zero");
            None
        }
        Ok(Err(e)) => {
            tracing::debug!(target: "curator", "one-shot io error: {e}");
            None
        }
        Err(_) => {
            tracing::debug!(target: "curator", "one-shot timed out");
            None
        }
    }
}

/// Curate a group's `context.md` and `summary.md` with a one-shot agent call.
///
/// `tool` selects the agent (the default caller passes `"claude"`). When `force`
/// is false and `context.md` has not grown since the last curation, the call is
/// a no-op (`SkippedNoChange`). On any failure (no one-shot agent, already
/// running, spawn/timeout/parse failure, storage error) nothing is written.
pub async fn curate(
    profile: &str,
    group_path: &str,
    tool: &str,
    force: bool,
) -> anyhow::Result<CurateOutcome> {
    if !force && !group_context::context_grew_since_last_curation(profile, group_path)? {
        return Ok(CurateOutcome::SkippedNoChange);
    }

    let Some(agent) = resolve_curator_agent(tool) else {
        return Ok(CurateOutcome::SkippedNoAgent(tool.to_string()));
    };

    let Some(_guard) = InflightGuard::acquire(group_path) else {
        return Ok(CurateOutcome::Failed("already running".to_string()));
    };

    let instances = crate::session::Storage::new_unwatched(profile)?
        .load_with_groups()?
        .0;
    let roster = roster_for_group(group_path, &instances);

    let (snapshot, snapshot_len) = group_context::snapshot_for_curation(profile, group_path)?;
    let prompt = build_curate_prompt(group_path, &snapshot, &roster);

    let Some(argv) = build_oneshot_argv(agent, &prompt) else {
        return Ok(CurateOutcome::SkippedNoAgent(tool.to_string()));
    };

    let cwd = group_context::paths_for(profile, group_path)?.dir;
    let cwd = cwd.to_string_lossy().into_owned();

    let Some(raw) = run_oneshot(&argv, &cwd, ONESHOT_TIMEOUT).await else {
        return Ok(CurateOutcome::Failed("agent run failed".to_string()));
    };

    let Some((context, summary)) = parse_curator_response(&raw) else {
        return Ok(CurateOutcome::Failed(
            "unparseable agent output".to_string(),
        ));
    };

    group_context::commit_curation(profile, group_path, &context, &summary, snapshot_len)?;
    Ok(CurateOutcome::Curated {
        context_bytes: context.len(),
        summary_bytes: summary.len(),
    })
}

/// Build the argv for the one-shot curate call, or `None` when the agent has no
/// one-shot mode. Always `[binary, oneshot_token, extra_args.., prompt]`; the
/// prompt stays the final argv element so it can never be read as a flag.
fn build_oneshot_argv(agent: &agents::AgentDef, prompt: &str) -> Option<Vec<String>> {
    let token = agent.oneshot_flag?;
    let mut argv = vec![agent.binary.to_string(), token.to_string()];
    argv.extend(agent.oneshot_extra_args().iter().map(|s| s.to_string()));
    argv.push(prompt.to_string());
    Some(argv)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roster() -> Vec<RosterMember> {
        vec![
            RosterMember {
                title: "fit net B".into(),
                tool: "claude".into(),
            },
            RosterMember {
                title: "plotter".into(),
                tool: "codex".into(),
            },
        ]
    }

    #[test]
    fn prompt_includes_context_roster_and_markers() {
        let ctx = "## 2026-06-25T10:00Z fit net B (claude, 7f3a2b1c)\nnet B best, r2=0.97\n";
        let p = build_curate_prompt("work/sysid", ctx, &roster());
        // Context body is embedded verbatim.
        assert!(p.contains("net B best, r2=0.97"));
        // Group path is named.
        assert!(p.contains("work/sysid"));
        // Roster members appear with their tools.
        assert!(p.contains("fit net B (claude)"));
        assert!(p.contains("plotter (codex)"));
        // All four section markers are present.
        assert!(p.contains(CONTEXT_BEGIN));
        assert!(p.contains(CONTEXT_END));
        assert!(p.contains(SUMMARY_BEGIN));
        assert!(p.contains(SUMMARY_END));
        // The no-invention guard is stated.
        assert!(p.contains("Do NOT invent facts"));
    }

    #[test]
    fn prompt_handles_empty_roster() {
        let p = build_curate_prompt("g1", "some context", &[]);
        assert!(p.contains("(no members recorded)"));
    }

    #[test]
    fn parse_accepts_well_formed_two_section_response() {
        let raw = format!(
            "preamble noise\n{CONTEXT_BEGIN}\n# Topic\nfact one\nOPEN: is X resolved?\n{CONTEXT_END}\n\
             {SUMMARY_BEGIN}\nThe group does X.\n\n| Member | Tool | Topics |\n{SUMMARY_END}\ntrailing"
        );
        let (ctx, sum) = parse_curator_response(&raw).expect("well-formed parses");
        assert!(ctx.contains("# Topic"));
        assert!(ctx.contains("OPEN: is X resolved?"));
        assert!(ctx.starts_with("# Topic"), "leading newline trimmed");
        assert!(sum.contains("The group does X."));
        assert!(sum.contains("| Member | Tool | Topics |"));
        // Markers themselves must not leak into the bodies.
        assert!(!ctx.contains(CONTEXT_END));
        assert!(!sum.contains(SUMMARY_END));
    }

    #[test]
    fn parse_strips_ansi_escapes() {
        let raw = format!(
            "\u{1b}[32m{CONTEXT_BEGIN}\u{1b}[0m\nclean body\n{CONTEXT_END}\n\
             {SUMMARY_BEGIN}\nsum body\n{SUMMARY_END}"
        );
        let (ctx, sum) = parse_curator_response(&raw).expect("ansi-wrapped parses");
        assert_eq!(ctx, "clean body");
        assert_eq!(sum, "sum body");
    }

    #[test]
    fn parse_rejects_missing_markers() {
        // Missing summary section.
        let only_ctx = format!("{CONTEXT_BEGIN}\nbody\n{CONTEXT_END}");
        assert!(parse_curator_response(&only_ctx).is_none());
        // Missing context section.
        let only_sum = format!("{SUMMARY_BEGIN}\nbody\n{SUMMARY_END}");
        assert!(parse_curator_response(&only_sum).is_none());
        // No markers at all.
        assert!(parse_curator_response("just some prose").is_none());
        // Context end before context begin (out of order).
        let reversed =
            format!("{CONTEXT_END}\nbody\n{CONTEXT_BEGIN}\n{SUMMARY_BEGIN}\nx\n{SUMMARY_END}");
        assert!(parse_curator_response(&reversed).is_none());
    }

    #[test]
    fn parse_rejects_empty_sections() {
        let empty_ctx = format!(
            "{CONTEXT_BEGIN}\n   \n{CONTEXT_END}\n{SUMMARY_BEGIN}\nreal summary\n{SUMMARY_END}"
        );
        assert!(parse_curator_response(&empty_ctx).is_none());
        let empty_sum = format!(
            "{CONTEXT_BEGIN}\nreal context\n{CONTEXT_END}\n{SUMMARY_BEGIN}\n\n{SUMMARY_END}"
        );
        assert!(parse_curator_response(&empty_sum).is_none());
    }

    #[test]
    fn parse_rejects_refusals() {
        let raw = format!(
            "{CONTEXT_BEGIN}\nI cannot help with that request.\n{CONTEXT_END}\n\
             {SUMMARY_BEGIN}\nok\n{SUMMARY_END}"
        );
        assert!(parse_curator_response(&raw).is_none());
    }

    #[test]
    fn resolve_returns_some_for_claude_none_for_made_up() {
        assert!(resolve_curator_agent("claude").is_some());
        // A known agent with no one-shot mode is rejected too.
        assert!(resolve_curator_agent("cursor").is_none());
        // A made-up tool name is rejected.
        assert!(resolve_curator_agent("not-a-real-tool-xyz").is_none());
    }

    #[test]
    fn argv_is_binary_token_then_prompt() {
        let agent = agents::get_agent("claude").unwrap();
        let argv = build_oneshot_argv(agent, "do the thing").expect("claude has one-shot");
        assert_eq!(argv, vec!["claude", "-p", "do the thing"]);
    }

    #[tokio::test]
    async fn run_oneshot_returns_none_for_nonexistent_binary() {
        // A failed spawn must surface as None so curate() writes nothing and a
        // later trigger can retry. A binary that does not exist is the
        // deterministic, machine-independent failure.
        let argv = vec![
            "aoe-curator-nonexistent-binary-xyz".to_string(),
            "-p".to_string(),
            "prompt".to_string(),
        ];
        assert!(run_oneshot(&argv, "", Duration::from_secs(5))
            .await
            .is_none());
    }
}
