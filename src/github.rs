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
use std::time::Duration as StdDuration;

const GITHUB_REQUEST_TIMEOUT: StdDuration = StdDuration::from_secs(30);

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
    pub confirmed_permission_lost_repos: Vec<ConfirmedRepositoryPermissionLoss>,
    pub confirmed_source_deletions: Vec<ConfirmedSourceDeletion>,
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

pub enum ClassifiedFetchOutcome {
    Fetched(FetchResult),
    Interrupted(LifecycleInterruption),
}

#[derive(Debug, Clone, PartialEq, Eq)]
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

pub enum ClassifiedTargetIssueFetchOutcome {
    Fetched(Box<TargetIssueFetch>),
    Confirmed {
        state: ConfirmedRemoteState,
        repo: String,
        issue_number: i64,
        lifecycle: TargetIssueLifecycle,
    },
    AuthenticationFailed,
    Backoff(BackoffPlan),
    Transient(GitHubTransientKind),
    AmbiguousForbidden,
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
    pub confirmed_permission_lost_repos: Vec<ConfirmedRepositoryPermissionLoss>,
    pub interruption: Option<LifecycleInterruption>,
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
    pub repo: String,
    pub entity_type: String,
    pub issue_number: i64,
    pub reason: String,
    pub state: ConfirmedRemoteState,
    pub http_status: u16,
}

