use aoe_plugin_api::{Capability, DetectionMode, ManifestError, PluginManifest, SettingWidget};

const FULL: &str = r#"
id = "aoe.status"
name = "Status Detection"
version = "0.1.0"
api_version = 1
description = "Per-agent status detection."
capabilities = ["pane-read", "events-publish", "cli-top-level"]

[[settings]]
key = "poll_interval_ms"
label = "Poll interval"
description = "How often hot panes are sampled."
widget = { kind = "number", min = 100, max = 10000 }
default = 1000

[[setting_defaults]]
target = "aoe.triage.auto_unarchive"
value = true
priority = 50
reason = "status plugin works best with auto unarchive"

[[commands]]
path = ["status"]
about = "Print detected status for a session"
rpc_method = "cli.status"

[[commands.args]]
name = "session"
required = true
help = "Session id or title"

[[actions]]
name = "redetect"
label = "Re-run status detection"
rpc_method = "actions.redetect"

[[keybinds]]
action = "redetect"
chord = "ctrl+r"
priority = 10

[[themes]]
file = "themes/status-dark.toml"

[[status_detection]]
agent = "claude"
mode = "declarative"

[[status_detection.rules]]
status = "running"
priority = 100
contains = ["esc to interrupt"]

[[status_detection.rules]]
status = "waiting"
priority = 90
regex = '\b(y/n|approve)\b'

[[status_detection.rules]]
status = "idle"
default = true

[[status_detection]]
agent = "codex"
mode = "rpc"
method = "status.detect_batch"

[runtime]
entrypoint = "bin/status-worker"
args = ["--socket-mode"]
"#;

#[test]
fn full_manifest_parses_and_round_trips() {
    let manifest = PluginManifest::from_toml_str(FULL).expect("fixture must parse");
    assert_eq!(manifest.id.as_str(), "aoe.status");
    assert_eq!(
        manifest.capabilities,
        vec![
            Capability::PaneRead,
            Capability::EventsPublish,
            Capability::CliTopLevel
        ]
    );
    assert!(matches!(
        manifest.settings[0].widget,
        SettingWidget::Number {
            min: Some(_),
            max: Some(_)
        }
    ));
    assert!(
        matches!(manifest.status_detection[0].mode, DetectionMode::Declarative { ref rules } if rules.len() == 3)
    );
    assert!(
        matches!(manifest.status_detection[1].mode, DetectionMode::Rpc { ref method } if method == "status.detect_batch")
    );

    let serialized = toml::to_string(&manifest).expect("manifest must serialize");
    let reparsed =
        PluginManifest::from_toml_str(&serialized).expect("serialized form must reparse");
    assert_eq!(reparsed.id, manifest.id);
    assert_eq!(reparsed.commands[0].path, manifest.commands[0].path);
}

#[test]
fn minimal_declarative_manifest_needs_no_runtime() {
    let manifest = PluginManifest::from_toml_str(
        r#"
id = "aoe.theme-pack"
name = "Theme Pack"
version = "1.0.0"
api_version = 1

[[themes]]
file = "themes/extra.toml"
"#,
    )
    .expect("tier 0 manifest must parse");
    assert!(manifest.runtime.is_none());
    assert!(manifest.capabilities.is_empty());
}

#[test]
fn min_aoe_version_is_optional_and_round_trips() {
    let absent = PluginManifest::from_toml_str(
        r#"
id = "aoe.no-min"
name = "No Min"
version = "1.0.0"
api_version = 1
"#,
    )
    .expect("manifest without min_aoe_version must parse");
    assert_eq!(absent.min_aoe_version, None);

    let manifest = PluginManifest::from_toml_str(
        r#"
id = "aoe.with-min"
name = "With Min"
version = "1.0.0"
api_version = 1
min_aoe_version = "0.5.0"
"#,
    )
    .expect("manifest with min_aoe_version must parse");
    assert_eq!(manifest.min_aoe_version.as_deref(), Some("0.5.0"));

    let serialized = toml::to_string(&manifest).expect("manifest must serialize");
    let reparsed =
        PluginManifest::from_toml_str(&serialized).expect("serialized form must reparse");
    assert_eq!(reparsed.min_aoe_version.as_deref(), Some("0.5.0"));
}

