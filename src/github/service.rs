//! The `aoe serve` daemon's GitHub PR/CI poller.
//!
//! One background loop, owned by the daemon, is the single source of GitHub
//! traffic and the only writer of `Instance.github_prs`. Each tick it:
//!   1. snapshots the live sessions (pull, no event bus),
//!   2. discovers open PRs per repo and diff-writes the tracked numbers back
//!      to storage only when the set changed,
//!   3. refreshes PR + CI status into the in-memory [`GithubStatusCache`],
//!   4. garbage-collects cache entries no session tracks anymore.
//!
//! Cadence and backoff come from [`GitHubConfig`]; the interval grows toward
//! the configured maximum while nothing changes and snaps back to the base on
//! any change. Rate-limit responses park the loop until the advertised reset.
//! The TUI is intentionally untouched (deferred to #676).

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::Utc;

use crate::github::error::GitHubError;
use crate::github::resolver::{resolve_github_context, RepoGithubContext};
use crate::github::status::{PrKey, PrStatus};
use crate::github::{
    GitHubClient, GitHubClientConfig, DEFAULT_GITHUB_API_BASE, DEFAULT_USER_AGENT,
};
use crate::server::AppState;
use crate::session::TrackedPr;

const CLIENT_TIMEOUT: Duration = Duration::from_secs(15);
/// Hard ceiling on a rate-limit park, so a bogus reset header can't wedge the
/// loop for hours.
const MAX_RATE_LIMIT_SLEEP: Duration = Duration::from_secs(3600);

/// Flatten resolved contexts into a sorted, de-duplicated tracked-PR list.
/// Sorted so the equality check against the persisted value is order-stable.
pub(crate) fn tracked_prs_from_contexts(contexts: &[RepoGithubContext]) -> Vec<TrackedPr> {
    let mut prs: Vec<TrackedPr> = contexts
        .iter()
        .flat_map(|ctx| {
            ctx.open_prs.iter().map(move |&number| TrackedPr {
                owner: ctx.owner.clone(),
                repo: ctx.repo.clone(),
                number,
            })
        })
        .collect();
    prs.sort_by(|a, b| {
        (a.owner.as_str(), a.repo.as_str(), a.number).cmp(&(
            b.owner.as_str(),
            b.repo.as_str(),
            b.number,
        ))
    });
    prs.dedup();
    prs
}

/// The cheap fields that decide whether a refresh is a "change" for backoff
/// purposes: PR open/closed/draft plus the rolled-up CI verdict.
fn status_signature(s: &PrStatus) -> (String, bool, crate::github::status::CiState) {
    (s.state.clone(), s.draft, s.ci.state)
}

/// Seconds to park after a rate-limit response, preferring the relative
/// `Retry-After` then the absolute `X-RateLimit-Reset`, clamped to a sane
/// ceiling.
fn rate_limit_sleep(retry_after: Option<Duration>, reset_epoch: Option<u64>) -> Duration {
    if let Some(d) = retry_after {
        return d.min(MAX_RATE_LIMIT_SLEEP);
    }
    if let Some(reset) = reset_epoch {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let secs = reset.saturating_sub(now);
        return Duration::from_secs(secs).min(MAX_RATE_LIMIT_SLEEP);
    }
    MAX_RATE_LIMIT_SLEEP.min(Duration::from_secs(60))
}

/// Resolve a token off the async runtime (the resolver may shell out to `gh`).
async fn resolve_token() -> Option<String> {
    tokio::task::spawn_blocking(|| {
        crate::github::auth::resolve_token_from_system()
            .ok()
            .map(|t| t.token)
    })
    .await
    .ok()
    .flatten()
}

fn build_client(token: Option<&str>) -> Option<GitHubClient> {
    let config = GitHubClientConfig {
        api_base: DEFAULT_GITHUB_API_BASE.to_string(),
        user_agent: DEFAULT_USER_AGENT.to_string(),
        timeout: CLIENT_TIMEOUT,
    };
    match token {
        Some(token) => GitHubClient::authenticated(config, token),
        None => GitHubClient::unauthenticated(config),
    }
    .ok()
}