pub enum LifecycleCheck {
    Active,
    Unavailable { reason: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmedRemoteState {
    SourceDeleted,
    SourceTransferred,
    RepositoryPermissionLoss,
}

impl ConfirmedRemoteState {
    pub fn reason(self) -> &'static str {
        match self {
            Self::SourceDeleted => "deleted",
            Self::SourceTransferred => "transferred",
            Self::RepositoryPermissionLoss => "permission_loss",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitHubTransientKind {
    Timeout,
    Network,
    Server,
    UnexpectedResponse,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmedRepositoryPermissionLoss {
    pub repo: String,
    pub http_status: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmedSourceDeletion {
    pub source_id: String,
    pub repo: String,
    pub entity_type: String,
    pub issue_number: i64,
    pub http_status: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepositoryAccessOutcome {
    Accessible,
    ConfirmedPermissionLoss(ConfirmedRepositoryPermissionLoss),
    AuthenticationFailed,
    Backoff(BackoffPlan),
    Transient(GitHubTransientKind),
    AmbiguousForbidden,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClassifiedLifecycleCheck {
    Active,
    Confirmed {
        state: ConfirmedRemoteState,
        http_status: u16,
    },
    AuthenticationFailed,
    Backoff(BackoffPlan),
    Transient(GitHubTransientKind),
    AmbiguousForbidden,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitHubInterruption {
    AuthenticationFailed,
    Backoff(BackoffPlan),
    Transient(GitHubTransientKind),
    AmbiguousForbidden,
}

pub type LifecycleInterruption = GitHubInterruption;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PermissionEvidence {
    Forbidden { http_status: u16 },
    NotFound { http_status: u16 },
    Gone { http_status: u16 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResponseTarget {
    Source,
    Repository,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ResponseDisposition {
    Success,
    Redirect,
    PermissionCandidate { http_status: u16 },
    SourceGone { http_status: u16 },
    AuthenticationFailed,
    Backoff(BackoffPlan),
    Transient(GitHubTransientKind),
    AmbiguousForbidden,
}

pub async fn fetch_issues(
    profile: &Profile,
    token: &str,
    cursors: &[StoredCursor],
    fetch_comments: bool,
    progress: Option<&dyn ProgressReporter>,
    commit_page: &mut dyn FnMut(FetchPage) -> Result<(), QghError>,
) -> Result<FetchOutcome, QghError> {
    legacy_fetch_outcome(
        fetch_issues_classified(
            profile,
            token,
            cursors,
            fetch_comments,
            progress,
            commit_page,
        )
        .await?,
    )
}

fn legacy_fetch_outcome(outcome: ClassifiedFetchOutcome) -> Result<FetchOutcome, QghError> {
    Ok(match outcome {
        ClassifiedFetchOutcome::Fetched(fetched) => FetchOutcome::Fetched(fetched),
        ClassifiedFetchOutcome::Interrupted(LifecycleInterruption::Backoff(plan)) => {
            FetchOutcome::Backoff(plan)
        }
        ClassifiedFetchOutcome::Interrupted(LifecycleInterruption::AuthenticationFailed) => {
            return Err(authentication_failure());
        }
        ClassifiedFetchOutcome::Interrupted(
            LifecycleInterruption::Transient(_) | LifecycleInterruption::AmbiguousForbidden,
        ) => return Err(github_unavailable()),
    })
}

pub async fn fetch_issues_classified(
    profile: &Profile,
    token: &str,
    cursors: &[StoredCursor],
    fetch_comments: bool,
    progress: Option<&dyn ProgressReporter>,
    commit_page: &mut dyn FnMut(FetchPage) -> Result<(), QghError>,
) -> Result<ClassifiedFetchOutcome, QghError> {
    let client = lifecycle_client()?;
    let cursor_map = cursor_map(cursors);
    let mut total_issues = 0;
    let mut total_comments = 0;
    let mut total_skipped_pull_requests = 0;
    let mut confirmed_permission_lost_repos = BTreeMap::new();
    let mut confirmed_source_deletions = BTreeMap::new();

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
        let mut response_etag = stored_cursor.and_then(|cursor| cursor.etag.clone());
        let mut repo_issue_count = 0;
        let mut repo_comment_count = 0;
        let mut repo_skipped_pull_requests = 0;
        let mut last_progress_issue_count = 0;
        'repo_pages: while let Some(url) = next_url.take() {
            let mut request = github_get(&client, &url, token);
            if let Some(etag) = stored_cursor.and_then(|cursor| cursor.etag.as_ref()) {
                request = request.header(IF_NONE_MATCH, etag);
            }
            let response = match request.send().await {
                Ok(response) => response,
                Err(error) => {
                    return Ok(ClassifiedFetchOutcome::Interrupted(
                        LifecycleInterruption::Transient(classify_transport_failure(&error)),
                    ));
                }
            };
            let status = response.status();
            let headers = response.headers().clone();
            if let Some(backoff) = backoff_from_response(status, &headers, &endpoint) {
                emit_backoff(progress, &backoff);
                wait_for_backoff(&backoff);
                return Ok(ClassifiedFetchOutcome::Interrupted(
                    LifecycleInterruption::Backoff(backoff),
                ));
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
                        not_modified: true,
                    }],
                })?;
                break;
            }
            if !status.is_success() {
                let body = if status == StatusCode::FORBIDDEN {
                    response.text().await.unwrap_or_default()
                } else {
                    String::new()
                };
                let disposition = classify_response(
                    status,
                    &headers,
                    &body,
                    &endpoint,
                    ResponseTarget::Repository,
                );
                if matches!(disposition, ResponseDisposition::PermissionCandidate { .. }) {
                    match confirm_repository_after_prior_denial(
                        &client, profile, token, repo, progress,
                    )
                    .await
                    {
                        RepositoryAccessOutcome::ConfirmedPermissionLoss(confirmed) => {
                            confirmed_permission_lost_repos
                                .entry(confirmed.repo.clone())
                                .or_insert(confirmed);
                            break;
                        }
                        RepositoryAccessOutcome::Backoff(plan) => {
                            wait_for_backoff(&plan);
                            return Ok(ClassifiedFetchOutcome::Interrupted(
                                LifecycleInterruption::Backoff(plan),
                            ));
                        }
                        RepositoryAccessOutcome::AuthenticationFailed => {
                            return Ok(ClassifiedFetchOutcome::Interrupted(
                                LifecycleInterruption::AuthenticationFailed,
                            ));
                        }
                        RepositoryAccessOutcome::Accessible => {
                            return Ok(ClassifiedFetchOutcome::Interrupted(
                                LifecycleInterruption::Transient(
                                    GitHubTransientKind::UnexpectedResponse,
                                ),
                            ));
                        }
                        RepositoryAccessOutcome::Transient(kind) => {
                            return Ok(ClassifiedFetchOutcome::Interrupted(
                                LifecycleInterruption::Transient(kind),
                            ));
                        }
                        RepositoryAccessOutcome::AmbiguousForbidden => {
                            return Ok(ClassifiedFetchOutcome::Interrupted(
                                LifecycleInterruption::AmbiguousForbidden,
                            ));
                        }
                    }
                }
                return Ok(ClassifiedFetchOutcome::Interrupted(match disposition {
                    ResponseDisposition::AuthenticationFailed => {
                        LifecycleInterruption::AuthenticationFailed
                    }
                    ResponseDisposition::Backoff(plan) => {
                        wait_for_backoff(&plan);
                        LifecycleInterruption::Backoff(plan)
                    }
                    ResponseDisposition::Transient(kind) => LifecycleInterruption::Transient(kind),
                    ResponseDisposition::AmbiguousForbidden => {
                        LifecycleInterruption::AmbiguousForbidden
                    }
                    ResponseDisposition::SourceGone { .. } | ResponseDisposition::Redirect => {
                        LifecycleInterruption::Transient(GitHubTransientKind::UnexpectedResponse)
                    }
                    ResponseDisposition::Success
                    | ResponseDisposition::PermissionCandidate { .. } => unreachable!(),
                }));
            }
            if let Some(etag) = header_string(&headers, ETAG) {
                response_etag = Some(etag);
            }
            let page: Vec<ApiIssue> = response
                .json()
                .await
                .map_err(|_| QghError::github("GitHub returned invalid issue JSON."))?;
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
                if fetch_comments {
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
                            return Ok(ClassifiedFetchOutcome::Interrupted(
                                LifecycleInterruption::Backoff(backoff),
                            ));
                        }
                        CommentFetchOutcome::ConfirmedRepositoryPermissionLoss(confirmed) => {
                            confirmed_permission_lost_repos
                                .entry(confirmed.repo.clone())
                                .or_insert(confirmed);
                            break 'repo_pages;
                        }
                        CommentFetchOutcome::SourceDeleted { http_status } => {
                            confirmed_source_deletions
                                .entry(issue.source_id.clone())
                                .or_insert(ConfirmedSourceDeletion {
                                    source_id: issue.source_id.clone(),
                                    repo: issue.repo.clone(),
                                    entity_type: "issue".to_string(),
                                    issue_number: issue.number,
                                    http_status,
                                });
                            break 'repo_pages;
                        }
                        CommentFetchOutcome::AuthenticationFailed => {
                            return Ok(ClassifiedFetchOutcome::Interrupted(
                                LifecycleInterruption::AuthenticationFailed,
                            ));
                        }
                        CommentFetchOutcome::Transient(kind) => {
                            return Ok(ClassifiedFetchOutcome::Interrupted(
                                LifecycleInterruption::Transient(kind),
                            ));
                        }
                        CommentFetchOutcome::AmbiguousForbidden => {
                            return Ok(ClassifiedFetchOutcome::Interrupted(
                                LifecycleInterruption::AmbiguousForbidden,
                            ));
                        }
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

    Ok(ClassifiedFetchOutcome::Fetched(FetchResult {
        issues: total_issues,
        comments: total_comments,
        skipped_pull_requests: total_skipped_pull_requests,
        confirmed_permission_lost_repos: confirmed_permission_lost_repos.into_values().collect(),
        confirmed_source_deletions: confirmed_source_deletions.into_values().collect(),
    }))
}

pub struct BackfillOutcome {
    pub issues: usize,
    pub comments: usize,
    pub skipped_pull_requests: usize,
    pub confirmed_permission_lost_repos: Vec<ConfirmedRepositoryPermissionLoss>,
    pub confirmed_source_deletions: Vec<ConfirmedSourceDeletion>,
    /// True only when every repo paginated its history to the end this run (no
    /// page/duration budget cutoff and no backoff), i.e. history is complete.
    pub all_reached_end: bool,
    pub backoff: Option<BackoffPlan>,
    pub interruption: Option<LifecycleInterruption>,
}

/// Budgeted historical backfill: walk issues older-first from each repo's
/// history cursor (`state=all&sort=updated&direction=asc`), fetching every
/// issue's comments so historical comment coverage is filled (repo-level `since`
/// listing only returns fresh comments). Each repo keeps its OWN `history:<repo>`
/// cursor committed per page, so a budget/backoff cutoff resumes each repo from
/// its own watermark without skipping later repos. Bounded by `max_pages`
/// (issue-list pages) and `max_duration_seconds`. Does not touch the live cursor.
#[allow(clippy::too_many_arguments)]
pub async fn fetch_backfill_issues(
    profile: &Profile,
    token: &str,
    cursors: &[StoredCursor],
    max_pages: Option<usize>,
    max_duration_seconds: Option<i64>,
    progress: Option<&dyn ProgressReporter>,
    commit_page: &mut dyn FnMut(FetchPage) -> Result<(), QghError>,
) -> Result<BackfillOutcome, QghError> {
    let outcome = fetch_backfill_issues_classified(
        profile,
        token,
        cursors,
        max_pages,
        max_duration_seconds,
        progress,
        commit_page,
    )
    .await?;
    match outcome.interruption.as_ref() {
        None => Ok(outcome),
        Some(LifecycleInterruption::AuthenticationFailed) => Err(authentication_failure()),
        Some(
            LifecycleInterruption::Backoff(_)
            | LifecycleInterruption::Transient(_)
            | LifecycleInterruption::AmbiguousForbidden,
        ) => Err(github_unavailable()),
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn fetch_backfill_issues_classified(
    profile: &Profile,
    token: &str,
    cursors: &[StoredCursor],
    max_pages: Option<usize>,
    max_duration_seconds: Option<i64>,
    progress: Option<&dyn ProgressReporter>,
    commit_page: &mut dyn FnMut(FetchPage) -> Result<(), QghError>,
) -> Result<BackfillOutcome, QghError> {
    const EPOCH: &str = "1970-01-01T00:00:00Z";
    let client = lifecycle_client()?;
    let cursor_map = cursor_map(cursors);
    // Empty so every backfilled issue's comments are fetched in full (historical
    // comment coverage), not filtered by a stored comment `since`.
    let comment_cursor_map: BTreeMap<String, StoredCursor> = BTreeMap::new();
    let started = Utc::now();
    let mut total_issues = 0;
    let mut total_comments = 0;
    let mut total_skipped_pull_requests = 0;
    let mut all_reached_end = true;
    let mut pages = 0usize;
    let mut backoff = None;
    let mut interruption = None;
    let mut confirmed_permission_lost_repos = BTreeMap::new();
    let mut confirmed_source_deletions = BTreeMap::new();

    'repos: for repo in &profile.repos {
        let history_endpoint = backfill_endpoint(repo);
        let since = cursor_map
            .get(&history_endpoint)
            .and_then(|cursor| cursor.cursor.clone())
            .unwrap_or_else(|| EPOCH.to_string());
        let mut watermark = Some(since.clone());
        let synthetic = StoredCursor {
            endpoint: history_endpoint.clone(),
            cursor: Some(since),
            etag: None,
        };
        let mut next_url = Some(issue_url(profile, repo, Some(&synthetic)));
        'backfill_pages: while let Some(url) = next_url.take() {
            if max_pages.is_some_and(|max| pages >= max)
                || max_duration_seconds
                    .is_some_and(|secs| (Utc::now() - started).num_seconds() >= secs)
            {
                all_reached_end = false;
                break 'repos;
            }
            let response = match github_get(&client, &url, token).send().await {
                Ok(response) => response,
                Err(error) => {
                    interruption = Some(LifecycleInterruption::Transient(
                        classify_transport_failure(&error),
                    ));
                    all_reached_end = false;
                    break 'repos;
                }
            };
            let status = response.status();
            let headers = response.headers().clone();
            if let Some(plan) = backoff_from_response(status, &headers, &history_endpoint) {
                emit_backoff(progress, &plan);
                wait_for_backoff(&plan);
                backoff = Some(plan);
                all_reached_end = false;
                break 'repos;
            }
            if status == StatusCode::NO_CONTENT || status == StatusCode::NOT_MODIFIED {
                break;
            }
            if !status.is_success() {
                let body = if status == StatusCode::FORBIDDEN {
                    response.text().await.unwrap_or_default()
                } else {
                    String::new()
                };
                let disposition = classify_response(
                    status,
                    &headers,
                    &body,
                    &history_endpoint,
                    ResponseTarget::Repository,
                );
                if matches!(disposition, ResponseDisposition::PermissionCandidate { .. }) {
                    match confirm_repository_after_prior_denial(
                        &client, profile, token, repo, progress,
                    )
                    .await
                    {
                        RepositoryAccessOutcome::ConfirmedPermissionLoss(confirmed) => {
                            confirmed_permission_lost_repos
                                .entry(confirmed.repo.clone())
                                .or_insert(confirmed);
                            all_reached_end = false;
                            break;
                        }
                        RepositoryAccessOutcome::Backoff(plan) => {
                            wait_for_backoff(&plan);
                            backoff = Some(plan);
                            all_reached_end = false;
                            break 'repos;
                        }
                        RepositoryAccessOutcome::AuthenticationFailed => {
                            interruption = Some(LifecycleInterruption::AuthenticationFailed);
                            all_reached_end = false;
                            break 'repos;
                        }
                        RepositoryAccessOutcome::Accessible => {
                            interruption = Some(LifecycleInterruption::Transient(
                                GitHubTransientKind::UnexpectedResponse,
                            ));
                            all_reached_end = false;
                            break 'repos;
                        }
                        RepositoryAccessOutcome::Transient(kind) => {
                            interruption = Some(LifecycleInterruption::Transient(kind));
                            all_reached_end = false;
                            break 'repos;
                        }
                        RepositoryAccessOutcome::AmbiguousForbidden => {
                            interruption = Some(LifecycleInterruption::AmbiguousForbidden);
                            all_reached_end = false;
                            break 'repos;
                        }
                    }
                }
                match disposition {
                    ResponseDisposition::AuthenticationFailed => {
                        interruption = Some(LifecycleInterruption::AuthenticationFailed);
                        all_reached_end = false;
                        break 'repos;
                    }
                    ResponseDisposition::Backoff(plan) => {
                        wait_for_backoff(&plan);
                        backoff = Some(plan);
                        all_reached_end = false;
                        break 'repos;
                    }
                    ResponseDisposition::Transient(kind) => {
                        interruption = Some(LifecycleInterruption::Transient(kind));
                        all_reached_end = false;
                        break 'repos;
                    }
                    ResponseDisposition::AmbiguousForbidden => {
                        interruption = Some(LifecycleInterruption::AmbiguousForbidden);
                        all_reached_end = false;
                        break 'repos;
                    }
                    ResponseDisposition::SourceGone { .. } | ResponseDisposition::Redirect => {
                        interruption = Some(LifecycleInterruption::Transient(
                            GitHubTransientKind::UnexpectedResponse,
                        ));
                        all_reached_end = false;
                        break 'repos;
                    }
                    ResponseDisposition::Success
                    | ResponseDisposition::PermissionCandidate { .. } => unreachable!(),
                }
            }
            let page: Vec<ApiIssue> = response
                .json()
                .await
                .map_err(|_| QghError::github("GitHub returned invalid issue JSON."))?;
            pages += 1;
            let indexed_at = now_rfc3339();
            let mut page_issues = Vec::new();
            let mut page_comments = Vec::new();
            let mut page_cursor_updates = Vec::new();
            let mut page_skipped_pull_requests = 0;
            for item in page {
                watermark = max_timestamp(watermark, &item.updated_at);
                if item.pull_request.is_some() {
                    total_skipped_pull_requests += 1;
                    page_skipped_pull_requests += 1;
                    continue;
                }
                let issue = item.into_record(profile, repo, &indexed_at);
                match fetch_issue_comments(
                    &client,
                    profile,
                    token,
                    &comment_cursor_map,
                    &mut page_cursor_updates,
                    repo,
                    &issue,
                    progress,
                )
                .await?
                {
                    CommentFetchOutcome::Fetched(fetched_comments) => {
                        total_comments += fetched_comments.len();
                        page_comments.extend(fetched_comments);
                    }
                    CommentFetchOutcome::Backoff(plan) => {
                        wait_for_backoff(&plan);
                        backoff = Some(plan);
                        break;
                    }
                    CommentFetchOutcome::ConfirmedRepositoryPermissionLoss(confirmed) => {
                        confirmed_permission_lost_repos
                            .entry(confirmed.repo.clone())
                            .or_insert(confirmed);
                        all_reached_end = false;
                        break 'backfill_pages;
                    }
                    CommentFetchOutcome::SourceDeleted { http_status } => {
                        confirmed_source_deletions
                            .entry(issue.source_id.clone())
                            .or_insert(ConfirmedSourceDeletion {
                                source_id: issue.source_id.clone(),
                                repo: issue.repo.clone(),
                                entity_type: "issue".to_string(),
                                issue_number: issue.number,
                                http_status,
                            });
                        all_reached_end = false;
                        break 'backfill_pages;
                    }
                    CommentFetchOutcome::AuthenticationFailed => {
                        interruption = Some(LifecycleInterruption::AuthenticationFailed);
                        all_reached_end = false;
                        break 'repos;
                    }
                    CommentFetchOutcome::Transient(kind) => {
                        interruption = Some(LifecycleInterruption::Transient(kind));
                        all_reached_end = false;
                        break 'repos;
                    }
                    CommentFetchOutcome::AmbiguousForbidden => {
                        interruption = Some(LifecycleInterruption::AmbiguousForbidden);
                        all_reached_end = false;
                        break 'repos;
                    }
                }
                page_issues.push(issue);
                total_issues += 1;
            }
            page_cursor_updates.push(CursorUpdate {
                endpoint: history_endpoint.clone(),
                cursor: watermark.clone(),
                etag: None,
                not_modified: false,
            });
            commit_page(FetchPage {
                issues: page_issues,
                comments: page_comments,
                skipped_pull_requests: page_skipped_pull_requests,
                cursor_updates: page_cursor_updates,
            })?;
            if backoff.is_some() {
                all_reached_end = false;
                break 'repos;
            }
            next_url = next_link(&headers);
        }
    }

    Ok(BackfillOutcome {
        issues: total_issues,
        comments: total_comments,
        skipped_pull_requests: total_skipped_pull_requests,
        confirmed_permission_lost_repos: confirmed_permission_lost_repos.into_values().collect(),
        confirmed_source_deletions: confirmed_source_deletions.into_values().collect(),
        all_reached_end,
        backoff,
        interruption,
    })
}

fn backfill_endpoint(repo: &RepoRef) -> String {
    format!("history:{}", repo.full_name())
}

pub async fn fetch_target_issue(
    profile: &Profile,
    token: &str,
    repo: &RepoRef,
    issue_number: i64,
    progress: Option<&dyn ProgressReporter>,
) -> Result<TargetIssueFetchOutcome, QghError> {
    legacy_target_issue_outcome(
        fetch_target_issue_classified(profile, token, repo, issue_number, progress).await?,
    )
}

fn legacy_target_issue_outcome(
    outcome: ClassifiedTargetIssueFetchOutcome,
) -> Result<TargetIssueFetchOutcome, QghError> {
    Ok(match outcome {
        ClassifiedTargetIssueFetchOutcome::Fetched(fetched) => {
            TargetIssueFetchOutcome::Fetched(fetched)
        }
        ClassifiedTargetIssueFetchOutcome::Confirmed { lifecycle, .. } => {
            TargetIssueFetchOutcome::Unavailable(lifecycle)
        }
        ClassifiedTargetIssueFetchOutcome::Backoff(plan) => TargetIssueFetchOutcome::Backoff(plan),
        ClassifiedTargetIssueFetchOutcome::AuthenticationFailed => {
            return Err(authentication_failure());
        }
        ClassifiedTargetIssueFetchOutcome::Transient(_)
        | ClassifiedTargetIssueFetchOutcome::AmbiguousForbidden => {
            return Err(github_unavailable());
        }
    })
}

pub async fn fetch_target_issue_classified(
    profile: &Profile,
    token: &str,
    repo: &RepoRef,
    issue_number: i64,
    progress: Option<&dyn ProgressReporter>,
) -> Result<ClassifiedTargetIssueFetchOutcome, QghError> {
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

        let response = match github_get(&client, &url, token).send().await {
            Ok(response) => response,
            Err(error) => {
                return Ok(ClassifiedTargetIssueFetchOutcome::Transient(
                    classify_transport_failure(&error),
                ));
            }
        };
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
            return Ok(ClassifiedTargetIssueFetchOutcome::Backoff(backoff));
        }

        match status {
            StatusCode::OK => {
                let indexed_at = now_rfc3339();
                let issue = response
                    .json::<ApiIssue>()
                    .await
                    .map_err(|_| QghError::github("GitHub returned invalid issue JSON."))?;
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
                        return Ok(ClassifiedTargetIssueFetchOutcome::Backoff(backoff));
                    }
                    CommentFetchOutcome::ConfirmedRepositoryPermissionLoss(confirmed) => {
                        return Ok(target_outcome_from_lifecycle_check(
                            ClassifiedLifecycleCheck::Confirmed {
                                state: ConfirmedRemoteState::RepositoryPermissionLoss,
                                http_status: confirmed.http_status,
                            },
                            &current_repo,
                            current_issue_number,
                            alias_chain,
                        ));
                    }
                    CommentFetchOutcome::SourceDeleted { http_status } => {
                        return Ok(target_outcome_from_lifecycle_check(
                            ClassifiedLifecycleCheck::Confirmed {
                                state: ConfirmedRemoteState::SourceDeleted,
                                http_status,
                            },
                            &current_repo,
                            current_issue_number,
                            alias_chain,
                        ));
                    }
                    CommentFetchOutcome::AuthenticationFailed => {
                        return Ok(ClassifiedTargetIssueFetchOutcome::AuthenticationFailed);
                    }
                    CommentFetchOutcome::Transient(kind) => {
                        return Ok(ClassifiedTargetIssueFetchOutcome::Transient(kind));
                    }
                    CommentFetchOutcome::AmbiguousForbidden => {
                        return Ok(ClassifiedTargetIssueFetchOutcome::AmbiguousForbidden);
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
                return Ok(ClassifiedTargetIssueFetchOutcome::Fetched(Box::new(
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
                    return Ok(ClassifiedTargetIssueFetchOutcome::Confirmed {
                        state: ConfirmedRemoteState::SourceTransferred,
                        repo: current_repo.full_name(),
                        issue_number: current_issue_number,
                        lifecycle: unavailable_lifecycle("transferred", status, alias_chain),
                    });
                };
                alias_chain.push(location.clone());
                let Some((next_repo, next_issue_number)) = parse_issue_location(profile, &location)
                else {
                    return Ok(ClassifiedTargetIssueFetchOutcome::Confirmed {
                        state: ConfirmedRemoteState::SourceTransferred,
                        repo: current_repo.full_name(),
                        issue_number: current_issue_number,
                        lifecycle: unavailable_lifecycle("transferred", status, alias_chain),
                    });
                };
                if !profile.allows_repo(&next_repo.full_name()) {
                    return Ok(ClassifiedTargetIssueFetchOutcome::Confirmed {
                        state: ConfirmedRemoteState::SourceTransferred,
                        repo: current_repo.full_name(),
                        issue_number: current_issue_number,
                        lifecycle: unavailable_lifecycle("transferred", status, alias_chain),
                    });
                }
                current_repo = next_repo;
                current_issue_number = next_issue_number;
            }
            StatusCode::NOT_FOUND | StatusCode::GONE => {
                let first = if status == StatusCode::GONE {
                    PermissionEvidence::Gone {
                        http_status: status.as_u16(),
                    }
                } else {
                    PermissionEvidence::NotFound {
                        http_status: status.as_u16(),
                    }
                };
                let confirmation = confirm_source_denial_with_repository(
                    &client,
                    profile,
                    token,
                    &current_repo,
                    first,
                    progress,
                )
                .await;
                return Ok(target_outcome_from_lifecycle_check(
                    confirmation,
                    &current_repo,
                    current_issue_number,
                    alias_chain,
                ));
            }
            StatusCode::UNAUTHORIZED => {
                return Ok(ClassifiedTargetIssueFetchOutcome::AuthenticationFailed);
            }
            StatusCode::FORBIDDEN => {
                let body = response.text().await.unwrap_or_default();
                let disposition =
                    classify_response(status, &headers, &body, &scope, ResponseTarget::Source);
                return Ok(match disposition {
                    ResponseDisposition::Backoff(plan) => {
                        emit_backoff(progress, &plan);
                        ClassifiedTargetIssueFetchOutcome::Backoff(plan)
                    }
                    ResponseDisposition::PermissionCandidate { http_status } => {
                        let confirmation = confirm_source_denial_with_repository(
                            &client,
                            profile,
                            token,
                            &current_repo,
                            PermissionEvidence::Forbidden { http_status },
                            progress,
                        )
                        .await;
                        target_outcome_from_lifecycle_check(
                            confirmation,
                            &current_repo,
                            current_issue_number,
                            alias_chain,
                        )
                    }
                    ResponseDisposition::AmbiguousForbidden => {
                        ClassifiedTargetIssueFetchOutcome::AmbiguousForbidden
                    }
                    _ => ClassifiedTargetIssueFetchOutcome::Transient(
                        GitHubTransientKind::UnexpectedResponse,
                    ),
                });
            }
            status if status.is_success() => {
                return Ok(ClassifiedTargetIssueFetchOutcome::Transient(
                    GitHubTransientKind::UnexpectedResponse,
                ));
            }
            status if status.is_server_error() => {
                return Ok(ClassifiedTargetIssueFetchOutcome::Transient(
                    GitHubTransientKind::Server,
                ));
            }
            _ => {
                return Ok(ClassifiedTargetIssueFetchOutcome::Transient(
                    GitHubTransientKind::UnexpectedResponse,
                ));
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
    let mut confirmed_permission_lost_repos = BTreeMap::new();
    let mut interruption = None;
    let total = candidates.len();
    let mut checked_sources = 0;
    for (index, candidate) in candidates.iter().enumerate() {
        if confirmed_permission_lost_repos.contains_key(&candidate.repo) {
            let covered = index + 1;
            if should_report_reconciliation_progress(covered, total) {
                emit(
                    progress,
                    ProgressEvent::ReconciliationProgress {
                        checked: covered,
                        total,
                    },
                );
            }
            continue;
        }
        let outcome =
            check_candidate_lifecycle_classified(&client, profile, token, candidate, progress)
                .await?;
        checked_sources += 1;
        match outcome {
            ClassifiedLifecycleCheck::Active => {}
            ClassifiedLifecycleCheck::Confirmed {
                state:
                    state @ (ConfirmedRemoteState::SourceDeleted
                    | ConfirmedRemoteState::SourceTransferred),
                http_status,
            } => {
                unavailable_sources.push(LifecycleFailure {
                    source_id: candidate.source_id.clone(),
                    repo: candidate.repo.clone(),
                    entity_type: candidate.entity_type.clone(),
                    issue_number: candidate.issue_number,
                    reason: state.reason().to_string(),
                    state,
                    http_status,
                });
            }
            ClassifiedLifecycleCheck::Confirmed {
                state: ConfirmedRemoteState::RepositoryPermissionLoss,
                http_status,
            } => {
                confirmed_permission_lost_repos
                    .entry(candidate.repo.clone())
                    .or_insert(ConfirmedRepositoryPermissionLoss {
                        repo: candidate.repo.clone(),
                        http_status,
                    });
            }
            ClassifiedLifecycleCheck::AuthenticationFailed => {
                interruption = Some(LifecycleInterruption::AuthenticationFailed);
            }
            ClassifiedLifecycleCheck::Backoff(plan) => {
                interruption = Some(LifecycleInterruption::Backoff(plan));
            }
            ClassifiedLifecycleCheck::Transient(kind) => {
                interruption = Some(LifecycleInterruption::Transient(kind));
            }
            ClassifiedLifecycleCheck::AmbiguousForbidden => {
                interruption = Some(LifecycleInterruption::AmbiguousForbidden);
            }
        }
        let checked = index + 1;
        if should_report_reconciliation_progress(checked, total) {
            emit(
                progress,
                ProgressEvent::ReconciliationProgress { checked, total },
            );
        }
        if interruption.is_some() {
            break;
        }
    }
    Ok(ReconciliationResult {
        checked_sources,
        unavailable_sources,
        confirmed_permission_lost_repos: confirmed_permission_lost_repos.into_values().collect(),
        interruption,
    })
}

pub async fn check_source_lifecycle(
    profile: &Profile,
    token: &str,
    candidate: &ReconciliationCandidate,
) -> Result<LifecycleCheck, QghError> {
    let client = lifecycle_client()?;
    legacy_lifecycle_check(
        check_candidate_lifecycle_classified(&client, profile, token, candidate, None).await?,
    )
}

fn legacy_lifecycle_check(check: ClassifiedLifecycleCheck) -> Result<LifecycleCheck, QghError> {
    match check {
        ClassifiedLifecycleCheck::Active => Ok(LifecycleCheck::Active),
        ClassifiedLifecycleCheck::Confirmed { state, .. } => Ok(LifecycleCheck::Unavailable {
            reason: state.reason().to_string(),
        }),
        ClassifiedLifecycleCheck::AuthenticationFailed => Err(authentication_failure()),
        ClassifiedLifecycleCheck::Backoff(_) => Err(github_unavailable()),
        ClassifiedLifecycleCheck::Transient(_) => Err(github_unavailable()),
        ClassifiedLifecycleCheck::AmbiguousForbidden => Err(github_unavailable()),
    }
}

pub async fn check_source_lifecycle_classified(
    profile: &Profile,
    token: &str,
    candidate: &ReconciliationCandidate,
    progress: Option<&dyn ProgressReporter>,
) -> Result<ClassifiedLifecycleCheck, QghError> {
    let client = lifecycle_client()?;
    check_candidate_lifecycle_classified(&client, profile, token, candidate, progress).await
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
        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => {
                return Ok(CommentFetchOutcome::Transient(classify_transport_failure(
                    &error,
                )));
            }
        };
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
            let body = if status == StatusCode::FORBIDDEN {
                response.text().await.unwrap_or_default()
            } else {
                String::new()
            };
            let disposition =
                classify_response(status, &headers, &body, &endpoint, ResponseTarget::Source);
            if let Some(first) = permission_evidence(&disposition) {
                return Ok(
                    match confirm_source_denial_with_repository(
                        client, profile, token, repo, first, progress,
                    )
                    .await
                    {
                        ClassifiedLifecycleCheck::Confirmed {
                            state: ConfirmedRemoteState::RepositoryPermissionLoss,
                            http_status,
                        } => CommentFetchOutcome::ConfirmedRepositoryPermissionLoss(
                            ConfirmedRepositoryPermissionLoss {
                                repo: repo.full_name(),
                                http_status,
                            },
                        ),
                        ClassifiedLifecycleCheck::Confirmed {
                            state: ConfirmedRemoteState::SourceDeleted,
                            http_status,
                        } => CommentFetchOutcome::SourceDeleted { http_status },
                        ClassifiedLifecycleCheck::AuthenticationFailed => {
                            CommentFetchOutcome::AuthenticationFailed
                        }
                        ClassifiedLifecycleCheck::Backoff(plan) => {
                            CommentFetchOutcome::Backoff(plan)
                        }
                        ClassifiedLifecycleCheck::Transient(kind) => {
                            CommentFetchOutcome::Transient(kind)
                        }
                        ClassifiedLifecycleCheck::AmbiguousForbidden
                        | ClassifiedLifecycleCheck::Active
                        | ClassifiedLifecycleCheck::Confirmed {
                            state: ConfirmedRemoteState::SourceTransferred,
                            ..
                        } => CommentFetchOutcome::AmbiguousForbidden,
                    },
                );
            }
            return Ok(match disposition {
                ResponseDisposition::AuthenticationFailed => {
                    CommentFetchOutcome::AuthenticationFailed
                }
                ResponseDisposition::Backoff(plan) => CommentFetchOutcome::Backoff(plan),
                ResponseDisposition::Transient(kind) => CommentFetchOutcome::Transient(kind),
                ResponseDisposition::AmbiguousForbidden => CommentFetchOutcome::AmbiguousForbidden,
                ResponseDisposition::Redirect => {
                    CommentFetchOutcome::Transient(GitHubTransientKind::UnexpectedResponse)
                }
                ResponseDisposition::Success
                | ResponseDisposition::PermissionCandidate { .. }
                | ResponseDisposition::SourceGone { .. } => unreachable!(),
            });
        }
        response_etag = header_string(&headers, ETAG).or(response_etag);
        let page: Vec<ApiComment> = response
            .json()
            .await
            .map_err(|_| QghError::github("GitHub returned invalid comment JSON."))?;
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

pub struct RepoCommentsResult {
    pub comments: Vec<CommentRecord>,
    pub cursor_updates: Vec<CursorUpdate>,
    pub skipped_pr_comments: usize,
    pub deferred_comments: usize,
    /// Present when a rate-limit interrupted the run; the caller should commit
    /// the (partial) comments/cursor_updates above, then surface backoff.
    pub backoff: Option<BackoffPlan>,
    pub confirmed_permission_lost_repos: Vec<ConfirmedRepositoryPermissionLoss>,
    pub interruption: Option<LifecycleInterruption>,
}

/// Fetch fresh issue comments repo-wide via `/issues/comments?sort=updated&since`
/// instead of one request per issue. Each comment's parent is resolved locally
/// via `resolve_parent`; when the parent is not in the local corpus the type is
/// classified remotely within `parent_resolution_budget` (PR comments are
/// skipped and cached; genuinely-unknown parents are deferred — never guessed).
///
/// The per-repo cursor only advances through comments that were definitively
/// handled with no preceding deferral, so a deferred comment (e.g. its parent
/// issue has not synced yet) is re-fetched next run instead of being silently
/// skipped.
pub async fn fetch_repo_comments(
    profile: &Profile,
    token: &str,
    cursors: &[StoredCursor],
    parent_resolution_budget: usize,
    resolve_parent: &dyn Fn(&str, i64) -> Option<CommentParent>,
    progress: Option<&dyn ProgressReporter>,
) -> Result<RepoCommentsResult, QghError> {
    let outcome = fetch_repo_comments_classified(
        profile,
        token,
        cursors,
        parent_resolution_budget,
        resolve_parent,
        progress,
    )
    .await?;
    match outcome.interruption.as_ref() {
        None => Ok(outcome),
        Some(LifecycleInterruption::AuthenticationFailed) => Err(authentication_failure()),
        Some(
            LifecycleInterruption::Backoff(_)
            | LifecycleInterruption::Transient(_)
            | LifecycleInterruption::AmbiguousForbidden,
        ) => Err(github_unavailable()),
    }
}

pub async fn fetch_repo_comments_classified(
    profile: &Profile,
    token: &str,
    cursors: &[StoredCursor],
    parent_resolution_budget: usize,
    resolve_parent: &dyn Fn(&str, i64) -> Option<CommentParent>,
    progress: Option<&dyn ProgressReporter>,
) -> Result<RepoCommentsResult, QghError> {
    let client = lifecycle_client()?;
    let cursor_map = cursor_map(cursors);
    let mut comments = Vec::new();
    let mut cursor_updates = Vec::new();
    let mut skipped_pr_comments = 0;
    let mut deferred_comments = 0;
    let mut remote_lookups = 0usize;
    let mut backoff = None;
    let mut confirmed_permission_lost_repos = BTreeMap::new();
    let mut interruption = None;

    'repos: for repo in &profile.repos {
        let repo_name = repo.full_name();
        let endpoint = repo_comment_endpoint(repo);
        let stored_cursor = cursor_map.get(&endpoint);
        let mut committed_watermark = stored_cursor.and_then(|cursor| cursor.cursor.clone());
        let mut response_etag = stored_cursor.and_then(|cursor| cursor.etag.clone());
        let mut conditional_etag = stored_cursor.and_then(|cursor| cursor.etag.clone());
        let mut known_pull_requests: BTreeSet<i64> = BTreeSet::new();
        let mut blocked = false;
        let mut next_url = Some(repo_comment_url(profile, repo, stored_cursor));
        while let Some(url) = next_url.take() {
            let mut request = github_get(&client, &url, token);
            if let Some(etag) = conditional_etag.take() {
                request = request.header(IF_NONE_MATCH, etag);
            }
            let response = match request.send().await {
                Ok(response) => response,
                Err(error) => {
                    interruption = Some(LifecycleInterruption::Transient(
                        classify_transport_failure(&error),
                    ));
                    break 'repos;
                }
            };
            let status = response.status();
            let headers = response.headers().clone();
            if let Some(plan) = backoff_from_response(status, &headers, &endpoint) {
                emit_backoff(progress, &plan);
                wait_for_backoff(&plan);
                backoff = Some(plan);
                break;
            }
            if status == StatusCode::NOT_MODIFIED || status == StatusCode::NO_CONTENT {
                break;
            }
            if !status.is_success() {
                let body = if status == StatusCode::FORBIDDEN {
                    response.text().await.unwrap_or_default()
                } else {
                    String::new()
                };
                let disposition = classify_response(
                    status,
                    &headers,
                    &body,
                    &endpoint,
                    ResponseTarget::Repository,
                );
                if matches!(disposition, ResponseDisposition::PermissionCandidate { .. }) {
                    match confirm_repository_after_prior_denial(
                        &client, profile, token, repo, progress,
                    )
                    .await
                    {
                        RepositoryAccessOutcome::ConfirmedPermissionLoss(confirmed) => {
                            confirmed_permission_lost_repos
                                .entry(confirmed.repo.clone())
                                .or_insert(confirmed);
                            break;
                        }
                        RepositoryAccessOutcome::AuthenticationFailed => {
                            interruption = Some(LifecycleInterruption::AuthenticationFailed);
                            break 'repos;
                        }
                        RepositoryAccessOutcome::Backoff(plan) => {
                            backoff = Some(plan);
                            break 'repos;
                        }
                        RepositoryAccessOutcome::Transient(kind) => {
                            interruption = Some(LifecycleInterruption::Transient(kind));
                            break 'repos;
                        }
                        RepositoryAccessOutcome::AmbiguousForbidden
                        | RepositoryAccessOutcome::Accessible => {
                            interruption = Some(LifecycleInterruption::AmbiguousForbidden);
                            break 'repos;
                        }
                    }
                }
                match disposition {
                    ResponseDisposition::AuthenticationFailed => {
                        interruption = Some(LifecycleInterruption::AuthenticationFailed);
                    }
                    ResponseDisposition::Backoff(plan) => backoff = Some(plan),
                    ResponseDisposition::Transient(kind) => {
                        interruption = Some(LifecycleInterruption::Transient(kind));
                    }
                    ResponseDisposition::AmbiguousForbidden => {
                        interruption = Some(LifecycleInterruption::AmbiguousForbidden);
                    }
                    ResponseDisposition::Redirect | ResponseDisposition::SourceGone { .. } => {
                        interruption = Some(LifecycleInterruption::Transient(
                            GitHubTransientKind::UnexpectedResponse,
                        ));
                    }
                    ResponseDisposition::Success
                    | ResponseDisposition::PermissionCandidate { .. } => unreachable!(),
                }
                break 'repos;
            }
            if let Some(etag) = header_string(&headers, ETAG) {
                response_etag = Some(etag);
            }
            let page: Vec<ApiComment> = response
                .json()
                .await
                .map_err(|_| QghError::github("GitHub returned invalid comment JSON."))?;
            let indexed_at = now_rfc3339();
            for comment in page {
                let updated_at = comment.updated_at.clone();
                let mut handled = false;
                if let Some(number) = parse_issue_number_from_url(&comment.issue_url) {
                    if known_pull_requests.contains(&number) {
                        skipped_pr_comments += 1;
                        handled = true;
                    } else if let Some(parent) = resolve_parent(&repo_name, number) {
                        comments.push(comment.into_record_for_parent(
                            profile,
                            repo,
                            &parent,
                            &indexed_at,
                        ));
                        handled = true;
                    } else if remote_lookups < parent_resolution_budget {
                        remote_lookups += 1;
                        match classify_parent(&client, profile, token, repo, number, progress)
                            .await?
                        {
                            ParentClass::PullRequest => {
                                known_pull_requests.insert(number);
                                skipped_pr_comments += 1;
                                handled = true;
                            }
                            ParentClass::IssueOrUnknown => {
                                deferred_comments += 1;
                            }
                            ParentClass::Backoff(plan) => {
                                wait_for_backoff(&plan);
                                backoff = Some(plan);
                                break;
                            }
                        }
                    } else {
                        deferred_comments += 1;
                    }
                } else {
                    deferred_comments += 1;
                }
                if handled {
                    if !blocked {
                        committed_watermark = max_timestamp(committed_watermark, &updated_at);
                    }
                } else {
                    // A deferred comment holds the cursor so it is retried later.
                    blocked = true;
                }
            }
            if backoff.is_some() {
                break;
            }
            next_url = next_link(&headers);
        }
        cursor_updates.push(CursorUpdate {
            endpoint,
            cursor: committed_watermark,
            etag: response_etag,
            not_modified: false,
        });
        if backoff.is_some() {
            break;
        }
    }

    Ok(RepoCommentsResult {
        comments,
        cursor_updates,
        skipped_pr_comments,
        deferred_comments,
        backoff,
        confirmed_permission_lost_repos: confirmed_permission_lost_repos.into_values().collect(),
        interruption,
    })
}

enum ParentClass {
    PullRequest,
    IssueOrUnknown,
    Backoff(BackoffPlan),
}

async fn classify_parent(
    client: &reqwest::Client,
    profile: &Profile,
    token: &str,
    repo: &RepoRef,
    issue_number: i64,
    progress: Option<&dyn ProgressReporter>,
) -> Result<ParentClass, QghError> {
    let url = issue_object_url(profile, repo, issue_number);
    let response = github_get(client, &url, token)
        .send()
        .await
        .map_err(|_| QghError::github("GitHub request failed before a response."))?;
    let status = response.status();
    let headers = response.headers().clone();
    if let Some(backoff) = backoff_from_response(status, &headers, &repo_comment_endpoint(repo)) {
        emit_backoff(progress, &backoff);
        return Ok(ParentClass::Backoff(backoff));
    }
    if !status.is_success() {
        let body = if status == StatusCode::FORBIDDEN {
            response.text().await.unwrap_or_default()
        } else {
            String::new()
        };
        if let ResponseDisposition::Backoff(plan) = classify_response(
            status,
            &headers,
            &body,
            &repo_comment_endpoint(repo),
            ResponseTarget::Source,
        ) {
            emit_backoff(progress, &plan);
            return Ok(ParentClass::Backoff(plan));
        }
        // Cannot classify (deleted/permission/etc.): defer rather than guess.
        return Ok(ParentClass::IssueOrUnknown);
    }
    let issue: ApiIssue = response
        .json()
        .await
        .map_err(|_| QghError::github("GitHub returned invalid issue JSON."))?;
    if issue.pull_request.is_some() {
        Ok(ParentClass::PullRequest)
    } else {
        Ok(ParentClass::IssueOrUnknown)
    }
}

fn repo_comment_endpoint(repo: &RepoRef) -> String {
    format!("repo-comments:{}", repo.full_name())
}

fn repo_comment_url(profile: &Profile, repo: &RepoRef, cursor: Option<&StoredCursor>) -> String {
    let mut url = format!(
        "{}/repos/{}/{}/issues/comments?sort=updated&direction=asc&per_page=100",
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

fn parse_issue_number_from_url(issue_url: &str) -> Option<i64> {
    issue_url
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .and_then(|segment| segment.parse::<i64>().ok())
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
    ConfirmedRepositoryPermissionLoss(ConfirmedRepositoryPermissionLoss),
    SourceDeleted { http_status: u16 },
    AuthenticationFailed,
    Transient(GitHubTransientKind),
    AmbiguousForbidden,
}

fn classify_response(
    status: StatusCode,
    headers: &HeaderMap,
    body: &str,
    scope: &str,
    target: ResponseTarget,
) -> ResponseDisposition {
    if status.is_success() {
        return ResponseDisposition::Success;
    }
    if status.is_redirection() {
        return ResponseDisposition::Redirect;
    }
    if let Some(backoff) = backoff_from_response(status, headers, scope) {
        return ResponseDisposition::Backoff(backoff);
    }
    match status {
        StatusCode::UNAUTHORIZED => ResponseDisposition::AuthenticationFailed,
        StatusCode::FORBIDDEN if response_body_looks_rate_limited(body) => {
            ResponseDisposition::Backoff(BackoffPlan {
                reason: "secondary_rate_limit".to_string(),
                scope: scope.to_string(),
                retry_after_seconds: 60,
                reset_at: None,
            })
        }
        StatusCode::FORBIDDEN if response_body_confirms_permission_loss(body) => {
            ResponseDisposition::PermissionCandidate {
                http_status: status.as_u16(),
            }
        }
        StatusCode::FORBIDDEN => ResponseDisposition::AmbiguousForbidden,
        StatusCode::NOT_FOUND => ResponseDisposition::PermissionCandidate {
            http_status: status.as_u16(),
        },
        StatusCode::GONE if target == ResponseTarget::Source => ResponseDisposition::SourceGone {
            http_status: status.as_u16(),
        },
        status if status.is_server_error() => {
            ResponseDisposition::Transient(GitHubTransientKind::Server)
        }
        _ => ResponseDisposition::Transient(GitHubTransientKind::UnexpectedResponse),
    }
}

fn classify_transport_failure(error: &reqwest::Error) -> GitHubTransientKind {
    if error.is_timeout() {
        GitHubTransientKind::Timeout
    } else {
        GitHubTransientKind::Network
    }
}

fn permission_evidence(disposition: &ResponseDisposition) -> Option<PermissionEvidence> {
    match disposition {
        ResponseDisposition::PermissionCandidate { http_status: 403 } => {
            Some(PermissionEvidence::Forbidden { http_status: 403 })
        }
        ResponseDisposition::PermissionCandidate { http_status } => {
            Some(PermissionEvidence::NotFound {
                http_status: *http_status,
            })
        }
        ResponseDisposition::SourceGone { http_status } => Some(PermissionEvidence::Gone {
            http_status: *http_status,
        }),
        _ => None,
    }
}

fn resolve_source_confirmation(
    first: PermissionEvidence,
    second: ResponseDisposition,
) -> ClassifiedLifecycleCheck {
    match second {
        ResponseDisposition::PermissionCandidate { http_status } => {
            ClassifiedLifecycleCheck::Confirmed {
                state: ConfirmedRemoteState::RepositoryPermissionLoss,
                http_status,
            }
        }
        ResponseDisposition::Success => match first {
            PermissionEvidence::NotFound { http_status }
            | PermissionEvidence::Gone { http_status } => ClassifiedLifecycleCheck::Confirmed {
                state: ConfirmedRemoteState::SourceDeleted,
                http_status,
            },
            PermissionEvidence::Forbidden { .. } => ClassifiedLifecycleCheck::AmbiguousForbidden,
        },
        ResponseDisposition::AuthenticationFailed => ClassifiedLifecycleCheck::AuthenticationFailed,
        ResponseDisposition::Backoff(plan) => ClassifiedLifecycleCheck::Backoff(plan),
        ResponseDisposition::Transient(kind) => ClassifiedLifecycleCheck::Transient(kind),
        ResponseDisposition::AmbiguousForbidden => ClassifiedLifecycleCheck::AmbiguousForbidden,
        ResponseDisposition::SourceGone { .. } | ResponseDisposition::Redirect => {
            ClassifiedLifecycleCheck::Transient(GitHubTransientKind::UnexpectedResponse)
        }
    }
}

fn repository_outcome(repo: &RepoRef, disposition: ResponseDisposition) -> RepositoryAccessOutcome {
    match disposition {
        ResponseDisposition::Success => RepositoryAccessOutcome::Accessible,
        ResponseDisposition::PermissionCandidate { http_status } => {
            RepositoryAccessOutcome::ConfirmedPermissionLoss(ConfirmedRepositoryPermissionLoss {
                repo: repo.full_name(),
                http_status,
            })
        }
        ResponseDisposition::AuthenticationFailed => RepositoryAccessOutcome::AuthenticationFailed,
        ResponseDisposition::Backoff(plan) => RepositoryAccessOutcome::Backoff(plan),
        ResponseDisposition::Transient(kind) => RepositoryAccessOutcome::Transient(kind),
        ResponseDisposition::AmbiguousForbidden => RepositoryAccessOutcome::AmbiguousForbidden,
        ResponseDisposition::SourceGone { .. } | ResponseDisposition::Redirect => {
            RepositoryAccessOutcome::Transient(GitHubTransientKind::UnexpectedResponse)
        }
    }
}

async fn repository_access_attempt(
    client: &reqwest::Client,
    profile: &Profile,
    token: &str,
    repo: &RepoRef,
) -> ResponseDisposition {
    repository_access_attempt_at(client, &profile.api_base_url, token, repo).await
}

async fn repository_access_attempt_at(
    client: &reqwest::Client,
    api_base_url: &str,
    token: &str,
    repo: &RepoRef,
) -> ResponseDisposition {
    let url = repository_object_url_at(api_base_url, repo);
    let scope = format!("repository:{}", repo.full_name());
    let response = match github_get(client, &url, token).send().await {
        Ok(response) => response,
        Err(error) => {
            return ResponseDisposition::Transient(classify_transport_failure(&error));
        }
    };
    let status = response.status();
    let headers = response.headers().clone();
    let body = if status == StatusCode::FORBIDDEN {
        response.text().await.unwrap_or_default()
    } else {
        String::new()
    };
    classify_response(status, &headers, &body, &scope, ResponseTarget::Repository)
}

async fn confirm_source_denial_with_repository(
    client: &reqwest::Client,
    profile: &Profile,
    token: &str,
    repo: &RepoRef,
    first: PermissionEvidence,
    progress: Option<&dyn ProgressReporter>,
) -> ClassifiedLifecycleCheck {
    let second = repository_access_attempt(client, profile, token, repo).await;
    if let ResponseDisposition::Backoff(plan) = &second {
        emit_backoff(progress, plan);
    }
    resolve_source_confirmation(first, second)
}

async fn confirm_repository_after_prior_denial(
    client: &reqwest::Client,
    profile: &Profile,
    token: &str,
    repo: &RepoRef,
    progress: Option<&dyn ProgressReporter>,
) -> RepositoryAccessOutcome {
    let second = repository_access_attempt(client, profile, token, repo).await;
    if let ResponseDisposition::Backoff(plan) = &second {
        emit_backoff(progress, plan);
    }
    repository_outcome(repo, second)
}

/// Probe a repository without prior denial evidence. Permission-shaped 403/404
/// responses are confirmed exactly once, so this function performs at most two
/// authenticated HTTP attempts. Callers that already have source-level denial
/// evidence must use the single-confirmation path instead.
pub async fn check_repository_access(
    profile: &Profile,
    token: &str,
    repo: &RepoRef,
    progress: Option<&dyn ProgressReporter>,
) -> Result<RepositoryAccessOutcome, QghError> {
    let client = lifecycle_client()?;
    Ok(
        check_repository_access_with_client(&client, &profile.api_base_url, token, repo, progress)
            .await,
    )
}

async fn check_repository_access_with_client(
    client: &reqwest::Client,
    api_base_url: &str,
    token: &str,
    repo: &RepoRef,
    progress: Option<&dyn ProgressReporter>,
) -> RepositoryAccessOutcome {
    let first = repository_access_attempt_at(client, api_base_url, token, repo).await;
    if !matches!(first, ResponseDisposition::PermissionCandidate { .. }) {
        if let ResponseDisposition::Backoff(plan) = &first {
            emit_backoff(progress, plan);
        }
        return repository_outcome(repo, first);
    }
    let second = repository_access_attempt_at(client, api_base_url, token, repo).await;
    if let ResponseDisposition::Backoff(plan) = &second {
        emit_backoff(progress, plan);
    }
    match second {
        ResponseDisposition::PermissionCandidate { .. } => repository_outcome(repo, second),
        ResponseDisposition::Success => RepositoryAccessOutcome::Accessible,
        other => repository_outcome(repo, other),
    }
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

    if status == StatusCode::FORBIDDEN && headers.get(RETRY_AFTER).is_none() {
        return None;
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
        || lower.contains("permission denied")
        || lower.contains("insufficient permission")
        || lower.contains("do not have permission")
        || lower.contains("does not have permission")
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

fn target_outcome_from_lifecycle_check(
    check: ClassifiedLifecycleCheck,
    repo: &RepoRef,
    issue_number: i64,
    alias_chain: Vec<String>,
) -> ClassifiedTargetIssueFetchOutcome {
    match check {
        ClassifiedLifecycleCheck::Confirmed { state, http_status } => {
            let reason = state.reason().to_string();
            ClassifiedTargetIssueFetchOutcome::Confirmed {
                state,
                repo: repo.full_name(),
                issue_number,
                lifecycle: TargetIssueLifecycle {
                    status: reason.clone(),
                    reason: Some(reason),
                    http_status: Some(http_status),
                    alias_chain,
                },
            }
        }
        ClassifiedLifecycleCheck::AuthenticationFailed => {
            ClassifiedTargetIssueFetchOutcome::AuthenticationFailed
        }
        ClassifiedLifecycleCheck::Backoff(plan) => ClassifiedTargetIssueFetchOutcome::Backoff(plan),
        ClassifiedLifecycleCheck::Transient(kind) => {
            ClassifiedTargetIssueFetchOutcome::Transient(kind)
        }
        ClassifiedLifecycleCheck::AmbiguousForbidden => {
            ClassifiedTargetIssueFetchOutcome::AmbiguousForbidden
        }
        ClassifiedLifecycleCheck::Active => {
            ClassifiedTargetIssueFetchOutcome::Transient(GitHubTransientKind::UnexpectedResponse)
        }
    }
}

fn lifecycle_client() -> Result<reqwest::Client, QghError> {
    lifecycle_client_with_timeout(GITHUB_REQUEST_TIMEOUT)
}

fn lifecycle_client_with_timeout(timeout: StdDuration) -> Result<reqwest::Client, QghError> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .build()
        .map_err(|_| QghError::github("GitHub HTTP client initialization failed."))
}

async fn check_candidate_lifecycle_classified(
    client: &reqwest::Client,
    profile: &Profile,
    token: &str,
    candidate: &ReconciliationCandidate,
    progress: Option<&dyn ProgressReporter>,
) -> Result<ClassifiedLifecycleCheck, QghError> {
    let url = source_check_url(profile, candidate)?;
    let response = match github_get(client, &url, token).send().await {
        Ok(response) => response,
        Err(error) => {
            return Ok(ClassifiedLifecycleCheck::Transient(
                classify_transport_failure(&error),
            ));
        }
    };
    let status = response.status();
    let headers = response.headers().clone();
    let scope = format!("lifecycle:{}", candidate.repo);
    let body = if status == StatusCode::FORBIDDEN {
        response.text().await.unwrap_or_default()
    } else {
        String::new()
    };
    let disposition = classify_response(status, &headers, &body, &scope, ResponseTarget::Source);
    if disposition == ResponseDisposition::Redirect {
        return Ok(ClassifiedLifecycleCheck::Confirmed {
            state: ConfirmedRemoteState::SourceTransferred,
            http_status: status.as_u16(),
        });
    }
    if let Some(first) = permission_evidence(&disposition) {
        let repo = candidate_repo(candidate)?;
        return Ok(confirm_source_denial_with_repository(
            client, profile, token, &repo, first, progress,
        )
        .await);
    }
    Ok(match disposition {
        ResponseDisposition::Success => ClassifiedLifecycleCheck::Active,
        ResponseDisposition::AuthenticationFailed => ClassifiedLifecycleCheck::AuthenticationFailed,
        ResponseDisposition::Backoff(plan) => {
            emit_backoff(progress, &plan);
            ClassifiedLifecycleCheck::Backoff(plan)
        }
        ResponseDisposition::Transient(kind) => ClassifiedLifecycleCheck::Transient(kind),
        ResponseDisposition::AmbiguousForbidden => ClassifiedLifecycleCheck::AmbiguousForbidden,
        ResponseDisposition::Redirect
        | ResponseDisposition::PermissionCandidate { .. }
        | ResponseDisposition::SourceGone { .. } => unreachable!(),
    })
}

fn candidate_repo(candidate: &ReconciliationCandidate) -> Result<RepoRef, QghError> {
    let Some((owner, name)) = candidate.repo.split_once('/') else {
        return Err(QghError::validation(
            "validation.invalid_repo",
            "Stored repo must use owner/repo format.",
        ));
    };
    Ok(RepoRef {
        owner: owner.to_string(),
        name: name.to_string(),
    })
}

fn authentication_failure() -> QghError {
    QghError::auth("GitHub authentication failed.")
        .with_hint("Refresh the configured GitHub token source, then retry.")
}

fn github_unavailable() -> QghError {
    QghError::github("GitHub request did not produce a confirmed lifecycle result.")
        .with_hint("Retry later; local content was not removed.")
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
    #[serde(default)]
    issue_url: String,
}

impl ApiComment {
    fn into_record(
        self,
        profile: &Profile,
        repo: &RepoRef,
        issue: &IssueRecord,
        indexed_at: &str,
    ) -> CommentRecord {
        let parent = CommentParent {
            source_id: issue.source_id.clone(),
            number: issue.number,
            title: issue.title.clone(),
            canonical_url: issue.canonical_url.clone(),
        };
        self.into_record_for_parent(profile, repo, &parent, indexed_at)
    }

    fn into_record_for_parent(
        self,
        profile: &Profile,
        repo: &RepoRef,
        parent: &CommentParent,
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
            parent_issue_source_id: parent.source_id.clone(),
            parent_issue_number: parent.number,
            parent_issue_title: parent.title.clone(),
            parent_issue_canonical_url: parent.canonical_url.clone(),
        }
    }
}

/// Resolved parent issue context for building a comment record from the
/// repo-level comments listing.
pub struct CommentParent {
    pub source_id: String,
    pub number: i64,
    pub title: String,
    pub canonical_url: String,
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

fn repository_object_url_at(api_base_url: &str, repo: &RepoRef) -> String {
    format!("{api_base_url}/repos/{}/{}", repo.owner, repo.name)
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

#[cfg(test)]
mod permission_classification_tests {
    use super::*;
    use crate::config::{
        BootstrapSettings, CommentsMode, FreshnessSettings, StaleBehavior, TokenSource,
    };
    use crate::paths::ProfilePaths;
    use reqwest::header::{HeaderMap, HeaderValue};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;

    const NOT_FOUND_RESPONSE: &str =
        "HTTP/1.1 404 Not Found\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}";
    const OK_RESPONSE: &str = "HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}";

    fn spawn_responses(
        responses: Vec<&'static str>,
    ) -> (String, Arc<AtomicUsize>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let count = Arc::new(AtomicUsize::new(0));
        let thread_count = Arc::clone(&count);
        let handle = thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().expect("accept test request");
                let mut request = [0_u8; 4096];
                let _ = stream.read(&mut request);
                thread_count.fetch_add(1, Ordering::SeqCst);
                stream
                    .write_all(response.as_bytes())
                    .expect("write test response");
            }
        });
        (format!("http://{address}"), count, handle)
    }

    fn spawn_owned_responses(
        responses: Vec<String>,
    ) -> (String, Arc<AtomicUsize>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let count = Arc::new(AtomicUsize::new(0));
        let thread_count = Arc::clone(&count);
        let handle = thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().expect("accept test request");
                let mut request = [0_u8; 4096];
                let _ = stream.read(&mut request);
                thread_count.fetch_add(1, Ordering::SeqCst);
                stream
                    .write_all(response.as_bytes())
                    .expect("write test response");
            }
        });
        (format!("http://{address}"), count, handle)
    }

    fn json_response(status: &str, extra_headers: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n{extra_headers}\r\n{body}",
            body.len()
        )
    }

    fn test_repo() -> RepoRef {
        RepoRef {
            owner: "owner".to_string(),
            name: "repo".to_string(),
        }
    }

    fn test_profile(api_base_url: &str) -> Profile {
        let root = PathBuf::from("/tmp/qgh-github-classification-test");
        Profile {
            id: "test".to_string(),
            host: "example.test".to_string(),
            api_base_url: api_base_url.to_string(),
            web_base_url: "https://example.test".to_string(),
            repos: vec![test_repo()],
            embedding: None,
            reconcile_after_seconds: None,
            freshness: FreshnessSettings {
                query_max_age_seconds: 60,
                query_stale_behavior: StaleBehavior::Warn,
                active_issue_max_age_seconds: None,
            },
            bootstrap: BootstrapSettings {
                lookback_seconds: 60,
            },
            sync_max_age_seconds: None,
            comments_mode: CommentsMode::PerIssue,
            comment_parent_resolution_budget: 1,
            max_in_flight_requests: 1,
            token_source: TokenSource::GithubCli,
            paths: ProfilePaths {
                config_file: root.join("config.toml"),
                profile_dir: root.clone(),
                cache_dir: root.join("cache"),
                log_dir: root.join("logs"),
                db_path: root.join("qgh.sqlite3"),
                index_root: root.join("tantivy"),
                index_active: root.join("tantivy/active"),
            },
        }
    }

    fn disposition(status: StatusCode, body: &str) -> ResponseDisposition {
        classify_response(
            status,
            &HeaderMap::new(),
            body,
            "test-scope",
            ResponseTarget::Source,
        )
    }

    #[test]
    fn response_matrix_keeps_non_confirmed_failures_non_destructive() {
        assert_eq!(
            disposition(StatusCode::UNAUTHORIZED, r#"{"message":"Bad credentials"}"#),
            ResponseDisposition::AuthenticationFailed
        );
        assert_eq!(
            disposition(StatusCode::INTERNAL_SERVER_ERROR, "private response body"),
            ResponseDisposition::Transient(GitHubTransientKind::Server)
        );
        assert_eq!(
            disposition(StatusCode::FORBIDDEN, r#"{"message":"forbidden"}"#),
            ResponseDisposition::AmbiguousForbidden
        );
        assert_eq!(
            disposition(
                StatusCode::FORBIDDEN,
                r#"{"message":"permission service temporarily unavailable"}"#
            ),
            ResponseDisposition::AmbiguousForbidden
        );
        assert_eq!(
            disposition(
                StatusCode::FORBIDDEN,
                r#"{"message":"resource not accessible"}"#
            ),
            ResponseDisposition::PermissionCandidate { http_status: 403 }
        );
        assert_eq!(
            disposition(StatusCode::NOT_FOUND, r#"{"message":"not found"}"#),
            ResponseDisposition::PermissionCandidate { http_status: 404 }
        );
        assert_eq!(
            disposition(StatusCode::GONE, r#"{"message":"gone"}"#),
            ResponseDisposition::SourceGone { http_status: 410 }
        );
    }

    #[test]
    fn rate_limit_evidence_wins_over_permission_shaped_body() {
        let mut headers = HeaderMap::new();
        headers.insert("x-ratelimit-remaining", HeaderValue::from_static("0"));
        headers.insert("x-ratelimit-reset", HeaderValue::from_static("0"));
        let result = classify_response(
            StatusCode::FORBIDDEN,
            &headers,
            r#"{"message":"resource not accessible"}"#,
            "test-scope",
            ResponseTarget::Source,
        );
        assert!(matches!(result, ResponseDisposition::Backoff(_)));

        let result = disposition(StatusCode::TOO_MANY_REQUESTS, "private body");
        assert!(matches!(result, ResponseDisposition::Backoff(_)));

        let result = disposition(
            StatusCode::FORBIDDEN,
            r#"{"message":"secondary rate limit; resource not accessible"}"#,
        );
        assert!(matches!(result, ResponseDisposition::Backoff(_)));
    }

    #[test]
    fn repository_gone_is_not_permission_loss_evidence() {
        assert_eq!(
            classify_response(
                StatusCode::GONE,
                &HeaderMap::new(),
                "gone",
                "test-scope",
                ResponseTarget::Repository,
            ),
            ResponseDisposition::Transient(GitHubTransientKind::UnexpectedResponse)
        );
    }

    #[test]
    fn confirmed_repository_identity_comes_only_from_the_probed_repo() {
        let repo = RepoRef {
            owner: "other-owner".to_string(),
            name: "other-repo".to_string(),
        };
        let outcome = repository_outcome(
            &repo,
            ResponseDisposition::PermissionCandidate { http_status: 404 },
        );
        let RepositoryAccessOutcome::ConfirmedPermissionLoss(confirmed) = outcome else {
            panic!("permission candidate must remain typed");
        };
        assert_eq!(confirmed.repo, "other-owner/other-repo");
        assert_ne!(confirmed.repo, "owner/repo");
    }

    #[test]
    fn source_denial_needs_one_repository_confirmation() {
        let confirmed = resolve_source_confirmation(
            PermissionEvidence::NotFound { http_status: 404 },
            ResponseDisposition::PermissionCandidate { http_status: 403 },
        );
        assert_eq!(
            confirmed,
            ClassifiedLifecycleCheck::Confirmed {
                state: ConfirmedRemoteState::RepositoryPermissionLoss,
                http_status: 403,
            }
        );

        let deleted = resolve_source_confirmation(
            PermissionEvidence::NotFound { http_status: 404 },
            ResponseDisposition::Success,
        );
        assert_eq!(
            deleted,
            ClassifiedLifecycleCheck::Confirmed {
                state: ConfirmedRemoteState::SourceDeleted,
                http_status: 404,
            }
        );

        let recovered = resolve_source_confirmation(
            PermissionEvidence::Forbidden { http_status: 403 },
            ResponseDisposition::Success,
        );
        assert_eq!(recovered, ClassifiedLifecycleCheck::AmbiguousForbidden);
    }

    #[test]
    fn second_transient_or_auth_response_never_confirms_permission_loss() {
        for second in [
            ResponseDisposition::AuthenticationFailed,
            ResponseDisposition::Transient(GitHubTransientKind::Timeout),
            ResponseDisposition::Transient(GitHubTransientKind::Network),
            ResponseDisposition::Transient(GitHubTransientKind::Server),
        ] {
            assert!(!matches!(
                resolve_source_confirmation(
                    PermissionEvidence::Forbidden { http_status: 403 },
                    second,
                ),
                ClassifiedLifecycleCheck::Confirmed {
                    state: ConfirmedRemoteState::RepositoryPermissionLoss,
                    ..
                }
            ));
        }
        let backoff = resolve_source_confirmation(
            PermissionEvidence::Forbidden { http_status: 403 },
            ResponseDisposition::Backoff(BackoffPlan {
                reason: "secondary_rate_limit".to_string(),
                scope: "content-free".to_string(),
                retry_after_seconds: 1,
                reset_at: None,
            }),
        );
        assert!(matches!(backoff, ClassifiedLifecycleCheck::Backoff(_)));
    }

    #[test]
    fn transient_debug_values_never_carry_remote_content() {
        let secret = "sensitive-content-must-not-escape";
        let value = GitHubTransientKind::Network;
        let rendered = format!("{value:?}");
        assert!(!rendered.contains(secret));
    }

    #[test]
    fn compatibility_wrappers_never_collapse_unconfirmed_failures_to_unavailable() {
        for check in [
            ClassifiedLifecycleCheck::AuthenticationFailed,
            ClassifiedLifecycleCheck::Transient(GitHubTransientKind::Timeout),
            ClassifiedLifecycleCheck::Transient(GitHubTransientKind::Network),
            ClassifiedLifecycleCheck::Transient(GitHubTransientKind::Server),
            ClassifiedLifecycleCheck::AmbiguousForbidden,
        ] {
            assert!(legacy_lifecycle_check(check).is_err());
        }
        assert!(
            legacy_lifecycle_check(ClassifiedLifecycleCheck::Backoff(BackoffPlan {
                reason: "primary_rate_limit".to_string(),
                scope: "content-free".to_string(),
                retry_after_seconds: 1,
                reset_at: None,
            }))
            .is_err()
        );

        for outcome in [
            ClassifiedTargetIssueFetchOutcome::AuthenticationFailed,
            ClassifiedTargetIssueFetchOutcome::Transient(GitHubTransientKind::Timeout),
            ClassifiedTargetIssueFetchOutcome::Transient(GitHubTransientKind::Network),
            ClassifiedTargetIssueFetchOutcome::Transient(GitHubTransientKind::Server),
            ClassifiedTargetIssueFetchOutcome::AmbiguousForbidden,
        ] {
            assert!(legacy_target_issue_outcome(outcome).is_err());
        }
        let backoff =
            legacy_target_issue_outcome(ClassifiedTargetIssueFetchOutcome::Backoff(BackoffPlan {
                reason: "secondary_rate_limit".to_string(),
                scope: "content-free".to_string(),
                retry_after_seconds: 1,
                reset_at: None,
            }))
            .unwrap();
        assert!(matches!(backoff, TargetIssueFetchOutcome::Backoff(_)));

        for interruption in [
            LifecycleInterruption::AuthenticationFailed,
            LifecycleInterruption::Transient(GitHubTransientKind::Timeout),
            LifecycleInterruption::Transient(GitHubTransientKind::Network),
            LifecycleInterruption::Transient(GitHubTransientKind::Server),
            LifecycleInterruption::AmbiguousForbidden,
        ] {
            assert!(
                legacy_fetch_outcome(ClassifiedFetchOutcome::Interrupted(interruption)).is_err()
            );
        }
        let fetch_backoff = legacy_fetch_outcome(ClassifiedFetchOutcome::Interrupted(
            LifecycleInterruption::Backoff(BackoffPlan {
                reason: "primary_rate_limit".to_string(),
                scope: "content-free".to_string(),
                retry_after_seconds: 1,
                reset_at: None,
            }),
        ))
        .unwrap();
        assert!(matches!(fetch_backoff, FetchOutcome::Backoff(_)));
    }

    #[test]
    fn compatibility_wrappers_only_mark_confirmed_states_unavailable() {
        let lifecycle = legacy_lifecycle_check(ClassifiedLifecycleCheck::Confirmed {
            state: ConfirmedRemoteState::SourceDeleted,
            http_status: 404,
        })
        .unwrap();
        assert!(matches!(lifecycle, LifecycleCheck::Unavailable { .. }));

        let target = legacy_target_issue_outcome(ClassifiedTargetIssueFetchOutcome::Confirmed {
            state: ConfirmedRemoteState::RepositoryPermissionLoss,
            repo: "owner/repo".to_string(),
            issue_number: 42,
            lifecycle: TargetIssueLifecycle {
                status: "permission_loss".to_string(),
                reason: Some("permission_loss".to_string()),
                http_status: Some(404),
                alias_chain: Vec::new(),
            },
        })
        .unwrap();
        assert!(matches!(target, TargetIssueFetchOutcome::Unavailable(_)));
    }

    #[tokio::test]
    async fn standalone_repository_confirmation_is_bounded_to_two_attempts() {
        let (base_url, request_count, handle) =
            spawn_responses(vec![NOT_FOUND_RESPONSE, NOT_FOUND_RESPONSE]);
        let client = lifecycle_client_with_timeout(StdDuration::from_secs(1)).unwrap();
        let outcome = check_repository_access_with_client(
            &client,
            &base_url,
            "test-token",
            &test_repo(),
            None,
        )
        .await;
        handle.join().unwrap();
        assert!(matches!(
            outcome,
            RepositoryAccessOutcome::ConfirmedPermissionLoss(_)
        ));
        assert_eq!(request_count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn repeated_invocations_are_idempotently_bounded_per_invocation() {
        let (base_url, request_count, handle) = spawn_responses(vec![
            NOT_FOUND_RESPONSE,
            NOT_FOUND_RESPONSE,
            NOT_FOUND_RESPONSE,
            NOT_FOUND_RESPONSE,
        ]);
        let client = lifecycle_client_with_timeout(StdDuration::from_secs(1)).unwrap();
        for _ in 0..2 {
            let outcome = check_repository_access_with_client(
                &client,
                &base_url,
                "test-token",
                &test_repo(),
                None,
            )
            .await;
            assert!(matches!(
                outcome,
                RepositoryAccessOutcome::ConfirmedPermissionLoss(_)
            ));
        }
        handle.join().unwrap();
        assert_eq!(request_count.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn reconciliation_reuses_one_confirmed_repo_loss_for_same_repo_candidates() {
        let (base_url, request_count, handle) =
            spawn_responses(vec![NOT_FOUND_RESPONSE, NOT_FOUND_RESPONSE]);
        let profile = test_profile(&base_url);
        let candidates = vec![
            ReconciliationCandidate {
                source_id: "qgh://example.test/issue/one".to_string(),
                entity_type: "issue".to_string(),
                repo: "owner/repo".to_string(),
                issue_number: 1,
                github_id: 1,
            },
            ReconciliationCandidate {
                source_id: "qgh://example.test/issue/two".to_string(),
                entity_type: "issue".to_string(),
                repo: "owner/repo".to_string(),
                issue_number: 2,
                github_id: 2,
            },
        ];
        let result = reconcile_sources(&profile, "test-token", &candidates, None)
            .await
            .unwrap();
        handle.join().unwrap();
        assert_eq!(request_count.load(Ordering::SeqCst), 2);
        assert_eq!(result.checked_sources, 1);
        assert_eq!(result.confirmed_permission_lost_repos.len(), 1);
        assert!(result.unavailable_sources.is_empty());
        assert!(result.interruption.is_none());
    }

    #[tokio::test]
    async fn full_fetch_preserves_confirmed_repository_loss_evidence() {
        let (base_url, request_count, handle) =
            spawn_responses(vec![NOT_FOUND_RESPONSE, NOT_FOUND_RESPONSE]);
        let profile = test_profile(&base_url);
        let mut commit = |_page: FetchPage| Ok(());
        let outcome =
            fetch_issues_classified(&profile, "test-token", &[], false, None, &mut commit)
                .await
                .unwrap();
        handle.join().unwrap();
        let ClassifiedFetchOutcome::Fetched(fetched) = outcome else {
            panic!("confirmed repo loss must be preserved in fetched evidence");
        };
        assert_eq!(fetched.confirmed_permission_lost_repos.len(), 1);
        assert_eq!(
            fetched.confirmed_permission_lost_repos[0].repo,
            "owner/repo"
        );
        assert_eq!(request_count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn full_fetch_preserves_issue_delete_evidence_from_comment_endpoint() {
        const ISSUE_PAGE: &str = r#"[{"id":1,"node_id":"I_ONE","number":1,"title":"Public title","body":"Public body","state":"open","labels":[],"milestone":null,"assignees":[],"user":{"login":"alice"},"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-02T00:00:00Z","closed_at":null,"html_url":"https://example.test/owner/repo/issues/1"}]"#;
        let responses = vec![
            json_response("200 OK", "", ISSUE_PAGE),
            json_response("404 Not Found", "", r#"{"message":"not found"}"#),
            json_response("200 OK", "", "{}"),
        ];
        let (base_url, request_count, handle) = spawn_owned_responses(responses);
        let profile = test_profile(&base_url);
        let mut commit = |_page: FetchPage| Ok(());
        let outcome = fetch_issues_classified(&profile, "test-token", &[], true, None, &mut commit)
            .await
            .unwrap();
        handle.join().unwrap();
        let ClassifiedFetchOutcome::Fetched(fetched) = outcome else {
            panic!("confirmed source deletion must remain typed");
        };
        assert_eq!(fetched.issues, 0);
        assert_eq!(fetched.confirmed_source_deletions.len(), 1);
        let deletion = &fetched.confirmed_source_deletions[0];
        assert_eq!(deletion.source_id, "qgh://example.test/issue/I_ONE");
        assert_eq!(deletion.repo, "owner/repo");
        assert_eq!(deletion.entity_type, "issue");
        assert_eq!(deletion.issue_number, 1);
        assert_eq!(deletion.http_status, 404);
        assert_eq!(request_count.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn transfer_outside_allowlist_is_confirmed_without_following_target() {
        const OUTSIDE_REDIRECT: &str = "HTTP/1.1 301 Moved Permanently\r\nlocation: /repos/outside/repo/issues/99\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}";
        let (base_url, request_count, handle) = spawn_responses(vec![OUTSIDE_REDIRECT]);
        let profile = test_profile(&base_url);
        let outcome = fetch_target_issue_classified(&profile, "test-token", &test_repo(), 42, None)
            .await
            .unwrap();
        handle.join().unwrap();
        let ClassifiedTargetIssueFetchOutcome::Confirmed {
            state,
            repo,
            issue_number,
            lifecycle,
        } = outcome
        else {
            panic!("outside-allowlist transfer must be confirmed");
        };
        assert_eq!(state, ConfirmedRemoteState::SourceTransferred);
        assert_eq!(repo, "owner/repo");
        assert_eq!(issue_number, 42);
        assert_eq!(lifecycle.status, "transferred");
        assert_eq!(
            lifecycle.alias_chain,
            vec!["/repos/outside/repo/issues/99".to_string()]
        );
        assert_eq!(request_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn second_success_recovers_without_confirming_permission_loss() {
        let (base_url, request_count, handle) =
            spawn_responses(vec![NOT_FOUND_RESPONSE, OK_RESPONSE]);
        let client = lifecycle_client_with_timeout(StdDuration::from_secs(1)).unwrap();
        let outcome = check_repository_access_with_client(
            &client,
            &base_url,
            "test-token",
            &test_repo(),
            None,
        )
        .await;
        handle.join().unwrap();
        assert_eq!(outcome, RepositoryAccessOutcome::Accessible);
        assert_eq!(request_count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn prior_source_denial_consumes_only_one_repository_confirmation_attempt() {
        let (base_url, request_count, handle) = spawn_responses(vec![NOT_FOUND_RESPONSE]);
        let client = lifecycle_client_with_timeout(StdDuration::from_secs(1)).unwrap();
        let second =
            repository_access_attempt_at(&client, &base_url, "test-token", &test_repo()).await;
        let outcome =
            resolve_source_confirmation(PermissionEvidence::NotFound { http_status: 404 }, second);
        handle.join().unwrap();
        assert!(matches!(
            outcome,
            ClassifiedLifecycleCheck::Confirmed {
                state: ConfirmedRemoteState::RepositoryPermissionLoss,
                ..
            }
        ));
        assert_eq!(request_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn short_timeout_seam_returns_typed_timeout_without_remote_content() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request);
            thread::sleep(StdDuration::from_millis(100));
        });
        let client = lifecycle_client_with_timeout(StdDuration::from_millis(10)).unwrap();
        let outcome = repository_access_attempt_at(
            &client,
            &format!("http://{address}"),
            "sensitive-marker-must-not-escape",
            &test_repo(),
        )
        .await;
        handle.join().unwrap();
        assert_eq!(
            outcome,
            ResponseDisposition::Transient(GitHubTransientKind::Timeout)
        );
        assert!(!format!("{outcome:?}").contains("sensitive-marker-must-not-escape"));
    }
}