fn invalid_messages(input: &str) -> Vec<String> {
    match PluginManifest::from_toml_str(input) {
        Err(ManifestError::Invalid(messages)) => messages,
        other => panic!("expected validation failure, got {other:?}"),
    }
}

#[test]
fn validation_collects_all_problems() {
    let messages = invalid_messages(
        r#"
id = "aoe.broken"
name = "Broken"
version = ""
api_version = 1

[[commands]]
path = ["review"]
about = "Top level without capability"
rpc_method = "cli.review"

[[keybinds]]
action = "missing"
chord = "ctrl+x"

[[status_detection]]
agent = "codex"
mode = "rpc"
method = "status.detect_batch"
"#,
    );
    let all = messages.join("\n");
    // Too-high api_version is its own error now (see
    // newer_api_version_reports_version_not_unknown_variant), not collected
    // here; this fixture targets the validate() problem list.
    assert!(all.contains("version must not be empty"), "{all}");
    assert!(all.contains("cli-top-level"), "{all}");
    assert!(all.contains("undeclared action"), "{all}");
    assert!(all.contains("pane-read"), "{all}");
    assert!(all.contains("[runtime]"), "{all}");
}

#[test]
fn newer_api_version_reports_version_not_unknown_variant() {
    // A v2 manifest may use a capability this v1 host does not know. The error
    // must name the api_version, not the (legitimately newer) capability.
    let err = PluginManifest::from_toml_str(
        r#"
id = "acme.future"
name = "Future"
version = "0.1.0"
api_version = 2
capabilities = ["webhooks-receive"]
"#,
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            ManifestError::UnsupportedApiVersion { found: 2, max: 1 }
        ),
        "expected an api_version error, got {err:?}"
    );
}

#[test]
fn fractional_number_bound_fails_to_parse() {
    let err = PluginManifest::from_toml_str(
        r#"
id = "acme.fract"
name = "Fractional"
version = "0.1.0"
api_version = 1

[[settings]]
key = "threshold"
label = "Threshold"
widget = { kind = "number", min = 0.5, max = 10 }
"#,
    )
    .unwrap_err();
    // A fractional bound is a loud TOML type error, not a silent truncation.
    assert!(
        matches!(err, ManifestError::Parse(_)),
        "expected a parse error, got {err:?}"
    );
}

#[test]
fn traversal_and_absolute_entrypoints_are_rejected() {
    for ep in ["/bin/sh", "../../bin/python3", "sub/../../escape", ""] {
        let all = invalid_messages(&format!(
            r#"
id = "aoe.evil"
name = "Evil"
version = "0.1.0"
api_version = 1

[[actions]]
name = "go"
label = "Go"
rpc_method = "go"

[runtime]
entrypoint = "{ep}"
"#
        ))
        .join("\n");
        assert!(
            all.contains("runtime.entrypoint"),
            "entrypoint {ep:?} should be rejected, got: {all}"
        );
    }
}

#[test]
fn traversal_and_absolute_theme_files_are_rejected() {
    for file in ["/etc/passwd", "../../secret.toml", ""] {
        let all = invalid_messages(&format!(
            r#"
id = "acme.theme"
name = "Theme"
version = "0.1.0"
api_version = 1

[[themes]]
file = "{file}"
"#
        ))
        .join("\n");
        assert!(
            all.contains("theme file"),
            "theme file {file:?} should be rejected, got: {all}"
        );
    }
}

