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
api_version = 99

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
    assert!(all.contains("api_version 99"), "{all}");
    assert!(all.contains("version must not be empty"), "{all}");
    assert!(all.contains("cli-top-level"), "{all}");
    assert!(all.contains("undeclared action"), "{all}");
    assert!(all.contains("pane-read"), "{all}");
    assert!(all.contains("[runtime]"), "{all}");
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
