# Per-Group Project Manager (Orchestrator) Design

Date: 2026-06-26
Status: Approved (design); implementation pending
Builds on L1/L2 (group context + curator) and the external `agent-chat` CLI.

## Model

General Manager (the human) -> one Project Manager (PM) per group -> Workers
(the group's other agents). Each group has exactly one PM: a permanent,
non-removable agent that absorbs three roles:
1. Curator: owns `GROUP_CONTEXT.md` and `summary.md` for the group.
2. Contact point: the group's `agent-chat` endpoint (answers about the group,
   asks Workers when needed, may ask other groups for info).
3. Project Manager: on a GM directive, decomposes the task and spawns/drives
   Workers to completion.

## Locked decisions

| Decision | Choice |
| --- | --- |
| Roles | One PM per group, absorbing curator + agent-chat endpoint + orchestrator. |
| Autonomy | Reactive only. The PM never starts work or spawns Workers on its own initiative; it acts only on a GM directive, then executes autonomously (no per-step approval). |
| Spawn cap | None. The PM spawns as many Workers as the task needs. |
| Scope | The PM only spawns/manages its own group's Workers. It may ASK other groups' agents questions (agent-chat), never command them. |
| Sidebar | A distinct, pinned row at the TOP of each group, visually unlike a normal session row, and non-removable. |
| Mechanism | The PM is "just an agent with a role file": no new control plane. Orchestration = the PM running `aoe add` / `aoe send` / `agent-chat` per its instructions. |

## Build pieces

1. PM instructions template (embedded const, `{group}` substituted), installed
   as the PM session's instruction file at create time. (See template below.)
2. Auto-create the PM session on group creation (CLI `aoe group create` and the
   TUI group-create flow); revive on restart; remove on group deletion. The PM
   needs a working directory (a per-PM scratch dir under the app dir is fine).
3. Sidebar: a new pinned item kind rendered at the top of each group's children,
   visually distinct, and protected from deletion (the delete action refuses it).
4. PM session permissions: a `settings.local.json` allowlist (`Bash(aoe:*)`,
   `Bash(agent-chat:*)`) in the PM cwd so it acts autonomously without approval
   prompts. Plus a kill switch (config to disable PMs, and a way to stop one PM).

## PM instructions template

```
# You are the Project Manager for the aoe group `{group}`

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
- When the GM gives you a task, decompose it and run it to completion:
  - Spawn Workers into THIS group: `aoe add <dir> -t "<role>" -g {group} --tool claude --launch`.
    As many as the task needs; a typical shape is one Worker to do it and one to verify it.
  - Give each a clear written task, drive and coordinate via `agent-chat`, monitor with `aoe list`.
  - Fold results into GROUP_CONTEXT.md and report progress to the GM.

## Boundaries
- Stay in your group: only spawn/manage this group's Workers; never spawn into or
  direct another group's agents (ask them questions only).
- Don't destroy work: never delete the GM's or a Worker's files/sessions; check
  before anything irreversible.
- Keep the GM informed: report what you spawned, what is running, and results.
```

## Cost and risk

This is the most expensive feature so far: a live PM per group plus the Workers it
spawns. The kill switch is mandatory. Reactive-only autonomy (never spawns without a
GM directive) is the main guardrail; "no cap" means a single directive can spin up
many Workers, so the PM must report what it spawned.

## Out of scope

Cross-group orchestration (a PM commanding another group's agents); automatic
project initiation (PM starting work without the GM).
