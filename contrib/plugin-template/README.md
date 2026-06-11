# AoE Plugin Template

A minimal, working Agent of Empires plugin skeleton. It exercises one of
each common contribution: a setting, an action with a TUI keybind
(`ctrl+t`), a dashboard card pushed over the host API, and a Node worker
speaking the newline-delimited JSON-RPC protocol with no dependencies
beyond `node:` builtins.

## Use it

```sh
cp -r contrib/plugin-template ~/my-plugin
cd ~/my-plugin
# 1. Pick your own id in aoe-plugin.toml (and update the contribution_id
#    references in worker.mjs if you rename the ui contribution).
# 2. Make sure the worker stays executable: chmod +x worker.mjs
aoe plugin install ./             # or: aoe plugin install ~/my-plugin
```

Installing copies the directory into the app data dir, so after editing
your source run `aoe plugin update <your-id>` to re-stage it. Check the
result with `aoe plugin info <your-id>`, open the TUI, and press `ctrl+t`;
the "Plugin Template" card (command palette, "Plugin panels") counts your
refreshes.

## Where to go next

- Developer guide: `docs/development/writing-plugins.md` (manifest
  surfaces, worker protocol, host API, shipping and featuring).
- Manifest schema source of truth: `aoe-plugin-api/src/manifest.rs`.
- User-facing plugin docs: `docs/plugins.md`.
