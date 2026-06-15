//! E2E coverage for the plugin management CLI (#268): list shows the bundled
//! plugins with trust and state, enable/disable round-trips through config,
//! info prints capabilities, settings explain prints provenance, and the
//! contributed worker answers a status batch.

use serial_test::serial;

use crate::harness::TuiTestHarness;

#[test]
#[serial]
fn test_plugin_list_shows_builtins_with_trust_and_state() {
    let h = TuiTestHarness::new("plugin_list");
    let output = h.run_cli(&["plugin", "list"]);
    assert!(
        output.status.success(),
        "aoe plugin list failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("aoe.status"),
        "missing aoe.status:\n{stdout}"
    );
    assert!(
        stdout.contains("builtin"),
        "missing builtin trust label:\n{stdout}"
    );
    assert!(
        stdout.contains("enabled"),
        "missing state column:\n{stdout}"
    );
}

#[test]
#[serial]
fn test_plugin_disable_enable_round_trip() {
    let h = TuiTestHarness::new("plugin_toggle");

    let disable = h.run_cli(&["plugin", "disable", "aoe.status"]);
    assert!(
        disable.status.success(),
        "disable failed: {}",
        String::from_utf8_lossy(&disable.stderr)
    );
    let list = h.run_cli(&["plugin", "list"]);
    let stdout = String::from_utf8_lossy(&list.stdout);
    let status_line = stdout
        .lines()
        .find(|l| l.contains("aoe.status"))
        .unwrap_or_else(|| panic!("aoe.status missing from list:\n{stdout}"));
    assert!(
        status_line.contains("disabled"),
        "aoe.status should be disabled:\n{status_line}"
    );

    let enable = h.run_cli(&["plugin", "enable", "aoe.status"]);
    assert!(
        enable.status.success(),
        "enable failed: {}",
        String::from_utf8_lossy(&enable.stderr)
    );
    let list = h.run_cli(&["plugin", "list"]);
    let stdout = String::from_utf8_lossy(&list.stdout);
    let status_line = stdout.lines().find(|l| l.contains("aoe.status")).unwrap();
    assert!(
        status_line.contains("enabled") && !status_line.contains("disabled"),
        "aoe.status should be enabled again:\n{status_line}"
    );
}

#[test]
#[serial]
fn test_plugin_info_prints_capabilities_and_runtime() {
    let h = TuiTestHarness::new("plugin_info");
    let output = h.run_cli(&["plugin", "info", "aoe.status"]);
    assert!(
        output.status.success(),
        "info failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("pane-read"),
        "capabilities missing:\n{stdout}"
    );
    assert!(
        stdout.contains("JSON-RPC worker"),
        "runtime line missing:\n{stdout}"
    );
}

