use crate::config::{Profile, RepoRef};
use crate::error::QghError;
use crate::model::{
    CommentRecord, CursorUpdate, IssueRecord, ReconciliationCandidate, StoredCursor,
};
use crate::time::now_rfc3339;
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use reqwest::header::{HeaderMap, ETAG, IF_NONE_MATCH, LINK, LOCATION, RETRY_AFTER};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

const SOURCE_ID_ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'/')
    .add(b':')
    .add(b';')
    .add(b'=')
    .add(b'@')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'|');
pub const GITHUB_API_VERSION: &str = "2022-11-28";

pub fn user_agent() -> String {
    format!("qgh/{}", env!("CARGO_PKG_VERSION"))
}

pub struct FetchResult {
    pub issues: usize,
    pub comments: usize,
    pub skipped_pull_requests: usize,
}

pub struct FetchPage {
    pub issues: Vec<IssueRecord>,
    pub comments: Vec<CommentRecord>,
    pub skipped_pull_requests: usize,
    pub cursor_updates: Vec<CursorUpdate>,
}

pub enum FetchOutcome {
    Fetched(FetchResult),
    Backoff(BackoffPlan),
}

pub struct BackoffPlan {
    pub reason: String,
    pub scope: String,
    pub retry_after_seconds: i64,
    pub reset_at: Option<String>,
}

pub struct TargetIssueFetch {
    pub issue: IssueRecord,
    pub comments: Vec<CommentRecord>,
    pub lifecycle: TargetIssueLifecycle,
}

pub enum TargetIssueFetchOutcome {
    Fetched(Box<TargetIssueFetch>),
    Unavailable(TargetIssueLifecycle),
    Backoff(BackoffPlan),
}

pub struct TargetIssueLifecycle {
    pub status: String,
    pub reason: Option<String>,
    pub http_status: Option<u16>,
    pub alias_chain: Vec<String>,
}

pub struct ReconciliationResult {
    pub checked_sources: usize,
    pub unavailable_sources: Vec<LifecycleFailure>,
}

pub trait ProgressReporter {
    fn report(&self, event: ProgressEvent);
}

pub enum ProgressEvent {
    RepoStarted {
        repo: String,
    },
    IssuePageFetched {
        repo: String,
        item_count: usize,
    },
    RepoProgress {
        repo: String,
        issues: usize,
        comments: usize,
        skipped_pull_requests: usize,
    },
    IssueEndpointNotModified {
        repo: String,
    },
    CommentPageFetched {
        repo: String,
        issue_number: i64,
        item_count: usize,
    },
    Backoff {
        reason: String,
        scope: String,
        retry_after_seconds: i64,
    },
    ReconciliationProgress {
        checked: usize,
        total: usize,
    },
}

pub struct LifecycleFailure {
    pub source_id: String,
    pub reason: String,
}

pub enum LifecycleCheck {
    Active,
    Unavailable { reason: String },
}

