//! In-memory cache of GitHub PR + CI status, plus the pure check-run
//! aggregation that feeds it.
//!
//! The cache is volatile by design: PR/CI status is recomputable from a
//! tracked PR's `(owner, repo, number)` identity, so it lives only in the
//! serve daemon's memory and is rebuilt after a restart. Nothing here is
//! persisted; storage only ever holds the PR identity (see
//! [`crate::session::TrackedPr`]).

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::github::client::{CheckRun, PullDetails};

/// Rolled-up CI verdict for a PR's head commit. Counts are derived purely
/// from the head ref's check runs; `state` collapses them into one label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct CiAggregate {
    /// Total check runs reported for the head commit.
    pub total: u64,
    pub passing: u64,
    pub failing: u64,
    pub pending: u64,
    pub state: CiState,
}

/// Single-label CI verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CiState {
    /// No check runs reported (CI not configured, or none have started).
    #[default]
    None,
    /// At least one run still queued or in progress, none failing.
    Pending,
    /// Every run completed without a failing conclusion.
    Passing,
    /// At least one run failed, was cancelled, timed out, or needs action.
    Failing,
}

/// Aggregate a head commit's check runs into a [`CiAggregate`].
///
/// Pure and total. Conclusion semantics follow GitHub's documented set:
/// `success`, `neutral`, and `skipped` count as passing (a skipped required
/// check is not a failure); `failure`, `cancelled`, `timed_out`,
/// `action_required`, `stale`, and `startup_failure` count as failing;
/// anything not yet `completed` (and a completed run with no/unrecognized
/// conclusion) counts as pending. The overall `state` is Failing if any run
/// fails, else Pending if any is in flight, else Passing, else None when
/// there are no runs at all.
pub fn aggregate_check_runs(runs: &[CheckRun]) -> CiAggregate {
    let mut passing = 0u64;
    let mut failing = 0u64;
    let mut pending = 0u64;

    for run in runs {
        if run.status != "completed" {
            pending += 1;
            continue;
        }
        match run.conclusion.as_deref() {
            Some("success") | Some("neutral") | Some("skipped") => passing += 1,
            Some("failure")
            | Some("cancelled")
            | Some("timed_out")
            | Some("action_required")
            | Some("stale")
            | Some("startup_failure") => failing += 1,
            _ => pending += 1,
        }
    }

    let total = runs.len() as u64;
    let state = if total == 0 {
        CiState::None
    } else if failing > 0 {
        CiState::Failing
    } else if pending > 0 {
        CiState::Pending
    } else {
        CiState::Passing
    };

    CiAggregate {
        total,
        passing,
        failing,
        pending,
        state,
    }
}

/// A single check run, trimmed to what the UI needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckSummary {
    pub name: String,
    pub status: String,
    pub conclusion: Option<String>,
}

/// Cache key for a tracked PR. Owner + repo + number, never a bare slug, so
/// multi-org workspaces stay unambiguous.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PrKey {
    pub owner: String,
    pub repo: String,
    pub number: u64,
}

/// Live status snapshot for one PR. Serialized straight to the web client;
/// `fetched_at` lets the UI show staleness.
#[derive(Debug, Clone, Serialize)]
pub struct PrStatus {
    pub owner: String,
    pub repo: String,
    pub number: u64,
    /// `"open"` or `"closed"`.
    pub state: String,
    pub draft: bool,
    pub merged: bool,
    pub mergeable_state: Option<String>,
    pub title: String,
    pub html_url: String,
    pub head_sha: String,
    pub ci: CiAggregate,
    pub checks: Vec<CheckSummary>,
    pub fetched_at: DateTime<Utc>,
}

impl PrStatus {
    /// Build a status snapshot from a PR detail fetch and its head-commit
    /// check runs, stamping `now` as the fetch time.
    pub fn from_parts(
        owner: String,
        repo: String,
        details: PullDetails,
        runs: Vec<CheckRun>,
        now: DateTime<Utc>,
    ) -> Self {
        let ci = aggregate_check_runs(&runs);
        let checks = runs
            .into_iter()
            .map(|r| CheckSummary {
                name: r.name,
                status: r.status,
                conclusion: r.conclusion,
            })
            .collect();
        Self {
            owner,
            repo,
            number: details.number,
            state: details.state,
            draft: details.draft,
            merged: details.merged,
            mergeable_state: details.mergeable_state,
            title: details.title,
            html_url: details.html_url,
            head_sha: details.head.sha,
            ci,
            checks,
            fetched_at: now,
        }
    }

