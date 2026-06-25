# Group Shared Context (L1) Design

Date: 2026-06-25
Status: Approved (design); implementation pending
Scope: L1 foundation only. L2 (curator agent) and L3 (inter-agent Q&A) are deferred.

## Motivation

Groups collect agents working on one topic (e.g. "sysid"). Today a `Group` is a pure
label with no shared state, so knowledge each agent gains stays trapped in that agent's
session. We want a per-group shared context every member agent can read and append to, so
e.g. a sysid actuator-fitting agent records the best networks and a policy-training agent in
the same group picks them up without re-deriving them. Agents in other groups should be able
to read a short outward-facing summary of a group, and learn who to ask for more.

## Architecture constraints (discovered)

These shaped the design and are worth keeping in mind:

| Constraint | Consequence |
| --- | --- |
| `Group` is `{name, path, collapsed, archived_at}` in `groups.json`; sessions reference a group via `group_path` string. Groups hold no sessions, no metadata, no directory. | The shared file needs a new home in the app data dir, keyed by group path. |
| Each session lives in its own cwd/worktree; groups can span repos. | One canonical file, surfaced into each member's cwd. Cannot live in a single repo. |
| Agents are independent CLI coding tools (claude, codex, gemini, ...). They already read and write files. | aoe provisions the file and makes agents aware of it; the agents do the writing. |
| No agent-to-agent message bus. Only `aoe send` (tmux keystrokes) and ACP prompt, plus polling. | Inter-agent Q&A (L3) is fragile; deferred. L1 needs no messaging. |
| Plugin system is Tier-0 (manifest + enable/disable only); no lifecycle hooks, no event bus. | This is core changes, not a plugin. |

## Decisions

1. Build L1 (shared files + access) first; L2/L3 are separate later specs.
2. Agents must be persistently aware and always able to update the context.
3. Writes are append-only, attributed, file-locked. Concurrent appends never clobber.
4. Awareness via Approach 1: a managed block in the agent's native instruction file plus the
   file surfaced into cwd.
5. All curation (cleaning/restructuring `context.md`, authoring `summary.md`) is L2's job.
   In L1 `summary.md` is an empty placeholder.
6. Include a read-only TUI viewer for the group context.

## Components

### 1. Storage module: `src/session/group_context.rs`

Canonical files, per profile, mirroring the group hierarchy:

```
<profile_dir>/groups/<group_path>/context.md
<profile_dir>/groups/<group_path>/summary.md
<profile_dir>/groups/<group_path>/.context.lock
```

Example: group `work/sysid` maps to `groups/work/sysid/`.

API (sketch):
- `paths_for(profile, group_path) -> GroupContextPaths`
- `ensure_files(profile, group_path)`: create dir and empty files if missing (summary seeded
  with a one-line placeholder comment).
- `append_entry(profile, group_path, author: Author, text: &str)`: advisory `flock` on
  `.context.lock` (same mechanism as `src/session/storage.rs`), then append a formatted entry.
- `read_context` / `read_summary`.
- `list_summaries(profile) -> Vec<(group_path, first_nonempty_line)>`.

`Author` carries session title, tool, and session id for attribution.

### 2. File formats

`context.md` is an append-only log. Each entry:

```
## 2026-06-25T14:03Z aoe-actuator-fit (claude, 7f3a2b1c)
<free text the agent wrote>
```

Header is machine-stable so L2's curator can parse and restructure. `summary.md` holds only
a placeholder comment until L2 writes it.

### 3. CLI: `src/cli/context.rs`

New `aoe context` subcommand group:

| Command | Purpose |
| --- | --- |
| `aoe context add "<note>"` | Universal write path. Resolves group + author, file-locked append. |
| `aoe context show [--group G]` | Print `context.md`. |
| `aoe context summary [<group>]` | Print a group's `summary.md`. |
| `aoe context summaries` | List all groups with a one-line digest of each summary. |
| `aoe context path [--group G]` | Print canonical file paths (for scripting). |

Group/author resolution order for `add`:
1. `.aoe-group` marker file in cwd (written at launch; holds `group_path` + `session_id`).
2. cwd matched against sessions' `project_path` / worktree dir, then that session's group.
3. `--group` flag.
4. Otherwise a clear error ("not inside a grouped aoe session; pass --group").

`docs/cli/reference.md` is regenerated via `cargo xtask gen-docs` (CI enforces).

### 4. Awareness wiring (Approach 1)

A single choke point runs on session launch/create in a group and on group move, covering
`src/cli/add.rs`, the ACP supervisor launch, and `aoe group move`. Two operations:

`group_context::attach(session)`:
1. `ensure_files`.
2. Symlink canonical `context.md` into cwd as `GROUP_CONTEXT.md` (read convenience; never
   overwrite a real file with that name).
3. Write the hidden `.aoe-group` marker into cwd.
4. Add `GROUP_CONTEXT.md` and `.aoe-group` to the repo's `.git/info/exclude` (not the tracked
   `.gitignore`; skipped for non-git cwds).