pub async fn fetch_issues(
    profile: &Profile,
    token: &str,
    cursors: &[StoredCursor],
    progress: Option<&dyn ProgressReporter>,
    commit_page: &mut dyn FnMut(FetchPage) -> Result<(), QghError>,
) -> Result<FetchOutcome, QghError> {
    let client = reqwest::Client::new();
    let cursor_map = cursor_map(cursors);
    let mut total_issues = 0;
    let mut total_comments = 0;
    let mut total_skipped_pull_requests = 0;

    for repo in &profile.repos {
        let repo_name = repo.full_name();
        emit(
            progress,
            ProgressEvent::RepoStarted {
                repo: repo_name.clone(),
            },
        );
        let endpoint = issue_endpoint(repo);
        let stored_cursor = cursor_map.get(&endpoint);
        let mut max_watermark = stored_cursor.and_then(|cursor| cursor.cursor.clone());
        let mut next_url = Some(issue_url(profile, repo, stored_cursor));
        let mut endpoint_not_modified = false;
        let mut response_etag = stored_cursor.and_then(|cursor| cursor.etag.clone());
        let mut repo_issue_count = 0;
        let mut repo_comment_count = 0;
        let mut repo_skipped_pull_requests = 0;
        let mut last_progress_issue_count = 0;
        while let Some(url) = next_url.take() {
            let mut request = github_get(&client, &url, token);
            if let Some(etag) = stored_cursor.and_then(|cursor| cursor.etag.as_ref()) {
                request = request.header(IF_NONE_MATCH, etag);
            }
            let response = request
                .send()
                .await
                .map_err(|error| QghError::github(error.to_string()))?;
            let status = response.status();
            let headers = response.headers().clone();
            if let Some(backoff) = backoff_from_response(status, &headers, &endpoint) {
                emit_backoff(progress, &backoff);
                wait_for_backoff(&backoff);
                return Ok(FetchOutcome::Backoff(backoff));
            }
            if status == StatusCode::NO_CONTENT {
                commit_page(FetchPage {
                    issues: Vec::new(),
                    comments: Vec::new(),
                    skipped_pull_requests: 0,
                    cursor_updates: vec![CursorUpdate {
                        endpoint: endpoint.clone(),
                        cursor: max_watermark.clone(),
                        etag: response_etag.clone(),
                        not_modified: false,
                    }],
                })?;
                break;
            }
            if status == StatusCode::NOT_MODIFIED {
                endpoint_not_modified = true;
                emit(
                    progress,
                    ProgressEvent::IssueEndpointNotModified {
                        repo: repo_name.clone(),
                    },
                );
                commit_page(FetchPage {
                    issues: Vec::new(),
                    comments: Vec::new(),
                    skipped_pull_requests: 0,
                    cursor_updates: vec![CursorUpdate {
                        endpoint: endpoint.clone(),
                        cursor: max_watermark.clone(),
                        etag: response_etag.clone(),
                        not_modified: endpoint_not_modified,
                    }],
                })?;
                break;
            }
            if !status.is_success() {
                return Err(QghError::github(format!(
                    "GitHub issues request failed with HTTP {status}."
                )));
            }
            if let Some(etag) = header_string(&headers, ETAG) {
                response_etag = Some(etag);
            }
            let page: Vec<ApiIssue> = response
                .json()
                .await
                .map_err(|error| QghError::github(format!("Invalid GitHub issue JSON: {error}")))?;
            emit(
                progress,
                ProgressEvent::IssuePageFetched {
                    repo: repo_name.clone(),
                    item_count: page.len(),
                },
            );
            let indexed_at = now_rfc3339();
            let mut page_issues = Vec::new();
            let mut page_comments = Vec::new();
            let mut page_cursor_updates = Vec::new();
            let mut page_skipped_pull_requests = 0;
            for item in page {
                max_watermark = max_timestamp(max_watermark, &item.updated_at);
                if item.pull_request.is_some() {
                    total_skipped_pull_requests += 1;
                    repo_skipped_pull_requests += 1;
                    page_skipped_pull_requests += 1;
                    continue;
                }
                let issue = item.into_record(profile, repo, &indexed_at);
                match fetch_issue_comments(
                    &client,
                    profile,
                    token,
                    &cursor_map,
                    &mut page_cursor_updates,
                    repo,
                    &issue,
                    progress,
                )
                .await?
                {
                    CommentFetchOutcome::Fetched(fetched_comments) => {
                        repo_comment_count += fetched_comments.len();
                        total_comments += fetched_comments.len();
                        page_comments.extend(fetched_comments);
                    }
                    CommentFetchOutcome::Backoff(backoff) => {
                        wait_for_backoff(&backoff);
                        return Ok(FetchOutcome::Backoff(backoff));
                    }
                }
                page_issues.push(issue);
                repo_issue_count += 1;
                total_issues += 1;
                if should_report_repo_progress(repo_issue_count) {
                    emit(
                        progress,
                        ProgressEvent::RepoProgress {
                            repo: repo_name.clone(),
                            issues: repo_issue_count,
                            comments: repo_comment_count,
                            skipped_pull_requests: repo_skipped_pull_requests,
                        },
                    );
                    last_progress_issue_count = repo_issue_count;
                }
            }
            page_cursor_updates.push(CursorUpdate {
                endpoint: endpoint.clone(),
                cursor: max_watermark.clone(),
                etag: response_etag.clone(),
                not_modified: false,
            });
            commit_page(FetchPage {
                issues: page_issues,
                comments: page_comments,
                skipped_pull_requests: page_skipped_pull_requests,
                cursor_updates: page_cursor_updates,
            })?;
            next_url = next_link(&headers);
        }
        if repo_issue_count != last_progress_issue_count || repo_skipped_pull_requests > 0 {
            emit(
                progress,
                ProgressEvent::RepoProgress {
                    repo: repo_name,
                    issues: repo_issue_count,
                    comments: repo_comment_count,
                    skipped_pull_requests: repo_skipped_pull_requests,
                },
            );
        }
    }

    Ok(FetchOutcome::Fetched(FetchResult {
        issues: total_issues,
        comments: total_comments,
        skipped_pull_requests: total_skipped_pull_requests,
    }))
}

