//! Tier 1 plugin worker host: spawn, supervise, and speak ndjson JSON-RPC
//! with plugin workers (D1, D8 of the plugin-system design).
//!
//! A worker is the executable named by the manifest's `[runtime]` section
//! (builtin plugins run `aoe __plugin-worker --id <id>` from the current
//! binary), spawned through the active [`super::sandbox::SandboxBackend`]
//! with piped stdio. The wire format is one JSON-RPC 2.0 object per line:
//!
//! - host -> worker requests: contributed actions, commands, status batches;
//! - worker -> host requests: the capability-gated host API below;
//! - host -> worker notifications: subscribed bus events (`events.event`).
//!
//! Every worker-initiated call passes through [`authorize`]: a method whose
//! capability was not granted for the worker's exact manifest hash is refused
//! (acceptance criterion 4 of #268). Workers are expected to exit on stdin
//! EOF; the host kills them on shutdown and respawns crashed workers within a
//! small per-process budget.
//!
//! The host is deliberately synchronous (std threads + channels): it is
//! called from the TUI event loop, one-shot CLI handlers, and (via
//! `spawn_blocking`) the server, and none of those want a runtime handoff.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use aoe_plugin_api::Capability;
use serde_json::{json, Value};
use tracing::{debug, warn};

use super::registry::LoadedPlugin;

/// Default timeout for a host -> worker call.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// Max events handed to a subscriber per replay pass.
const REPLAY_LIMIT: usize = 1000;

/// Spawns per plugin per process before the host refuses to respawn.
const RESPAWN_BUDGET: u32 = 3;

static HOST: OnceLock<PluginHost> = OnceLock::new();

/// The process-wide host. Workers spawn on first use per plugin.
pub fn host() -> &'static PluginHost {
    HOST.get_or_init(PluginHost::new)
}

pub struct PluginHost {
    workers: Mutex<HashMap<String, Arc<Worker>>>,
    spawn_counts: Mutex<HashMap<String, u32>>,
}

struct Worker {
    plugin_id: String,
    capabilities: Vec<Capability>,
    child: Mutex<Child>,
    stdin: Mutex<ChildStdin>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, mpsc::Sender<Result<Value, String>>>>,
    alive: AtomicBool,
}

impl PluginHost {
    fn new() -> Self {
        Self {
            workers: Mutex::new(HashMap::new()),
            spawn_counts: Mutex::new(HashMap::new()),
        }
    }

    /// Call `method` on `plugin_id`'s worker, spawning it if needed.
    pub fn call(&self, plugin_id: &str, method: &str, params: Value) -> Result<Value> {
        self.call_with_timeout(plugin_id, method, params, CALL_TIMEOUT)
    }

    pub fn call_with_timeout(
        &self,
        plugin_id: &str,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
        let worker = self.ensure_worker(plugin_id)?;
        let outcome = call_worker(&worker, method, params, timeout);
        if matches!(outcome, Err(CallError::Wire(_))) {
            // A dead or hung worker poisons every later call; kill it and
            // let the respawn budget decide whether it gets another life.
            self.mark_dead(&worker);
        }
        outcome.map_err(|e| anyhow!("plugin {plugin_id} {method}: {e}"))
    }

    /// Kill and forget every worker. Called on daemon/TUI shutdown.
    pub fn shutdown(&self) {
        let workers: Vec<Arc<Worker>> = self
            .workers
            .lock()
            .expect("workers lock")
            .drain()
            .map(|(_, w)| w)
            .collect();
        for worker in workers {
            self.mark_dead(&worker);
        }
    }

    /// Kill every worker AND reset the respawn budgets. Called on registry
    /// reload (enable/disable/install/uninstall/update): a disabled or
    /// updated plugin must not keep its old worker, subscriptions, or grant
    /// set alive, and an intentional disable/enable cycle starts with a
    /// fresh budget. Still-active plugins respawn on their next call.
    pub fn reset(&self) {
        self.shutdown();
        self.spawn_counts.lock().expect("spawn counts lock").clear();
    }

