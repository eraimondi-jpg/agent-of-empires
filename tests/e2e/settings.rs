//! E2E coverage for the Settings TUI.

use serial_test::serial;

use crate::harness::{require_tmux, TuiTestHarness};

/// The mouse-capture toggle (issue #1346) must be reachable from the Settings
/// search and editable, so the env-only `AOE_MOUSE_CAPTURE` escape hatch is no
/// longer the only knob. Search jumps to the field in the Interaction tab;
/// Space flips it from the default-on state to Disabled.
#[test]
#[serial]
fn settings_exposes_editable_mouse_capture_toggle() {
    require_tmux!();

    let mut h = TuiTestHarness::new("settings_mouse_capture");
    h.spawn_tui();

    h.wait_for("No sessions yet");
    h.send_keys("s");
    h.wait_for("Settings");

    // Settings-wide search jumps straight to the field regardless of which
    // category it lives in.
    h.send_keys("/");
    h.type_text("mouse capture");
    h.wait_for("Mouse Capture");
    h.send_keys("Enter");
    h.assert_screen_contains("Mouse Capture");

    // Default is on; toggling lands on the Disabled state.
    h.send_keys("Space");
    h.wait_for("Disabled");
}

/// The Plugins settings category hosts the plugin manager inline (#268). A
/// search hit on a builtin plugin's setting drills into that plugin's
/// settings, and Esc steps back to the manager list, which shows the installed
/// plugins. One manager implementation, shared with the command palette and
/// the web dashboard's view-model.
#[test]
#[serial]
fn settings_plugins_tab_hosts_manager_and_drills_into_settings() {
    require_tmux!();

    let mut h = TuiTestHarness::new("settings_plugins_tab");
    h.spawn_tui();

    h.wait_for("No sessions yet");
    h.send_keys("s");
    h.wait_for("Settings");

    // Search jumps to the builtin status plugin's setting and drills into it,
    // so the field is visible rather than hidden behind the manager list. Use a
    // phrase unique to that setting's description (the label alone fuzzy-matches
    // a core status field).
    h.send_keys("/");
    h.type_text("always showing them as idle");
    h.wait_for("Detect custom agent status");
    h.send_keys("Enter");
    h.assert_screen_contains("Detect custom agent status");

    // Esc steps back from the per-plugin settings to the embedded manager
    // list, which shows the installed plugins by name plus the inline footer.
    h.send_keys("Escape");
    h.wait_for("Agent Status Detection");
    h.assert_screen_contains("opens settings");

    // Toggling enable/disable STAGES like any other settings row (no immediate
    // disk write that would bypass the save flow): the title's unsaved marker
    // appears, persisted only on Ctrl-s.
    h.send_keys("Space");
    h.wait_for("Settings *");
}
