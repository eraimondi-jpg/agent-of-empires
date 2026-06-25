//! Integration tests for the `aoe context` CLI. Each test runs the real `aoe`
//! binary against an isolated temp HOME/XDG so it never touches user state.

use std::process::Command;

fn aoe() -> Command {
    Command::new(env!("CARGO_BIN_EXE_aoe"))
}

#[test]
fn context_add_and_show_via_group_flag() {
    let tmp = tempfile::tempdir().unwrap();
    let env = [("XDG_CONFIG_HOME", tmp.path()), ("HOME", tmp.path())];

    let add = aoe()
        .args(["context", "add", "hello world", "--group", "g1"])
        .envs(env.iter().map(|(k, v)| (*k, *v)))
        .output()
        .unwrap();
    assert!(
        add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    let show = aoe()
        .args(["context", "show", "--group", "g1"])
        .envs(env.iter().map(|(k, v)| (*k, *v)))
        .output()
        .unwrap();
    assert!(show.status.success());
    assert!(String::from_utf8_lossy(&show.stdout).contains("hello world"));
}

#[test]
fn summaries_lists_created_groups() {
    let tmp = tempfile::tempdir().unwrap();
    let env = [("XDG_CONFIG_HOME", tmp.path()), ("HOME", tmp.path())];

    aoe()
        .args(["context", "add", "x", "--group", "alpha"])
        .envs(env.iter().map(|(k, v)| (*k, *v)))
        .output()
        .unwrap();

    let out = aoe()
        .args(["context", "summaries"])
        .envs(env.iter().map(|(k, v)| (*k, *v)))
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("alpha"));
}

#[test]
fn add_without_group_outside_session_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let env = [("XDG_CONFIG_HOME", tmp.path()), ("HOME", tmp.path())];

    let out = aoe()
        .args(["context", "add", "orphan note"])
        .current_dir(tmp.path())
        .envs(env.iter().map(|(k, v)| (*k, *v)))
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "expected failure with no group resolvable"
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("not inside a grouped aoe session"));
}
