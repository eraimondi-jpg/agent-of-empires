//! Thin typed GitHub HTTP client built on the already-present `reqwest`.
//!
//! This is the single surface for talking to `api.github.com`. It owns the
//! base URL, the standard headers, the optional bearer token, and the mapping
//! from HTTP responses to the typed [`GitHubError`] taxonomy. Authenticated
//! callers resolve a token via [`crate::github::auth`] and pass it to
//! [`GitHubClient::authenticated`]; unauthenticated public reads (such as the
//! update check) use [`GitHubClient::unauthenticated`].

use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, AUTHORIZATION};
use reqwest::StatusCode;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use std::time::Duration;

use crate::github::error::{GitHubError, Result};

/// Configuration for constructing a [`GitHubClient`].
#[derive(Debug, Clone)]
pub struct GitHubClientConfig {
    /// API base, normally `https://api.github.com`. Overridable for tests.
    pub api_base: String,
    pub user_agent: String,
    pub timeout: Duration,
}

/// A configured GitHub HTTP client.
pub struct GitHubClient {
    http: reqwest::Client,
    api_base: String,
}

/// A GitHub release, the only DTO needed by the current callers.
#[derive(Debug, Clone, Deserialize)]
pub struct GitHubRelease {
    pub tag_name: String,
    #[serde(default)]
    pub body: Option<String>,
    pub published_at: Option<String>,
}

/// One side of a pull request's branch ref (`head` or `base`).
#[derive(Debug, Clone, Deserialize)]
pub struct PullBranchRef {
    /// Short branch name, e.g. `feature/x`.
    #[serde(rename = "ref")]
    pub ref_name: String,
    /// Commit SHA at the tip of that branch. Used as the git ref for
    /// check-runs lookups.
    pub sha: String,
}

/// A pull request as returned by the list endpoint
/// (`GET /repos/{o}/{r}/pulls`). Mergeability is absent here; fetch
/// [`PullDetails`] via [`GitHubClient::get_pull`] for that.
#[derive(Debug, Clone, Deserialize)]
pub struct PullRef {
    pub number: u64,
    /// `"open"` or `"closed"`.
    pub state: String,
    #[serde(default)]
    pub draft: bool,
    pub title: String,
    pub html_url: String,
    pub head: PullBranchRef,
    pub base: PullBranchRef,
}

/// A pull request as returned by the single-PR endpoint
/// (`GET /repos/{o}/{r}/pulls/{n}`), which carries merge state the list
/// endpoint omits.
#[derive(Debug, Clone, Deserialize)]
pub struct PullDetails {
    pub number: u64,
    pub state: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub merged: bool,
    /// GitHub's computed mergeability label (`"clean"`, `"dirty"`,
    /// `"blocked"`, `"behind"`, ...). `null` while GitHub is still
    /// computing it just after a push, hence `Option`.
    #[serde(default)]
    pub mergeable_state: Option<String>,
    pub title: String,
    pub html_url: String,
    pub head: PullBranchRef,
    pub base: PullBranchRef,
}

/// One check run from `GET /repos/{o}/{r}/commits/{ref}/check-runs`.
#[derive(Debug, Clone, Deserialize)]
pub struct CheckRun {
    pub name: String,
    /// `"queued"`, `"in_progress"`, or `"completed"`.
    pub status: String,
    /// Set once `status == "completed"`: `"success"`, `"failure"`,
    /// `"neutral"`, `"cancelled"`, `"skipped"`, `"timed_out"`,
    /// `"action_required"`.
    #[serde(default)]
    pub conclusion: Option<String>,
}

/// Response envelope for the check-runs endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct CheckRunsResponse {
    pub total_count: u64,
    pub check_runs: Vec<CheckRun>,
}

#[derive(Deserialize)]
struct ApiErrorBody {
    message: Option<String>,
}

impl GitHubClient {
    /// Client for public, unauthenticated requests.
    pub fn unauthenticated(config: GitHubClientConfig) -> Result<Self> {
        Self::build(config, None)
    }

    /// Client that sends `Authorization: Bearer <token>` on every request.
    /// Resolve the token with [`crate::github::auth::resolve_token_from_system`].
    pub fn authenticated(config: GitHubClientConfig, token: &str) -> Result<Self> {
        Self::build(config, Some(token))
    }