pub async fn fetch_target_issue(
    profile: &Profile,
    token: &str,
    repo: &RepoRef,
    issue_number: i64,
    progress: Option<&dyn ProgressReporter>,
) -> Result<TargetIssueFetchOutcome, QghError> {
    const TRANSFER_FOLLOW_LIMIT: usize = 8;

    let client = lifecycle_client()?;
    let mut current_repo = repo.clone();
    let mut current_issue_number = issue_number;
    let mut alias_chain = Vec::new();
    let mut visited = BTreeSet::new();

    loop {
        let url = issue_object_url(profile, &current_repo, current_issue_number);
        if !visited.insert(url.clone()) {
            return Err(QghError::validation(
                "sync.transfer_cycle",
                "Issue transfer alias chain contains a cycle.",
            )
            .with_details(json!({
                "repo": repo.full_name(),
                "issue_number": issue_number,
                "alias_chain": alias_chain
            }))
            .with_hint("Run targeted refresh for the final issue location after the transfer is corrected upstream."));
        }
        if visited.len() > TRANSFER_FOLLOW_LIMIT {
            return Err(QghError::validation(
                "sync.transfer_chain_too_long",
                "Issue transfer alias chain exceeded the follow limit.",
            )
            .with_details(json!({
                "repo": repo.full_name(),
                "issue_number": issue_number,
                "alias_chain": alias_chain,
                "limit": TRANSFER_FOLLOW_LIMIT
            }))
            .with_hint("Run targeted refresh for the final issue location directly."));
        }

        let response = github_get(&client, &url, token)
            .send()
            .await
            .map_err(|error| QghError::github(error.to_string()))?;
        let status = response.status();
        let headers = response.headers().clone();
        let scope = format!(
            "issue:{}#{}",
            current_repo.full_name(),
            current_issue_number
        );
        if let Some(backoff) = targeted_backoff_from_response(status, &headers, &scope) {
            emit_backoff(progress, &backoff);
            wait_for_backoff(&backoff);
            return Ok(TargetIssueFetchOutcome::Backoff(backoff));
        }

        match status {
            StatusCode::OK => {
                let indexed_at = now_rfc3339();
                let issue = response.json::<ApiIssue>().await.map_err(|error| {
                    QghError::github(format!("Invalid GitHub issue JSON: {error}"))
                })?;
                if issue.pull_request.is_some() {
                    return Err(QghError::validation(
                        "validation.unsupported_source_type",
                        "Targeted sync issue refresh does not index pull requests.",
                    )
                    .with_details(json!({
                        "repo": current_repo.full_name(),
                        "issue_number": current_issue_number
                    }))
                    .with_hint("Use a GitHub Issue number, not a pull request number."));
                }
                let issue = issue.into_record(profile, &current_repo, &indexed_at);
                let mut cursor_updates = Vec::new();
                let empty_cursors = BTreeMap::new();
                let comments = match fetch_issue_comments(
                    &client,
                    profile,
                    token,
                    &empty_cursors,
                    &mut cursor_updates,
                    &current_repo,
                    &issue,
                    progress,
                )
                .await?
                {
                    CommentFetchOutcome::Fetched(comments) => comments,
                    CommentFetchOutcome::Backoff(backoff) => {
                        wait_for_backoff(&backoff);
                        return Ok(TargetIssueFetchOutcome::Backoff(backoff));
                    }
                };
                let lifecycle = if alias_chain.is_empty() {
                    TargetIssueLifecycle {
                        status: "active".to_string(),
                        reason: None,
                        http_status: None,
                        alias_chain,
                    }
                } else {
                    TargetIssueLifecycle {
                        status: "transferred".to_string(),
                        reason: Some("transferred".to_string()),
                        http_status: Some(301),
                        alias_chain,
                    }
                };
                return Ok(TargetIssueFetchOutcome::Fetched(Box::new(
                    TargetIssueFetch {
                        issue,
                        comments,
                        lifecycle,
                    },
                )));
            }
            StatusCode::MOVED_PERMANENTLY
            | StatusCode::FOUND
            | StatusCode::TEMPORARY_REDIRECT
            | StatusCode::PERMANENT_REDIRECT => {
                let Some(location) = header_string(&headers, LOCATION) else {
                    return Ok(TargetIssueFetchOutcome::Unavailable(unavailable_lifecycle(
                        "transferred",
                        status,
                        alias_chain,
                    )));
                };
                alias_chain.push(location.clone());
                let Some((next_repo, next_issue_number)) = parse_issue_location(profile, &location)
                else {
                    return Ok(TargetIssueFetchOutcome::Unavailable(unavailable_lifecycle(
                        "transferred",
                        status,
                        alias_chain,
                    )));
                };
                if !profile.allows_repo(&next_repo.full_name()) {
                    return Err(QghError::validation(
                        "validation.invalid_repo",
                        "Transferred issue target is outside the selected profile allowlist.",
                    )
                    .with_details(json!({
                        "requested_repo": repo.full_name(),
                        "requested_issue_number": issue_number,
                        "transferred_repo": next_repo.full_name(),
                        "transferred_issue_number": next_issue_number
                    }))
                    .with_hint("Add the transferred target repo to the profile allowlist before refreshing it."));
                }
                current_repo = next_repo;
                current_issue_number = next_issue_number;
            }
            StatusCode::NOT_FOUND | StatusCode::GONE => {
                return Ok(TargetIssueFetchOutcome::Unavailable(unavailable_lifecycle(
                    "deleted",
                    status,
                    alias_chain,
                )));
            }
            StatusCode::UNAUTHORIZED => {
                return Err(QghError::auth(
                    "GitHub authentication failed during targeted issue refresh.",
                )
                .with_details(json!({
                    "http_status": status.as_u16(),
                    "scope": scope
                }))
                .with_hint("Refresh the configured GitHub token source, then retry sync issue."));
            }
            StatusCode::FORBIDDEN => {
                let body = response.text().await.unwrap_or_default();
                if response_body_looks_rate_limited(&body) {
                    return Ok(TargetIssueFetchOutcome::Backoff(BackoffPlan {
                        reason: "secondary_rate_limit".to_string(),
                        scope,
                        retry_after_seconds: 60,
                        reset_at: None,
                    }));
                }
                if response_body_confirms_permission_loss(&body) {
                    return Ok(TargetIssueFetchOutcome::Unavailable(unavailable_lifecycle(
                        "permission_loss",
                        status,
                        alias_chain,
                    )));
                }
                return Err(QghError::github(
                    "GitHub targeted issue refresh failed with ambiguous HTTP 403.",
                )
                .with_details(json!({
                    "http_status": status.as_u16(),
                    "scope": scope
                }))
                .with_hint("Retry later or run qgh doctor to distinguish authorization from GitHub throttling."));
            }
            status if status.is_success() => {
                return Err(QghError::github(format!(
                    "GitHub targeted issue refresh returned unsupported HTTP {status}."
                )));
            }
            status => {
                return Err(QghError::github(format!(
                    "GitHub targeted issue refresh failed with HTTP {status}."
                )));
            }
        }
    }
}