#[test]
#[serial]
fn test_settings_explain_resolves_plugin_default() {
    let h = TuiTestHarness::new("plugin_settings_explain");
    let output = h.run_cli(&["settings", "explain", "aoe.status.custom_agent_rules"]);
    assert!(
        output.status.success(),
        "settings explain failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    // No user value set: the manifest default wins and the chain says so.
    assert!(
        stdout.contains("manifest default"),
        "expected manifest-default provenance:\n{stdout}"
    );
    assert!(
        stdout.contains("true"),
        "expected the default value in the output:\n{stdout}"
    );
}

#[test]
#[serial]
fn test_install_and_update_track_tree_hash_not_just_manifest() {
    let h = TuiTestHarness::new("plugin_tree_hash");
    let source = tempfile::tempdir().expect("plugin source dir");
    std::fs::write(
        source.path().join("aoe-plugin.toml"),
        r#"
id = "acme.demo"
name = "Demo"
version = "1.0.0"
api_version = 1
description = "Tree hash e2e fixture."

[[settings]]
key = "verbose"
label = "Verbose"
widget = { kind = "toggle" }
default = false
"#,
    )
    .unwrap();
    std::fs::write(source.path().join("notes.txt"), "v1").unwrap();
    let dir = source.path().to_str().unwrap();

    let install = h.run_cli(&["plugin", "install", dir, "--yes"]);
    assert!(
        install.status.success(),
        "install failed: {}",
        String::from_utf8_lossy(&install.stderr)
    );
    assert!(
        String::from_utf8_lossy(&install.stdout).contains("Installed acme.demo v1.0.0"),
        "unexpected install output"
    );

    let same = h.run_cli(&["plugin", "update", "acme.demo", "--yes"]);
    assert!(
        String::from_utf8_lossy(&same.stdout).contains("up to date"),
        "unchanged tree must be up to date:\n{}",
        String::from_utf8_lossy(&same.stdout)
    );

    // Code-only change: the manifest is untouched but the tree differs, so
    // the update must install it (and needs no capability re-prompt).
    std::fs::write(source.path().join("notes.txt"), "v2").unwrap();
    let changed = h.run_cli(&["plugin", "update", "acme.demo", "--yes"]);
    assert!(
        String::from_utf8_lossy(&changed.stdout).contains("Updated acme.demo to v1.0.0"),
        "code-only change must update:\n{}",
        String::from_utf8_lossy(&changed.stdout)
    );

    let uninstall = h.run_cli(&["plugin", "uninstall", "acme.demo"]);
    assert!(
        uninstall.status.success(),
        "uninstall failed: {}",
        String::from_utf8_lossy(&uninstall.stderr)
    );
}

#[test]
#[serial]
fn test_core_setting_default_override_applies_and_explains() {
    let h = TuiTestHarness::new("plugin_core_override");
    let source = tempfile::tempdir().expect("plugin source dir");
    std::fs::write(
        source.path().join("aoe-plugin.toml"),
        r#"
id = "acme.coreover"
name = "Core Override Fixture"
version = "1.0.0"
api_version = 1

[[setting_defaults]]
target = "session.yolo_mode_default"
value = true
priority = 40
reason = "fixture flips a core default"
"#,
    )
    .unwrap();
    let install = h.run_cli(&[
        "plugin",
        "install",
        source.path().to_str().unwrap(),
        "--yes",
    ]);
    assert!(
        install.status.success(),
        "install failed: {}",
        String::from_utf8_lossy(&install.stderr)
    );

    let explain = h.run_cli(&["settings", "explain", "session.yolo_mode_default"]);
    assert!(
        explain.status.success(),
        "explain failed: {}",
        String::from_utf8_lossy(&explain.stderr)
    );
    let stdout = String::from_utf8_lossy(&explain.stdout);
    assert!(
        stdout.contains("default override by plugin acme.coreover"),
        "core override must win and be attributed:\n{stdout}"
    );
    assert!(
        stdout.contains("built-in default"),
        "chain must show the losing built-in default:\n{stdout}"
    );

    // The override stops applying the moment the plugin is disabled.
    h.run_cli(&["plugin", "disable", "acme.coreover"]);
    let explain = h.run_cli(&["settings", "explain", "session.yolo_mode_default"]);
    let stdout = String::from_utf8_lossy(&explain.stdout);
    assert!(
        stdout.contains("resolved from: built-in default"),
        "disabled plugin must not override:\n{stdout}"
    );

    h.run_cli(&["plugin", "uninstall", "acme.coreover"]);
}

#[test]
#[serial]
fn test_outdated_reports_path_source_drift() {
    let h = TuiTestHarness::new("plugin_outdated");
    let source = tempfile::tempdir().expect("plugin source dir");
    std::fs::write(
        source.path().join("aoe-plugin.toml"),
        r#"
id = "acme.outdated"
name = "Outdated Fixture"
version = "1.0.0"
api_version = 1
"#,
    )
    .unwrap();
    let dir = source.path().to_str().unwrap();
    let install = h.run_cli(&["plugin", "install", dir, "--yes"]);
    assert!(
        install.status.success(),
        "install failed: {}",
        String::from_utf8_lossy(&install.stderr)
    );

    let fresh = h.run_cli(&["plugin", "outdated"]);
    let stdout = String::from_utf8_lossy(&fresh.stdout);
    assert!(
        stdout.contains("acme.outdated") && stdout.contains("up to date"),
        "fresh install must be up to date:\n{stdout}"
    );

    std::fs::write(source.path().join("new-file.txt"), "drift").unwrap();
    let drifted = h.run_cli(&["plugin", "outdated"]);
    let stdout = String::from_utf8_lossy(&drifted.stdout);
    assert!(
        stdout.contains("update available"),
        "source drift must report an available update:\n{stdout}"
    );

    h.run_cli(&["plugin", "uninstall", "acme.outdated"]);
}

/// Regression for the grafted-command dispatch bug: a plugin CLI command is
/// unknown to the core clap derive, which parses it as `command: None`. The
/// dispatcher must claim it and route to the worker instead of falling
/// through to the bare-`aoe` TUI launch ("requires an interactive TTY").
#[cfg(unix)]
#[test]
#[serial]
fn test_grafted_cli_command_dispatches_to_worker() {
    use std::os::unix::fs::PermissionsExt;

    let h = TuiTestHarness::new("plugin_graft_dispatch");
    let source = tempfile::tempdir().expect("plugin source dir");
    std::fs::write(
        source.path().join("aoe-plugin.toml"),
        r#"
id = "acme.cmd"
name = "Command Fixture"
version = "1.0.0"
api_version = 1

[[commands]]
path = ["acmedemo", "ping"]
about = "ping the worker"
rpc_method = "demo.ping"

[runtime]
entrypoint = "worker.sh"
"#,
    )
    .unwrap();
    // Portable ndjson JSON-RPC worker: echo a result with the request's id.
    let worker = source.path().join("worker.sh");
    std::fs::write(
        &worker,
        "#!/bin/sh\nwhile IFS= read -r line; do\n  id=$(printf '%s' \"$line\" | sed -n 's/.*\"id\":\\([0-9]*\\).*/\\1/p')\n  [ -z \"$id\" ] && continue\n  printf '{\"jsonrpc\":\"2.0\",\"id\":%s,\"result\":\"GRAFTED_OK\"}\\n' \"$id\"\ndone\n",
    )
    .unwrap();
    std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o755)).unwrap();

    let install = h.run_cli(&[
        "plugin",
        "install",
        source.path().to_str().unwrap(),
        "--yes",
    ]);
    assert!(
        install.status.success(),
        "install failed: {}",
        String::from_utf8_lossy(&install.stderr)
    );

    let output = h.run_cli(&["acmedemo", "ping"]);
    assert!(
        output.status.success(),
        "grafted command did not dispatch (fell through to TUI?): {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("GRAFTED_OK"),
        "worker result missing from grafted command output:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );

    h.run_cli(&["plugin", "uninstall", "acme.cmd"]);
}

#[test]
#[serial]
fn test_builtin_worker_answers_status_batch() {
    let h = TuiTestHarness::new("plugin_worker_batch");
    let request = r#"{"jsonrpc":"2.0","id":1,"method":"status.detect_batch","params":{"snapshots":[{"session_id":"s1","agent":"codex","pane_text":"Working (esc to interrupt)"}]}}"#;
    let output = h.run_cli_with_stdin(&["__plugin-worker", "--id", "aoe.status"], request);
    assert!(
        output.status.success(),
        "worker failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("\"session_id\":\"s1\"") && stdout.contains("\"status\""),
        "expected a per-snapshot result:\n{stdout}"
    );
}