    fn mark_dead(&self, worker: &Arc<Worker>) {
        worker.alive.store(false, Ordering::SeqCst);
        if let Ok(mut child) = worker.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.workers
            .lock()
            .expect("workers lock")
            .retain(|_, w| !Arc::ptr_eq(w, worker));
        // Wake every caller still waiting on this worker.
        let mut pending = worker.pending.lock().expect("pending lock");
        for (_, tx) in pending.drain() {
            let _ = tx.send(Err("worker died".to_string()));
        }
    }

    fn ensure_worker(&self, plugin_id: &str) -> Result<Arc<Worker>> {
        // Single-flight: the workers lock is held across check, spawn, and
        // insert, so two concurrent first calls cannot both observe "no
        // worker" and spawn duplicates (each burning respawn budget). The
        // spawn itself is per-plugin rare and fast; holding the map lock
        // through it is the simple correct shape.
        let mut workers = self.workers.lock().expect("workers lock");
        if let Some(worker) = workers.get(plugin_id) {
            if worker.alive.load(Ordering::SeqCst) {
                return Ok(worker.clone());
            }
        }
        let registry = super::registry();
        let plugin = registry
            .get(plugin_id)
            .filter(|p| p.active())
            .ok_or_else(|| anyhow!("plugin {plugin_id} is not active"))?;
        {
            let mut counts = self.spawn_counts.lock().expect("spawn counts lock");
            let count = counts.entry(plugin_id.to_string()).or_insert(0);
            if *count >= RESPAWN_BUDGET {
                bail!(
                    "plugin {plugin_id} worker crashed {RESPAWN_BUDGET} times; not respawning \
                     (disable and re-enable the plugin to reset)"
                );
            }
            *count += 1;
        }
        let (worker, stdout) = spawn_worker(plugin)?;
        let worker = Arc::new(worker);
        workers.insert(plugin_id.to_string(), worker.clone());
        drop(workers);
        // The reader can call mark_dead (which takes the workers lock) the
        // moment it starts, so it must spawn after the lock is released.
        start_reader(&worker, stdout);
        Ok(worker)
    }
}

/// How a single worker call failed. Wire failures (dead pipe, timeout) kill
/// the worker; RPC errors are the plugin answering "no" and leave it alive.
#[derive(Debug)]
enum CallError {
    Wire(String),
    Rpc(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Wire(msg) => write!(f, "{msg}"),
            CallError::Rpc(msg) => write!(f, "{msg}"),
        }
    }
}

/// One request/response round trip on an already-spawned worker.
fn call_worker(
    worker: &Arc<Worker>,
    method: &str,
    params: Value,
    timeout: Duration,
) -> Result<Value, CallError> {
    let id = worker.next_id.fetch_add(1, Ordering::SeqCst);
    let (tx, rx) = mpsc::channel();
    worker.pending.lock().expect("pending lock").insert(id, tx);
    let line = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
    let write_result = {
        let mut stdin = worker.stdin.lock().expect("stdin lock");
        writeln!(stdin, "{line}").and_then(|_| stdin.flush())
    };
    if let Err(e) = write_result {
        worker.pending.lock().expect("pending lock").remove(&id);
        return Err(CallError::Wire(format!("worker write failed: {e}")));
    }
    match rx.recv_timeout(timeout) {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(rpc_error)) => Err(CallError::Rpc(rpc_error)),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            worker.pending.lock().expect("pending lock").remove(&id);
            Err(CallError::Wire(format!(
                "timed out after {timeout:?}; worker killed"
            )))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err(CallError::Wire("worker exited mid-call".to_string()))
        }
    }
}

/// Resolve the worker executable: manifest entrypoint relative to the plugin
/// root for installed plugins, the current binary's hidden `__plugin-worker`
/// subcommand for builtins.
fn worker_command(plugin: &LoadedPlugin) -> Result<(PathBuf, Vec<String>, PathBuf)> {
    let runtime = plugin
        .manifest
        .runtime
        .as_ref()
        .ok_or_else(|| anyhow!("plugin {} declares no [runtime] section", plugin.id()))?;
    match &plugin.root {
        Some(root) => {
            let entrypoint = root.join(&runtime.entrypoint);
            if !entrypoint.is_file() {
                bail!(
                    "plugin {} entrypoint missing: {}",
                    plugin.id(),
                    entrypoint.display()
                );
            }
            Ok((entrypoint, runtime.args.clone(), root.clone()))
        }
        None => {
            let exe = std::env::current_exe().context("resolving current executable")?;
            let mut args = vec![
                "__plugin-worker".to_string(),
                "--id".to_string(),
                plugin.id().to_string(),
            ];
            args.extend(runtime.args.clone());
            let workdir = crate::session::get_app_dir()?;
            Ok((exe, args, workdir))
        }
    }
}

