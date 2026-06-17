# Writing Plugins

A practical guide to building an Agent of Empires plugin. The user-facing
overview (managing, discovering, installing) is in `docs/plugins.md`; the
architecture rationale is in `docs/development/internals/plugin-system.md`.
The manifest schema and capability types you build against live in the
`aoe-plugin-api` crate. A ready-to-copy skeleton is in
`contrib/plugin-template/`.

## What a plugin is

A plugin is a directory with an `aoe-plugin.toml` manifest at its root.
The manifest declares an id, metadata, and contributions; unknown keys are
rejected, so typos fail loudly at install. The id is lowercase ASCII
segments separated by dots (`aoe.status`, `someuser.review-helper`);
segments may contain digits and hyphens but must start with a letter, and
the whole id is at most 64 bytes. The id namespaces everything the plugin
touches: its config table (`[plugins."<id>"]`), its per-session
`plugin_meta` slot, its event topics (`plugin.<id>.*`), and its canonical
action names (`plugin.<id>.<action>`).

Contributions come in two tiers:

- **Tier 0, declarative.** Settings, `setting_defaults`, keybinds, themes,
  and declarative status rules are pure data, evaluated by the host. No
  plugin code runs and no capability is needed.
- **Tier 1, worker.** Commands, actions, `[[ui]]` contributions, and RPC
  status detection are dispatched to the plugin's worker: an executable
  named by the `[runtime]` section, speaking newline-delimited JSON-RPC
  2.0 on stdio, in any language. Manifest validation rejects these
  contributions without a `[runtime]` section.

Trust is two-level: builtin plugins ship inside the aoe binary and are
auto-granted; everything installed from a GitHub slug or a local path is
community code behind an explicit capability approval. The grant is
pinned to the sha256 of the manifest bytes (in
`<app_dir>/plugin_grants.toml`); any manifest change makes the grant
stale and the plugin's runtime contributions deactivate until the user
re-approves. A second hash, the tree hash over every file in the plugin
directory, is recorded in `plugins.lock` and drives update detection and
featured-release pinning.

## Quick start

The smallest valid manifest:

```toml
id = "you.my-plugin"
name = "My Plugin"
version = "0.1.0"
api_version = 1
description = "One sentence about what it does."
```

`api_version` is the manifest schema version; the host currently supports
`1` (the `API_VERSION` constant in `aoe-plugin-api`).

To require a minimum aoe release, add `min_aoe_version` (dotted numeric):

```toml
min_aoe_version = "0.5.0"
```

The host refuses to load the plugin when the running build is older, listing
it as a warning in `aoe plugin list` rather than activating it with missing
features. Omit it to support any host. This differs from `api_version`: the
latter gates the manifest schema, the former gates the host's release version.

Install it:

```sh
aoe plugin install ./my-plugin
aoe plugin list
aoe plugin info you.my-plugin
```

Installation copies the directory into `<app_dir>/plugins/<id>/`, so
editing your source tree does nothing until you re-stage it:

```sh
aoe plugin update you.my-plugin   # re-copies from the recorded source path
```

The update is detected by tree hash, so any file change counts, not just
the manifest. For releases, print the hash that pins a version:

```sh
aoe plugin hash ./my-plugin
```

## Settings

Settings render generically on the TUI and web settings surfaces with no
plugin UI code, and are stored under `[plugins."<id>".settings]` in
`config.toml`. Keys must be snake_case.

```toml
[[settings]]
key = "poll_interval_ms"
label = "Poll interval (ms)"
description = "How often the worker refreshes its data."
default = 1000

[settings.widget]
kind = "number"
min = 100
max = 60000
```

Widget kinds are `toggle`, `text`, `number` (optional `min` / `max`), and
`select` (`options = ["a", "b"]`). The inline form
`widget = { kind = "toggle" }` works too.

A plugin may also override the DEFAULT of another setting with
`[[setting_defaults]]`. The target is either another plugin's setting
(`<plugin-id>.<key>`) or a core setting (`<section>.<field>`):

```toml
[[setting_defaults]]
target = "aoe.status.custom_agent_rules"
value = false
priority = 10
reason = "This plugin ships its own custom-agent detection."

[[setting_defaults]]
target = "session.strict_hotkeys"
value = true
priority = 5
reason = "Example core-setting target."
```