#[test]
fn duplicate_contributions_are_rejected() {
    let all = invalid_messages(
        r#"
id = "aoe.dup"
name = "Dup"
version = "1.0.0"
api_version = 1

[[settings]]
key = "x"
label = "X"
widget = { kind = "toggle" }

[[settings]]
key = "x"
label = "X again"
widget = { kind = "toggle" }
"#,
    )
    .join("\n");
    assert!(all.contains("duplicate setting key"), "{all}");
}

#[test]
fn malformed_override_targets_dup_paths_and_double_defaults_are_rejected() {
    let all = invalid_messages(
        r#"
id = "aoe.morebroken"
name = "More Broken"
version = "1.0.0"
api_version = 1
capabilities = ["cli-top-level", "pane-read"]

[[setting_defaults]]
target = "no-dot-separator"
value = 1
priority = 10

[[commands]]
path = ["review"]
about = "first"
rpc_method = "cli.review"

[[commands]]
path = ["review"]
about = "second copy"
rpc_method = "cli.review2"

[[status_detection]]
agent = "custom"
mode = "declarative"

[[status_detection.rules]]
status = "idle"
default = true

[[status_detection.rules]]
status = "running"
default = true

[runtime]
entrypoint = "worker"
"#,
    )
    .join("\n");
    assert!(all.contains("setting_defaults target"), "{all}");
    assert!(all.contains("duplicate command path"), "{all}");
    assert!(all.contains("more than one default rule"), "{all}");
}

#[test]
fn event_handlers_parse_and_round_trip() {
    let manifest = PluginManifest::from_toml_str(
        r#"
id = "acme.watcher"
name = "Watcher"
version = "0.1.0"
api_version = 1
capabilities = ["events-subscribe"]

[[event_handlers]]
on = "session.created"
rpc_method = "on_session"

[[event_handlers]]
on = "plugin.acme.*"
rpc_method = "on_plugin"

[runtime]
entrypoint = "worker"
"#,
    )
    .expect("event-handler manifest must parse");
    assert_eq!(manifest.event_handlers.len(), 2);
    assert_eq!(manifest.event_handlers[0].on, "session.created");
    assert_eq!(manifest.event_handlers[1].rpc_method, "on_plugin");

    let serialized = toml::to_string(&manifest).expect("must serialize");
    let reparsed = PluginManifest::from_toml_str(&serialized).expect("must reparse");
    assert_eq!(
        reparsed.event_handlers[0].rpc_method,
        manifest.event_handlers[0].rpc_method
    );
}

#[test]
fn event_handlers_require_runtime_and_capability() {
    // No [runtime] and no events-subscribe capability: both problems collected.
    let all = invalid_messages(
        r#"
id = "acme.watcher"
name = "Watcher"
version = "0.1.0"
api_version = 1

[[event_handlers]]
on = "session.created"
rpc_method = "on_session"
"#,
    )
    .join("\n");
    assert!(all.contains("[runtime]"), "{all}");
    assert!(all.contains("events-subscribe"), "{all}");
}

#[test]
fn event_handler_empty_fields_are_rejected() {
    let all = invalid_messages(
        r#"
id = "acme.watcher"
name = "Watcher"
version = "0.1.0"
api_version = 1
capabilities = ["events-subscribe"]

[[event_handlers]]
on = ""
rpc_method = ""

[runtime]
entrypoint = "worker"
"#,
    )
    .join("\n");
    assert!(all.contains("non-empty topic and rpc_method"), "{all}");
}

#[test]
fn link_handlers_parse_and_round_trip() {
    let manifest = PluginManifest::from_toml_str(
        r#"
id = "acme.linker"
name = "Linker"
version = "0.1.0"
api_version = 1
capabilities = ["terminal-links"]

[[link_handlers]]
pattern = '#\d+'
rpc_method = "open_issue"

[runtime]
entrypoint = "worker"
"#,
    )
    .expect("link-handler manifest must parse");
    assert_eq!(manifest.link_handlers.len(), 1);
    assert_eq!(manifest.link_handlers[0].pattern, r"#\d+");
    assert_eq!(manifest.link_handlers[0].rpc_method, "open_issue");

    let serialized = toml::to_string(&manifest).expect("must serialize");
    let reparsed = PluginManifest::from_toml_str(&serialized).expect("must reparse");
    assert_eq!(reparsed.link_handlers[0].pattern, r"#\d+");
}