fn spawn_worker(plugin: &LoadedPlugin) -> Result<(Worker, std::process::ChildStdout)> {
    let (entrypoint, args, workdir) = worker_command(plugin)?;
    let mut cmd = super::sandbox::backend().command(&entrypoint, &args, &workdir);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning plugin {} worker", plugin.id()))?;
    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    let worker = Worker {
        plugin_id: plugin.id().to_string(),
        capabilities: plugin.manifest.capabilities.clone(),
        child: Mutex::new(child),
        stdin: Mutex::new(stdin),
        next_id: AtomicU64::new(1),
        pending: Mutex::new(HashMap::new()),
        alive: AtomicBool::new(true),
    };

    // stderr -> tracing, one line at a time.
    let id_for_stderr = worker.plugin_id.clone();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(|l| l.ok()) {
            warn!(target: "plugin", plugin = %id_for_stderr, "worker stderr: {line}");
        }
    });

    debug!(target: "plugin", plugin = %worker.plugin_id, "worker spawned");
    Ok((worker, stdout))
}

/// Start the reader thread for `worker`. Split from `spawn_worker` because
/// the thread needs the final `Arc` to route responses and host-API calls.
fn start_reader(worker: &Arc<Worker>, stdout: std::process::ChildStdout) {
    let weak = Arc::downgrade(worker);
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            let Some(worker) = weak.upgrade() else { break };
            let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                warn!(target: "plugin", plugin = %worker.plugin_id, "non-JSON worker line ignored");
                continue;
            };
            route_message(&worker, msg);
        }
        if let Some(worker) = weak.upgrade() {
            // Full teardown, not just a flag flip: a worker that closed
            // stdout without exiting would otherwise keep running as a
            // stray process once a replacement is spawned. mark_dead kills
            // and reaps the child, removes it from the map, and drains
            // pending callers.
            host().mark_dead(&worker);
            debug!(target: "plugin", plugin = %worker.plugin_id, "worker stdout closed");
        }
    });
}

fn route_message(worker: &Arc<Worker>, msg: Value) {
    let id = msg.get("id").and_then(Value::as_u64);
    let method = msg.get("method").and_then(Value::as_str);
    match (id, method) {
        // Response to a host -> worker call.
        (Some(id), None) => {
            let outcome = if let Some(error) = msg.get("error") {
                Err(error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error")
                    .to_string())
            } else {
                Ok(msg.get("result").cloned().unwrap_or(Value::Null))
            };
            if let Some(tx) = worker.pending.lock().expect("pending lock").remove(&id) {
                let _ = tx.send(outcome);
            }
        }
        // Worker -> host request: the capability-gated host API.
        (Some(id), Some(method)) => {
            let params = msg.get("params").cloned().unwrap_or(Value::Null);
            let response = match handle_host_call(worker, method, params) {
                Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                Err(e) => json!({
                    "jsonrpc": "2.0", "id": id,
                    "error": { "code": -32000, "message": format!("{e:#}") },
                }),
            };
            write_line(worker, &response);
        }
        // Worker notification; nothing defined in v1.
        (None, Some(method)) => {
            debug!(target: "plugin", plugin = %worker.plugin_id, method, "worker notification ignored");
        }
        _ => {}
    }
}

fn write_line(worker: &Worker, value: &Value) {
    let mut stdin = worker.stdin.lock().expect("stdin lock");
    if writeln!(stdin, "{value}")
        .and_then(|_| stdin.flush())
        .is_err()
    {
        worker.alive.store(false, Ordering::SeqCst);
    }
}