pub async fn reconcile_sources(
    profile: &Profile,
    token: &str,
    candidates: &[ReconciliationCandidate],
    progress: Option<&dyn ProgressReporter>,
) -> Result<ReconciliationResult, QghError> {
    let client = lifecycle_client()?;
    let mut unavailable_sources = Vec::new();
    let total = candidates.len();
    for (index, candidate) in candidates.iter().enumerate() {
        match check_candidate_lifecycle(&client, profile, token, candidate).await? {
            LifecycleCheck::Active => {}
            LifecycleCheck::Unavailable { reason } => {
                unavailable_sources.push(LifecycleFailure {
                    source_id: candidate.source_id.clone(),
                    reason,
                });
            }
        }
        let checked = index + 1;
        if should_report_reconciliation_progress(checked, total) {
            emit(
                progress,
                ProgressEvent::ReconciliationProgress { checked, total },
            );
        }
    }
    Ok(ReconciliationResult {
        checked_sources: candidates.len(),
        unavailable_sources,
    })
}

pub async fn check_source_lifecycle(
    profile: &Profile,
    token: &str,
    candidate: &ReconciliationCandidate,
) -> Result<LifecycleCheck, QghError> {
    let client = lifecycle_client()?;
    check_candidate_lifecycle(&client, profile, token, candidate).await
}