Core targets are restricted to a curated allowlist of cosmetic and workflow
defaults (theme, a handful of `session.*` UX toggles). Security-load-bearing
defaults (`auth.*`, `web.*`, `updates.*`, `telemetry.*`, sandbox/container
settings, agent command/args/hooks, `session.yolo_mode_default`) are NOT
plugin-overridable; an override targeting one is silently ignored. Any core
default a plugin does change is listed on the install prompt.

Overrides never beat the user: a value set in config.toml always wins,
and a core override only applies while the on-disk value is absent or
still equal to the built-in default. Overrides from ungranted plugins
fail closed. Resolution order is user value, then the highest-priority
active plugin's override, then the owning manifest's `default`, then the
widget's zero value. The full chain, winners and losers, is inspectable:

```sh
aoe settings explain my-plugin.poll_interval_ms   # plugin setting
aoe settings explain session.yolo_mode_default    # core setting
aoe settings explain    # every plugin setting (and overridden core field)
```

The same provenance is served at `GET /api/settings/resolved`. Note that
the v1 host API has no method for a worker to read its own settings;
settings drive host-side behavior (rendered surfaces, declarative-rule
gates, resolution) rather than worker logic.

## CLI commands

Plugin commands are grafted into the `aoe` command tree per invocation and
dispatched to the worker:

```toml
capabilities = ["cli-top-level"]

[[commands]]
path = ["review"]
about = "Run a review over a session"
rpc_method = "review.run"

[[commands.args]]
name = "target"
required = false
help = "Session title to review"

[[commands]]
path = ["session", "archive-all"]
about = "Archive every idle session"
rpc_method = "review.archive_all"

[runtime]
entrypoint = "worker.mjs"
```

Rules:

- A top-level path (`["review"]`) requires the `cli-top-level` capability
  so the install prompt surfaces it; a nested path under an existing
  group (`["session", "archive-all"]`) does not. Paths deeper than two
  levels are not supported.
- Core always wins on the full path: grafting `["session"]` or
  `["session", "ls"]` is rejected because core owns them, while a free
  leaf under a core group is fine. The `plugin` head is reserved for
  plugin management. Collisions are dropped with a visible warning, never
  silently shadowed, and earlier grafts win prefix overlaps between
  plugins.

When the command runs, the host calls `rpc_method` on your worker with
`{ "args": { "<name>": "<value>", ... } }` (string-valued args only). A
string result is printed verbatim; other non-null JSON is printed as is.

## Keybinds and actions

An action is a named entry point on your worker; a keybind gives it a
default chord in the TUI:

```toml
[[actions]]
name = "run_review"
label = "Run review"
rpc_method = "review.run"

[[keybinds]]
action = "run_review"
chord = "ctrl+r"
priority = 0

[runtime]
entrypoint = "worker.mjs"
```

Action names are snake_case; the canonical external id is
`plugin.<id>.<action>`. Chord syntax: a single character (`R`, uppercase
implies Shift), `ctrl+<char>`, or `f<n>`. Constraints, enforced when the
binding table is built:

- The bare navigation keys `j`, `k`, `h`, `l`, `G`, `<`, `>`, `{`, `}`
  are reserved structural chords and are rejected.
- Core chords always shadow plugin chords; conflicts between plugins
  resolve by declared priority (then plugin id). `aoe plugin info <id>`
  shows your resolved bindings and exactly which core action shadows a
  chord in which mode.
- In strict-hotkeys mode, bare lowercase chords never fire (those keys
  belong to the typing guard), so prefer `ctrl+` chords.

A fired keybind calls `rpc_method` on the worker with
`{ "session_id": <selected session id or null> }`.

## Themes

A theme is a TOML file the host adds to the theme list while the plugin is
active; the theme name is the file stem:

```toml
[[themes]]
file = "themes/solar-flare.toml"
```

The path is relative to the plugin root. Builtin theme names and existing
custom themes win name collisions.

## Status detection

Two tiers, matching the contribution model. Plugin detection is consulted
before the builtin per-agent detectors, so an active plugin claiming an
agent owns it; disabling the plugin falls back cleanly.

**Tier 0, declarative rules**, evaluated in-core with no plugin code
running (taken from the bundled `aoe.status` plugin):

```toml
[[status_detection]]
agent = "*"
mode = "declarative"

[[status_detection.rules]]
status = "running"
priority = 100
contains = ["esc to interrupt"]

[[status_detection.rules]]
status = "waiting"
priority = 90
regex = "\\(y/n\\)|\\[y/n\\]|approve|allow"

[[status_detection.rules]]
status = "idle"
priority = 0
default = true
```

