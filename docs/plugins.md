# Plugins

Agent of Empires keeps its core small (sessions, tmux, worktrees) and ships
everything else as plugins you can enable, disable, install, update, and
remove at runtime. First-party plugins are bundled with the binary and
enabled by default; community plugins install from a GitHub slug or a local
directory at your own risk, behind an explicit capability approval.

## Managing plugins

Three equivalent surfaces:

- **CLI**: `aoe plugin list`, `aoe plugin info <id>`, `aoe plugin install
  <owner/repo | path>`, `aoe plugin enable|disable <id>`, `aoe plugin update
  <id>`, `aoe plugin uninstall <id>`.
- **TUI**: open the command palette and run "Manage plugins". Space toggles
  enable/disable, `i` installs, `u` updates, `x` uninstalls.
- **Web dashboard**: Settings, then the Plugins tab. The same list, toggles,
  install form, and capability approval dialog. Plugin mutations require an
  elevated (passphrase) session when login is enabled and are blocked in
  read-only mode.

Disabling a plugin removes its commands, keybinds, settings rows, and themes
from every surface on the next render; its settings stay on disk and come
back when re-enabled.

## Bundled plugins

| Plugin | What it does | Disabled behavior |
|---|---|---|
| `aoe.status` | Agent status detection: declarative rules plus a worker for complex parsers (codex). Also gives custom `--cmd` agents basic running/waiting detection. | Falls back to the builtin in-core detectors; custom agents show idle. |
| `aoe.attention` | Attention-sort metadata (extraction in progress). | Attention sort keeps working from core. |
| `aoe.web` | The web dashboard management marker (serve builds only). | `aoe serve` refuses to start until re-enabled. |

## Installing community plugins

```sh
aoe plugin install owner/repo      # shallow git clone from GitHub
aoe plugin install ./my-plugin     # local directory with aoe-plugin.toml
```

Before anything is written, aoe shows the plugin's declared capabilities
(for example `pane-read`, `net-fetch`, `sessions-meta-write`) and asks once.
The approval is pinned to the exact manifest: if an update changes the
declared capability set, the plugin's runtime contributions deactivate until
you approve again.

Be honest with yourself about what that approval means: capability gating
controls what a plugin can ask aoe to do through its API. It is not an OS
sandbox; a plugin's worker process runs with your user's permissions.
OS-level isolation backends are planned and will tighten this over time.

## Keeping plugins up to date

```sh
aoe plugin outdated         # check every community plugin against its source
aoe plugin update <id>      # fetch and install; re-prompts if capabilities changed
```

The check is cheap: GitHub installs compare the recorded commit against
`git ls-remote` (no clone), local-path installs re-hash the source
directory. The TUI plugin manager checks with `c`, and the web Plugins tab
has a "Check for updates" button; both mark plugins with an available
update.

Set "Auto-update Plugins" in Settings (Updates section, `updates.
auto_update_plugins` in config.toml, off by default) to update community
plugins automatically at startup. Auto-update is deliberately conservative:
an update that changes a plugin's declared capability set is NEVER applied
silently; it stays pending until you run `aoe plugin update <id>` and
approve the new set yourself.

## Featured plugins

Featured plugins are community plugins the AoE maintainers vouch for at
specific releases. The binary ships a curated index (`plugins/featured.toml`
in the repository) pinning each vetted version to a content hash of the
whole plugin directory. Installing or updating a featured plugin verifies
the fetched files against that pin:

- Hash matches: the capability prompt marks the release as validated.
- Hash mismatch for a pinned version: the install is refused outright; the
  source may have been tampered with.
- Version not in the index yet (newer than the last curation pass): it
  installs as an ordinary, unvalidated community plugin and the prompt says
  so.

Updates compare the same tree hash, so `aoe plugin update` catches releases
that change code without touching the manifest. To compute the hash for a
release (for example to submit a plugin for featuring):

```sh
aoe plugin hash ./my-plugin
```

## Plugin settings

A plugin's settings render in the normal settings surfaces (TUI Settings
under Plugins, web Settings Plugins tab) with no extra UI code, and are
stored under `[plugins."<id>".settings]` in `config.toml`. Plugins may also
override setting DEFAULTS by declared priority, both another plugin's
settings and core settings (`target = "session.auto_archive"` style); a
value you chose yourself always wins, and a core override only applies
while the field still sits at its built-in default. Inspect any resolution
with:

```sh
aoe settings explain <plugin-id>.<key>     # plugin setting
aoe settings explain session.yolo_mode_default   # core setting
```

## Writing a plugin

A plugin is a directory with an `aoe-plugin.toml` manifest declaring its
contributions (settings, CLI commands, TUI keybinds, themes, status
detection rules) and, when it needs to run code, a `[runtime]` entrypoint:
an executable speaking newline-delimited JSON-RPC on stdio, in any language.
The manifest schema and capability list live in the `aoe-plugin-api` crate;
the full design is in `docs/development/internals/plugin-system.md`.