#[allow(clippy::too_many_arguments)]
async fn fetch_issue_comments(
    client: &reqwest::Client,
    profile: &Profile,
    token: &str,
    cursor_map: &BTreeMap<String, StoredCursor>,
    cursor_updates: &mut Vec<CursorUpdate>,
    repo: &RepoRef,
    issue: &IssueRecord,
    progress: Option<&dyn ProgressReporter>,
) -> Result<CommentFetchOutcome, QghError> {
    let mut comments = Vec::new();
    let endpoint = comment_endpoint(repo, issue.number);
    let stored_cursor = cursor_map.get(&endpoint);
    let mut max_watermark = stored_cursor.and_then(|cursor| cursor.cursor.clone());
    let mut response_etag = stored_cursor.and_then(|cursor| cursor.etag.clone());
    let mut endpoint_not_modified = false;
    let mut next_url = Some(comment_url(profile, repo, issue.number, stored_cursor));
    while let Some(url) = next_url.take() {
        let mut request = github_get(client, &url, token);
        if let Some(etag) = stored_cursor.and_then(|cursor| cursor.etag.as_ref()) {
            request = request.header(IF_NONE_MATCH, etag);
        }
        let response = request
            .send()
            .await
            .map_err(|error| QghError::github(error.to_string()))?;
        let status = response.status();
        let headers = response.headers().clone();
        if let Some(backoff) = backoff_from_response(status, &headers, &endpoint) {
            emit_backoff(progress, &backoff);
            return Ok(CommentFetchOutcome::Backoff(backoff));
        }
        if status == StatusCode::NO_CONTENT {
            break;
        }
        if status == StatusCode::NOT_MODIFIED {
            endpoint_not_modified = true;
            break;
        }
        if !status.is_success() {
            return Err(QghError::github(format!(
                "GitHub issue comments request failed with HTTP {status}."
            )));
        }
        response_etag = header_string(&headers, ETAG).or(response_etag);
        let page: Vec<ApiComment> = response
            .json()
            .await
            .map_err(|error| QghError::github(format!("Invalid GitHub comment JSON: {error}")))?;
        if !page.is_empty() {
            emit(
                progress,
                ProgressEvent::CommentPageFetched {
                    repo: repo.full_name(),
                    issue_number: issue.number,
                    item_count: page.len(),
                },
            );
        }
        let indexed_at = now_rfc3339();
        for comment in page {
            max_watermark = max_timestamp(max_watermark, &comment.updated_at);
            comments.push(comment.into_record(profile, repo, issue, &indexed_at));
        }
        next_url = next_link(&headers);
    }
    cursor_updates.push(CursorUpdate {
        endpoint,
        cursor: max_watermark,
        etag: response_etag,
        not_modified: endpoint_not_modified,
    });
    Ok(CommentFetchOutcome::Fetched(comments))
}

fn emit(progress: Option<&dyn ProgressReporter>, event: ProgressEvent) {
    if let Some(progress) = progress {
        progress.report(event);
    }
}

fn emit_backoff(progress: Option<&dyn ProgressReporter>, backoff: &BackoffPlan) {
    emit(
        progress,
        ProgressEvent::Backoff {
            reason: backoff.reason.clone(),
            scope: backoff.scope.clone(),
            retry_after_seconds: backoff.retry_after_seconds,
        },
    );
}

fn should_report_repo_progress(issue_count: usize) -> bool {
    issue_count == 1 || issue_count.is_multiple_of(25)
}

fn should_report_reconciliation_progress(checked: usize, total: usize) -> bool {
    checked == 1 || checked == total || checked.is_multiple_of(25)
}

