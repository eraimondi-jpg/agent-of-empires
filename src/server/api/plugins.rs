//! Plugin management REST API: list, enable/disable, install, update,
//! uninstall, grant. The web twin of `aoe plugin` (#268).
//!
//! Every mutation is an execution surface (installing a plugin runs code on
//! the host), so all of them require read-write mode AND an elevated session
//! when login is enabled, mirroring the requires-elevation settings fields.
//! Installs are two-phase: a request without `confirm_capabilities` returns
//! the declared capability set and the honest isolation summary instead of
//! installing; the client re-sends with `confirm_capabilities: true` after
//! the user approved. The grant is pinned to the manifest hash, so a later
//! update that changes capabilities re-prompts the same way.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use super::AppState;
use crate::plugin::install::{InstallOutcome, InstallPrompt};
use crate::plugin::{self, grants::GrantStatus};
use crate::server::auth::AuthenticatedSession;

fn error_response(status: StatusCode, code: &str, message: String) -> Response {
    (status, Json(json!({ "error": code, "message": message }))).into_response()
}

/// Resolve the read-only and elevation gates shared by every mutation.
async fn mutation_gate(
    state: &AppState,
    session: Option<&AuthenticatedSession>,
) -> Result<(), Response> {
    if state.read_only {
        return Err(error_response(
            StatusCode::FORBIDDEN,
            "read_only",
            "Server is in read-only mode".into(),
        ));
    }
    let elevated = if state.login_manager.is_enabled() {
        match session {
            Some(AuthenticatedSession(id)) => state.login_manager.is_elevated(id).await,
            None => false,
        }
    } else {
        true
    };
    if !elevated {
        return Err(error_response(
            StatusCode::FORBIDDEN,
            "elevation_required",
            "Re-enter the passphrase to continue".into(),
        ));
    }
    Ok(())
}

fn plugin_json(p: &plugin::LoadedPlugin) -> serde_json::Value {
    json!({
        "id": p.id(),
        "name": p.manifest.name,
        "version": p.manifest.version,
        "description": p.manifest.description,
        "source": p.source.describe_redacted(),
        "trust": p.trust(),
        "enabled": p.enabled,
        "grant": match p.grant {
            GrantStatus::Granted => "granted",
            GrantStatus::Missing => "missing",
            GrantStatus::Stale => "stale",
        },
        "active": p.active(),
        "capabilities": p.manifest.capabilities,
        "has_runtime": p.manifest.runtime.is_some(),
        "setting_count": p.manifest.settings.len(),
        "builtin": p.root.is_none(),
    })
}

/// `GET /api/plugins`: every known plugin plus load errors and the honest
/// isolation summary used to word install prompts.
pub async fn list_plugins() -> Json<serde_json::Value> {
    let registry = plugin::registry();
    Json(json!({
        "plugins": registry.all().iter().map(plugin_json).collect::<Vec<_>>(),
        "load_errors": registry.load_errors(),
        "isolation_summary": plugin::sandbox::backend().isolation_summary(),
    }))
}

