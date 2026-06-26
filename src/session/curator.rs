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

use chrono::{DateTime, Utc};
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
/// stable session id, title, and the tool that ran it. `idle` is whether the
/// session was `Status::Idle` when the roster was built, so the ask flow can
/// address questions only to members that can answer without being revived.
pub struct RosterMember {
    pub id: String,
    pub title: String,
    pub tool: String,
    pub idle: bool,
}

/// Markers wrapping the optional third section the model emits when `ask` is on:
/// up to two clarifying questions for idle members, one per line as
/// `=== <member_id> | <question>`. Same unambiguous style as the body markers.
const ASKS_BEGIN: &str = "===AOE_ASKS_BEGIN===";
const ASKS_END: &str = "===AOE_ASKS_END===";

/// Hard cap on clarifying questions per manual curate run.
const MAX_ASKS: usize = 2;

/// How long each `agent-chat ask` may block before it is treated as a timeout
/// (empty stdout) and skipped.
const ASK_TIMEOUT_SECS: u64 = 90;

/// Outcome of a curate attempt, surfaced to the CLI and auto-trigger callers.
pub enum CurateOutcome {
    /// The rewrite was produced and committed. Byte counts are the cleaned
    /// `context.md` body and the `summary.md` body that were written.
    /// `asked` is how many clarifying questions were proposed and gated as
    /// sendable, `answered` how many of those actually replied (both 0 unless
    /// this was a manual `ask`-enabled run).
    Curated {
        context_bytes: usize,
        summary_bytes: usize,
        asked: usize,
        answered: usize,
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

/// Distinct non-empty groups among `instances` that are due for an automatic
/// curation: `context.md` has grown since the last curation AND either there is
/// no prior state or at least `interval` has elapsed since `last_run_at`. State
/// read errors degrade to "not due" so a transient failure never spams the
/// agent. Cheap: only metadata and small state-file reads, no LLM work.
pub fn due_groups(
    profile: &str,
    instances: &[crate::session::Instance],
    interval: Duration,
    now: DateTime<Utc>,
) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut due = Vec::new();
    let interval =
        chrono::Duration::from_std(interval).unwrap_or_else(|_| chrono::Duration::zero());
    for inst in instances {
        let group = inst.group_path.trim();
        if group.is_empty() || !seen.insert(group.to_string()) {
            continue;
        }
        if !group_context::context_grew_since_last_curation(profile, group).unwrap_or(false) {
            continue;
        }
        let elapsed = match group_context::read_curator_state(profile, group) {
            Ok(Some(state)) => now - state.last_run_at >= interval,
            Ok(None) => true,
            Err(_) => false,
        };
        if elapsed {
            due.push(group.to_string());
        }
    }
    due
}

/// Build the roster of members for a group from loaded instances: every
/// instance whose `group_path` is the group itself or a descendant of it.
fn roster_for_group(group_path: &str, instances: &[crate::session::Instance]) -> Vec<RosterMember> {
    let prefix = format!("{group_path}/");
    instances
        .iter()
        .filter(|i| i.group_path == group_path || i.group_path.starts_with(&prefix))
        .map(|i| RosterMember {
            id: i.id.clone(),
            title: i.title.clone(),
            tool: i.tool.clone(),
            idle: i.status == crate::session::Status::Idle,
        })
        .collect()
}

/// Render the roster as a markdown bullet list the prompt embeds verbatim. Each
/// line carries the stable id so the summary table can reference it and (when
/// `ask` is on) the model can address questions by id. The idle marker is only
/// shown when `ask` is on, since it only matters for the asks section.
fn render_roster(roster: &[RosterMember], ask: bool) -> String {
    if roster.is_empty() {
        return "(no members recorded)".to_string();
    }
    let mut out = String::new();
    for m in roster {
        if ask {
            let avail = if m.idle { "idle" } else { "busy" };
            out.push_str(&format!(
                "- {} (id: {}, tool: {}, {})\n",
                m.title, m.id, m.tool, avail
            ));
        } else {
            out.push_str(&format!("- {} (id: {}, tool: {})\n", m.title, m.id, m.tool));
        }
    }
    out
}

/// Build the curate prompt: the instructions, the member roster, the required
/// output sections with their markers, and the current `context.md` body. Pure
/// so it can be unit-tested without an agent. When `ask` is true the prompt also
/// requests an optional third section listing up to two clarifying questions
/// addressed to idle members; Rust still decides whether to send any of them.
pub fn build_curate_prompt(
    group_path: &str,
    context: &str,
    roster: &[RosterMember],
    ask: bool,
) -> String {
    let roster_md = render_roster(roster, ask);
    let asks_section = if ask {
        format!(
            "\n\
             You MAY also emit an optional THIRD section with up to {MAX_ASKS} clarifying\n\
             questions for genuinely unresolved `OPEN:` items, addressed ONLY to members marked\n\
             `idle` in the roster (others cannot answer). Omit the section, or leave it empty, if\n\
             nothing genuinely needs asking. Use the member's id, not the title:\n\
             \n\
             {ASKS_BEGIN}\n\
             === <member_id> | <one clear question>\n\
             === <member_id> | <one clear question>\n\
             {ASKS_END}\n"
        )
    } else {
        String::new()
    };
    format!(
        "You are the curator for the shared working memory of the agent group `{group_path}`.\n\
         You are given the group's current `context.md` and its member roster. Rewrite the\n\
         context and author an outward-facing summary. Do NOT invent facts: use only what is\n\
         present in the context below.\n\
         \n\
         Return the two required sections, each wrapped in its markers and nothing outside them:\n\
         \n\
         {CONTEXT_BEGIN}\n\
         <the rewritten context.md body>\n\
         {CONTEXT_END}\n\
         {SUMMARY_BEGIN}\n\
         <the summary.md body>\n\
         {SUMMARY_END}\n\
         {asks_section}\
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
         - Then a `who to ask for what` markdown table with columns: Member, id, Topics, how-to-ask.\n\
         Build one row per roster member; infer each member's topics from the attribution\n\
         headers in the context (header format `## <ts> <title> (<tool>, <sid>)`). Put the\n\
         member's stable id in the id column, and in the how-to-ask column the literal command an\n\
         outside agent would run to reach them: `agent-chat ask <id> \"<question>\"`.\n\
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

/// Extract the clarifying questions the model proposed, as `(id, question)`
/// pairs, capped at [`MAX_ASKS`]. Tolerates the section being absent, empty, or
/// malformed: any line that is not `=== <id> | <question>` with both parts
/// non-empty is skipped. ANSI escapes are stripped first, matching the body
/// parser. The caller still gates every pair (idle + in-group) before sending.
pub fn parse_curator_asks(raw: &str) -> Vec<(String, String)> {
    let cleaned = strip_ansi(raw);
    let Some(body) = extract_between(&cleaned, ASKS_BEGIN, ASKS_END) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("===") else {
            continue;
        };
        let Some((id, question)) = rest.split_once('|') else {
            continue;
        };
        let id = id.trim();
        let question = question.trim();
        if id.is_empty() || question.is_empty() {
            continue;
        }
        out.push((id.to_string(), question.to_string()));
        if out.len() == MAX_ASKS {
            break;
        }
    }
    out
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

/// Filter and cap proposed asks down to the ones we will actually send: each
/// `(id, question)` is kept only if an instance with that id exists, belongs to
/// `group_path` (the group itself or a descendant), and is `Status::Idle`. The
/// result is capped at [`MAX_ASKS`]. Pure over the instance slice so it is unit
/// testable without live sessions. Returns `(id, title, question)` so the caller
/// has the recipient title for the recorded Q&A.
fn gate_asks(
    group_path: &str,
    asks: &[(String, String)],
    instances: &[crate::session::Instance],
) -> Vec<(String, String, String)> {
    let prefix = format!("{group_path}/");
    let mut out = Vec::new();
    for (id, question) in asks {
        let Some(inst) = instances.iter().find(|i| &i.id == id) else {
            continue;
        };
        let in_group = inst.group_path == group_path || inst.group_path.starts_with(&prefix);
        if !in_group || inst.status != crate::session::Status::Idle {
            continue;
        }
        out.push((inst.id.clone(), inst.title.clone(), question.clone()));
        if out.len() == MAX_ASKS {
            break;
        }
    }
    out
}

/// The `AGENT_CHAT_ID` value the curator runs `agent-chat` under: `id:title`
/// split on the FIRST colon. The id part must carry no colon or whitespace, so
/// the group is sanitized into it; the title part is human-readable.
fn agent_chat_identity(group_path: &str) -> String {
    let sanitized: String = group_path
        .chars()
        .map(|c| {
            if c == ':' || c.is_whitespace() {
                '-'
            } else {
                c
            }
        })
        .collect();
    format!("curator-{sanitized}:{group_path} curator")
}

/// Strip a leading `--- reply from ... ---` header line from `agent-chat`'s
/// stdout and return the remaining answer body, trimmed. Empty stdout means a
/// timeout (the recipient never answered) and yields `None`.
fn answer_from_reply(stdout: &str) -> Option<String> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return None;
    }
    let body = match trimmed.split_once('\n') {
        Some((first, rest)) if first.trim_start().starts_with("--- reply from") => rest,
        _ => trimmed,
    };
    let body = body.trim();
    if body.is_empty() {
        None
    } else {
        Some(body.to_string())
    }
}

/// Ask up to [`MAX_ASKS`] idle in-group agents the curator's clarifying
/// questions via the external `agent-chat` CLI, serially (no parallelism), and
/// return the `(recipient_title, question, answer)` for each one that replied.
///
/// Best-effort: if `agent-chat` is not on PATH, nothing is asked. Every ask is
/// gated to a live idle member of this group (see [`gate_asks`]); a non-idle,
/// out-of-group, or unknown recipient is skipped. A timeout (empty stdout) is
/// skipped silently.
pub async fn run_curator_asks(
    profile: &str,
    group_path: &str,
    asks: &[(String, String)],
) -> Vec<(String, String, String)> {
    if asks.is_empty() {
        return Vec::new();
    }
    if which_agent_chat().is_none() {
        return Vec::new();
    }
    let instances =
        match crate::session::Storage::new_unwatched(profile).and_then(|s| s.load_with_groups()) {
            Ok((instances, _)) => instances,
            Err(e) => {
                tracing::debug!(target: "curator", "ask: load instances failed: {e}");
                return Vec::new();
            }
        };
    let gated = gate_asks(group_path, asks, &instances);
    let identity = agent_chat_identity(group_path);
    let mut answered = Vec::new();
    for (id, title, question) in gated {
        let argv = vec![
            "agent-chat".to_string(),
            "ask".to_string(),
            id,
            question.clone(),
            "--timeout".to_string(),
            ASK_TIMEOUT_SECS.to_string(),
        ];
        let Some(stdout) = run_agent_chat(&argv, &identity).await else {
            continue;
        };
        if let Some(answer) = answer_from_reply(&stdout) {
            answered.push((title, question, answer));
        }
    }
    answered
}

/// Whether `agent-chat` resolves on PATH. Used to keep the ask flow best-effort.
fn which_agent_chat() -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("agent-chat"))
        .find(|cand| cand.is_file())
}