enum CommentFetchOutcome {
    Fetched(Vec<CommentRecord>),
    Backoff(BackoffPlan),
}

fn backoff_from_response(
    status: StatusCode,
    headers: &HeaderMap,
    scope: &str,
) -> Option<BackoffPlan> {
    if status != StatusCode::FORBIDDEN && status != StatusCode::TOO_MANY_REQUESTS {
        return None;
    }

    let remaining = headers
        .get("x-ratelimit-remaining")
        .and_then(|value| value.to_str().ok());
    if remaining == Some("0") {
        let reset_epoch = headers
            .get("x-ratelimit-reset")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<i64>().ok());
        let reset_at = reset_epoch
            .and_then(|epoch| DateTime::from_timestamp(epoch, 0))
            .map(|timestamp| timestamp.to_rfc3339_opts(SecondsFormat::Secs, true));
        let retry_after_seconds = reset_epoch
            .map(|epoch| (epoch - Utc::now().timestamp()).max(0))
            .unwrap_or(60);
        return Some(BackoffPlan {
            reason: "primary_rate_limit".to_string(),
            scope: scope.to_string(),
            retry_after_seconds,
            reset_at,
        });
    }

    let retry_after_seconds = headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(60);
    Some(BackoffPlan {
        reason: "secondary_rate_limit".to_string(),
        scope: scope.to_string(),
        retry_after_seconds,
        reset_at: None,
    })
}

fn targeted_backoff_from_response(
    status: StatusCode,
    headers: &HeaderMap,
    scope: &str,
) -> Option<BackoffPlan> {
    if status == StatusCode::TOO_MANY_REQUESTS {
        return backoff_from_response(status, headers, scope);
    }
    if status != StatusCode::FORBIDDEN {
        return None;
    }
    let remaining = headers
        .get("x-ratelimit-remaining")
        .and_then(|value| value.to_str().ok());
    if remaining == Some("0") || headers.get(RETRY_AFTER).is_some() {
        return backoff_from_response(status, headers, scope);
    }
    None
}

fn response_body_looks_rate_limited(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("rate limit") || lower.contains("abuse detection")
}

fn response_body_confirms_permission_loss(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("resource not accessible")
        || lower.contains("permission")
        || lower.contains("must have")
}

fn wait_for_backoff(backoff: &BackoffPlan) {
    let seconds = backoff.retry_after_seconds.clamp(0, 1);
    if seconds > 0 {
        std::thread::sleep(std::time::Duration::from_secs(seconds as u64));
    }
}

fn unavailable_lifecycle(
    reason: &str,
    status: StatusCode,
    alias_chain: Vec<String>,
) -> TargetIssueLifecycle {
    TargetIssueLifecycle {
        status: reason.to_string(),
        reason: Some(reason.to_string()),
        http_status: Some(status.as_u16()),
        alias_chain,
    }
}

fn lifecycle_client() -> Result<reqwest::Client, QghError> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| QghError::github(error.to_string()))
}

async fn check_candidate_lifecycle(
    client: &reqwest::Client,
    profile: &Profile,
    token: &str,
    candidate: &ReconciliationCandidate,
) -> Result<LifecycleCheck, QghError> {
    let url = source_check_url(profile, candidate)?;
    let response = github_get(client, &url, token)
        .send()
        .await
        .map_err(|error| QghError::github(error.to_string()))?;
    // Reason *strings* are unified with the targeted-refresh path
    // (transferred / deleted / permission_loss). The control flow still differs:
    // fetch_target_issue disambiguates 403 (rate-limit vs permission) and backs
    // off, whereas this reconcile probe maps 401/403 straight to permission_loss.
    Ok(match response.status() {
        StatusCode::OK => LifecycleCheck::Active,
        StatusCode::NOT_FOUND | StatusCode::GONE => LifecycleCheck::Unavailable {
            reason: "deleted".to_string(),
        },
        StatusCode::MOVED_PERMANENTLY
        | StatusCode::FOUND
        | StatusCode::TEMPORARY_REDIRECT
        | StatusCode::PERMANENT_REDIRECT => LifecycleCheck::Unavailable {
            reason: "transferred".to_string(),
        },
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => LifecycleCheck::Unavailable {
            reason: "permission_loss".to_string(),
        },
        status if status.is_success() => LifecycleCheck::Active,
        status => {
            return Err(QghError::github(format!(
                "GitHub source lifecycle check failed with HTTP {status}."
            )));
        }
    })
}