#[test]
fn link_handlers_require_runtime_and_capability() {
    let all = invalid_messages(
        r#"
id = "acme.linker"
name = "Linker"
version = "0.1.0"
api_version = 1

[[link_handlers]]
pattern = '#\d+'
rpc_method = "open_issue"
"#,
    )
    .join("\n");
    assert!(all.contains("[runtime]"), "{all}");
    assert!(all.contains("terminal-links"), "{all}");
}

#[test]
fn link_handler_empty_fields_are_rejected() {
    let all = invalid_messages(
        r#"
id = "acme.linker"
name = "Linker"
version = "0.1.0"
api_version = 1
capabilities = ["terminal-links"]

[[link_handlers]]
pattern = ""
rpc_method = ""

[runtime]
entrypoint = "worker"
"#,
    )
    .join("\n");
    assert!(all.contains("non-empty pattern and rpc_method"), "{all}");
}

#[test]
fn panes_parse_and_round_trip() {
    let manifest = PluginManifest::from_toml_str(
        r#"
id = "acme.panes"
name = "Panes"
version = "0.1.0"
api_version = 1
capabilities = ["terminal-pane"]

[[panes]]
id = "logs"
title = "Tail logs"
command = ["tail", "-f", "log.txt"]
"#,
    )
    .expect("pane manifest must parse");
    assert_eq!(manifest.panes.len(), 1);
    assert_eq!(manifest.panes[0].id, "logs");
    assert_eq!(manifest.panes[0].command, ["tail", "-f", "log.txt"]);

    let serialized = toml::to_string(&manifest).expect("must serialize");
    let reparsed = PluginManifest::from_toml_str(&serialized).expect("must reparse");
    assert_eq!(reparsed.panes[0].command, manifest.panes[0].command);
}

#[test]
fn panes_require_capability_but_not_runtime() {
    // Panes are host-spawned, so no [runtime] is needed, but the capability is.
    let manifest = PluginManifest::from_toml_str(
        r#"
id = "acme.panes"
name = "Panes"
version = "0.1.0"
api_version = 1
capabilities = ["terminal-pane"]

[[panes]]
id = "logs"
title = "Tail logs"
command = ["tail", "-f", "log.txt"]
"#,
    )
    .expect("pane manifest needs no runtime");
    assert!(manifest.runtime.is_none());

    let all = invalid_messages(
        r#"
id = "acme.panes"
name = "Panes"
version = "0.1.0"
api_version = 1

[[panes]]
id = "logs"
title = "Tail logs"
command = ["tail"]
"#,
    )
    .join("\n");
    assert!(all.contains("terminal-pane"), "{all}");
}

#[test]
fn pane_empty_command_and_dup_ids_are_rejected() {
    let all = invalid_messages(
        r#"
id = "acme.panes"
name = "Panes"
version = "0.1.0"
api_version = 1
capabilities = ["terminal-pane"]

[[panes]]
id = "logs"
title = "First"
command = []

[[panes]]
id = "logs"
title = "Second"
command = ["echo", ""]
"#,
    )
    .join("\n");
    assert!(all.contains("non-empty argv"), "{all}");
    assert!(all.contains("duplicate pane id"), "{all}");
}

#[test]
fn unknown_manifest_fields_are_rejected() {
    let err = PluginManifest::from_toml_str(
        r#"
id = "aoe.typo"
name = "Typo"
version = "1.0.0"
api_version = 1
capabilitties = ["pane-read"]
"#,
    )
    .unwrap_err();
    assert!(matches!(err, ManifestError::Parse(_)), "{err:?}");
}
