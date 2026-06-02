//! Per-session GitHub context resolver.
//!
//! Given a session [`Instance`], work out which GitHub repo(s) it lives in
//! and which open PR(s) exist for its branch(es). Single-repo sessions map
//! through `worktree_info`; multi-repo workspaces yield one entry per repo.
//!
//! This module is **read-only**: it returns data and never writes session
//! storage. The daemon poller (`github::service`) owns the write path that
//! persists discovered PR numbers onto the `Instance`.

use crate::github::GitHubClient;
use crate::session::Instance;

/// Resolved GitHub context for a single repo in a session.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RepoGithubContext {
    pub owner: String,
    pub repo: String,
    /// Best-effort base branch (per-session override, else the worktree's
    /// recorded base). `None` when the session took the repo default and no
    /// override is set; resolving the live default is deferred to consumers.
    pub base_branch: Option<String>,
    /// The session's working branch in this repo.
    pub branch: String,
    /// Open PR numbers whose head is `{owner}:{branch}`, ascending and
    /// de-duplicated. `Some(vec![])` means discovery succeeded and found
    /// none; `None` means the discovery request failed (network, auth, rate
    /// limit). The distinction matters: callers must not treat a failed
    /// fetch as "no PRs" and wipe previously tracked state.
    pub open_prs: Option<Vec<u64>>,
}

/// One repo to resolve, derived purely from the session model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CandidateRepo {
    pub main_repo_path: String,
    pub branch: String,
    pub base_branch: Option<String>,
}

/// Derive the repos to resolve from the session model alone. Pure: no git or
/// network access. Multi-repo workspaces win over the single worktree; a
/// session with neither yields nothing.
pub(crate) fn candidate_repos(inst: &Instance) -> Vec<CandidateRepo> {
    if let Some(workspace) = &inst.workspace_info {
        return workspace
            .repos
            .iter()
            .map(|r| CandidateRepo {
                main_repo_path: r.main_repo_path.clone(),
                branch: r.branch.clone(),
                // Per-repo base is not recorded on WorkspaceRepo; the
                // session-wide override still applies if set.
                base_branch: inst.base_branch_override.clone(),
            })
            .collect();
    }

    if let Some(worktree) = &inst.worktree_info {
        return vec![CandidateRepo {
            main_repo_path: worktree.main_repo_path.clone(),
            branch: worktree.branch.clone(),
            base_branch: inst
                .base_branch_override
                .clone()
                .or_else(|| worktree.base_branch.clone()),
        }];
    }

    Vec::new()
}

/// Resolve the GitHub context for every repo in the session.
///
/// Repos whose origin is not a github.com remote are skipped. A discovery
/// failure for one repo (no network, rate limit, auth) is logged and yields
/// an entry with empty `open_prs` rather than failing the whole resolve, so
/// callers always get one entry per resolvable repo.
pub async fn resolve_github_context(
    inst: &Instance,
    client: &GitHubClient,
) -> Vec<RepoGithubContext> {
    let mut contexts = Vec::new();

    for candidate in candidate_repos(inst) {
        let Some((owner, repo)) =
            crate::git::github_slug(std::path::Path::new(&candidate.main_repo_path))
        else {
            continue;
        };

        let open_prs = match client
            .list_open_pulls_for_branch(&owner, &repo, &candidate.branch)
            .await
        {
            Ok(prs) => {
                let mut numbers: Vec<u64> = prs.into_iter().map(|p| p.number).collect();
                numbers.sort_unstable();
                numbers.dedup();
                Some(numbers)
            }
            Err(err) => {
                tracing::debug!(
                    target: "github.resolver",
                    %owner,
                    %repo,
                    branch = %candidate.branch,
                    error = %err,
                    "PR discovery failed; open_prs unknown for this tick"
                );
                None
            }
        };

        contexts.push(RepoGithubContext {
            owner,
            repo,
            base_branch: candidate.base_branch,
            branch: candidate.branch,
            open_prs,
        });
    }

    contexts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{WorkspaceInfo, WorkspaceRepo, WorktreeInfo};
    use chrono::Utc;

    fn base_instance() -> Instance {
        Instance::new("title", "/proj")
    }

    fn worktree(branch: &str, base: Option<&str>, main_repo: &str) -> WorktreeInfo {
        WorktreeInfo {
            branch: branch.to_string(),
            main_repo_path: main_repo.to_string(),
            managed_by_aoe: true,
            created_at: Utc::now(),
            base_branch: base.map(str::to_string),
        }
    }

    #[test]
    fn no_worktree_no_workspace_yields_nothing() {
        let inst = base_instance();
        assert!(candidate_repos(&inst).is_empty());
    }

    #[test]
    fn single_repo_uses_worktree_branch_and_base() {
        let mut inst = base_instance();
        inst.worktree_info = Some(worktree("feature/x", Some("main"), "/repo"));
        let cands = candidate_repos(&inst);
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].branch, "feature/x");
        assert_eq!(cands[0].base_branch.as_deref(), Some("main"));
        assert_eq!(cands[0].main_repo_path, "/repo");
    }

    #[test]
    fn base_branch_override_wins_over_worktree_base() {
        let mut inst = base_instance();
        inst.worktree_info = Some(worktree("feature/x", Some("main"), "/repo"));
        inst.base_branch_override = Some("release-1.2".to_string());
        let cands = candidate_repos(&inst);
        assert_eq!(cands[0].base_branch.as_deref(), Some("release-1.2"));
    }

    #[test]
    fn multi_repo_yields_one_candidate_per_repo() {
        let mut inst = base_instance();
        inst.workspace_info = Some(WorkspaceInfo {
            branch: "feature/x".to_string(),
            workspace_dir: "/ws".to_string(),
            repos: vec![
                WorkspaceRepo {
                    name: "api".to_string(),
                    source_path: "/src/api".to_string(),
                    branch: "feature/x".to_string(),
                    worktree_path: "/ws/api".to_string(),
                    main_repo_path: "/src/api".to_string(),
                    managed_by_aoe: true,
                },
                WorkspaceRepo {
                    name: "web".to_string(),
                    source_path: "/src/web".to_string(),
                    branch: "feature/x".to_string(),
                    worktree_path: "/ws/web".to_string(),
                    main_repo_path: "/src/web".to_string(),
                    managed_by_aoe: true,
                },
            ],
            created_at: Utc::now(),
            cleanup_on_delete: true,
        });
        let cands = candidate_repos(&inst);
        assert_eq!(cands.len(), 2);
        assert_eq!(cands[0].main_repo_path, "/src/api");
        assert_eq!(cands[1].main_repo_path, "/src/web");
    }
}