fn github_get(client: &reqwest::Client, url: &str, token: &str) -> reqwest::RequestBuilder {
    client
        .get(url)
        .bearer_auth(token)
        .header("accept", "application/vnd.github+json")
        .header("user-agent", user_agent())
        .header("x-github-api-version", GITHUB_API_VERSION)
}

fn source_check_url(
    profile: &Profile,
    candidate: &ReconciliationCandidate,
) -> Result<String, QghError> {
    let Some((owner, repo)) = candidate.repo.split_once('/') else {
        return Err(QghError::validation(
            "validation.invalid_repo",
            "Stored repo must use owner/repo format.",
        ));
    };
    match candidate.entity_type.as_str() {
        "issue" => Ok(format!(
            "{}/repos/{owner}/{repo}/issues/{}",
            profile.api_base_url, candidate.issue_number
        )),
        "issue_comment" => Ok(format!(
            "{}/repos/{owner}/{repo}/issues/comments/{}",
            profile.api_base_url, candidate.github_id
        )),
        _ => Err(QghError::validation(
            "validation.unsupported_source_type",
            "Unsupported source type for lifecycle check.",
        )),
    }
}

fn parse_issue_location(profile: &Profile, location: &str) -> Option<(RepoRef, i64)> {
    let base = reqwest::Url::parse(&profile.api_base_url).ok()?;
    let url = reqwest::Url::parse(location)
        .ok()
        .or_else(|| base.join(location).ok())?;
    let segments = url.path_segments()?.collect::<Vec<_>>();
    let repos_index = segments.iter().position(|segment| *segment == "repos")?;
    let owner = segments.get(repos_index + 1)?;
    let name = segments.get(repos_index + 2)?;
    if segments.get(repos_index + 3) != Some(&"issues") {
        return None;
    }
    let issue_number = segments.get(repos_index + 4)?.parse::<i64>().ok()?;
    Some((
        RepoRef {
            owner: owner.to_string(),
            name: name.to_string(),
        },
        issue_number,
    ))
}

#[derive(Debug, Deserialize)]
struct ApiIssue {
    id: i64,
    node_id: String,
    number: i64,
    title: String,
    body: Option<String>,
    state: String,
    labels: Vec<ApiLabel>,
    milestone: Option<ApiMilestone>,
    assignees: Vec<ApiUser>,
    user: Option<ApiUser>,
    created_at: String,
    updated_at: String,
    closed_at: Option<String>,
    html_url: String,
    #[serde(default)]
    pull_request: Option<serde_json::Value>,
}

