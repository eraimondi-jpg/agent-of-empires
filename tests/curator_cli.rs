//! Integration tests for the `aoe curator` CLI. Each test runs the real `aoe`
//! binary against an isolated temp HOME/XDG and a fake `claude` shim on PATH, so
//! no real agent is needed and user state is never touched.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

/// Read a group's context.md back through the binary, so the test stays
/// independent of the on-disk profile/app-dir layout (which differs between
/// debug and release builds).
fn read_context(env: &[(&str, &str)], group: &str) -> String {
    let out = aoe()
        .args(["context", "show", "-g", group])
        .envs(env.iter().copied())
        .output()
        .unwrap();
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn read_summary(env: &[(&str, &str)], group: &str) -> String {
    let out = aoe()
        .args(["context", "summary", "-g", group])
        .envs(env.iter().copied())
        .output()
        .unwrap();
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn aoe() -> Command {
    Command::new(env!("CARGO_BIN_EXE_aoe"))
}

/// Write an executable `claude` shim that ignores its args and prints a valid
/// two-section curator response. Returns the dir to prepend to PATH.
fn install_claude_shim(dir: &Path) {
    let script = "#!/bin/sh\n\
        cat <<'EOF'\n\
        ===AOE_CONTEXT_BEGIN===\n\
        # Sysid\n\
        - net B best\n\
        ===AOE_CONTEXT_END===\n\
        ===AOE_SUMMARY_BEGIN===\n\
        sysid: actuator work\n\
        ===AOE_SUMMARY_END===\n\
        EOF\n";
    let shim = dir.join("claude");
    fs::write(&shim, script).unwrap();
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn curator_run_rewrites_context_and_summary() {
    let tmp = tempfile::tempdir().unwrap();
    let bindir = tmp.path().join("bin");
    fs::create_dir_all(&bindir).unwrap();
    install_claude_shim(&bindir);

    let path = format!(
        "{}:{}",
        bindir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let env = [
        ("XDG_CONFIG_HOME", tmp.path().to_str().unwrap()),
        ("HOME", tmp.path().to_str().unwrap()),
        ("PATH", path.as_str()),
    ];

    // Seed: create the group + context.md with a raw note.
    let add = aoe()
        .args(["context", "add", "raw note", "-g", "g1"])
        .envs(env.iter().copied())
        .output()
        .unwrap();
    assert!(
        add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    // Run the curator over the group.
    let run = aoe()
        .args(["curator", "run", "-g", "g1"])
        .envs(env.iter().copied())
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "curator run failed: stdout={} stderr={}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(
        stdout.contains("Curated"),
        "expected a Curated summary, got: {stdout}"
    );

    // The on-disk context.md must now hold the shim's curated text.
    let context = read_context(&env, "g1");
    assert!(context.contains("# Sysid"), "context.md: {context}");
    assert!(context.contains("net B best"), "context.md: {context}");

    // And summary.md must hold the shim's summary.
    let summary = read_summary(&env, "g1");
    assert!(
        summary.contains("sysid: actuator work"),
        "summary.md: {summary}"
    );
}

#[test]
fn curator_status_reports_never_then_up_to_date() {
    let tmp = tempfile::tempdir().unwrap();
    let bindir = tmp.path().join("bin");
    fs::create_dir_all(&bindir).unwrap();
    install_claude_shim(&bindir);

    let path = format!(
        "{}:{}",
        bindir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let env = [
        ("XDG_CONFIG_HOME", tmp.path().to_str().unwrap()),
        ("HOME", tmp.path().to_str().unwrap()),
        ("PATH", path.as_str()),
    ];

    aoe()
        .args(["context", "add", "raw note", "-g", "g1"])
        .envs(env.iter().copied())
        .output()
        .unwrap();

    // Before any curation: never curated, pending changes.
    let before = aoe()
        .args(["curator", "status", "-g", "g1"])
        .envs(env.iter().copied())
        .output()
        .unwrap();
    assert!(before.status.success());
    let before_out = String::from_utf8_lossy(&before.stdout);
    assert!(before_out.contains("never curated"), "{before_out}");
    assert!(before_out.contains("pending changes"), "{before_out}");

    // After a curate: state exists and nothing is pending.
    aoe()
        .args(["curator", "run", "-g", "g1"])
        .envs(env.iter().copied())
        .output()
        .unwrap();
    let after = aoe()
        .args(["curator", "status", "-g", "g1"])
        .envs(env.iter().copied())
        .output()
        .unwrap();
    assert!(after.status.success());
    let after_out = String::from_utf8_lossy(&after.stdout);
    assert!(after_out.contains("Last size:"), "{after_out}");
    assert!(after_out.contains("up to date"), "{after_out}");
}