/// Run `agent-chat` with `AGENT_CHAT_ID` set, capturing stdout. `None` on spawn
/// error, non-zero exit (e.g. unknown recipient), or io error; the child is
/// killed on drop. A timeout is reported by the tool as empty stdout, exit 0,
/// so it returns `Some("")` and the caller treats it as no answer.
async fn run_agent_chat(argv: &[String], identity: &str) -> Option<String> {
    use tokio::process::Command;
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .env("AGENT_CHAT_ID", identity)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!(target: "curator", "agent-chat spawn failed: {e}");
            return None;
        }
    };
    match child.wait_with_output().await {
        Ok(out) if out.status.success() => Some(String::from_utf8_lossy(&out.stdout).into_owned()),
        Ok(out) => {
            tracing::debug!(target: "curator", code = ?out.status.code(), "agent-chat exited non-zero");
            None
        }
        Err(e) => {
            tracing::debug!(target: "curator", "agent-chat io error: {e}");
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
///
/// When `ask` is true (manual runs only), the prompt invites the model to
/// propose clarifying questions; after committing the rewrite, the gated subset
/// is sent to idle in-group agents via `agent-chat` and every reply is appended
/// back into `context.md`. Auto-curate always passes `ask=false`.
pub async fn curate(
    profile: &str,
    group_path: &str,
    tool: &str,
    force: bool,
    ask: bool,
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
    let prompt = build_curate_prompt(group_path, &snapshot, &roster, ask);

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

    let (asked, answered) = if ask {
        run_asks_and_record(profile, group_path, &raw).await
    } else {
        (0, 0)
    };

    Ok(CurateOutcome::Curated {
        context_bytes: context.len(),
        summary_bytes: summary.len(),
        asked,
        answered,
    })
}

/// Parse the asks the model proposed, send the gated subset, and append each
/// reply into `context.md`. Returns `(asked, answered)`: how many questions were
/// sent and how many replied. The appended Q&A folds into the NEXT curation, not
/// this one (we have already committed the rewrite), a deliberate one-cycle
/// delay that keeps each run a single LLM call.
async fn run_asks_and_record(profile: &str, group_path: &str, raw: &str) -> (usize, usize) {
    let asks = parse_curator_asks(raw);
    if asks.is_empty() {
        return (0, 0);
    }
    let answers = run_curator_asks(profile, group_path, &asks).await;
    let answered = answers.len();
    let group_leaf = group_path.rsplit('/').next().unwrap_or(group_path);
    let curator_author = group_context::Author {
        title: format!("{group_leaf} curator"),
        tool: "curator".into(),
        session_id: String::new(),
    };
    for (title, question, answer) in &answers {
        let entry = format!("Q to {title}: {question}\nA: {answer}");
        if let Err(e) = group_context::append_entry(profile, group_path, &curator_author, &entry) {
            tracing::debug!(target: "curator", "ask: append answer failed: {e}");
        }
    }
    // `asked` counts what we actually attempted to send (the gated subset),
    // recomputed here so the report matches what went out, not the raw proposal.
    let asked = {
        let instances = crate::session::Storage::new_unwatched(profile)
            .and_then(|s| s.load_with_groups())
            .map(|(i, _)| i)
            .unwrap_or_default();
        gate_asks(group_path, &asks, &instances).len()
    };
    (asked, answered)
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
                id: "sid-aaa".into(),
                title: "fit net B".into(),
                tool: "claude".into(),
                idle: true,
            },
            RosterMember {
                id: "sid-bbb".into(),
                title: "plotter".into(),
                tool: "codex".into(),
                idle: false,
            },
        ]
    }

    #[test]
    fn prompt_includes_context_roster_and_markers() {
        let ctx = "## 2026-06-25T10:00Z fit net B (claude, 7f3a2b1c)\nnet B best, r2=0.97\n";
        let p = build_curate_prompt("work/sysid", ctx, &roster(), false);
        // Context body is embedded verbatim.
        assert!(p.contains("net B best, r2=0.97"));
        // Group path is named.
        assert!(p.contains("work/sysid"));
        // Roster members appear with their ids and tools.
        assert!(p.contains("fit net B (id: sid-aaa, tool: claude)"));
        assert!(p.contains("plotter (id: sid-bbb, tool: codex)"));
        // All four body section markers are present.
        assert!(p.contains(CONTEXT_BEGIN));
        assert!(p.contains(CONTEXT_END));
        assert!(p.contains(SUMMARY_BEGIN));
        assert!(p.contains(SUMMARY_END));
        // The no-invention guard is stated.
        assert!(p.contains("Do NOT invent facts"));
    }

    #[test]
    fn summary_table_lists_id_column_and_literal_ask_command() {
        let p = build_curate_prompt("work/sysid", "ctx", &roster(), false);
        // The who-to-ask table must reference the stable id and the literal
        // command an outside agent runs to reach a member.
        assert!(p.contains("Member, id, Topics, how-to-ask"));
        assert!(p.contains("agent-chat ask <id>"));
    }

    #[test]
    fn prompt_requests_asks_section_only_when_ask_enabled() {
        let with = build_curate_prompt("g1", "ctx", &roster(), true);
        assert!(with.contains(ASKS_BEGIN));
        assert!(with.contains(ASKS_END));
        // Idle/busy availability is shown only in ask mode.
        assert!(with.contains("idle"));
        assert!(with.contains("busy"));

        let without = build_curate_prompt("g1", "ctx", &roster(), false);
        assert!(!without.contains(ASKS_BEGIN));
        assert!(!without.contains(ASKS_END));
    }

    #[test]
    fn prompt_handles_empty_roster() {
        let p = build_curate_prompt("g1", "some context", &[], false);
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

    fn instance_in_group(group: &str) -> crate::session::Instance {
        let mut inst = crate::session::Instance::new("member", "/tmp/test");
        inst.group_path = group.to_string();
        inst
    }

    #[test]
    #[serial_test::serial]
    fn due_groups_reports_grown_and_interval_elapsed() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_CONFIG_HOME", tmp.path());
        std::env::set_var("HOME", tmp.path());
        let profile = "default";
        let now = Utc::now();
        let interval = Duration::from_secs(3600);

        // grown is a never-curated group whose context exists: due.
        group_context::write_context(profile, "grown", "seed\n").unwrap();
        // soon was just curated and has not grown: not due.
        group_context::write_context(profile, "soon", "seed\n").unwrap();
        let (_c, len) = group_context::snapshot_for_curation(profile, "soon").unwrap();
        group_context::commit_curation(profile, "soon", "clean", "digest", len).unwrap();

        let instances = vec![
            instance_in_group("grown"),
            instance_in_group("soon"),
            // empty group_path is ignored.
            crate::session::Instance::new("loose", "/tmp/test"),
        ];

        let due = due_groups(profile, &instances, interval, now);
        assert_eq!(
            due,
            vec!["grown".to_string()],
            "only the grown group is due"
        );
    }

    #[test]
    #[serial_test::serial]
    fn due_groups_excludes_curated_until_interval_elapses() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_CONFIG_HOME", tmp.path());
        std::env::set_var("HOME", tmp.path());
        let profile = "default";
        let interval = Duration::from_secs(3600);

        group_context::write_context(profile, "g1", "seed\n").unwrap();
        let (_c, len) = group_context::snapshot_for_curation(profile, "g1").unwrap();
        group_context::commit_curation(profile, "g1", "clean", "digest", len).unwrap();
        // An append makes it grow again; only the interval now gates it.
        let author = group_context::Author {
            title: "x".into(),
            tool: "claude".into(),
            session_id: "abcd1234".into(),
        };
        group_context::append_entry(profile, "g1", &author, "new note").unwrap();

        let instances = vec![instance_in_group("g1")];

        // Just after curation: grown but interval not elapsed, so excluded.
        let just_after = group_context::read_curator_state(profile, "g1")
            .unwrap()
            .unwrap()
            .last_run_at;
        assert!(
            due_groups(profile, &instances, interval, just_after).is_empty(),
            "grown but too soon is excluded"
        );

        // Past the interval: now due.
        let later = just_after + chrono::Duration::seconds(3601);
        assert_eq!(
            due_groups(profile, &instances, interval, later),
            vec!["g1".to_string()],
            "grown and interval elapsed is due"
        );
    }

    #[test]
    fn asks_parser_extracts_up_to_two_and_tolerates_missing() {
        // No section at all yields no asks.
        assert!(parse_curator_asks("just prose").is_empty());
        // Empty section yields no asks.
        let empty = format!("{ASKS_BEGIN}\n{ASKS_END}");
        assert!(parse_curator_asks(&empty).is_empty());

        // A well-formed section with three lines is capped at two; malformed
        // lines (missing pipe, empty id, empty question) are skipped.
        let raw = format!(
            "{CONTEXT_BEGIN}\nx\n{CONTEXT_END}\n{SUMMARY_BEGIN}\ny\n{SUMMARY_END}\n\
             {ASKS_BEGIN}\n\
             === sid-1 | what units for torque?\n\
             === sid-2 | which net is canonical?\n\
             === sid-3 | a third question past the cap\n\
             === no-pipe-here\n\
             ===  | empty id is dropped\n\
             {ASKS_END}"
        );
        let asks = parse_curator_asks(&raw);
        assert_eq!(asks.len(), 2, "capped at MAX_ASKS");
        assert_eq!(
            asks[0],
            ("sid-1".to_string(), "what units for torque?".to_string())
        );
        assert_eq!(
            asks[1],
            ("sid-2".to_string(), "which net is canonical?".to_string())
        );
    }

    fn member(id: &str, group: &str, status: crate::session::Status) -> crate::session::Instance {
        let mut inst = crate::session::Instance::new("member", "/tmp/test");
        inst.id = id.to_string();
        inst.group_path = group.to_string();
        inst.status = status;
        inst
    }

    #[test]
    fn gate_asks_keeps_only_idle_in_group_and_caps_at_two() {
        use crate::session::Status;
        let instances = vec![
            member("idle-in", "work/sysid", Status::Idle),
            member("idle-sub", "work/sysid/plots", Status::Idle),
            member("busy-in", "work/sysid", Status::Running),
            member("idle-other", "work/other", Status::Idle),
            member("idle-extra", "work/sysid", Status::Idle),
        ];
        let asks = vec![
            ("idle-in".to_string(), "q1".to_string()),
            ("busy-in".to_string(), "q-busy".to_string()), // not idle: drop
            ("idle-other".to_string(), "q-other".to_string()), // wrong group: drop
            ("missing".to_string(), "q-missing".to_string()), // unknown id: drop
            ("idle-sub".to_string(), "q2".to_string()),    // descendant group: keep
            ("idle-extra".to_string(), "q3".to_string()),  // past the cap
        ];
        let gated = gate_asks("work/sysid", &asks, &instances);
        assert_eq!(gated.len(), 2, "capped at MAX_ASKS");
        assert_eq!(gated[0].0, "idle-in");
        assert_eq!(gated[1].0, "idle-sub");
    }

    #[test]
    fn answer_from_reply_strips_header_and_handles_timeout() {
        // Empty stdout (timeout) -> no answer.
        assert!(answer_from_reply("").is_none());
        assert!(answer_from_reply("   \n").is_none());
        // Leading reply header is stripped; the body remains.
        let with_header = "--- reply from plotter ---\nuse SI units.\nsecond line";
        assert_eq!(
            answer_from_reply(with_header),
            Some("use SI units.\nsecond line".to_string())
        );
        // No header: the whole trimmed body is the answer.
        assert_eq!(
            answer_from_reply("just an answer"),
            Some("just an answer".to_string())
        );
    }

    #[test]
    fn agent_chat_identity_sanitizes_id_part() {
        let id = agent_chat_identity("work/sys id:x");
        let (left, _right) = id.split_once(':').unwrap();
        assert!(
            !left.contains(char::is_whitespace),
            "id part has no whitespace"
        );
        assert_eq!(left, "curator-work/sys-id-x");
        assert!(id.ends_with("work/sys id:x curator"));
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