enum RefreshOutcome {
    Status(PrStatus),
    /// Rate limited; park for this long before the next tick.
    RateLimited(Duration),
    /// Transient per-PR failure; skip this PR this tick.
    Skip,
}

async fn refresh_pr(client: &GitHubClient, key: &PrKey) -> RefreshOutcome {
    let details = match client.get_pull(&key.owner, &key.repo, key.number).await {
        Ok(d) => d,
        Err(GitHubError::RateLimited {
            retry_after,
            reset_epoch,
        }) => return RefreshOutcome::RateLimited(rate_limit_sleep(retry_after, reset_epoch)),
        Err(err) => {
            tracing::debug!(
                target: "github.poller",
                owner = %key.owner, repo = %key.repo, number = key.number,
                error = %err, "get_pull failed"
            );
            return RefreshOutcome::Skip;
        }
    };

    // Check runs are best-effort: a failure here still yields a PrStatus with
    // an empty CI aggregate rather than dropping the whole PR.
    let runs = match client
        .list_check_runs(&key.owner, &key.repo, &details.head.sha)
        .await
    {
        Ok(resp) => resp.check_runs,
        Err(GitHubError::RateLimited {
            retry_after,
            reset_epoch,
        }) => return RefreshOutcome::RateLimited(rate_limit_sleep(retry_after, reset_epoch)),
        Err(err) => {
            tracing::debug!(
                target: "github.poller",
                owner = %key.owner, repo = %key.repo, number = key.number,
                error = %err, "list_check_runs failed; empty CI"
            );
            Vec::new()
        }
    };

    RefreshOutcome::Status(PrStatus::from_parts(
        key.owner.clone(),
        key.repo.clone(),
        details,
        runs,
        Utc::now(),
    ))
}

/// Diff-write the tracked PR set onto the in-memory instance and storage.
/// Returns true when the set actually changed. The only writer of
/// `Instance.github_prs`.
async fn persist_tracked(state: &Arc<AppState>, id: &str, tracked: Vec<TrackedPr>) -> bool {
    let new_value = (!tracked.is_empty()).then_some(tracked);

    {
        let mut instances = state.instances.write().await;
        match instances.iter_mut().find(|i| i.id == id) {
            Some(inst) if inst.github_prs == new_value => return false,
            Some(inst) => inst.github_prs = new_value.clone(),
            None => return false,
        }
    }

    let profile = state.profile.clone();
    let id = id.to_string();
    let persisted = new_value.clone();
    let result = tokio::task::spawn_blocking(move || {
        let storage = crate::session::Storage::new(&profile)?;
        storage.update(|instances, _groups| {
            if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
                inst.github_prs = persisted;
            }
            Ok(())
        })
    })
    .await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            tracing::warn!(target: "github.poller", error = %e, "failed to persist github_prs")
        }
        Err(e) => {
            tracing::warn!(target: "github.poller", error = %e, "github_prs persist task panicked")
        }
    }
    true
}