5. Maintain a fenced managed block in the agent's native always-loaded instruction file,
   selected by a per-tool map (`claude` -> `CLAUDE.md`, `codex` -> `AGENTS.md`,
   `gemini` -> `GEMINI.md`; extensible). Delimited by
   `<!-- aoe:group-context:start -->` / `<!-- aoe:group-context:end -->`, idempotently
   replaced, only ever touching content between the markers. The block instructs the agent to:
   read `GROUP_CONTEXT.md` before starting; record durable findings with `aoe context add`
   (do not hand-edit, concurrent edits are lost); discover other groups via
   `aoe context summaries`.

`group_context::detach(session)` (on delete or move out of group): remove the symlink, the
marker, and the managed block.

This delivers "always aware": the pointer rides the agent's own instruction file, reloaded
each session/turn, surviving long conversations, revive, and re-attach.

### 5. Cross-group discovery

`aoe context summaries` and `aoe context summary <group>` let any agent read other groups'
outward-facing summaries. The managed block advertises these. Summaries are empty until L2.
This is discovery by convention, not access control: every file is readable on the same
filesystem; aoe controls only what it surfaces.

### 6. TUI viewer: `src/tui/dialogs/group_context.rs`

A read-only scrollable overlay (`GroupContextDialog`), mirroring `src/tui/dialogs/changelog.rs`.

- Opens on the selected sidebar row when it is a group (`HomeView.selected_group`), via a new
  `ActionId::ShowGroupContext` and a `Binding` in `src/tui/home/bindings.rs` (auto-listed in
  help and command palette). If the row is not a group, show a "select a group first" info
  dialog.
- Reads the group's `context.md` and `summary.md` from disk; `Tab` toggles the two panes.
- Scroll: `j`/`k`, arrows, PageUp/PageDown, Home/End. Esc/`q` closes.
- Minimal line styling for `##` headers and `-` bullets (matching changelog), no full markdown
  engine. Read-only (curation is L2).
- Wiring: register in `src/tui/dialogs/mod.rs`; add `Option<GroupContextDialog>` to
  `HomeView` and init in `new()`; add to the `render_dialogs!` dispatch in
  `src/tui/home/render.rs`; add a key-dispatch arm in `src/tui/home/input.rs`.

## Data flow

1. User creates session in group `work/sysid` -> `attach` provisions files, symlink, marker,
   exclude entry, managed block.
2. Agent reads `GROUP_CONTEXT.md`; later runs `aoe context add "fit done; net B best, r2=0.97"`.
3. `add` resolves group via marker, locks `.context.lock`, appends an attributed entry.
4. Another member agent reads the updated `GROUP_CONTEXT.md`.
5. An outside agent runs `aoe context summary work/sysid` (empty until L2).
6. User selects the `work/sysid` group in the TUI and opens the viewer to read both files.

## Error handling

- cwd not a grouped session and no `--group`: explicit error.
- `GROUP_CONTEXT.md` name already taken by a real file: warn, skip symlink, do not fail launch.
- symlink / instruction-file write failure: warn, never block session launch.
- instruction file already exists: only insert/replace the fenced block, never touch user
  content. Non-git cwd: skip the exclude step.
- empty summary: print "(no summary yet)".

## Testing

- Unit (`#[cfg(test)]`): path/hierarchy resolution, entry formatting, managed-block
  insert/replace/remove idempotency, summaries listing, dialog scroll clamp + Tab toggle + Esc.
- Integration (`tests/`): create group + session -> files/symlink/marker/managed-block appear;
  `aoe context add` appends under lock, including a concurrent-append test; move session out ->
  wiring removed; `summaries` spans groups. Use temp dirs, never real user state.
- E2E (`tests/e2e/`): CLI path for `add`/`show`/`summaries`; one TUI test opening the viewer,
  scrolling, toggling, closing.
- Diff is Rust-only (nothing under `web/`), so Codecov patch coverage is N/A.

## Out of scope (L1)

- L2: curator agent that cleans/restructures `context.md` and authors/maintains `summary.md`,
  and may ask group members for clarification.
- L3: inter-agent Q&A (curator or any agent asking peers and collecting answers).
- Rich `summary.md` content (L2 owns it).
- Web dashboard UI for the context.
- Any access control / isolation between groups.

## Risks

- Touching the agent's instruction file is the one intrusive step; mitigated by a fenced,
  removable managed block and an opt-in fallback if disliked.
- Direct hand-edits of `GROUP_CONTEXT.md` bypass the lock; the managed block discourages this.
- Per-tool instruction-filename map needs upkeep as new agent tools are added.
- The recent upstream `src/acp` -> worker/event substrate refactor means the ACP launch choke
  point must be located against current `main`, not stale references.

## Files touched (estimate)

New: `src/session/group_context.rs`, `src/cli/context.rs`,
`src/tui/dialogs/group_context.rs`, integration + e2e tests.
Modified: `src/session/mod.rs` (module + path helpers), `src/cli/mod.rs` (subcommand),
`src/cli/add.rs` and ACP launch + `src/cli/group.rs` (attach/detach choke points),
`src/tui/dialogs/mod.rs`, `src/tui/home/mod.rs`, `src/tui/home/render.rs`,
`src/tui/home/input.rs`, `src/tui/home/bindings.rs`, `docs/cli/reference.md` (generated).
