//! E2E: the group context viewer is wired into the live TUI. We assert via the
//! command palette (deterministic, no sidebar-cursor navigation) that the
//! "View group context" action is registered and discoverable. The dialog's
//! scroll/toggle/close logic is unit-tested in
//! `src/tui/dialogs/group_context.rs`.

use std::time::Duration;

use serial_test::serial;

use crate::harness::{require_tmux, TuiTestHarness};

#[test]
#[serial]
fn group_context_command_is_in_palette() {
    require_tmux!();

    let mut h = TuiTestHarness::new("group_context_palette");
    h.spawn_tui();

    h.wait_for(" aoe ");
    h.send_keys("C-k");
    h.wait_for("Commands");

    h.type_text("group context");
    std::thread::sleep(Duration::from_millis(150));
    h.assert_screen_contains("View group context");
}
