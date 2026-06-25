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

#[test]
fn context_add_attributes_by_aoe_instance_id_env() {
    let tmp = tempfile::tempdir().unwrap();
    let env = [("XDG_CONFIG_HOME", tmp.path()), ("HOME", tmp.path())];

    // A session named "Agent One" in group g1.
    let work = tmp.path().join("repo");
    std::fs::create_dir_all(&work).unwrap();
    let add = aoe()
        .args([
            "add",
            work.to_str().unwrap(),
            "-t",
            "Agent One",
            "-g",
            "g1",
            "--tool",
            "claude",
            "-y",
        ])
        .envs(env.iter().map(|(k, v)| (*k, *v)))
        .output()
        .unwrap();
    assert!(
        add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );
    let stdout = String::from_utf8_lossy(&add.stdout);
    let id = stdout
        .lines()
        .find_map(|l| l.trim().strip_prefix("ID:"))
        .map(|s| s.trim().to_string())
        .expect("session id in add output");

    // From an UNRELATED cwd, with AOE_INSTANCE_ID set, the write must resolve
    // both the group (g1) and the author (Agent One) from the env, not the cwd.
    let elsewhere = tmp.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let added = aoe()
        .args(["context", "add", "fitted the model"])
        .current_dir(&elsewhere)
        .env("AOE_INSTANCE_ID", &id)
        .envs(env.iter().map(|(k, v)| (*k, *v)))
        .output()
        .unwrap();
    assert!(
        added.status.success(),
        "context add failed: {}",
        String::from_utf8_lossy(&added.stderr)
    );

    let show = aoe()
        .args(["context", "show", "-g", "g1"])
        .envs(env.iter().map(|(k, v)| (*k, *v)))
        .output()
        .unwrap();
    let body = String::from_utf8_lossy(&show.stdout);
    assert!(body.contains("Agent One"), "attribution missing in: {body}");
    assert!(body.contains("fitted the model"));
}
