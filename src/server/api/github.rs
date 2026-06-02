//! Read-only REST surface over the daemon's GitHub PR/CI cache.
//!
//! Both handlers are pure projections: they read `state.instances` (for the
//! persisted PR refs) and `state.github_status` (for live PR/CI status) and
//! never call GitHub, never write storage, and never block on the network.
//! The batch endpoint exists so a future per-row consumer (#676) hydrates
//! every session's chips in one round trip instead of N.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Serialize;

use super::AppState;
use crate::github::status::{GithubStatusCache, PrKey, PrStatus};
use crate::session::{Instance, TrackedPr};

/// One session's GitHub view: the persisted PR refs plus whatever live status
/// the cache currently holds for them. `statuses` may be shorter than `refs`
/// when the poller has not fetched a PR yet (or could not).
#[derive(Debug, Serialize)]
pub struct SessionGithubResponse {
    pub refs: Vec<TrackedPr>,
    pub statuses: Vec<PrStatus>,
}

/// Batch payload keyed by session id. Only sessions with at least one tracked
/// PR appear, keeping the response small.
#[derive(Debug, Serialize)]
pub struct GithubStatusResponse {
    pub sessions: BTreeMap<String, SessionGithubResponse>,
}

/// Project one instance plus the cache into a [`SessionGithubResponse`]. Pure.
fn build_session_github(inst: &Instance, cache: &GithubStatusCache) -> SessionGithubResponse {
    let refs = inst.github_prs.clone().unwrap_or_default();
    let statuses = refs
        .iter()
        .filter_map(|t| {
            cache
                .get(&PrKey {
                    owner: t.owner.clone(),
                    repo: t.repo.clone(),
                    number: t.number,
                })
                .cloned()
        })
        .collect();
    SessionGithubResponse { refs, statuses }
}

/// `GET /api/github/status` — every session that tracks a PR, with cached
/// status. One round trip hydrates the whole sidebar.
pub async fn github_status(State(state): State<Arc<AppState>>) -> Json<GithubStatusResponse> {
    let instances = state.instances.read().await;
    let cache = state.github_status.read().await;
    let sessions = instances
        .iter()
        .filter(|inst| inst.github_prs.as_ref().is_some_and(|p| !p.is_empty()))
        .map(|inst| (inst.id.clone(), build_session_github(inst, &cache)))
        .collect();
    Json(GithubStatusResponse { sessions })
}

/// `GET /api/sessions/{id}/github` — one session's refs + cached status. A
/// thin filter over the same cache; 404 only when the session is unknown.
pub async fn session_github(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    let instances = state.instances.read().await;
    let Some(inst) = instances.iter().find(|i| i.id == id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not_found", "message": "Session not found"})),
        )
            .into_response();
    };
    let cache = state.github_status.read().await;
    Json(build_session_github(inst, &cache)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::status::CiAggregate;
    use chrono::Utc;

    fn status(owner: &str, repo: &str, number: u64) -> PrStatus {
        PrStatus {
            owner: owner.to_string(),
            repo: repo.to_string(),
            number,
            state: "open".to_string(),
            draft: false,
            merged: false,
            mergeable_state: Some("clean".to_string()),
            title: "t".to_string(),
            html_url: "u".to_string(),
            head_sha: "sha".to_string(),
            ci: CiAggregate::default(),
            checks: vec![],
            fetched_at: Utc::now(),
        }
    }

    #[test]
    fn projects_refs_and_present_statuses() {
        let mut inst = Instance::new("t", "/p");
        inst.github_prs = Some(vec![
            TrackedPr {
                owner: "o".to_string(),
                repo: "r".to_string(),
                number: 1,
            },
            TrackedPr {
                owner: "o".to_string(),
                repo: "r".to_string(),
                number: 2,
            },
        ]);
        let mut cache = GithubStatusCache::new();
        cache.upsert(status("o", "r", 1)); // only PR #1 has a cached status

        let resp = build_session_github(&inst, &cache);
        assert_eq!(resp.refs.len(), 2, "all tracked refs are returned");
        assert_eq!(
            resp.statuses.len(),
            1,
            "only PRs present in the cache yield a status"
        );
        assert_eq!(resp.statuses[0].number, 1);
    }

    #[test]
    fn no_tracked_prs_yields_empty() {
        let inst = Instance::new("t", "/p");
        let cache = GithubStatusCache::new();
        let resp = build_session_github(&inst, &cache);
        assert!(resp.refs.is_empty());
        assert!(resp.statuses.is_empty());
    }
}