/// Run the daemon's GitHub poll loop until shutdown. Holds no lock across a
/// network call: statuses are fetched into a local vec, then applied to the
/// cache under a brief write lock.
pub async fn run_poll_loop(state: Arc<AppState>) {
    let shutdown = state.shutdown.clone();
    let mut interval = Duration::from_secs(30);

    loop {
        let cfg = crate::session::profile_config::resolve_config_or_warn(&state.profile).github;
        let base = Duration::from_secs(cfg.poll_interval_secs.max(1));
        let max = Duration::from_secs(
            cfg.max_poll_interval_secs
                .max(cfg.poll_interval_secs)
                .max(1),
        );

        if !cfg.enabled {
            if sleep_or_shutdown(&shutdown, base).await {
                return;
            }
            continue;
        }

        let token = resolve_token().await;
        if token.is_none() && !cfg.allow_unauthenticated_polling {
            // No credentials and unauthenticated polling is off: idle without
            // burning the 60 req/hr public budget. Keep cached state as-is.
            if sleep_or_shutdown(&shutdown, max).await {
                return;
            }
            continue;
        }

        let Some(client) = build_client(token.as_deref()) else {
            if sleep_or_shutdown(&shutdown, base).await {
                return;
            }
            continue;
        };

        let instances = state.instances.read().await.clone();

        // Discovery + tracked-PR diff-write.
        let mut desired: HashSet<PrKey> = HashSet::new();
        let mut changed = false;
        for inst in &instances {
            let contexts = resolve_github_context(inst, &client).await;
            let tracked = tracked_prs_from_contexts(&contexts);
            for t in &tracked {
                desired.insert(PrKey {
                    owner: t.owner.clone(),
                    repo: t.repo.clone(),
                    number: t.number,
                });
            }
            if persist_tracked(&state, &inst.id, tracked).await {
                changed = true;
            }
        }

        // Status refresh (no lock held across the network).
        let mut fetched: Vec<PrStatus> = Vec::new();
        let mut rate_limit_park: Option<Duration> = None;
        for key in &desired {
            match refresh_pr(&client, key).await {
                RefreshOutcome::Status(s) => fetched.push(s),
                RefreshOutcome::RateLimited(park) => {
                    rate_limit_park = Some(park);
                    break;
                }
                RefreshOutcome::Skip => {}
            }
        }

        {
            let mut cache = state.github_status.write().await;
            for status in fetched {
                let key = status.key();
                if cache.get(&key).map(status_signature) != Some(status_signature(&status)) {
                    changed = true;
                }
                cache.upsert(status);
            }
            cache.retain_keys(&desired);
        }

        // Adaptive backoff: snap to base on any change, otherwise grow 1.5x
        // toward the configured ceiling. A rate-limit park overrides both.
        interval = if let Some(park) = rate_limit_park {
            park
        } else if changed {
            base
        } else {
            Duration::from_secs(((interval.as_secs() as f64 * 1.5) as u64).max(base.as_secs()))
                .min(max)
        };

        if sleep_or_shutdown(&shutdown, interval).await {
            return;
        }
    }
}

/// Sleep for `dur` or return early. Returns true when shutdown fired (the
/// caller should stop the loop).
async fn sleep_or_shutdown(shutdown: &tokio_util::sync::CancellationToken, dur: Duration) -> bool {
    tokio::select! {
        _ = shutdown.cancelled() => true,
        _ = tokio::time::sleep(dur) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(owner: &str, repo: &str, prs: &[u64]) -> RepoGithubContext {
        RepoGithubContext {
            owner: owner.to_string(),
            repo: repo.to_string(),
            base_branch: None,
            branch: "feature/x".to_string(),
            open_prs: prs.to_vec(),
        }
    }

    #[test]
    fn tracked_prs_are_sorted_and_deduped() {
        let contexts = vec![ctx("o", "b", &[3]), ctx("o", "a", &[2, 2, 1])];
        let tracked = tracked_prs_from_contexts(&contexts);
        let tuples: Vec<_> = tracked
            .iter()
            .map(|t| (t.owner.as_str(), t.repo.as_str(), t.number))
            .collect();
        assert_eq!(
            tuples,
            vec![("o", "a", 1), ("o", "a", 2), ("o", "b", 3)],
            "sorted by owner/repo/number with duplicates removed"
        );
    }

    #[test]
    fn empty_contexts_yield_no_tracked_prs() {
        assert!(tracked_prs_from_contexts(&[ctx("o", "r", &[])]).is_empty());
    }

    #[test]
    fn rate_limit_sleep_prefers_retry_after() {
        let d = rate_limit_sleep(Some(Duration::from_secs(42)), Some(9_999_999_999));
        assert_eq!(d, Duration::from_secs(42));
    }

    #[test]
    fn rate_limit_sleep_is_clamped() {
        let d = rate_limit_sleep(Some(Duration::from_secs(100_000)), None);
        assert_eq!(d, MAX_RATE_LIMIT_SLEEP);
    }
}