`status` is one of `running`, `waiting`, `idle`, `error`. Rules are
evaluated highest priority first against the LOWERCASED pane text:
`contains` matches when every literal appears, `regex` when the pattern
matches (write patterns lowercase). At most one rule per agent may set
`default = true`, the fallback when nothing matches. The wildcard agent
`"*"` applies only to tools with no builtin agent entry, so custom
`--cmd` agents get detection without shadowing first-party detectors.

**Tier 1, worker RPC**, for parsers too stateful for rules. Requires
`[runtime]` and the `pane-read` capability:

```toml
capabilities = ["pane-read"]

[[status_detection]]
agent = "codex"
mode = "rpc"
method = "status.detect_batch"
```

Pane snapshots are batched per plugin per poll window into one call:

```json
{ "method": "status.detect_batch", "params": { "snapshots": [
  { "session_id": "s1", "agent": "codex", "pane_text": "..." } ] } }
```

Reply with per-snapshot results, errors isolated per snapshot:

```json
{ "results": [
  { "session_id": "s1", "status": "running" },
  { "session_id": "s2", "error": "no parser for this agent" } ] }
```

Host policy: snapshots are truncated to their last 64 KiB, the batch has
an 800 ms deadline, and results are cached for about a second; a slow or
crashed worker answers from cache while the supervisor's respawn budget
decides its fate.

## Event handlers

A plugin can bind a bus topic to a worker RPC method declaratively, so the
host subscribes on its behalf and calls the method for each matching event.
This is the no-boilerplate alternative to running an `events.subscribe` loop
in the worker: you react to lifecycle facts without writing a receive loop.

```toml
capabilities = ["events-subscribe"]

[[event_handlers]]
on = "session.created"
rpc_method = "on_session"

[[event_handlers]]
on = "plugin.acme.*"
rpc_method = "on_plugin_event"

[runtime]
entrypoint = "worker.mjs"
```

`on` is a topic pattern (exact, or a trailing-`*` prefix like
`plugin.acme.*`). For each matching event the host calls `rpc_method` as a
normal host-to-worker request with params `{ topic, payload, seq }`; answer
it like any other contributed method. Event handlers need the
`events-subscribe` capability and a `[runtime]` worker.

Handlers fire only on events published after the plugin's handler thread
starts (on load or after a reload); there is no replay or resume cursor, so
use `events.subscribe { after_seq }` instead if you need to catch up on
events missed while disabled. Events are delivered sequentially per plugin.

## Terminal link handlers

A plugin can make terminal output clickable: declare a regex and the worker
method a click routes to. In the TUI a Ctrl+click on matching preview text
fires the handler; in the web dashboard the match becomes a clickable link.

```toml
capabilities = ["terminal-links"]

[[link_handlers]]
pattern = '#\d+'
rpc_method = "open_issue"

[runtime]
entrypoint = "worker.mjs"
```

`pattern` is a regex matched against a line of terminal text; every match is
a link. A click calls `rpc_method` with `{ text, session_id }`, where `text`
is the matched substring and `session_id` is the focused session (may be
null). Link handlers need the `terminal-links` capability and a `[runtime]`
worker. The host compiles each pattern once; an invalid regex is skipped with
a warning rather than failing the plugin. The web endpoint validates the
clicked method against the declared handlers, so a click can never reach an
undeclared worker method.

## Terminal panes

A plugin can contribute a command that runs in a real terminal. The host runs
it in a dedicated tmux session with the plugin install root as the working
directory; the TUI attaches to it (command palette: "Open pane: ..."), and the
web dashboard renders it as a full terminal panel.

```toml
capabilities = ["terminal-pane"]

[[panes]]
id = "logs"
title = "Tail plugin logs"
command = ["tail", "-f", "plugin.log"]
```