/// The single authorization gate: every worker-initiated method maps to a
/// capability, and an ungranted capability is a refusal, not a no-op.
fn authorize(plugin_id: &str, granted: &[Capability], method: &str) -> Result<()> {
    let needed = match method {
        "events.publish" => Capability::EventsPublish,
        "events.subscribe" => Capability::EventsSubscribe,
        "sessions.list" | "session.meta.get" => Capability::SessionsRead,
        "session.meta.set" | "session.meta.cas" => Capability::SessionsMetaWrite,
        // UI pushes are authorized by the manifest's declared contributions
        // (checked per call in the ui store), not by a capability: the
        // content is host-validated display state, never code.
        "ui.state.set" | "ui.state.remove" | "ui.notify" => return Ok(()),
        other => bail!("unknown host method {other:?}"),
    };
    if granted.contains(&needed) {
        Ok(())
    } else {
        bail!(
            "plugin {plugin_id} called {method} without the {} capability",
            needed.as_str()
        )
    }
}

fn handle_host_call(worker: &Arc<Worker>, method: &str, params: Value) -> Result<Value> {
    authorize(&worker.plugin_id, &worker.capabilities, method)?;
    match method {
        "events.publish" => {
            let topic = params
                .get("topic")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("events.publish needs a topic"))?;
            let own_prefix = format!("plugin.{}.", worker.plugin_id);
            if !topic.starts_with(&own_prefix) {
                bail!(
                    "plugin {} may only publish under {own_prefix}*",
                    worker.plugin_id
                );
            }
            let payload = params.get("payload").cloned().unwrap_or(Value::Null);
            let seq = crate::events::global()?.publish(topic, payload)?;
            Ok(json!({ "seq": seq }))
        }
        "events.subscribe" => {
            let topics: Vec<String> = params
                .get("topics")
                .and_then(|t| serde_json::from_value(t.clone()).ok())
                .ok_or_else(|| anyhow!("events.subscribe needs topics: [string]"))?;
            let after_seq = params.get("after_seq").and_then(Value::as_u64);
            start_event_forwarder(worker, topics, after_seq)?;
            Ok(json!({ "subscribed": true }))
        }
        "sessions.list" => {
            let storage = crate::session::Storage::new_unwatched("")?;
            let instances = storage.load()?;
            Ok(serde_json::to_value(&instances)?)
        }
        "session.meta.get" => {
            let session_id = required_str(&params, "session_id")?;
            let storage = crate::session::Storage::new_unwatched("")?;
            let instances = storage.load()?;
            let instance = instances
                .iter()
                .find(|i| i.id == session_id)
                .ok_or_else(|| anyhow!("unknown session {session_id}"))?;
            Ok(instance
                .plugin_meta
                .get(&worker.plugin_id)
                .cloned()
                .unwrap_or(Value::Null))
        }
        "session.meta.set" => {
            let session_id = required_str(&params, "session_id")?.to_string();
            let value = params
                .get("value")
                .cloned()
                .ok_or_else(|| anyhow!("session.meta.set needs a value"))?;
            set_meta(&worker.plugin_id, &session_id, None, value).map(|outcome| json!(outcome))
        }
        "session.meta.cas" => {
            let session_id = required_str(&params, "session_id")?.to_string();
            let expected = params.get("expected").cloned().unwrap_or(Value::Null);
            let value = params
                .get("value")
                .cloned()
                .ok_or_else(|| anyhow!("session.meta.cas needs a value"))?;
            set_meta(&worker.plugin_id, &session_id, Some(expected), value)
                .map(|outcome| json!(outcome))
        }
        "ui.state.set" => {
            let contribution_id = required_str(&params, "contribution_id")?.to_string();
            let session_id = params
                .get("session_id")
                .and_then(Value::as_str)
                .map(str::to_string);
            let ttl_ms = params.get("ttl_ms").and_then(Value::as_u64);
            let payload: super::ui::UiPayload = serde_json::from_value(
                params
                    .get("payload")
                    .cloned()
                    .ok_or_else(|| anyhow!("ui.state.set needs a payload"))?,
            )
            .map_err(|e| anyhow!("invalid ui payload: {e}"))?;
            super::ui::set_state(
                &worker.plugin_id,
                &contribution_id,
                session_id,
                ttl_ms,
                payload,
            )?;
            Ok(json!({ "ok": true }))
        }
        "ui.state.remove" => {
            let contribution_id = required_str(&params, "contribution_id")?.to_string();
            let session_id = params
                .get("session_id")
                .and_then(Value::as_str)
                .map(str::to_string);
            super::ui::remove_state(&worker.plugin_id, &contribution_id, session_id)?;
            Ok(json!({ "ok": true }))
        }
        "ui.notify" => {
            let title = required_str(&params, "title")?.to_string();
            let body = params
                .get("body")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let severity: super::ui::Severity = params
                .get("severity")
                .map(|s| serde_json::from_value(s.clone()))
                .transpose()
                .map_err(|e| anyhow!("invalid severity: {e}"))?
                .unwrap_or_default();
            let session_id = params
                .get("session_id")
                .and_then(Value::as_str)
                .map(str::to_string);
            super::ui::notify(&worker.plugin_id, title, body, severity, session_id)?;
            Ok(json!({ "ok": true }))
        }
        _ => unreachable!("authorize() rejects unknown methods"),
    }
}