/// `GET /api/plugins/updates`: check every community plugin against its
/// recorded source. Read-only but does network IO (git ls-remote per GitHub
/// plugin), so it is on-demand rather than folded into the list endpoint.
pub async fn check_plugin_updates() -> Response {
    let result = tokio::task::spawn_blocking(plugin::update_check::check_all).await;
    match result {
        Ok(Ok(statuses)) => {
            let updates: serde_json::Map<String, serde_json::Value> = statuses
                .into_iter()
                .map(|(id, status)| (id, serde_json::to_value(status).expect("serializable")))
                .collect();
            Json(json!({ "updates": updates })).into_response()
        }
        Ok(Err(e)) => error_response(StatusCode::BAD_REQUEST, "plugin_error", format!("{e:#}")),
        Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
}

/// `GET /api/plugins/discover`: search GitHub for repositories tagged
/// `aoe-plugin`, marked against the featured index and the local install
/// state. Read-only but network-bound, so only ever called on an explicit
/// dashboard action, never on panel load.
pub async fn discover_plugins() -> Response {
    match plugin::discover::discover().await {
        Ok(found) => Json(json!({ "plugins": found })).into_response(),
        Err(e) => error_response(StatusCode::BAD_GATEWAY, "discover_failed", format!("{e:#}")),
    }
}

/// `GET /api/ui/state`: every live plugin UI contribution entry plus the
/// notification ring, with a revision counter the client polls against.
/// Read-only cache snapshot; never touches a plugin worker.
pub async fn get_plugin_ui_state() -> Json<serde_json::Value> {
    Json(json!({
        "revision": plugin::ui::revision(),
        "entries": plugin::ui::all_entries(),
        "notifications": plugin::ui::notifications(),
    }))
}

#[derive(Deserialize)]
pub struct SetEnabledBody {
    pub enabled: bool,
}

/// `POST /api/plugins/{id}/enabled`
pub async fn set_plugin_enabled(
    State(state): State<std::sync::Arc<AppState>>,
    session: Option<axum::Extension<AuthenticatedSession>>,
    Path(id): Path<String>,
    Json(body): Json<SetEnabledBody>,
) -> Response {
    if let Err(resp) = mutation_gate(&state, session.as_deref()).await {
        return resp;
    }
    let result =
        tokio::task::spawn_blocking(move || plugin::install::set_enabled(&id, body.enabled)).await;
    match result {
        Ok(Ok(())) => list_plugins().await.into_response(),
        Ok(Err(e)) => error_response(StatusCode::BAD_REQUEST, "plugin_error", format!("{e:#}")),
        Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
}

#[derive(Deserialize)]
pub struct InstallBody {
    /// `owner/repo` or a host-local directory path.
    pub source: String,
    /// False (or absent) returns the capability prompt instead of installing.
    #[serde(default)]
    pub confirm_capabilities: bool,
    /// Hash of the manifest the user approved, from the 409 prompt payload.
    /// Confirmation only proceeds when the re-staged source still serves
    /// this exact manifest; anything else re-prompts (the source changed
    /// between review and confirm).
    #[serde(default)]
    pub expected_manifest_hash: Option<String>,
}

fn prompt_json(prompt: &InstallPrompt) -> serde_json::Value {
    json!({
        "needs_confirmation": true,
        "id": prompt.id,
        "name": prompt.name,
        "version": prompt.version,
        "description": prompt.description,
        "capabilities": prompt.capabilities,
        "previous_capabilities": prompt.previous_capabilities,
        "trust": prompt.trust,
        "source": prompt.source.describe_redacted(),
        "featured": prompt.featured,
        "manifest_hash": prompt.manifest_hash,
        "isolation_summary": plugin::sandbox::backend().isolation_summary(),
    })
}

fn outcome_response(outcome: InstallOutcome, prompt: Option<serde_json::Value>) -> Response {
    match outcome {
        InstallOutcome::Declined => match prompt {
            // Two-phase install: the "decline" carried the prompt the client
            // should show. 409 so success paths stay 200.
            Some(p) => (StatusCode::CONFLICT, Json(p)).into_response(),
            None => error_response(
                StatusCode::BAD_REQUEST,
                "declined",
                "capability grant declined".into(),
            ),
        },
        InstallOutcome::Installed { id, version } => {
            Json(json!({ "installed": { "id": id, "version": version } })).into_response()
        }
        InstallOutcome::Updated { id, version } => {
            Json(json!({ "updated": { "id": id, "version": version } })).into_response()
        }
        InstallOutcome::UpToDate { id, version } => {
            Json(json!({ "up_to_date": { "id": id, "version": version } })).into_response()
        }
    }
}

/// `POST /api/plugins/install`
pub async fn install_plugin(
    State(state): State<std::sync::Arc<AppState>>,
    session: Option<axum::Extension<AuthenticatedSession>>,
    Json(body): Json<InstallBody>,
) -> Response {
    if let Err(resp) = mutation_gate(&state, session.as_deref()).await {
        return resp;
    }
    let result = tokio::task::spawn_blocking(move || {
        let source = plugin::install::parse_source(&body.source)?;
        let mut prompt_payload = None;
        let confirm = body.confirm_capabilities;
        let expected = body.expected_manifest_hash;
        let outcome = plugin::install::install(source, &mut |prompt| {
            // The confirm pass re-stages the source, so approval must bind
            // to the manifest the user actually reviewed: anything else
            // (source moved between review and confirm) re-prompts.
            if confirm && expected.as_deref() == Some(prompt.manifest_hash.as_str()) {
                true
            } else {
                prompt_payload = Some(prompt_json(prompt));
                false
            }
        })?;
        Ok::<_, anyhow::Error>((outcome, prompt_payload))
    })
    .await;
    match result {
        Ok(Ok((outcome, prompt))) => outcome_response(outcome, prompt),
        Ok(Err(e)) => error_response(StatusCode::BAD_REQUEST, "plugin_error", format!("{e:#}")),
        Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
}

#[derive(Deserialize)]
pub struct UpdateBody {
    #[serde(default)]
    pub confirm_capabilities: bool,
    /// See `InstallBody::expected_manifest_hash`.
    #[serde(default)]
    pub expected_manifest_hash: Option<String>,
}

/// `POST /api/plugins/{id}/update`
pub async fn update_plugin(
    State(state): State<std::sync::Arc<AppState>>,
    session: Option<axum::Extension<AuthenticatedSession>>,
    Path(id): Path<String>,
    Json(body): Json<UpdateBody>,
) -> Response {
    if let Err(resp) = mutation_gate(&state, session.as_deref()).await {
        return resp;
    }
    let result = tokio::task::spawn_blocking(move || {
        let mut prompt_payload = None;
        let confirm = body.confirm_capabilities;
        let expected = body.expected_manifest_hash;
        let outcome = plugin::install::update(&id, &mut |prompt| {
            if confirm && expected.as_deref() == Some(prompt.manifest_hash.as_str()) {
                true
            } else {
                prompt_payload = Some(prompt_json(prompt));
                false
            }
        })?;
        Ok::<_, anyhow::Error>((outcome, prompt_payload))
    })
    .await;
    match result {
        Ok(Ok((outcome, prompt))) => outcome_response(outcome, prompt),
        Ok(Err(e)) => error_response(StatusCode::BAD_REQUEST, "plugin_error", format!("{e:#}")),
        Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
}

/// `DELETE /api/plugins/{id}`
pub async fn uninstall_plugin(
    State(state): State<std::sync::Arc<AppState>>,
    session: Option<axum::Extension<AuthenticatedSession>>,
    Path(id): Path<String>,
) -> Response {
    if let Err(resp) = mutation_gate(&state, session.as_deref()).await {
        return resp;
    }
    let result = tokio::task::spawn_blocking(move || plugin::install::uninstall(&id)).await;
    match result {
        Ok(Ok(())) => list_plugins().await.into_response(),
        Ok(Err(e)) => error_response(StatusCode::BAD_REQUEST, "plugin_error", format!("{e:#}")),
        Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
}
