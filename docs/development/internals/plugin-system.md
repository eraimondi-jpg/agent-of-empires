# Plugin System Internals

Code-level design for the plugin system (issue #268). The high-level architecture (three tiers, the core/plugin boundary test, the trust and distribution model, status detection as the first extraction) was agreed in the issue thread; this document pins the runtime mechanism, the contribution registries, the storage shapes, and the phased implementation order. Each decision below is grounded in the current tree; file references describe the code as of the design pass.

The `aoe-plugin-api` crate (workspace member) holds the manifest and capability types described here. It is the stable surface a plugin author compiles against; the host side lands in later phases.

## Decision summary

| # | Decision |
|---|---|
| D1 | Plugin runtime is out-of-process JSON-RPC subprocesses on a generalized `src/process/` substrate. WASM and embedded scripting rejected. |
| D2 | The Core Event Bus generalizes the existing event-store pattern: durable seq log, best-effort broadcast, replay-on-connect. Publication only; never the write path. |
| D3 | Plugin settings live in an explicit typed `Config.plugins` map and a runtime settings-schema registry. Root-level `serde(flatten)` rejected. |
| D4 | CLI: core keeps clap derive; plugin commands are grafted into the derived tree at runtime. `aoe plugin` is reserved for plugin management. |
| D5 | Keybinds and actions: hybrid `ActionId { Core, Plugin }`, canonical strings externally; static core bindings table chained with a runtime plugin table. |
| D6 | Per-session plugin data: namespaced `plugin_meta` map on `Instance` plus a CAS host API. Triage stays core-typed; no generic state engine until a second consumer exists. |
| D7 | Status detection is the first reference plugin: Tier 0 declarative rules in-core for simple agents, Tier 1 batched RPC for complex parsers. |
| D8 | Security: capability gating at the host API boundary is enforcement for cooperative plugins, not a sandbox. OS-level isolation is a separate `SandboxBackend`. |

## D1. Runtime mechanism: subprocess JSON-RPC on a generalized substrate

The ACP subsystem already contains a protocol-agnostic subprocess substrate:

- `src/acp/runner.rs`: the `aoe __acp-runner` shim owns the child process across daemon restarts, binds a unix socket, proxies stdio bytes, and ring-buffers child output while detached. The wire format is newline-delimited JSON-RPC with no shim-specific framing.
- `src/acp/worker_registry.rs`: on-disk JSON records (`<app_dir>/acp-workers/<id>.json`) carrying pid, socket path, and build version; the source of truth for reattach across daemon restarts.
- `src/acp/supervisor.rs`: spawn/attach/shutdown orchestration with concurrency caps, respawn budgets, restart history, and RAII reservation guards.
- Transport: the `agent-client-protocol` crate's `ByteStreams`, generic ndjson JSON-RPC over any `AsyncRead`/`AsyncWrite` pair.

None of that is ACP-specific. The plugin host reuses it instead of introducing a second runtime:

- The substrate moves to a neutral `src/process/` module (runner, worker registry, supervisor). Both `src/acp/` and the plugin host consume it. ACP must never depend on plugin-host concepts; the dependency arrow points from both consumers down to `src/process/`, not between them.
- A Tier 1 plugin is an executable speaking ndjson JSON-RPC on stdio or a unix socket, supervised exactly like an ACP worker: registry record, respawn budget, concurrency cap.
- Plugins can be written in any language; the protocol is plain JSON-RPC.

WASM (wasmtime/extism) was rejected for v1: it adds a large dependency, requires host-binding boilerplate for everything a plugin actually needs (tmux, git, filesystem, network), and the things plugins need most are exactly the things a WASM sandbox would have to punch holes for. Embedded scripting (Lua/Rhai) was rejected: in-process crashes, no language choice, weak protocol boundaries. The manifest and capability model below is mechanism-agnostic, so a WASM tier could be added later without redesign.

Tier 2 (trusted native) remains compile-time Rust in-tree, registered through the same contribution registries. No `dlopen`; Rust has no stable ABI worth building on.

## D2. Core Event Bus: generalize the event-store pattern

The open question from #268 was whether live streaming (ACP agent output fanout) survives a pure event/command boundary or forces a dedicated RPC stream channel into the contribution model. The answer is in the tree already: streaming works today as

1. a durable per-session monotonically increasing seq log in SQLite (`src/acp/event_store.rs`),
2. a best-effort tokio broadcast channel for live delivery,
3. replay-from-disk on connect or lag (`src/server/acp_ws.rs` subscribes before snapshotting, then drains the replay so nothing is dropped).

Both the web dashboard and the native TUI structured view already consume the identical surface. That triple is an event-boundary streaming design; the Core Event Bus generalizes it rather than inventing anything:

- `src/events/` owns an `EventBus` keyed by topic (`session.created`, `status.changed`, `plugin.<id>.<event>`), with the same seq log + broadcast + replay semantics.
- Plugins subscribe over JSON-RPC (`events.subscribe { topics, after_seq }`) and receive events as notifications on their worker socket. Publication goes through a capability-checked `events.publish`.
- The bus is publication only. Writes flow through core services (storage mutators, tmux operations) which then publish post-mutation facts. The bus never enforces invariants and is never the mutation authority.
- High-volume pane snapshots do not enter the durable log. Status detection receives pane text via request/response RPC (D7); only semantic results (`status.changed`) become events.

This also sets the web dashboard decoupling path: the server today mutates session storage directly (`Storage::update` with flock, reload loops in `src/server/mod.rs`). Once mutations route through core services and reads ride the bus, the dashboard becomes one more consumer, which is the prerequisite for extracting it (Phase P9).

## D3. Settings: typed plugins map plus a runtime schema registry

Two hard facts drive this design:

1. `Config` does not preserve unknown TOML keys. There is no flatten or extra map on the struct (`src/session/config.rs`), so unknown keys are silently dropped on the next save. Plugin settings stored as unknown keys would be destroyed.
2. The settings schema is a static compile-time list: `schema()` concatenates the section descriptor lists emitted by `#[derive(SettingsSection)]` (`src/session/settings_schema/registry.rs:14`). The TUI and web render from it; the server validates PATCHes against it.

The fix is one explicit typed field, not a flatten map (a root flatten turns every typo into "plugin data" and the `toml` crate's flatten handling is historically buggy):

```rust
#[serde(default)]
pub plugins: BTreeMap<String, PluginConfig>,

pub struct PluginConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub settings: toml::Table,
}
```

```toml
[plugins."aoe.status"]
enabled = true

[plugins."aoe.status".settings]
poll_interval_ms = 1000
```

Settings of a disabled plugin stay on disk but disappear from every rendered surface; re-enabling restores them. No dead toggles (acceptance criterion 3).

Schema contribution: `schema()` grows a runtime layer. Core sections stay compile-time derived exactly as today; enabled plugin manifests contribute additional `FieldDescriptor`-shaped sections at startup:

```rust
pub fn runtime_schema(registry: &PluginRegistry) -> Vec<FieldDescriptor> {
    let mut all = schema(); // compile-time core sections, unchanged
    all.extend(registry.enabled_setting_descriptors());
    all
}
```

The TUI settings screen, the web `GET /api/settings/schema`, and the server PATCH validator all consume `runtime_schema` instead of `schema`, so plugin settings render generically on both surfaces with zero per-plugin UI code (acceptance criterion 2).

Default-override resolution (acceptance criterion 5): a manifest may override another plugin's default with a declared priority. Resolution order:

1. user-set value in config.toml,
2. highest-priority enabled plugin default override,
3. owning plugin's manifest default,
4. schema type default.

The resolved value carries provenance:

```rust
pub struct ResolvedSetting {
    pub key: String,
    pub value: serde_json::Value,
    pub source: SettingSource, // UserConfig | PluginDefault { plugin, priority } | ManifestDefault | SchemaDefault
    pub candidates: Vec<SettingCandidate>, // the full losing chain
}
```

Inspectable via `aoe settings explain <key>` and `GET /api/settings/resolved`.

## D4. CLI: keep clap derive, graft plugin commands at runtime

The CLI today is a compile-time clap enum plus a dispatch match and two telemetry tables (`src/cli/definition.rs`, `src/main.rs`). Two extremes were debated and both rejected:

- Replacing the whole tree with the clap builder API loses derive ergonomics, compile-time exhaustiveness on core dispatch, and generated help quality, for no product gain.
- A `aoe plugin <id> <cmd>` ghetto for all plugin commands breaks the long-term goal: when an existing core feature (triage commands, `aoe serve`) is extracted into a plugin, its commands must keep their current paths or user muscle memory breaks.

The synthesis: clap derive produces a `Command` via `CommandFactory`; runtime subcommands can be appended to it. Core stays derived; plugins graft in:

```rust
let mut root = Cli::command(); // derive-generated, unchanged
for cmd in plugin_registry.cli_commands() {
    root = graft(root, &cmd.path, build_clap_command(cmd)); // top-level or nested, e.g. ["session", "archive"]
}
let matches = root.get_matches();
match Cli::from_arg_matches(&matches) {
    Ok(cli) => dispatch_core(cli),                  // existing exhaustive match, untouched
    Err(_) if plugin_registry.owns(&matches) => plugin_registry.dispatch_cli(&matches),
    Err(e) => e.exit(),
}
```

Rules:

- Name conflicts: core wins, always. A manifest command colliding with a core path is rejected at manifest load with a visible error, not shadowed.
- A plugin command declares a `path` (`["review"]` for a top-level verb, `["session", "archive"]` to slot under an existing group). Top-level placement requires the `cli-top-level` capability so the install prompt surfaces it.
- `aoe plugin` is reserved for management: `install`, `list`, `enable`, `disable`, `info`.
- Telemetry: the existing command-name allowlist tables do not enumerate plugin commands; plugin invocations are recorded generically as `plugin:<id>:<command>`. Core tables stay as they are.
- Disabling a plugin removes its grafted subcommands on the next invocation (the tree is built per-invocation from the enabled set), satisfying acceptance criterion 3 for the CLI surface.

## D5. Keybinds and actions: hybrid ID, chained tables

Today `ActionId` is a compile-time enum (`src/tui/home/bindings.rs:32`) and `BINDINGS` a static table resolved by linear scan with context guards. Both stay, with one extension each:

```rust
pub enum ActionId {
    Core(CoreActionId),            // today's enum, renamed; exhaustive core dispatch unchanged
    Plugin(PluginActionId),        // (plugin id, action name)
}
```

Externally (manifests, config, telemetry, web payloads) actions are canonical strings (`core.session.archive`, `plugin.aoe.example.run_review`) via `Display`/`FromStr`. Internally exactly one dispatcher branch routes `Plugin(_)` through a registry to the owning plugin's RPC (`actions.invoke`); everything else treats `ActionId` as an opaque key. Compile-time exhaustiveness on core dispatch is preserved; strings-everywhere was rejected because it converts every future core refactor from a compiler error into a runtime dispatch miss.

Bindings: the static core table stays; a runtime `Vec<Binding>` populated from enabled manifests is chained after it. Resolution scans core first, then plugin bindings by declared priority. Conflicts are not silent: the resolver records active vs shadowed bindings with source and priority, surfaced alongside settings provenance.

Themes are already runtime data-driven TOML scanned from disk, so a manifest theme contribution is just a file the host copies into the theme search path. Cheapest contribution; lands first.

## D6. Per-session plugin data: namespaced map now, state engine later

`Instance` has no generic metadata today; triage flags (`archived_at`, `snoozed_until`, `favorited_at`, `pinned_at`) are typed fields whose mutators enforce mutual exclusion (archive clears favorite, snooze, pin; touch auto-unarchives) and feed the attention-sort key (`src/session/instance.rs`, `src/session/groups.rs`).

v1 adds only:

```rust
#[serde(default)]
pub plugin_meta: BTreeMap<String, serde_json::Value>, // keyed by plugin id
```

with a v013 migration and a host API:

- `session.meta.get { session_id }`
- `session.meta.set { session_id, value }`
- `session.meta.cas { session_id, expected, value }` (compare-and-swap, because the daemon, TUI, and CLI mutate storage concurrently)

A plugin reads and writes only its own namespace; writes go through the storage service and publish a `session.meta.changed` event.

Triage stays core-typed in v1, deliberately. A generic "exclusive state family" engine was debated and deferred: designing it against a single consumer (triage) produces a triage-shaped abstraction that gets redesigned when the second consumer arrives. The hot-path sorting concern is solvable either way (compute the sort tuple on mutation and cache it), so performance is not the blocker; entanglement is. Triage extraction is plugin #3 in the agreed proving sequence, and the state primitive gets designed at that point, against attention-sort consumption (plugin #2) and triage ownership simultaneously. Retention policy for `plugin_meta` of uninstalled plugins is a setting (`plugins.retain_orphaned_meta`, default keep) handled in the same phase.

## D7. Status detection: the first reference plugin

Status detection is the most plugin-ready behavior in the tree: dispatch is already a function pointer on the agent registry (`detect_status: fn(&str) -> Status`, `src/agents.rs:119`) and most detectors are keyword/marker matching over ~50 lines of pane text captured at about 1 Hz for hot sessions. Three coupling points need cleanup first: the claude/codex reconcile dispatch is name-matched in `src/session/instance.rs`, and hook install/uninstall special-cases agent formats (JSON/TOML/YAML) by name in `src/hooks/mod.rs`. Those become declarative fields on the contribution rather than name checks.

Two-level contribution:

**Tier 0, declarative rules.** Ordered marker/regex tables in the manifest, evaluated in-core with regexes compiled once at plugin load (the `regex` crate has no backtracking, so hostile pane text cannot blow up evaluation):

```toml
[[status_detection.rules]]
status = "running"
priority = 100
contains = ["esc to interrupt"]

[[status_detection.rules]]
status = "waiting"
priority = 90
regex = "\\b(y/n|approve|continue)\\b"

[[status_detection.rules]]
status = "idle"
priority = 0
default = true
```

This covers the keyword-matching majority of agents with zero plugin code running, and keeps working while the plugin's process tier is disabled.

**Tier 1, batched RPC.** Complex parsers (codex turn-block parsing) run as a worker. The host sends one batch per plugin per poll tick, not one call per session:

```json
{ "method": "status.detect_batch", "params": { "snapshots": [
    { "session_id": "s1", "agent": "codex", "pane_text": "...", "captured_at": "..." } ] } }
```

The response carries per-snapshot results or per-snapshot errors, so one bad snapshot does not fail its siblings. Host policy: per-batch timeout with cached-previous-status fallback, max bytes per snapshot and per batch, split-and-quarantine when a specific snapshot repeatedly poisons the batch, and supervisor respawn budgets when the worker itself is sick. Batching was chosen over per-session calls because the failure domain is the plugin process either way (per-session calls stall behind the same hung event loop) and batching gives one timeout budget, one health-accounting unit, and shared parser state per tick.

Pane text only flows to a plugin that declared and was granted `pane-read`.

## D8. Security model, stated honestly

Two distinct layers, never conflated:

1. **Capability enforcement at the host API boundary.** Every JSON-RPC method the host exposes passes through one authorization middleware: `authorize(plugin_id, method, params, grants)`. Undeclared capability use is refused (acceptance criterion 4). This is CI-testable and stops cooperative plugins from drifting beyond their manifest. It is not a sandbox: a subprocess can open files, sockets, and exec children regardless of what the host RPC layer refuses.
2. **OS-level isolation, separate and progressive.** A `SandboxBackend` trait with `NoSandbox` first, then a restricted-env backend (stripped env, controlled cwd, explicit socket/path passing), then landlock/bubblewrap on Linux and the sandbox-exec equivalent on macOS. Required before arbitrary-slug installs can be described as isolated; until then the install prompt language says exactly what is and is not enforced.

Grants are persisted pinned to a manifest hash:

```toml
[plugin_grants."example.plugin"]
manifest_hash = "sha256:..."
granted_at = "..."
capabilities = ["events-subscribe", "pane-read"]
```

A plugin update that changes its declared capability set no longer matches the stored hash and re-prompts.

### Capability taxonomy v1

Sized to what the first three plugins (status detection, attention sort, triage) plus the acceptance criteria need, nothing speculative:

| Capability | Grants |
|---|---|
| `sessions-read` | read the session list and instance fields |
| `sessions-meta-write` | write the plugin's own `plugin_meta` namespace |
| `pane-read` | receive captured tmux pane text |
| `events-subscribe` | subscribe to declared bus topics |
| `events-publish` | publish under `plugin.<id>.*` topics |
| `process-spawn` | ask the host to spawn a subprocess |
| `net-fetch` | outbound HTTP through the host |
| `fs-read` / `fs-write` | scoped filesystem access through host RPCs |
| `agent-reconcile` | contribute hook/pane status reconciliation |
| `agent-hooks` | contribute agent hook install/uninstall declarations |
| `cli-top-level` | place a CLI command at the top level of the tree |

Tier 0 contributions (settings, keybinds, themes, declarative rules) are implicit and need no runtime capability. A future "security plugin" that gates other plugins' capability use is out of scope for v1; the manifest reserves a `gates` field name for it.

## The ACP seam (open question 2 of #268)

Core keeps the generic substrate: process supervision (`src/process/` after the lift), the event store and bus, the single-writer state actor pattern, and the fs/terminal delegation handler interfaces. The ACP plugin layer owns protocol semantics: the ACP client method shapes, approvals and permission building, the agent registry and compatibility checks, agent profiles, and context priming. Core's `Instance` already needs only `acp_session_id` from all of ACP, which confirms the split is real rather than aspirational. The extraction itself is late-phase (after the contribution model is proven); nothing in earlier phases blocks on it.

## Implementation phases

Each phase is an independently shippable PR chain, filed as a sub-issue of #268:

| Phase | Content |
|---|---|
| P1 | This design doc plus the `aoe-plugin-api` crate (manifest and capability types, parse and validation, no host code). |
| P2 | `Instance.plugin_meta` + v013 migration; typed `Config.plugins` field. Pure storage groundwork, no behavior. |
| P3 | Substrate lift: `src/acp/{runner,worker_registry,supervisor}` to `src/process/`; event store generalized into `src/events/`; ACP becomes a consumer. No behavior change, heavy on tests. |
| P4 | Manifest loading, capability grants (hash-pinned, one-time prompt), `aoe plugin` management CLI, lockfile, curated index plus gh-slug install. |
| P5 | Tier 0 registries: runtime settings schema merge with provenance and `settings explain`; hybrid ActionId and chained bindings; CLI grafting; theme contributions. |
| P6 | Tier 1 host: plugin worker protocol over `src/process/`, host API with the capability middleware, `SandboxBackend` with `NoSandbox`. |
| P7 | Status detection reference plugin: Tier 0 rules for simple agents, Tier 1 batched codex worker, de-name-matching of reconcile and hook install. The acceptance-criteria milestone. |
| P8 | Runtime web-dashboard disable independent of the cargo feature; CI job running core with all default plugins disabled (acceptance criterion 1). |
| P9 | Attention-sort plugin (consumption proof), then triage extraction with the exclusive-state primitive designed against both, then web mutations through core services and web crate extraction. |

## Relationship to the issue #268 acceptance criteria

1. Core shippable with all default plugins disabled: P8 CI job.
2. One manifest contributes a CLI command, a setting, a TUI keybind, and a web setting with zero core changes: P5 registries plus a fixture plugin exercised in CI.
3. Disabling removes contributions cleanly: enabled-set-derived registries everywhere; settings persist hidden.
4. Arbitrary-slug install prompts once for declared capabilities and refuses undeclared use: P4 grants plus the P6 authorization middleware.
5. Cross-plugin default override with inspectable priority: D3 resolution chain and `ResolvedSetting` provenance.