fn required_str<'a>(params: &'a Value, key: &str) -> Result<&'a str> {
    params
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing string param {key:?}"))
}

#[derive(serde::Serialize)]
struct MetaWriteOutcome {
    swapped: bool,
    current: Value,
}

/// Write the plugin's namespace in a session's `plugin_meta`, optionally
/// compare-and-swap. Publishes `session.meta.changed` on success.
fn set_meta(
    plugin_id: &str,
    session_id: &str,
    expected: Option<Value>,
    value: Value,
) -> Result<MetaWriteOutcome> {
    let storage = crate::session::Storage::new_unwatched("")?;
    let outcome = storage.update(|instances, _groups| {
        let instance = instances
            .iter_mut()
            .find(|i| i.id == session_id)
            .ok_or_else(|| anyhow!("unknown session {session_id}"))?;
        let current = instance
            .plugin_meta
            .get(plugin_id)
            .cloned()
            .unwrap_or(Value::Null);
        if let Some(expected) = &expected {
            if &current != expected {
                return Ok(MetaWriteOutcome {
                    swapped: false,
                    current,
                });
            }
        }
        if value.is_null() {
            instance.plugin_meta.remove(plugin_id);
        } else {
            instance
                .plugin_meta
                .insert(plugin_id.to_string(), value.clone());
        }
        Ok(MetaWriteOutcome {
            swapped: true,
            current: value.clone(),
        })
    })?;
    if outcome.swapped {
        if let Ok(bus) = crate::events::global() {
            let _ = bus.publish(
                crate::events::topics::SESSION_META_CHANGED,
                json!({ "session_id": session_id, "plugin_id": plugin_id }),
            );
        }
    }
    Ok(outcome)
}