impl ApiIssue {
    fn into_record(self, profile: &Profile, repo: &RepoRef, indexed_at: &str) -> IssueRecord {
        let body = self.body.unwrap_or_default();
        let body_hash = hex_sha256(&body);
        let encoded_node_id = utf8_percent_encode(&self.node_id, SOURCE_ID_ENCODE_SET).to_string();
        IssueRecord {
            source_id: format!("qgh://{}/issue/{}", profile.host, encoded_node_id),
            host: profile.host.clone(),
            repo: repo.full_name(),
            node_id: self.node_id,
            github_id: self.id,
            number: self.number,
            title: self.title,
            body,
            state: self.state,
            labels: self.labels.into_iter().map(|label| label.name).collect(),
            milestone: self.milestone.map(|milestone| milestone.title),
            assignees: self.assignees.into_iter().map(|user| user.login).collect(),
            author: self.user.map(|user| user.login),
            created_at: self.created_at,
            updated_at: self.updated_at,
            closed_at: self.closed_at,
            canonical_url: self.html_url,
            body_hash,
            indexed_at: indexed_at.to_string(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct ApiLabel {
    name: String,
}

#[derive(Debug, Deserialize)]
struct ApiMilestone {
    title: String,
}

#[derive(Debug, Deserialize)]
struct ApiUser {
    login: String,
}

#[derive(Debug, Deserialize)]
struct ApiComment {
    id: i64,
    node_id: String,
    body: Option<String>,
    html_url: String,
    created_at: String,
    updated_at: String,
    user: Option<ApiUser>,
}

impl ApiComment {
    fn into_record(
        self,
        profile: &Profile,
        repo: &RepoRef,
        issue: &IssueRecord,
        indexed_at: &str,
    ) -> CommentRecord {
        let body = self.body.unwrap_or_default();
        let body_hash = hex_sha256(&body);
        let encoded_node_id = utf8_percent_encode(&self.node_id, SOURCE_ID_ENCODE_SET).to_string();
        CommentRecord {
            source_id: format!("qgh://{}/issue-comment/{}", profile.host, encoded_node_id),
            host: profile.host.clone(),
            repo: repo.full_name(),
            node_id: self.node_id,
            github_id: self.id,
            body,
            author: self.user.map(|user| user.login),
            created_at: self.created_at,
            updated_at: self.updated_at,
            canonical_url: self.html_url,
            body_hash,
            indexed_at: indexed_at.to_string(),
            parent_issue_source_id: issue.source_id.clone(),
            parent_issue_number: issue.number,
            parent_issue_title: issue.title.clone(),
            parent_issue_canonical_url: issue.canonical_url.clone(),
        }
    }
}

fn next_link(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(LINK)?.to_str().ok()?;
    for part in value.split(',') {
        let trimmed = part.trim();
        if !trimmed.contains("rel=\"next\"") {
            continue;
        }
        let start = trimmed.find('<')? + 1;
        let end = trimmed[start..].find('>')? + start;
        return Some(trimmed[start..end].to_string());
    }
    None
}

fn cursor_map(cursors: &[StoredCursor]) -> BTreeMap<String, StoredCursor> {
    cursors
        .iter()
        .cloned()
        .map(|cursor| (cursor.endpoint.clone(), cursor))
        .collect()
}

fn issue_endpoint(repo: &RepoRef) -> String {
    format!("issues:{}", repo.full_name())
}

fn comment_endpoint(repo: &RepoRef, issue_number: i64) -> String {
    format!("comments:{}#{issue_number}", repo.full_name())
}

fn issue_url(profile: &Profile, repo: &RepoRef, cursor: Option<&StoredCursor>) -> String {
    let mut url = format!(
        "{}/repos/{}/{}/issues?state=all&sort=updated&direction=asc&per_page=100",
        profile.api_base_url, repo.owner, repo.name
    );
    if let Some(since) = cursor
        .and_then(|cursor| cursor.cursor.as_deref())
        .map(overlapped_since)
    {
        url.push_str("&since=");
        url.push_str(&utf8_percent_encode(&since, SOURCE_ID_ENCODE_SET).to_string());
    }
    url
}

fn issue_object_url(profile: &Profile, repo: &RepoRef, issue_number: i64) -> String {
    format!(
        "{}/repos/{}/{}/issues/{issue_number}",
        profile.api_base_url, repo.owner, repo.name
    )
}

fn comment_url(
    profile: &Profile,
    repo: &RepoRef,
    issue_number: i64,
    cursor: Option<&StoredCursor>,
) -> String {
    let mut url = format!(
        "{}/repos/{}/{}/issues/{issue_number}/comments?per_page=100",
        profile.api_base_url, repo.owner, repo.name
    );
    if let Some(since) = cursor
        .and_then(|cursor| cursor.cursor.as_deref())
        .map(overlapped_since)
    {
        url.push_str("&since=");
        url.push_str(&utf8_percent_encode(&since, SOURCE_ID_ENCODE_SET).to_string());
    }
    url
}

fn overlapped_since(timestamp: &str) -> String {
    DateTime::parse_from_rfc3339(timestamp)
        .map(|parsed| {
            (parsed.with_timezone(&Utc) - Duration::seconds(60))
                .to_rfc3339_opts(SecondsFormat::Secs, true)
        })
        .unwrap_or_else(|_| timestamp.to_string())
}

fn max_timestamp(current: Option<String>, candidate: &str) -> Option<String> {
    match current {
        Some(current) if current.as_str() >= candidate => Some(current),
        _ => Some(candidate.to_string()),
    }
}

fn header_string(headers: &HeaderMap, name: reqwest::header::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_string())
}

fn hex_sha256(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}