`command` is an argv array run directly (no shell, no interpolation). Panes
need the `terminal-pane` capability; no `[runtime]` worker is required because
the pane is a host-spawned process rather than a JSON-RPC worker. The host
injects discrete context env vars into the command: `AOE_PLUGIN_ID`,
`AOE_PLUGIN_ROOT`, `AOE_PANE_ID`, `AOE_SESSION_ID` (the focused agent session,
may be empty), and `AOE_WORKTREE` (that session's working directory).

Panes are ephemeral. Opening the same pane for the same session twice refocuses
the existing one rather than spawning a second. A pane is killed when it is
closed, when its plugin is disabled or uninstalled, when its bound session is
deleted, and on daemon restart (a startup sweep clears strays); plugin process
state is not resumed across a restart, so reopen the pane to get it back.

## UI extension points

Plugins never render. The manifest declares contributions against fixed
slots; the worker pushes small typed payloads; the host validates and
renders them with its own widgets on both surfaces (the TUI reads the
cache synchronously per frame, the web polls `GET /api/ui/state`).

```toml
[[ui]]
id = "review_badge"
slot = "session-list-row-badge"
title = "Review"
priority = 10

[runtime]
entrypoint = "worker.mjs"
```

| Slot | Scope | Payload kind |
|---|---|---|
| `status-bar-segment` | global | `badge` |
| `dashboard-card` | global | `blocks` |
| `session-list-row-badge` | per session | `badge` |
| `session-list-column` | per session | `cell` |
| `session-list-sort-key` | per session | `sort_key` |
| `session-list-filter-facet` | per session | `facets` |
| `session-detail-header-badge` | per session | `badge` |
| `session-detail-panel` | per session | `blocks` |

The worker pushes state through the `ui.state.set` host RPC (no
capability needed; ownership is checked against the declared `[[ui]]`
contributions in the approved manifest):

```json
{ "method": "ui.state.set", "params": {
  "contribution_id": "review_badge",
  "session_id": "s1",
  "ttl_ms": 60000,
  "payload": { "kind": "badge", "text": "needs review",
               "severity": "warning", "tooltip": "" } } }
```

`session_id` is required for session-scoped slots and must be omitted for
global ones; `ttl_ms` is optional expiry. Payload kinds (`kind` field):

- `badge`: `text`, optional `severity`, optional `tooltip`.
- `cell`: `text`, optional `severity`, optional numeric `sort_key`.
- `sort_key`: finite `key` (higher sorts first), optional `reason`.
- `facets`: `values` (list of strings the session list can filter on).
- `blocks`: optional `severity` plus `blocks`, a list of typed blocks:
  `{ "type": "text", "text": "..." }`, `{ "type": "kv", "items":
  [["k", "v"], ...] }`, `{ "type": "list", "items": [...] }`,
  `{ "type": "metric", "label": "...", "value": "..." }`.

Severities are `info`, `success`, `warning`, `error`. Size caps are
enforced on push: 200 chars per text field, 32 blocks, 64 kv/list items,
8 facet values. `ui.state.remove { contribution_id, session_id? }` clears
state (all sessions when `session_id` is omitted), and
`ui.notify { title, body?, severity?, session_id? }` emits a
host-rendered notification (newest in the status bar / web top bar, last
50 kept).

UI state is ephemeral by design: a worker restart repushes it, and
disabling or uninstalling the plugin evicts it immediately. Canonical
data belongs in `plugin_meta`, not the UI cache.

## The worker runtime

```toml
[runtime]
entrypoint = "worker.mjs"
args = []
```

`entrypoint` is an executable path relative to the plugin root (so it
needs a shebang and the executable bit for a script); `args` are appended
at spawn. The worker runs with the plugin root as its working directory
and is spawned lazily on the first call. Builtin plugins are the
exception: their workers run as the hidden `aoe __plugin-worker --id <id>`
subcommand of the installed binary.

The wire format is one JSON-RPC 2.0 object per line on stdin/stdout.
There is no init handshake; the host simply starts sending requests:

- **Host to worker requests**: your contributed `rpc_method` values
  (actions, commands) and status batch methods. Answer each request's
  `id` with a `result` or an `error` object; the default per-call timeout
  is 10 seconds, and a timeout kills the worker.
- **Worker to host requests**: the capability-gated host API below. Use
  your own `id` space; the host's reply carries your `id` and no
  `method`, which is how you tell replies from incoming requests.
- **Host to worker notifications**: subscribed bus events arrive as
  `events.event` notifications with
  `{ "seq": n, "topic": "...", "payload": ... }` and no `id`.

Host API methods and the capability each one requires:

| Method | Capability |
|---|---|
| `sessions.list` | `sessions-read` |
| `session.meta.get { session_id }` | `sessions-read` |
| `session.meta.set { session_id, value }` | `sessions-meta-write` |
| `session.meta.cas { session_id, expected, value }` | `sessions-meta-write` |
| `events.subscribe { topics, after_seq? }` | `events-subscribe` |
| `events.publish { topic, payload }` | `events-publish` |
| `ui.state.set` / `ui.state.remove` / `ui.notify` | none |

Calling a method whose capability was not granted is refused, not a
no-op. `events.publish` is additionally restricted to the plugin's own
`plugin.<id>.*` topic prefix. `events.subscribe` with `after_seq` replays
missed events before the live stream; successful `session.meta.set` /
`session.meta.cas` writes publish `session.meta.changed` on the bus.
`session.meta.*` operates only on the plugin's own namespace within a
session's `plugin_meta`, and setting a null value removes the entry.

The full declared capability list (kebab-case, from
`aoe-plugin-api/src/capability.rs`): `sessions-read`,
`sessions-meta-write`, `pane-read`, `events-subscribe`, `events-publish`,
`process-spawn`, `net-fetch`, `fs-read`, `fs-write`, `agent-reconcile`,
`agent-hooks`, `cli-top-level`, `terminal-links`, `terminal-pane`.

### Capabilities not yet implemented

`process-spawn`, `net-fetch`, `fs-read`, `fs-write`, `agent-reconcile`, and
`agent-hooks` are RESERVED forward declarations. They parse in a manifest, but
the host has no RPC for them yet, so a worker that calls one gets an
"unknown host method" error. Declare them only when the host actually
implements them (a later `api_version`). The implemented set today is
`sessions.list`, `session.meta.*`, `events.publish`, `events.subscribe`, and
the `ui.*` pushes; `pane-read`, `cli-top-level`, `terminal-links`, and
`terminal-pane` are consumed by the host (manifest validation, link routing,
pane spawning), not as worker-initiated RPCs.

When the reserved capabilities land, the prompt will spell out their reach
(outbound network, arbitrary host file read/write, spawning host processes):
they are far broader than the session/event capabilities, and capability
gating is not an OS sandbox, so approving them means trusting the plugin's
code with your user's permissions.

Lifecycle: exit when stdin reaches EOF (that is how the host shuts you
down); anything you write to stderr lands in the host log line by line. A
crashing worker is respawned at most 3 times per process; after that the
plugin must be disabled and re-enabled to reset the budget.

## Shipping a plugin

Publish the plugin directory as a GitHub repository with
`aoe-plugin.toml` at the root, and tag the repository with the
`aoe-plugin` topic: `aoe plugin discover` (and the equivalent TUI/web
search) lists repositories with that topic, featured first, and users
install with `aoe plugin install owner/repo`.

Keep everything the plugin needs in the repository itself. Install does a
shallow `git clone` (`--depth 1`, blobs capped at 10 MB) and does NOT recurse
submodules, so a submodule's contents are absent at runtime (its gitlink
hashes as empty). Symlinks in the tree are rejected outright.