/// Forward matching bus events to the worker as `events.event` notifications:
/// replay from `after_seq` first, then the live broadcast, on a dedicated
/// thread (broadcast lag falls back to replay, mirroring the ACP ws path).
fn start_event_forwarder(
    worker: &Arc<Worker>,
    topics: Vec<String>,
    after_seq: Option<u64>,
) -> Result<()> {
    let bus = crate::events::global()?.clone();
    let weak = Arc::downgrade(worker);
    std::thread::spawn(move || {
        let mut rx = bus.subscribe();
        let mut last_seq = after_seq.unwrap_or_else(|| bus.highest_seq());
        if after_seq.is_some() {
            match bus.replay_from(&topics, last_seq, REPLAY_LIMIT) {
                Ok(events) => {
                    for event in events {
                        last_seq = event.seq;
                        let Some(worker) = weak.upgrade() else { return };
                        notify_event(&worker, &event);
                    }
                }
                Err(e) => warn!(target: "plugin", "event replay failed: {e:#}"),
            }
        }
        loop {
            match rx.blocking_recv() {
                Ok(event) => {
                    if event.seq <= last_seq
                        || !topics
                            .iter()
                            .any(|p| crate::events::topic_matches(p, &event.topic))
                    {
                        continue;
                    }
                    last_seq = event.seq;
                    let Some(worker) = weak.upgrade() else { return };
                    if !worker.alive.load(Ordering::SeqCst) {
                        return;
                    }
                    notify_event(&worker, &event);
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    match bus.replay_from(&topics, last_seq, REPLAY_LIMIT) {
                        Ok(events) => {
                            for event in events {
                                last_seq = event.seq;
                                let Some(worker) = weak.upgrade() else { return };
                                notify_event(&worker, &event);
                            }
                        }
                        Err(e) => warn!(target: "plugin", "event replay failed: {e:#}"),
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    });
    Ok(())
}

fn notify_event(worker: &Worker, event: &crate::events::BusEvent) {
    let line = json!({
        "jsonrpc": "2.0",
        "method": "events.event",
        "params": { "seq": event.seq, "topic": event.topic, "payload": event.payload },
    });
    write_line(worker, &line);
}

#[cfg(test)]
mod tests {
    use super::*;
    use aoe_plugin_api::PluginManifest;

    fn script_plugin(dir: &std::path::Path, script_body: &str) -> LoadedPlugin {
        let manifest_toml = r#"
id = "test.worker"
name = "Test Worker"
version = "0.1.0"
api_version = 1

[runtime]
entrypoint = "worker.sh"
"#;
        let script_path = dir.join("worker.sh");
        std::fs::write(&script_path, script_body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        LoadedPlugin {
            manifest: PluginManifest::from_toml_str(manifest_toml).unwrap(),
            manifest_hash: "sha256:test".into(),
            root: Some(dir.to_path_buf()),
            source: super::super::PluginSource::Path {
                path: dir.display().to_string(),
            },
            enabled: true,
            grant: super::super::grants::GrantStatus::Granted,
            settings: toml::Table::new(),
        }
    }

    #[cfg(unix)]
    #[test]
    fn call_round_trips_against_a_script_worker() {
        let dir = tempfile::tempdir().unwrap();
        // Answers the first request with a fixed result for id 1, then exits
        // on stdin EOF like a well-behaved worker.
        let plugin = script_plugin(
            dir.path(),
            "#!/bin/sh\nread line\nprintf '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\\n'\n",
        );
        let (worker, stdout) = spawn_worker(&plugin).unwrap();
        let worker = Arc::new(worker);
        start_reader(&worker, stdout);
        let result = call_worker(&worker, "test.run", json!({}), Duration::from_secs(5)).unwrap();
        assert_eq!(result, json!({"ok": true}));
        let _ = worker.child.lock().unwrap().kill();
    }

    #[cfg(unix)]
    #[test]
    fn rpc_error_and_timeout_are_distinguished() {
        let dir = tempfile::tempdir().unwrap();
        let plugin = script_plugin(
            dir.path(),
            "#!/bin/sh\nread line\nprintf '{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":1,\"message\":\"nope\"}}\\n'\nsleep 60\n",
        );
        let (worker, stdout) = spawn_worker(&plugin).unwrap();
        let worker = Arc::new(worker);
        start_reader(&worker, stdout);
        // First call: a clean RPC refusal.
        let err = call_worker(&worker, "test.run", json!({}), Duration::from_secs(5)).unwrap_err();
        assert!(
            matches!(err, CallError::Rpc(ref m) if m == "nope"),
            "{err:?}"
        );
        // Second call: the script never answers again; wire timeout.
        let err =
            call_worker(&worker, "test.run", json!({}), Duration::from_millis(200)).unwrap_err();
        assert!(matches!(err, CallError::Wire(_)), "{err:?}");
        let _ = worker.child.lock().unwrap().kill();
    }

    #[test]
    fn authorize_refuses_undeclared_capabilities() {
        // Acceptance criterion 4 of #268: undeclared capability use is refused.
        let granted = [Capability::SessionsRead];
        assert!(authorize("p", &granted, "sessions.list").is_ok());
        assert!(authorize("p", &granted, "events.publish").is_err());
        assert!(authorize("p", &granted, "session.meta.set").is_err());
        assert!(authorize("p", &granted, "made.up.method").is_err());
    }
}