    fn build(config: GitHubClientConfig, token: Option<&str>) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/vnd.github+json"),
        );
        headers.insert(
            HeaderName::from_static("x-github-api-version"),
            HeaderValue::from_static("2022-11-28"),
        );
        if let Some(token) = token {
            let mut value = HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|_| GitHubError::Unauthorized)?;
            value.set_sensitive(true);
            headers.insert(AUTHORIZATION, value);
        }

        let http = reqwest::Client::builder()
            .user_agent(config.user_agent)
            .timeout(config.timeout)
            .default_headers(headers)
            .build()
            .map_err(GitHubError::Http)?;

        Ok(Self {
            http,
            api_base: config.api_base.trim_end_matches('/').to_string(),
        })
    }

    /// `GET /repos/{owner}/{repo}/releases?per_page={per_page}`
    pub async fn list_releases(
        &self,
        owner: &str,
        repo: &str,
        per_page: u8,
    ) -> Result<Vec<GitHubRelease>> {
        let url = format!(
            "{}/repos/{}/{}/releases?per_page={}",
            self.api_base, owner, repo, per_page
        );
        self.send_json(self.http.get(url)).await
    }

    /// `GET /repos/{owner}/{repo}/releases/latest`
    pub async fn latest_release(&self, owner: &str, repo: &str) -> Result<GitHubRelease> {
        let url = format!("{}/repos/{}/{}/releases/latest", self.api_base, owner, repo);
        self.send_json(self.http.get(url)).await
    }

    /// Open pull requests whose head is `{owner}:{branch}`.
    ///
    /// `GET /repos/{owner}/{repo}/pulls?state=open&head={owner}:{branch}`.
    /// The `head` filter only matches same-owner branches, so PRs opened
    /// from a fork are not returned. Branch names may contain `/`, so the
    /// query is built with [`reqwest::RequestBuilder::query`] rather than
    /// string interpolation.
    pub async fn list_open_pulls_for_branch(
        &self,
        owner: &str,
        repo: &str,
        branch: &str,
    ) -> Result<Vec<PullRef>> {
        let url = format!("{}/repos/{}/{}/pulls", self.api_base, owner, repo);
        let head = format!("{owner}:{branch}");
        let request = self
            .http
            .get(url)
            .query(&[("state", "open"), ("head", head.as_str())]);
        self.send_json(request).await
    }

    /// `GET /repos/{owner}/{repo}/pulls/{number}` for merge state the list
    /// endpoint omits.
    pub async fn get_pull(&self, owner: &str, repo: &str, number: u64) -> Result<PullDetails> {
        let url = format!(
            "{}/repos/{}/{}/pulls/{}",
            self.api_base, owner, repo, number
        );
        self.send_json(self.http.get(url)).await
    }

    /// Check runs for a git ref.
    ///
    /// `GET /repos/{owner}/{repo}/commits/{git_ref}/check-runs`. Pass a commit
    /// SHA: `git_ref` is interpolated into the path, so a branch name
    /// containing `/` would break the URL. Paginates at 100 per page until
    /// `total_count` runs have been collected, so a commit with more than one
    /// page of checks is not silently truncated (which would skew the CI
    /// aggregate). Covers the modern Checks API only; legacy Commit Status
    /// contexts are not aggregated (documented limitation, tracked for a
    /// follow-up).
    pub async fn list_check_runs(
        &self,
        owner: &str,
        repo: &str,
        git_ref: &str,
    ) -> Result<CheckRunsResponse> {
        let url = format!(
            "{}/repos/{}/{}/commits/{}/check-runs",
            self.api_base, owner, repo, git_ref
        );

        // First page establishes total_count; page through the rest until we
        // have them all (or a page comes back empty, defensive against a lying
        // total_count looping forever).
        let page_query = |page: u32| [("per_page", "100".to_string()), ("page", page.to_string())];
        let first: CheckRunsResponse = self
            .send_json(self.http.get(&url).query(&page_query(1)))
            .await?;
        let total_count = first.total_count;
        let mut collected = first.check_runs;

        let mut page = 2u32;
        while (collected.len() as u64) < total_count {
            let resp: CheckRunsResponse = self
                .send_json(self.http.get(&url).query(&page_query(page)))
                .await?;
            if resp.check_runs.is_empty() {
                break;
            }
            collected.extend(resp.check_runs);
            page += 1;
        }

        Ok(CheckRunsResponse {
            total_count,
            check_runs: collected,
        })
    }

    async fn send_json<T: DeserializeOwned>(&self, request: reqwest::RequestBuilder) -> Result<T> {
        let response = request.send().await.map_err(classify_transport_error)?;
        let status = response.status();
        if status.is_success() {
            return response.json::<T>().await.map_err(GitHubError::Decode);
        }
        let headers = response.headers().clone();
        let body = response.text().await.unwrap_or_default();
        Err(classify_status(status, &headers, &body))
    }
}

fn classify_transport_error(error: reqwest::Error) -> GitHubError {
    if error.is_timeout() || error.is_connect() {
        GitHubError::Network { source: error }
    } else {
        GitHubError::Http(error)
    }
}

