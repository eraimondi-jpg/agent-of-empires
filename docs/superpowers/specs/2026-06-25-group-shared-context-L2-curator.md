# Group Shared Context L2: Curator Agent (Design)

Date: 2026-06-25
Status: Approved (design); implementation pending
Builds on L1 (`2026-06-25-group-shared-context-design.md`).

## Goal

A per-group "curator" that autonomously keeps the group's `context.md` clean and
hierarchical and authors the outward-facing `summary.md`. It runs as a headless
one-shot agent, reusing the existing `src/session/smart_rename.rs` pattern.

## Locked decisions

| Decision | Choice |
| --- | --- |
| Curator process | Headless one-shot (`claude -p "<prompt>"`), runs and exits. Not a tmux session, not an ACP worker. Reuse `smart_rename` (oneshot_flag, timeout, kill_on_drop). |
| Tool | Default `claude`; overridable via a `curator.agent` setting. No "dominant tool" logic. Skip (logged) if the tool has no one-shot mode or is not installed. |
| Files | Single `context.md`, rewritten in place. No `curated.md`. `GROUP_CONTEXT.md` keeps pointing at `context.md`. |
| Summary | Curator authors/maintains `summary.md` from the group's member roster + attribution headers. |
| Trigger | Auto on by default, change-gated (runs only if `context.md` has new content since the last run), configurable interval (default 60 min); plus a TUI key and a command-palette entry to run on the selected group on demand. |
| Asking other agents | Out of scope (L3). aoe has no agent-to-agent reply channel. The curator records unresolved points as `OPEN:` lines in `context.md` instead. |

## Lossless in-place rewrite (the crux)

The curator must not lose agent appends that arrive during its LLM call. With a
single `context.md`:

1. Under a brief flock (the existing `.context.lock`), read `context.md` content
   and its byte length `L`. Release the lock.
2. Run the one-shot agent on that snapshot (no lock held; this is the slow part).
   It returns a cleaned `context.md` body and a fresh `summary.md` body.
3. Re-acquire the flock briefly: re-read `context.md`, take any bytes appended
   past `L` (concurrent agent notes) verbatim, then write `cleaned_body + tail`
   via tempfile + atomic rename. Write `summary.md` the same way. Release.

Result: clean at the top, any just-arrived raw notes at the bottom (folded next
run). The lock is never held across the LLM call, so `aoe context add` never
stalls. On agent failure/timeout, write nothing (idempotent, lossless).

A small `.curator.json` sidecar stores `last_run_at` and `last_size` so the
change-gate can skip unchanged groups. This is run-state, not a content file.

## Phased tasks

- T1: storage. Add `snapshot_for_curation`, `commit_curation(cleaned, summary, snapshot_len)`,
  and `CuratorState` read/write to `src/session/group_context.rs`. Unit tests incl. a
  concurrent-append-preserved test.
- T2: curator engine `src/session/curator.rs`. Prompt builders, headless one-shot run
  (share `smart_rename`'s run helper), response parse/sanitize, tool resolution
  (default claude + `curator.agent`), member discovery for the summary, `curate(profile, group)`
  with an inflight guard. Unit tests.
- T3: CLI `aoe curator run <group>` and `aoe curator status`. Wire `definition.rs` / `main.rs` /
  `cli/mod.rs`. Integration test with a fake `claude` shim.
- T4: auto-trigger. `CuratorConfig { auto: bool (default true), interval_secs, agent }` via the
  single-source `#[setting(...)]` pattern; `due_groups` change-gate; fire from the TUI status-poll
  loop and the serve status loop; add a TUI keybinding + palette entry to curate the selected group.
- T5: docs (`cargo xtask gen-docs`), full `fmt`/`clippy`/`test` gate, rebuild the binary.

## Out of scope

L3 (curator asking teammates and collecting answers); any agent-to-agent messaging.