To get a release featured (vouched for by the AoE maintainers and
hash-verified on install), it has to be pinned in `plugins/featured.toml`.
A maintainer does that with the xtask helper, which clones the default
branch exactly as `aoe plugin install` does, computes the tree hash install
will see, and gates the write on an explicit safety attestation:

```sh
cargo xtask feature-plugin owner/repo
```

It prints the resolved id, version, capabilities, and hash, then asks the
maintainer to attest they reviewed the source and tested that version
before writing the entry (pass `--yes` to skip the prompt in automation).
A new version of an already-featured plugin adds another
`[[featured.releases]]` line; the helper refuses to re-pin an
already-released version to a different hash (bump the version instead) and
refuses a slug that now serves a different plugin id. The resulting entry:

```toml
[[featured]]
id = "owner.plugin-name"
slug = "owner/repo"

[[featured.releases]]
version = "1.0.0"
tree_hash = "sha256:..."
```

Installing or updating a featured plugin verifies the fetched tree against
the pin: a mismatch refuses the install, an unlisted newer version installs
as ordinary unvalidated community code. The index is compiled into the
binary, so a featuring only takes effect in an aoe build that includes the
edit. (`aoe plugin hash ./dir` prints the same hash for a local directory if
you want to inspect it by hand.)

Updates and capabilities interact conservatively. `aoe plugin outdated`
compares the recorded clone commit (GitHub) or re-hashes the source
directory (local path); `aoe plugin update <id>` re-prompts only when the
new manifest's capability set differs from the granted one, otherwise the
grant is silently re-pinned to the new manifest hash. The opt-in
auto-updater (`updates.auto_update_plugins`, off by default) never
applies a capability-changing update; it leaves it pending until the user
runs `aoe plugin update <id>` and approves the new set.

## Starting point

Copy `contrib/plugin-template/` for a working skeleton: a manifest
exercising a setting, an action with a keybind, a dashboard card, and a
Node worker (`worker.mjs`, node builtins only) that implements the
protocol above and pushes one `ui.state.set` example.