/// Map a non-success HTTP response to the typed error with the right hint.
/// Pure and header-driven so it is unit-testable without a live API.
fn classify_status(status: StatusCode, headers: &HeaderMap, body: &str) -> GitHubError {
    match status {
        StatusCode::UNAUTHORIZED => GitHubError::Unauthorized,
        StatusCode::TOO_MANY_REQUESTS => rate_limited(headers),
        StatusCode::FORBIDDEN => {
            if is_rate_limited(headers) {
                rate_limited(headers)
            } else if let Some(scopes) = missing_scope(headers, body) {
                GitHubError::InsufficientScope { scopes }
            } else {
                GitHubError::Api {
                    status,
                    message: api_message(body),
                }
            }
        }
        StatusCode::NOT_FOUND => GitHubError::NotFound {
            resource: api_message(body),
        },
        _ => GitHubError::Api {
            status,
            message: api_message(body),
        },
    }
}

/// Build a [`GitHubError::RateLimited`] carrying whatever retry timing the
/// response advertised. `Retry-After` is relative seconds (secondary limits);
/// `X-RateLimit-Reset` is an absolute Unix epoch (primary limits). Both are
/// passed through raw so this stays a pure, clock-free transform; the poller
/// resolves them against its own clock.
fn rate_limited(headers: &HeaderMap) -> GitHubError {
    let retry_after = headers
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs);
    let reset_epoch = headers
        .get("x-ratelimit-reset")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    GitHubError::RateLimited {
        retry_after,
        reset_epoch,
    }
}

fn is_rate_limited(headers: &HeaderMap) -> bool {
    let remaining_zero = headers
        .get("x-ratelimit-remaining")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim() == "0")
        .unwrap_or(false);
    remaining_zero || headers.contains_key("retry-after")
}

/// A 403 is only treated as a missing-scope failure when the response body
/// actually says so. GitHub sends `X-Accepted-OAuth-Scopes` on many responses,
/// including ones that are forbidden for unrelated reasons, so the header alone
/// is not evidence. The named scope still comes from that header. Precise
/// per-operation scope mapping is tracked in the scope-elevation follow-up.
fn missing_scope(headers: &HeaderMap, body: &str) -> Option<String> {
    if !body.to_lowercase().contains("scope") {
        return None;
    }
    accepted_scopes(headers)
}