    pub fn key(&self) -> PrKey {
        PrKey {
            owner: self.owner.clone(),
            repo: self.repo.clone(),
            number: self.number,
        }
    }
}

/// In-memory map of `PrKey -> PrStatus`. Owned by the daemon poller; read by
/// the API handlers. Not thread-safe on its own; wrap in a lock at the call
/// site.
#[derive(Debug, Default)]
pub struct GithubStatusCache {
    entries: HashMap<PrKey, PrStatus>,
}

impl GithubStatusCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace the status for a PR.
    pub fn upsert(&mut self, status: PrStatus) {
        self.entries.insert(status.key(), status);
    }

    pub fn get(&self, key: &PrKey) -> Option<&PrStatus> {
        self.entries.get(key)
    }

    /// Drop every entry whose key is not in `keep`. Called each poll tick
    /// after the work-list is derived, so statuses for closed/removed PRs
    /// and deleted sessions do not linger.
    pub fn retain_keys(&mut self, keep: &HashSet<PrKey>) {
        self.entries.retain(|k, _| keep.contains(k));
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// All cached statuses, in arbitrary order. Used by the batch API.
    pub fn iter(&self) -> impl Iterator<Item = &PrStatus> {
        self.entries.values()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(status: &str, conclusion: Option<&str>) -> CheckRun {
        CheckRun {
            name: "check".to_string(),
            status: status.to_string(),
            conclusion: conclusion.map(str::to_string),
        }
    }

    #[test]
    fn empty_runs_are_state_none() {
        let agg = aggregate_check_runs(&[]);
        assert_eq!(agg.state, CiState::None);
        assert_eq!(agg.total, 0);
    }

    #[test]
    fn all_success_is_passing() {
        let runs = vec![
            run("completed", Some("success")),
            run("completed", Some("success")),
        ];
        let agg = aggregate_check_runs(&runs);
        assert_eq!(agg.state, CiState::Passing);
        assert_eq!(agg.passing, 2);
        assert_eq!(agg.total, 2);
    }

    #[test]
    fn skipped_and_neutral_count_as_passing_not_failing() {
        let runs = vec![
            run("completed", Some("skipped")),
            run("completed", Some("neutral")),
            run("completed", Some("success")),
        ];
        let agg = aggregate_check_runs(&runs);
        assert_eq!(agg.failing, 0);
        assert_eq!(agg.passing, 3);
        assert_eq!(agg.state, CiState::Passing);
    }

    #[test]
    fn any_failure_makes_state_failing_even_with_pending() {
        let runs = vec![
            run("completed", Some("failure")),
            run("in_progress", None),
            run("completed", Some("success")),
        ];
        let agg = aggregate_check_runs(&runs);
        assert_eq!(agg.failing, 1);
        assert_eq!(agg.pending, 1);
        assert_eq!(agg.passing, 1);
        assert_eq!(agg.state, CiState::Failing);
    }

    #[test]
    fn in_progress_without_failure_is_pending() {
        let runs = vec![
            run("queued", None),
            run("in_progress", None),
            run("completed", Some("success")),
        ];
        let agg = aggregate_check_runs(&runs);
        assert_eq!(agg.state, CiState::Pending);
        assert_eq!(agg.pending, 2);
    }

    #[test]
    fn completed_with_unknown_conclusion_is_pending() {
        // Truly unrecognized / missing conclusions stay pending.
        let runs = vec![run("completed", None), run("completed", Some("mystery"))];
        let agg = aggregate_check_runs(&runs);
        assert_eq!(agg.pending, 2);
        assert_eq!(agg.state, CiState::Pending);
    }

    #[test]
    fn stale_and_startup_failure_are_failing() {
        for conclusion in ["stale", "startup_failure"] {
            let agg = aggregate_check_runs(&[run("completed", Some(conclusion))]);
            assert_eq!(agg.failing, 1, "{conclusion} should count as failing");
            assert_eq!(agg.state, CiState::Failing, "{conclusion} -> Failing");
        }
    }

    #[test]
    fn cache_upsert_get_and_gc() {
        let mut cache = GithubStatusCache::new();
        let status = PrStatus {
            owner: "o".to_string(),
            repo: "r".to_string(),
            number: 1,
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
        };
        let key = status.key();
        cache.upsert(status);
        assert!(cache.get(&key).is_some());
        assert_eq!(cache.len(), 1);

        // GC with an empty keep-set evicts everything.
        cache.retain_keys(&HashSet::new());
        assert!(cache.is_empty());
        assert!(cache.get(&key).is_none());
    }
}