/// The scopes GitHub says the endpoint accepts, taken from
/// `X-Accepted-OAuth-Scopes` so the hint names the real missing scope.
fn accepted_scopes(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-accepted-oauth-scopes")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn api_message(body: &str) -> String {
    if let Ok(parsed) = serde_json::from_str::<ApiErrorBody>(body) {
        if let Some(message) = parsed.message {
            return message;
        }
    }
    let trimmed = body.trim();
    if trimmed.is_empty() {
        "no response body".to_string()
    } else {
        trimmed.chars().take(200).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> GitHubClientConfig {
        GitHubClientConfig {
            api_base: "https://api.github.com".to_string(),
            user_agent: "agent-of-empires-test".to_string(),
            timeout: Duration::from_secs(5),
        }
    }

    #[test]
    fn unauthenticated_client_builds() {
        assert!(GitHubClient::unauthenticated(config()).is_ok());
    }

    #[test]
    fn authenticated_client_builds_with_token() {
        assert!(GitHubClient::authenticated(config(), "gho_token123").is_ok());
    }

    #[test]
    fn api_base_trailing_slash_is_trimmed() {
        let mut cfg = config();
        cfg.api_base = "https://example.test/".to_string();
        let client = GitHubClient::unauthenticated(cfg).unwrap();
        assert_eq!(client.api_base, "https://example.test");
    }

    fn headers_with(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(
                HeaderName::from_static(name),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
    }

    #[test]
    fn unauthorized_maps_to_unauthorized() {
        let err = classify_status(StatusCode::UNAUTHORIZED, &HeaderMap::new(), "");
        assert!(matches!(err, GitHubError::Unauthorized));
    }

    #[test]
    fn forbidden_with_scope_error_names_the_scope() {
        let headers = headers_with(&[("x-accepted-oauth-scopes", "repo")]);
        let err = classify_status(
            StatusCode::FORBIDDEN,
            &headers,
            r#"{"message":"requires the repo scope"}"#,
        );
        match err {
            GitHubError::InsufficientScope { scopes } => assert_eq!(scopes, "repo"),
            other => panic!("expected InsufficientScope, got {other:?}"),
        }
    }

    #[test]
    fn forbidden_with_workflow_scope_names_workflow() {
        let headers = headers_with(&[("x-accepted-oauth-scopes", "repo, workflow")]);
        let err = classify_status(
            StatusCode::FORBIDDEN,
            &headers,
            r#"{"message":"missing the workflow scope"}"#,
        );
        match err {
            GitHubError::InsufficientScope { scopes } => assert!(scopes.contains("workflow")),
            other => panic!("expected InsufficientScope, got {other:?}"),
        }
    }

    #[test]
    fn forbidden_with_scope_header_but_no_scope_message_is_api() {
        // The header alone is not evidence; many 403s carry it.
        let headers = headers_with(&[("x-accepted-oauth-scopes", "repo")]);
        let err = classify_status(
            StatusCode::FORBIDDEN,
            &headers,
            r#"{"message":"Resource not accessible by integration"}"#,
        );
        assert!(matches!(err, GitHubError::Api { .. }));
    }

    #[test]
    fn forbidden_rate_limited_maps_to_rate_limited() {
        let headers = headers_with(&[("x-ratelimit-remaining", "0"), ("x-ratelimit-reset", "999")]);
        let err = classify_status(StatusCode::FORBIDDEN, &headers, "");
        match err {
            GitHubError::RateLimited {
                reset_epoch,
                retry_after,
            } => {
                assert_eq!(reset_epoch, Some(999));
                assert!(retry_after.is_none());
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[test]
    fn too_many_requests_carries_retry_after() {
        let headers = headers_with(&[("retry-after", "42")]);
        let err = classify_status(StatusCode::TOO_MANY_REQUESTS, &headers, "");
        match err {
            GitHubError::RateLimited { retry_after, .. } => {
                assert_eq!(retry_after, Some(Duration::from_secs(42)));
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[test]
    fn pull_ref_decodes_list_payload() {
        let json = r#"[{
            "number": 7,
            "state": "open",
            "draft": false,
            "title": "Add thing",
            "html_url": "https://github.com/o/r/pull/7",
            "head": { "ref": "feature/x", "sha": "abc123" },
            "base": { "ref": "main", "sha": "def456" }
        }]"#;
        let prs: Vec<PullRef> = serde_json::from_str(json).unwrap();
        assert_eq!(prs.len(), 1);
        assert_eq!(prs[0].number, 7);
        assert_eq!(prs[0].head.ref_name, "feature/x");
        assert_eq!(prs[0].head.sha, "abc123");
    }

    #[test]
    fn pull_details_decodes_null_mergeable_state() {
        let json = r#"{
            "number": 7,
            "state": "open",
            "draft": true,
            "merged": false,
            "mergeable_state": null,
            "title": "WIP",
            "html_url": "https://github.com/o/r/pull/7",
            "head": { "ref": "x", "sha": "s" },
            "base": { "ref": "main", "sha": "b" }
        }"#;
        let pr: PullDetails = serde_json::from_str(json).unwrap();
        assert!(pr.draft);
        assert!(!pr.merged);
        assert!(pr.mergeable_state.is_none());
    }

    #[test]
    fn check_runs_decode_with_missing_conclusion() {
        let json = r#"{
            "total_count": 2,
            "check_runs": [
                { "name": "build", "status": "completed", "conclusion": "success" },
                { "name": "test", "status": "in_progress" }
            ]
        }"#;
        let resp: CheckRunsResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.total_count, 2);
        assert_eq!(resp.check_runs[1].conclusion, None);
    }

    #[test]
    fn plain_forbidden_maps_to_api_error() {
        let err = classify_status(
            StatusCode::FORBIDDEN,
            &HeaderMap::new(),
            r#"{"message":"Resource protected"}"#,
        );
        match err {
            GitHubError::Api { status, message } => {
                assert_eq!(status, StatusCode::FORBIDDEN);
                assert_eq!(message, "Resource protected");
            }
            other => panic!("expected Api, got {other:?}"),
        }
    }

    #[test]
    fn not_found_carries_message() {
        let err = classify_status(
            StatusCode::NOT_FOUND,
            &HeaderMap::new(),
            r#"{"message":"Not Found"}"#,
        );
        match err {
            GitHubError::NotFound { resource } => assert_eq!(resource, "Not Found"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn server_error_maps_to_api() {
        let err = classify_status(StatusCode::INTERNAL_SERVER_ERROR, &HeaderMap::new(), "");
        match err {
            GitHubError::Api { status, message } => {
                assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
                assert_eq!(message, "no response body");
            }
            other => panic!("expected Api, got {other:?}"),
        }
    }

    #[test]
    fn api_message_falls_back_to_raw_body() {
        assert_eq!(api_message("plain text error"), "plain text error");
    }
}
