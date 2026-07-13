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

#[allow(dead_code)]
pub enum FetchOutcome {
    Fetched(FetchResult),
    Backoff(BackoffPlan),
}

pub struct ClassifiedFetchOutcome {
    /// All counters and confirmed lifecycle evidence accumulated before the
    /// optional interruption. Callers must persist/queue this evidence before
    /// surfacing the interruption.
    pub result: FetchResult,
    pub interruption: Option<LifecycleInterruption>,
    /// A stable, content-free operation failure observed after any lifecycle
    /// evidence above. Callers must persist/queue evidence before surfacing it.
    pub terminal_error: Option<QghError>,
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
    #[allow(dead_code)]
    pub confirmed_transition: Option<ConfirmedRemoteState>,
}

#[allow(dead_code)]
pub enum TargetIssueFetchOutcome {
    Fetched(Box<TargetIssueFetch>),
    Unavailable(TargetIssueLifecycle),
    Backoff(BackoffPlan),
}

pub struct ClassifiedTargetIssueFetchOutcome {
    /// Every canonical permanent redirect confirmed before the terminal result.
    /// Callers must queue these explicit source identities before handling an
    /// interruption or a second confirmed lifecycle state.
    pub confirmed_transitions: Vec<ConfirmedIssueTransition>,
    pub terminal: ClassifiedTargetIssueTerminal,
}

pub enum ClassifiedTargetIssueTerminal {
    Fetched(Box<TargetIssueFetch>),
    Confirmed {
        state: ConfirmedRemoteState,
        repo: String,
        issue_number: i64,
        lifecycle: TargetIssueLifecycle,
    },
    AuthenticationFailed,
    Backoff(BackoffPlan),
    #[allow(dead_code)]
    Transient(GitHubTransientKind),
    AmbiguousForbidden,
    Failed(QghError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmedIssueTransition {
    pub source_repo: String,
    pub source_issue_number: i64,
    pub target_repo: String,
    pub target_issue_number: i64,
    pub state: ConfirmedRemoteState,
    pub http_status: u16,
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
    pub terminal_error: Option<QghError>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleFailure {
    pub source_id: String,
    pub repo: String,
    pub entity_type: String,
    pub issue_number: i64,
    #[allow(dead_code)]
    pub reason: String,
    pub state: ConfirmedRemoteState,
    #[allow(dead_code)]
    pub http_status: u16,
}

#[allow(dead_code)]
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
pub enum ConfirmedFetchLifecycle {
    RepositoryPermissionLoss(ConfirmedRepositoryPermissionLoss),
    SourceDeletion(ConfirmedSourceDeletion),
    ReconciliationFailure(LifecycleFailure),
}

type LifecycleCommit<'a> = &'a mut dyn FnMut(&ConfirmedFetchLifecycle) -> Result<(), QghError>;

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

#[allow(dead_code)]
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

#[allow(dead_code)]
fn legacy_fetch_outcome(outcome: ClassifiedFetchOutcome) -> Result<FetchOutcome, QghError> {
    if !outcome.result.confirmed_permission_lost_repos.is_empty()
        || !outcome.result.confirmed_source_deletions.is_empty()
    {
        return Err(confirmed_lifecycle_requires_typed_handling());
    }
    if let Some(error) = outcome.terminal_error {
        return Err(error);
    }
    Ok(match outcome.interruption {
        None => FetchOutcome::Fetched(outcome.result),
        Some(LifecycleInterruption::Backoff(plan)) => FetchOutcome::Backoff(plan),
        Some(LifecycleInterruption::AuthenticationFailed) => {
            return Err(authentication_failure());
        }
        Some(LifecycleInterruption::Transient(_) | LifecycleInterruption::AmbiguousForbidden) => {
            return Err(github_unavailable())
        }
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
    fetch_issues_classified_with_client(
        &client,
        profile,
        token,
        cursors,
        fetch_comments,
        progress,
        commit_page,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn fetch_issues_classified_with_lifecycle_commit(
    profile: &Profile,
    token: &str,
    cursors: &[StoredCursor],
    fetch_comments: bool,
    progress: Option<&dyn ProgressReporter>,
    commit_page: &mut dyn FnMut(FetchPage) -> Result<(), QghError>,
    commit_lifecycle: &mut dyn FnMut(&ConfirmedFetchLifecycle) -> Result<(), QghError>,
) -> Result<ClassifiedFetchOutcome, QghError> {
    let client = lifecycle_client()?;
    fetch_issues_classified_with_client(
        &client,
        profile,
        token,
        cursors,
        fetch_comments,
        progress,
        commit_page,
        Some(commit_lifecycle),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn fetch_issues_classified_with_client(
    client: &reqwest::Client,
    profile: &Profile,
    token: &str,
    cursors: &[StoredCursor],
    fetch_comments: bool,
    progress: Option<&dyn ProgressReporter>,
    commit_page: &mut dyn FnMut(FetchPage) -> Result<(), QghError>,
    mut commit_lifecycle: Option<LifecycleCommit<'_>>,
) -> Result<ClassifiedFetchOutcome, QghError> {
    let cursor_map = cursor_map(cursors);
    let mut total_issues = 0;
    let mut total_comments = 0;
    let mut total_skipped_pull_requests = 0;
    let mut confirmed_permission_lost_repos = BTreeMap::new();
    let mut confirmed_source_deletions = BTreeMap::new();
    macro_rules! finish_fetch {
        ($interruption:expr) => {
            return Ok(ClassifiedFetchOutcome {
                result: FetchResult {
                    issues: total_issues,
                    comments: total_comments,
                    skipped_pull_requests: total_skipped_pull_requests,
                    confirmed_permission_lost_repos: confirmed_permission_lost_repos
                        .into_values()
                        .collect(),
                    confirmed_source_deletions: confirmed_source_deletions.into_values().collect(),
                },
                interruption: $interruption,
                terminal_error: None,
            })
        };
    }
    macro_rules! fail_fetch {
        ($error:expr) => {
            return Ok(ClassifiedFetchOutcome {
                result: FetchResult {
                    issues: total_issues,
                    comments: total_comments,
                    skipped_pull_requests: total_skipped_pull_requests,
                    confirmed_permission_lost_repos: confirmed_permission_lost_repos
                        .into_values()
                        .collect(),
                    confirmed_source_deletions: confirmed_source_deletions.into_values().collect(),
                },
                interruption: None,
                terminal_error: Some($error),
            })
        };
    }

    'repos: for repo in &profile.repos {
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
        while let Some(url) = next_url.take() {
            let mut request = github_get(client, &url, token, &profile.api_base_url)?;
            if let Some(etag) = stored_cursor.and_then(|cursor| cursor.etag.as_ref()) {
                request = request.header(IF_NONE_MATCH, etag);
            }
            let response = match request.send().await {
                Ok(response) => response,
                Err(error) => {
                    finish_fetch!(Some(LifecycleInterruption::Transient(
                        classify_transport_failure(&error)
                    )));
                }
            };
            let status = response.status();
            let headers = response.headers().clone();
            if let Some(backoff) = backoff_from_response(status, &headers, &endpoint) {
                emit_backoff(progress, &backoff);
                wait_for_backoff(&backoff);
                finish_fetch!(Some(LifecycleInterruption::Backoff(backoff)));
            }
            if status == StatusCode::NO_CONTENT {
                if let Err(error) = commit_page(FetchPage {
                    issues: Vec::new(),
                    comments: Vec::new(),
                    skipped_pull_requests: 0,
                    cursor_updates: vec![CursorUpdate {
                        endpoint: endpoint.clone(),
                        cursor: max_watermark.clone(),
                        etag: response_etag.clone(),
                        not_modified: false,
                    }],
                }) {
                    fail_fetch!(content_free_commit_error(error));
                }
                break;
            }
            if status == StatusCode::NOT_MODIFIED {
                emit(
                    progress,
                    ProgressEvent::IssueEndpointNotModified {
                        repo: repo_name.clone(),
                    },
                );
                if let Err(error) = commit_page(FetchPage {
                    issues: Vec::new(),
                    comments: Vec::new(),
                    skipped_pull_requests: 0,
                    cursor_updates: vec![CursorUpdate {
                        endpoint: endpoint.clone(),
                        cursor: max_watermark.clone(),
                        etag: response_etag.clone(),
                        not_modified: true,
                    }],
                }) {
                    fail_fetch!(content_free_commit_error(error));
                }
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
                        client, profile, token, repo, progress,
                    )
                    .await
                    {
                        RepositoryAccessOutcome::ConfirmedPermissionLoss(confirmed) => {
                            confirmed_permission_lost_repos
                                .entry(confirmed.repo.clone())
                                .or_insert_with(|| confirmed.clone());
                            if let Some(commit) = commit_lifecycle.as_deref_mut() {
                                if let Err(error) = commit(
                                    &ConfirmedFetchLifecycle::RepositoryPermissionLoss(confirmed),
                                ) {
                                    fail_fetch!(content_free_commit_error(error));
                                }
                                continue 'repos;
                            }
                            finish_fetch!(None);
                        }
                        RepositoryAccessOutcome::Backoff(plan) => {
                            wait_for_backoff(&plan);
                            finish_fetch!(Some(LifecycleInterruption::Backoff(plan)));
                        }
                        RepositoryAccessOutcome::AuthenticationFailed => {
                            finish_fetch!(Some(LifecycleInterruption::AuthenticationFailed));
                        }
                        RepositoryAccessOutcome::Accessible => {
                            finish_fetch!(Some(LifecycleInterruption::Transient(
                                GitHubTransientKind::UnexpectedResponse,
                            )));
                        }
                        RepositoryAccessOutcome::Transient(kind) => {
                            finish_fetch!(Some(LifecycleInterruption::Transient(kind)));
                        }
                        RepositoryAccessOutcome::AmbiguousForbidden => {
                            finish_fetch!(Some(LifecycleInterruption::AmbiguousForbidden));
                        }
                    }
                }
                let interruption = match disposition {
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
                };
                finish_fetch!(Some(interruption));
            }
            if let Some(etag) = header_string(&headers, ETAG) {
                response_etag = Some(etag);
            }
            let page: Vec<ApiIssue> = match response.json().await {
                Ok(page) => page,
                Err(_) => fail_fetch!(invalid_issue_response()),
            };
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
                        client,
                        profile,
                        token,
                        &cursor_map,
                        &mut page_cursor_updates,
                        repo,
                        &issue,
                        progress,
                    )
                    .await
                    {
                        CommentFetchOutcome::Fetched(fetched_comments) => {
                            repo_comment_count += fetched_comments.len();
                            total_comments += fetched_comments.len();
                            page_comments.extend(fetched_comments);
                        }
                        CommentFetchOutcome::Backoff(backoff) => {
                            wait_for_backoff(&backoff);
                            finish_fetch!(Some(LifecycleInterruption::Backoff(backoff)));
                        }
                        CommentFetchOutcome::ConfirmedRepositoryPermissionLoss(confirmed) => {
                            confirmed_permission_lost_repos
                                .entry(confirmed.repo.clone())
                                .or_insert_with(|| confirmed.clone());
                            if let Some(commit) = commit_lifecycle.as_deref_mut() {
                                if let Err(error) = commit(
                                    &ConfirmedFetchLifecycle::RepositoryPermissionLoss(confirmed),
                                ) {
                                    fail_fetch!(content_free_commit_error(error));
                                }
                                continue 'repos;
                            }
                            finish_fetch!(None);
                        }
                        CommentFetchOutcome::SourceDeleted { http_status } => {
                            let confirmed = ConfirmedSourceDeletion {
                                source_id: issue.source_id.clone(),
                                repo: issue.repo.clone(),
                                entity_type: "issue".to_string(),
                                issue_number: issue.number,
                                http_status,
                            };
                            confirmed_source_deletions
                                .entry(issue.source_id.clone())
                                .or_insert_with(|| confirmed.clone());
                            if let Some(commit) = commit_lifecycle.as_deref_mut() {
                                if let Err(error) =
                                    commit(&ConfirmedFetchLifecycle::SourceDeletion(confirmed))
                                {
                                    fail_fetch!(content_free_commit_error(error));
                                }
                                continue;
                            }
                            finish_fetch!(None);
                        }
                        CommentFetchOutcome::AuthenticationFailed => {
                            finish_fetch!(Some(LifecycleInterruption::AuthenticationFailed));
                        }
                        CommentFetchOutcome::Transient(kind) => {
                            finish_fetch!(Some(LifecycleInterruption::Transient(kind)));
                        }
                        CommentFetchOutcome::AmbiguousForbidden => {
                            finish_fetch!(Some(LifecycleInterruption::AmbiguousForbidden));
                        }
                        CommentFetchOutcome::Failed(error) => fail_fetch!(error),
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
            if let Err(error) = commit_page(FetchPage {
                issues: page_issues,
                comments: page_comments,
                skipped_pull_requests: page_skipped_pull_requests,
                cursor_updates: page_cursor_updates,
            }) {
                fail_fetch!(content_free_commit_error(error));
            }
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

    finish_fetch!(None);
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
    pub terminal_error: Option<QghError>,
}

/// Budgeted historical backfill: walk issues older-first from each repo's
/// history cursor (`state=all&sort=updated&direction=asc`), fetching every
/// issue's comments so historical comment coverage is filled (repo-level `since`
/// listing only returns fresh comments). Each repo keeps its OWN `history:<repo>`
/// cursor committed per page, so a budget/backoff cutoff resumes each repo from
/// its own watermark without skipping later repos. Bounded by `max_pages`
/// (issue-list pages) and `max_duration_seconds`. Does not touch the live cursor.
#[allow(clippy::too_many_arguments)]
#[allow(dead_code)]
pub async fn fetch_backfill_issues(
    profile: &Profile,
    token: &str,
    cursors: &[StoredCursor],
    max_pages: Option<usize>,
    max_duration_seconds: Option<i64>,
    progress: Option<&dyn ProgressReporter>,
    commit_page: &mut dyn FnMut(FetchPage) -> Result<(), QghError>,
) -> Result<BackfillOutcome, QghError> {
    legacy_backfill_outcome(
        fetch_backfill_issues_classified(
            profile,
            token,
            cursors,
            max_pages,
            max_duration_seconds,
            progress,
            commit_page,
        )
        .await?,
    )
}

#[allow(dead_code)]
fn legacy_backfill_outcome(outcome: BackfillOutcome) -> Result<BackfillOutcome, QghError> {
    if !outcome.confirmed_permission_lost_repos.is_empty()
        || !outcome.confirmed_source_deletions.is_empty()
    {
        return Err(confirmed_lifecycle_requires_typed_handling());
    }
    if let Some(error) = outcome.terminal_error.clone() {
        return Err(error);
    }
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
    fetch_backfill_issues_classified_inner(
        profile,
        token,
        cursors,
        max_pages,
        max_duration_seconds,
        progress,
        commit_page,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn fetch_backfill_issues_classified_with_lifecycle_commit(
    profile: &Profile,
    token: &str,
    cursors: &[StoredCursor],
    max_pages: Option<usize>,
    max_duration_seconds: Option<i64>,
    progress: Option<&dyn ProgressReporter>,
    commit_page: &mut dyn FnMut(FetchPage) -> Result<(), QghError>,
    commit_lifecycle: &mut dyn FnMut(&ConfirmedFetchLifecycle) -> Result<(), QghError>,
) -> Result<BackfillOutcome, QghError> {
    fetch_backfill_issues_classified_inner(
        profile,
        token,
        cursors,
        max_pages,
        max_duration_seconds,
        progress,
        commit_page,
        Some(commit_lifecycle),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn fetch_backfill_issues_classified_inner(
    profile: &Profile,
    token: &str,
    cursors: &[StoredCursor],
    max_pages: Option<usize>,
    max_duration_seconds: Option<i64>,
    progress: Option<&dyn ProgressReporter>,
    commit_page: &mut dyn FnMut(FetchPage) -> Result<(), QghError>,
    mut commit_lifecycle: Option<LifecycleCommit<'_>>,
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
    let mut terminal_error = None;
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
        while let Some(url) = next_url.take() {
            if max_pages.is_some_and(|max| pages >= max)
                || max_duration_seconds
                    .is_some_and(|secs| (Utc::now() - started).num_seconds() >= secs)
            {
                all_reached_end = false;
                break 'repos;
            }
            let response = match github_get(&client, &url, token, &profile.api_base_url)?
                .send()
                .await
            {
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
                                .or_insert_with(|| confirmed.clone());
                            all_reached_end = false;
                            if let Some(commit) = commit_lifecycle.as_deref_mut() {
                                if let Err(error) = commit(
                                    &ConfirmedFetchLifecycle::RepositoryPermissionLoss(confirmed),
                                ) {
                                    terminal_error = Some(content_free_commit_error(error));
                                    break 'repos;
                                }
                                continue 'repos;
                            }
                            break 'repos;
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
            let page: Vec<ApiIssue> = match response.json().await {
                Ok(page) => page,
                Err(_) => {
                    terminal_error = Some(invalid_issue_response());
                    all_reached_end = false;
                    break 'repos;
                }
            };
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
                .await
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
                            .or_insert_with(|| confirmed.clone());
                        all_reached_end = false;
                        if let Some(commit) = commit_lifecycle.as_deref_mut() {
                            if let Err(error) = commit(
                                &ConfirmedFetchLifecycle::RepositoryPermissionLoss(confirmed),
                            ) {
                                terminal_error = Some(content_free_commit_error(error));
                                break 'repos;
                            }
                            continue 'repos;
                        }
                        break 'repos;
                    }
                    CommentFetchOutcome::SourceDeleted { http_status } => {
                        let confirmed = ConfirmedSourceDeletion {
                            source_id: issue.source_id.clone(),
                            repo: issue.repo.clone(),
                            entity_type: "issue".to_string(),
                            issue_number: issue.number,
                            http_status,
                        };
                        confirmed_source_deletions
                            .entry(issue.source_id.clone())
                            .or_insert_with(|| confirmed.clone());
                        all_reached_end = false;
                        if let Some(commit) = commit_lifecycle.as_deref_mut() {
                            if let Err(error) =
                                commit(&ConfirmedFetchLifecycle::SourceDeletion(confirmed))
                            {
                                terminal_error = Some(content_free_commit_error(error));
                                break 'repos;
                            }
                            continue;
                        }
                        break 'repos;
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
                    CommentFetchOutcome::Failed(error) => {
                        terminal_error = Some(error);
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
            if let Err(error) = commit_page(FetchPage {
                issues: page_issues,
                comments: page_comments,
                skipped_pull_requests: page_skipped_pull_requests,
                cursor_updates: page_cursor_updates,
            }) {
                terminal_error = Some(content_free_commit_error(error));
                all_reached_end = false;
                break 'repos;
            }
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
        terminal_error,
    })
}

fn backfill_endpoint(repo: &RepoRef) -> String {
    format!("history:{}", repo.full_name())
}

#[allow(dead_code)]
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

#[allow(dead_code)]
fn legacy_target_issue_outcome(
    outcome: ClassifiedTargetIssueFetchOutcome,
) -> Result<TargetIssueFetchOutcome, QghError> {
    if !outcome.confirmed_transitions.is_empty()
        || matches!(
            outcome.terminal,
            ClassifiedTargetIssueTerminal::Confirmed { .. }
        )
    {
        return Err(confirmed_lifecycle_requires_typed_handling());
    }
    Ok(match outcome.terminal {
        ClassifiedTargetIssueTerminal::Fetched(fetched) => {
            TargetIssueFetchOutcome::Fetched(fetched)
        }
        ClassifiedTargetIssueTerminal::Confirmed { .. } => unreachable!(),
        ClassifiedTargetIssueTerminal::Backoff(plan) => TargetIssueFetchOutcome::Backoff(plan),
        ClassifiedTargetIssueTerminal::AuthenticationFailed => {
            return Err(authentication_failure());
        }
        ClassifiedTargetIssueTerminal::Transient(_)
        | ClassifiedTargetIssueTerminal::AmbiguousForbidden => {
            return Err(github_unavailable());
        }
        ClassifiedTargetIssueTerminal::Failed(error) => return Err(error),
    })
}

pub async fn fetch_target_issue_classified(
    profile: &Profile,
    token: &str,
    repo: &RepoRef,
    issue_number: i64,
    progress: Option<&dyn ProgressReporter>,
) -> Result<ClassifiedTargetIssueFetchOutcome, QghError> {
    let client = lifecycle_client()?;
    fetch_target_issue_classified_with_client(&client, profile, token, repo, issue_number, progress)
        .await
}

/// Fetches a targeted issue while durably committing each confirmed permanent
/// transition before the next remote request is attempted. The callback is the
/// fail-closed boundary used by command orchestration to queue the old source
/// for purge before following its replacement location.
pub async fn fetch_target_issue_classified_with_transition_commit(
    profile: &Profile,
    token: &str,
    repo: &RepoRef,
    issue_number: i64,
    progress: Option<&dyn ProgressReporter>,
    commit_transition: &mut dyn FnMut(&ConfirmedIssueTransition) -> Result<(), QghError>,
) -> Result<ClassifiedTargetIssueFetchOutcome, QghError> {
    let client = lifecycle_client()?;
    fetch_target_issue_classified_with_client_and_transition_commit(
        &client,
        profile,
        token,
        repo,
        issue_number,
        progress,
        commit_transition,
    )
    .await
}

async fn fetch_target_issue_classified_with_client(
    client: &reqwest::Client,
    profile: &Profile,
    token: &str,
    repo: &RepoRef,
    issue_number: i64,
    progress: Option<&dyn ProgressReporter>,
) -> Result<ClassifiedTargetIssueFetchOutcome, QghError> {
    let mut ignore_transition = |_transition: &ConfirmedIssueTransition| Ok(());
    fetch_target_issue_classified_with_client_and_transition_commit(
        client,
        profile,
        token,
        repo,
        issue_number,
        progress,
        &mut ignore_transition,
    )
    .await
}

async fn fetch_target_issue_classified_with_client_and_transition_commit(
    client: &reqwest::Client,
    profile: &Profile,
    token: &str,
    repo: &RepoRef,
    issue_number: i64,
    progress: Option<&dyn ProgressReporter>,
    commit_transition: &mut dyn FnMut(&ConfirmedIssueTransition) -> Result<(), QghError>,
) -> Result<ClassifiedTargetIssueFetchOutcome, QghError> {
    const TRANSFER_FOLLOW_LIMIT: usize = 8;

    let mut current_repo = repo.clone();
    let mut current_issue_number = issue_number;
    let mut alias_chain = Vec::new();
    let mut visited = BTreeSet::new();
    let mut confirmed_transition = None;
    let mut confirmed_transition_http_status = None;
    let mut confirmed_transitions = Vec::new();
    macro_rules! finish_target {
        ($terminal:expr) => {
            return Ok(ClassifiedTargetIssueFetchOutcome {
                confirmed_transitions,
                terminal: $terminal,
            })
        };
    }

    loop {
        let identity = (
            current_repo.full_name().to_ascii_lowercase(),
            current_issue_number,
        );
        if !visited.insert(identity) {
            finish_target!(ClassifiedTargetIssueTerminal::Failed(transfer_cycle_error(
                repo,
                issue_number,
                &alias_chain,
            )));
        }
        if visited.len() > TRANSFER_FOLLOW_LIMIT {
            finish_target!(ClassifiedTargetIssueTerminal::Failed(
                transfer_chain_too_long_error(repo, issue_number, &alias_chain)
            ));
        }

        let url = issue_object_url(profile, &current_repo, current_issue_number);

        let request = match github_get(client, &url, token, &profile.api_base_url) {
            Ok(request) => request,
            Err(_) => {
                finish_target!(ClassifiedTargetIssueTerminal::Transient(
                    GitHubTransientKind::UnexpectedResponse
                ));
            }
        };
        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => {
                finish_target!(ClassifiedTargetIssueTerminal::Transient(
                    classify_transport_failure(&error)
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
            finish_target!(ClassifiedTargetIssueTerminal::Backoff(backoff));
        }

        match status {
            StatusCode::OK => {
                let indexed_at = now_rfc3339();
                let issue = match response.json::<ApiIssue>().await {
                    Ok(issue) => issue,
                    Err(_) => finish_target!(ClassifiedTargetIssueTerminal::Failed(
                        invalid_issue_response()
                    )),
                };
                if issue.pull_request.is_some() {
                    finish_target!(ClassifiedTargetIssueTerminal::Failed(
                        unsupported_source_type_error()
                    ));
                }
                let issue = issue.into_record(profile, &current_repo, &indexed_at);
                let mut cursor_updates = Vec::new();
                let empty_cursors = BTreeMap::new();
                let comments = match fetch_issue_comments(
                    client,
                    profile,
                    token,
                    &empty_cursors,
                    &mut cursor_updates,
                    &current_repo,
                    &issue,
                    progress,
                )
                .await
                {
                    CommentFetchOutcome::Fetched(comments) => comments,
                    CommentFetchOutcome::Backoff(backoff) => {
                        wait_for_backoff(&backoff);
                        finish_target!(ClassifiedTargetIssueTerminal::Backoff(backoff));
                    }
                    CommentFetchOutcome::ConfirmedRepositoryPermissionLoss(confirmed) => {
                        finish_target!(target_terminal_from_lifecycle_check(
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
                        finish_target!(target_terminal_from_lifecycle_check(
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
                        finish_target!(ClassifiedTargetIssueTerminal::AuthenticationFailed);
                    }
                    CommentFetchOutcome::Transient(kind) => {
                        finish_target!(ClassifiedTargetIssueTerminal::Transient(kind));
                    }
                    CommentFetchOutcome::AmbiguousForbidden => {
                        finish_target!(ClassifiedTargetIssueTerminal::AmbiguousForbidden);
                    }
                    CommentFetchOutcome::Failed(error) => {
                        finish_target!(ClassifiedTargetIssueTerminal::Failed(error));
                    }
                };
                let lifecycle = if confirmed_transition.is_none() {
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
                        http_status: confirmed_transition_http_status,
                        alias_chain,
                    }
                };
                finish_target!(ClassifiedTargetIssueTerminal::Fetched(Box::new(
                    TargetIssueFetch {
                        issue,
                        comments,
                        lifecycle,
                        confirmed_transition,
                    },
                )));
            }
            StatusCode::MOVED_PERMANENTLY => {
                let Some(location) = header_string(&headers, LOCATION) else {
                    finish_target!(ClassifiedTargetIssueTerminal::Transient(
                        GitHubTransientKind::UnexpectedResponse,
                    ));
                };
                let Some((next_repo, next_issue_number)) = parse_issue_location(profile, &location)
                else {
                    finish_target!(ClassifiedTargetIssueTerminal::Transient(
                        GitHubTransientKind::UnexpectedResponse,
                    ));
                };
                if same_issue_identity(
                    &next_repo.full_name(),
                    next_issue_number,
                    &current_repo.full_name(),
                    current_issue_number,
                ) {
                    finish_target!(ClassifiedTargetIssueTerminal::Transient(
                        GitHubTransientKind::UnexpectedResponse,
                    ));
                }
                alias_chain.push(location);
                confirmed_transition = Some(ConfirmedRemoteState::SourceTransferred);
                confirmed_transition_http_status.get_or_insert(status.as_u16());
                let transition = ConfirmedIssueTransition {
                    source_repo: current_repo.full_name(),
                    source_issue_number: current_issue_number,
                    target_repo: next_repo.full_name(),
                    target_issue_number: next_issue_number,
                    state: ConfirmedRemoteState::SourceTransferred,
                    http_status: status.as_u16(),
                };
                confirmed_transitions.push(transition.clone());
                if let Err(error) = commit_transition(&transition) {
                    finish_target!(ClassifiedTargetIssueTerminal::Failed(
                        content_free_commit_error(error)
                    ));
                }
                if !profile.allows_repo(&next_repo.full_name()) {
                    finish_target!(ClassifiedTargetIssueTerminal::Confirmed {
                        state: ConfirmedRemoteState::SourceTransferred,
                        repo: current_repo.full_name(),
                        issue_number: current_issue_number,
                        lifecycle: unavailable_lifecycle("transferred", status, alias_chain),
                    });
                }
                current_repo = next_repo;
                current_issue_number = next_issue_number;
            }
            StatusCode::FOUND | StatusCode::TEMPORARY_REDIRECT | StatusCode::PERMANENT_REDIRECT => {
                finish_target!(ClassifiedTargetIssueTerminal::Transient(
                    GitHubTransientKind::UnexpectedResponse,
                ));
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
                    client,
                    profile,
                    token,
                    &current_repo,
                    first,
                    progress,
                )
                .await;
                finish_target!(target_terminal_from_lifecycle_check(
                    confirmation,
                    &current_repo,
                    current_issue_number,
                    alias_chain,
                ));
            }
            StatusCode::UNAUTHORIZED => {
                finish_target!(ClassifiedTargetIssueTerminal::AuthenticationFailed);
            }
            StatusCode::FORBIDDEN => {
                let body = response.text().await.unwrap_or_default();
                let disposition =
                    classify_response(status, &headers, &body, &scope, ResponseTarget::Source);
                let terminal = match disposition {
                    ResponseDisposition::Backoff(plan) => {
                        emit_backoff(progress, &plan);
                        ClassifiedTargetIssueTerminal::Backoff(plan)
                    }
                    ResponseDisposition::PermissionCandidate { http_status } => {
                        let confirmation = confirm_source_denial_with_repository(
                            client,
                            profile,
                            token,
                            &current_repo,
                            PermissionEvidence::Forbidden { http_status },
                            progress,
                        )
                        .await;
                        target_terminal_from_lifecycle_check(
                            confirmation,
                            &current_repo,
                            current_issue_number,
                            alias_chain,
                        )
                    }
                    ResponseDisposition::AmbiguousForbidden => {
                        ClassifiedTargetIssueTerminal::AmbiguousForbidden
                    }
                    _ => ClassifiedTargetIssueTerminal::Transient(
                        GitHubTransientKind::UnexpectedResponse,
                    ),
                };
                finish_target!(terminal);
            }
            status if status.is_success() => {
                finish_target!(ClassifiedTargetIssueTerminal::Transient(
                    GitHubTransientKind::UnexpectedResponse,
                ));
            }
            status if status.is_server_error() => {
                finish_target!(ClassifiedTargetIssueTerminal::Transient(
                    GitHubTransientKind::Server,
                ));
            }
            _ => {
                finish_target!(ClassifiedTargetIssueTerminal::Transient(
                    GitHubTransientKind::UnexpectedResponse,
                ));
            }
        }
    }
}

#[allow(dead_code)]
pub async fn reconcile_sources(
    profile: &Profile,
    token: &str,
    candidates: &[ReconciliationCandidate],
    progress: Option<&dyn ProgressReporter>,
) -> Result<ReconciliationResult, QghError> {
    reconcile_sources_inner(profile, token, candidates, progress, None).await
}

pub async fn reconcile_sources_with_lifecycle_commit(
    profile: &Profile,
    token: &str,
    candidates: &[ReconciliationCandidate],
    progress: Option<&dyn ProgressReporter>,
    commit_lifecycle: &mut dyn FnMut(&ConfirmedFetchLifecycle) -> Result<(), QghError>,
) -> Result<ReconciliationResult, QghError> {
    reconcile_sources_inner(profile, token, candidates, progress, Some(commit_lifecycle)).await
}

async fn reconcile_sources_inner(
    profile: &Profile,
    token: &str,
    candidates: &[ReconciliationCandidate],
    progress: Option<&dyn ProgressReporter>,
    mut commit_lifecycle: Option<LifecycleCommit<'_>>,
) -> Result<ReconciliationResult, QghError> {
    let client = lifecycle_client()?;
    let mut unavailable_sources = Vec::new();
    let mut confirmed_permission_lost_repos = BTreeMap::new();
    let mut interruption = None;
    let mut terminal_error = None;
    let total = candidates.len();
    let mut checked_sources = 0;
    'candidates: for (index, candidate) in candidates.iter().enumerate() {
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
        let outcome = match check_candidate_lifecycle_classified(
            &client, profile, token, candidate, progress,
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(error) => {
                terminal_error = Some(content_free_validation_error(error));
                break;
            }
        };
        checked_sources += 1;
        match outcome {
            ClassifiedLifecycleCheck::Active => {}
            ClassifiedLifecycleCheck::Confirmed {
                state:
                    state @ (ConfirmedRemoteState::SourceDeleted
                    | ConfirmedRemoteState::SourceTransferred),
                http_status,
            } => {
                let failure = LifecycleFailure {
                    source_id: candidate.source_id.clone(),
                    repo: candidate.repo.clone(),
                    entity_type: candidate.entity_type.clone(),
                    issue_number: candidate.issue_number,
                    reason: state.reason().to_string(),
                    state,
                    http_status,
                };
                unavailable_sources.push(failure.clone());
                if let Some(commit) = commit_lifecycle.as_deref_mut() {
                    if let Err(error) =
                        commit(&ConfirmedFetchLifecycle::ReconciliationFailure(failure))
                    {
                        terminal_error = Some(content_free_commit_error(error));
                        break 'candidates;
                    }
                } else {
                    break 'candidates;
                }
            }
            ClassifiedLifecycleCheck::Confirmed {
                state: ConfirmedRemoteState::RepositoryPermissionLoss,
                http_status,
            } => {
                let confirmed = ConfirmedRepositoryPermissionLoss {
                    repo: candidate.repo.clone(),
                    http_status,
                };
                confirmed_permission_lost_repos
                    .entry(candidate.repo.clone())
                    .or_insert_with(|| confirmed.clone());
                if let Some(commit) = commit_lifecycle.as_deref_mut() {
                    if let Err(error) = commit(&ConfirmedFetchLifecycle::RepositoryPermissionLoss(
                        confirmed,
                    )) {
                        terminal_error = Some(content_free_commit_error(error));
                        break 'candidates;
                    }
                } else {
                    break 'candidates;
                }
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
        terminal_error,
    })
}

#[allow(dead_code)]
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

#[allow(dead_code)]
fn legacy_lifecycle_check(check: ClassifiedLifecycleCheck) -> Result<LifecycleCheck, QghError> {
    match check {
        ClassifiedLifecycleCheck::Active => Ok(LifecycleCheck::Active),
        ClassifiedLifecycleCheck::Confirmed { .. } => {
            Err(confirmed_lifecycle_requires_typed_handling())
        }
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
) -> CommentFetchOutcome {
    let mut comments = Vec::new();
    let endpoint = comment_endpoint(repo, issue.number);
    let stored_cursor = cursor_map.get(&endpoint);
    let mut max_watermark = stored_cursor.and_then(|cursor| cursor.cursor.clone());
    let mut response_etag = stored_cursor.and_then(|cursor| cursor.etag.clone());
    let mut endpoint_not_modified = false;
    let mut next_url = Some(comment_url(profile, repo, issue.number, stored_cursor));
    while let Some(url) = next_url.take() {
        let mut request = match github_get(client, &url, token, &profile.api_base_url) {
            Ok(request) => request,
            Err(error) => return CommentFetchOutcome::Failed(error),
        };
        if let Some(etag) = stored_cursor.and_then(|cursor| cursor.etag.as_ref()) {
            request = request.header(IF_NONE_MATCH, etag);
        }
        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => {
                return CommentFetchOutcome::Transient(classify_transport_failure(&error));
            }
        };
        let status = response.status();
        let headers = response.headers().clone();
        if let Some(backoff) = backoff_from_response(status, &headers, &endpoint) {
            emit_backoff(progress, &backoff);
            return CommentFetchOutcome::Backoff(backoff);
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
                return match confirm_source_denial_with_repository(
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
                    ClassifiedLifecycleCheck::Backoff(plan) => CommentFetchOutcome::Backoff(plan),
                    ClassifiedLifecycleCheck::Transient(kind) => {
                        CommentFetchOutcome::Transient(kind)
                    }
                    ClassifiedLifecycleCheck::AmbiguousForbidden
                    | ClassifiedLifecycleCheck::Active
                    | ClassifiedLifecycleCheck::Confirmed {
                        state: ConfirmedRemoteState::SourceTransferred,
                        ..
                    } => CommentFetchOutcome::AmbiguousForbidden,
                };
            }
            return match disposition {
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
            };
        }
        response_etag = header_string(&headers, ETAG).or(response_etag);
        let page: Vec<ApiComment> = match response.json().await {
            Ok(page) => page,
            Err(_) => return CommentFetchOutcome::Failed(invalid_comment_response()),
        };
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
    CommentFetchOutcome::Fetched(comments)
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
    pub terminal_error: Option<QghError>,
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
#[allow(dead_code)]
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
    legacy_repo_comments_outcome(outcome)
}

#[allow(dead_code)]
fn legacy_repo_comments_outcome(
    outcome: RepoCommentsResult,
) -> Result<RepoCommentsResult, QghError> {
    if !outcome.confirmed_permission_lost_repos.is_empty() {
        return Err(confirmed_lifecycle_requires_typed_handling());
    }
    if let Some(error) = outcome.terminal_error.clone() {
        return Err(error);
    }
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
    fetch_repo_comments_classified_inner(
        profile,
        token,
        cursors,
        parent_resolution_budget,
        resolve_parent,
        progress,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn fetch_repo_comments_classified_with_lifecycle_commit(
    profile: &Profile,
    token: &str,
    cursors: &[StoredCursor],
    parent_resolution_budget: usize,
    resolve_parent: &dyn Fn(&str, i64) -> Option<CommentParent>,
    progress: Option<&dyn ProgressReporter>,
    commit_lifecycle: &mut dyn FnMut(&ConfirmedFetchLifecycle) -> Result<(), QghError>,
) -> Result<RepoCommentsResult, QghError> {
    fetch_repo_comments_classified_inner(
        profile,
        token,
        cursors,
        parent_resolution_budget,
        resolve_parent,
        progress,
        Some(commit_lifecycle),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn fetch_repo_comments_classified_inner(
    profile: &Profile,
    token: &str,
    cursors: &[StoredCursor],
    parent_resolution_budget: usize,
    resolve_parent: &dyn Fn(&str, i64) -> Option<CommentParent>,
    progress: Option<&dyn ProgressReporter>,
    mut commit_lifecycle: Option<LifecycleCommit<'_>>,
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
    let mut terminal_error = None;

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
            let mut request = github_get(&client, &url, token, &profile.api_base_url)?;
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
                                .or_insert_with(|| confirmed.clone());
                            if let Some(commit) = commit_lifecycle.as_deref_mut() {
                                if let Err(error) = commit(
                                    &ConfirmedFetchLifecycle::RepositoryPermissionLoss(confirmed),
                                ) {
                                    terminal_error = Some(content_free_commit_error(error));
                                    break 'repos;
                                }
                                continue 'repos;
                            }
                            break 'repos;
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
            let page: Vec<ApiComment> = match response.json().await {
                Ok(page) => page,
                Err(_) => {
                    terminal_error = Some(invalid_comment_response());
                    break 'repos;
                }
            };
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
                        match classify_parent(&client, profile, token, repo, number, progress).await
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
                            ParentClass::Transient(kind) => {
                                interruption = Some(LifecycleInterruption::Transient(kind));
                                break 'repos;
                            }
                            ParentClass::Failed(error) => {
                                terminal_error = Some(error);
                                break 'repos;
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
        terminal_error,
    })
}

enum ParentClass {
    PullRequest,
    IssueOrUnknown,
    Backoff(BackoffPlan),
    Transient(GitHubTransientKind),
    Failed(QghError),
}

async fn classify_parent(
    client: &reqwest::Client,
    profile: &Profile,
    token: &str,
    repo: &RepoRef,
    issue_number: i64,
    progress: Option<&dyn ProgressReporter>,
) -> ParentClass {
    let url = issue_object_url(profile, repo, issue_number);
    let request = match github_get(client, &url, token, &profile.api_base_url) {
        Ok(request) => request,
        Err(error) => return ParentClass::Failed(error),
    };
    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => return ParentClass::Transient(classify_transport_failure(&error)),
    };
    let status = response.status();
    let headers = response.headers().clone();
    if let Some(backoff) = backoff_from_response(status, &headers, &repo_comment_endpoint(repo)) {
        emit_backoff(progress, &backoff);
        return ParentClass::Backoff(backoff);
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
            return ParentClass::Backoff(plan);
        }
        // Cannot classify (deleted/permission/etc.): defer rather than guess.
        return ParentClass::IssueOrUnknown;
    }
    let issue: ApiIssue = match response.json().await {
        Ok(issue) => issue,
        Err(_) => return ParentClass::Failed(invalid_issue_response()),
    };
    if issue.pull_request.is_some() {
        ParentClass::PullRequest
    } else {
        ParentClass::IssueOrUnknown
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
    Failed(QghError),
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
    let request = match github_get(client, &url, token, api_base_url) {
        Ok(request) => request,
        Err(_) => return ResponseDisposition::Transient(GitHubTransientKind::UnexpectedResponse),
    };
    let response = match request.send().await {
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
#[allow(dead_code)]
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

#[allow(dead_code)]
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
            .map(|epoch| epoch.saturating_sub(Utc::now().timestamp()).max(0))
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
        .map(|value| value.max(0))
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

fn target_terminal_from_lifecycle_check(
    check: ClassifiedLifecycleCheck,
    repo: &RepoRef,
    issue_number: i64,
    alias_chain: Vec<String>,
) -> ClassifiedTargetIssueTerminal {
    match check {
        ClassifiedLifecycleCheck::Confirmed { state, http_status } => {
            let reason = state.reason().to_string();
            ClassifiedTargetIssueTerminal::Confirmed {
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
            ClassifiedTargetIssueTerminal::AuthenticationFailed
        }
        ClassifiedLifecycleCheck::Backoff(plan) => ClassifiedTargetIssueTerminal::Backoff(plan),
        ClassifiedLifecycleCheck::Transient(kind) => ClassifiedTargetIssueTerminal::Transient(kind),
        ClassifiedLifecycleCheck::AmbiguousForbidden => {
            ClassifiedTargetIssueTerminal::AmbiguousForbidden
        }
        ClassifiedLifecycleCheck::Active => {
            ClassifiedTargetIssueTerminal::Transient(GitHubTransientKind::UnexpectedResponse)
        }
    }
}

fn lifecycle_client() -> Result<reqwest::Client, QghError> {
    github_http_client()
}

pub(crate) fn github_http_client() -> Result<reqwest::Client, QghError> {
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
    let response = match github_get(client, &url, token, &profile.api_base_url)?
        .send()
        .await
    {
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
        let destination = header_string(&headers, LOCATION)
            .as_deref()
            .and_then(|location| parse_issue_location(profile, location));
        return Ok(
            if status == StatusCode::MOVED_PERMANENTLY
                && candidate.entity_type == "issue"
                && destination.is_some_and(|(repo, issue_number)| {
                    !same_issue_identity(
                        &repo.full_name(),
                        issue_number,
                        &candidate.repo,
                        candidate.issue_number,
                    )
                })
            {
                ClassifiedLifecycleCheck::Confirmed {
                    state: ConfirmedRemoteState::SourceTransferred,
                    http_status: status.as_u16(),
                }
            } else {
                ClassifiedLifecycleCheck::Transient(GitHubTransientKind::UnexpectedResponse)
            },
        );
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

#[allow(dead_code)]
fn authentication_failure() -> QghError {
    QghError::auth("GitHub authentication failed.")
        .with_hint("Refresh the configured GitHub token source, then retry.")
}

#[allow(dead_code)]
pub(crate) fn github_unavailable() -> QghError {
    QghError::github("GitHub request did not produce a confirmed lifecycle result.")
        .with_hint("Retry later; local content was not removed.")
        .with_retryable(true)
}

#[allow(dead_code)]
fn confirmed_lifecycle_requires_typed_handling() -> QghError {
    QghError::new(
        "github.confirmed_lifecycle_requires_typed_handling",
        "Confirmed lifecycle evidence requires typed purge handling.",
        3,
    )
    .with_hint(
        "Retry through a command that queues confirmed lifecycle evidence before continuing.",
    )
}

fn invalid_issue_response() -> QghError {
    QghError::new(
        "github.invalid_issue_json",
        "GitHub returned invalid issue JSON.",
        3,
    )
}

fn invalid_comment_response() -> QghError {
    QghError::new(
        "github.invalid_comment_json",
        "GitHub returned invalid comment JSON.",
        3,
    )
}

fn unsupported_source_type_error() -> QghError {
    QghError::validation(
        "validation.unsupported_source_type",
        "The requested source type is not supported.",
    )
}

fn transfer_cycle_error(repo: &RepoRef, issue_number: i64, alias_chain: &[String]) -> QghError {
    QghError::validation(
        "sync.transfer_cycle",
        "Issue transfer alias chain contains a cycle.",
    )
    .with_details(json!({
        "repo": repo.full_name(),
        "issue_number": issue_number,
        "alias_chain": alias_chain
    }))
    .with_hint(
        "Run targeted refresh for the final issue location after the transfer is corrected upstream.",
    )
}

fn transfer_chain_too_long_error(
    repo: &RepoRef,
    issue_number: i64,
    alias_chain: &[String],
) -> QghError {
    QghError::validation(
        "sync.transfer_chain_too_long",
        "Issue transfer alias chain exceeded the follow limit.",
    )
    .with_details(json!({
        "repo": repo.full_name(),
        "issue_number": issue_number,
        "alias_chain": alias_chain,
        "max_redirects": 8
    }))
    .with_hint("Run targeted refresh for the final issue location directly.")
}

fn content_free_commit_error(error: QghError) -> QghError {
    let (code, exit_code) = stable_error_identity(&error, "sync.commit_page_failed", 6);
    QghError::new(code, "Local fetch checkpoint commit failed.", exit_code)
}

fn content_free_validation_error(error: QghError) -> QghError {
    let (code, exit_code) = stable_error_identity(&error, "validation.lifecycle_failed", 2);
    QghError::new(code, "Lifecycle validation failed.", exit_code)
}

fn stable_error_identity(
    error: &QghError,
    fallback_code: &str,
    fallback_exit: i32,
) -> (String, i32) {
    let stable_code = !error.code.is_empty()
        && error.code.len() <= 96
        && error
            .code
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if stable_code {
        (error.code.clone(), error.exit_code)
    } else {
        (fallback_code.to_string(), fallback_exit)
    }
}

pub(crate) fn github_get(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    trusted_api_base_url: &str,
) -> Result<reqwest::RequestBuilder, QghError> {
    let target = reqwest::Url::parse(url).map_err(|_| untrusted_github_request_origin())?;
    let trusted =
        reqwest::Url::parse(trusted_api_base_url).map_err(|_| untrusted_github_request_origin())?;
    if !target.username().is_empty()
        || target.password().is_some()
        || target.fragment().is_some()
        || !trusted.username().is_empty()
        || trusted.password().is_some()
        || trusted.query().is_some()
        || trusted.fragment().is_some()
        || !same_url_origin(&target, &trusted)
        || !url_path_is_within_base(&target, &trusted)
    {
        return Err(untrusted_github_request_origin());
    }
    let request = client.get(url);
    let request = if github_request_may_receive_token(url, trusted_api_base_url) {
        request.bearer_auth(token)
    } else {
        request
    };
    Ok(request
        .header("accept", "application/vnd.github+json")
        .header("user-agent", user_agent())
        .header("x-github-api-version", GITHUB_API_VERSION))
}

fn untrusted_github_request_origin() -> QghError {
    QghError::github("GitHub request origin did not match the configured API endpoint.")
}

fn github_request_may_receive_token(url: &str, trusted_api_base_url: &str) -> bool {
    let Ok(target) = reqwest::Url::parse(url) else {
        return false;
    };
    let Ok(trusted) = reqwest::Url::parse(trusted_api_base_url) else {
        return false;
    };
    if target.scheme() != "https" || !same_url_origin(&target, &trusted) {
        return false;
    }
    !target.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    })
}

fn same_url_origin(left: &reqwest::Url, right: &reqwest::Url) -> bool {
    left.scheme() == right.scheme()
        && left
            .host_str()
            .zip(right.host_str())
            .is_some_and(|(left, right)| left.eq_ignore_ascii_case(right))
        && left.port_or_known_default() == right.port_or_known_default()
}

fn url_path_is_within_base(target: &reqwest::Url, trusted: &reqwest::Url) -> bool {
    let base = trusted.path().trim_end_matches('/');
    base.is_empty()
        || target.path() == base
        || target
            .path()
            .strip_prefix(base)
            .is_some_and(|suffix| suffix.starts_with('/'))
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
    if url.origin() != base.origin() || url.query().is_some() || url.fragment().is_some() {
        return None;
    }
    let segments = url.path_segments()?.collect::<Vec<_>>();
    let repos_index = segments.iter().position(|segment| *segment == "repos")?;
    let base_prefix = base
        .path_segments()?
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    let location_prefix = &segments[..repos_index];
    if !location_prefix.is_empty() && location_prefix != base_prefix {
        return None;
    }
    if segments.len() != repos_index + 5 {
        return None;
    }
    let owner = segments.get(repos_index + 1)?;
    let name = segments.get(repos_index + 2)?;
    if segments.get(repos_index + 3) != Some(&"issues") {
        return None;
    }
    let issue_number = segments.get(repos_index + 4)?.parse::<i64>().ok()?;
    if issue_number <= 0
        || [owner, name].into_iter().any(|part| {
            part.is_empty()
                || part.contains('%')
                || part.chars().any(char::is_whitespace)
                || part.chars().any(char::is_control)
        })
    {
        return None;
    }
    Some((
        RepoRef {
            owner: owner.to_string(),
            name: name.to_string(),
        },
        issue_number,
    ))
}

fn same_issue_identity(
    left_repo: &str,
    left_issue_number: i64,
    right_repo: &str,
    right_issue_number: i64,
) -> bool {
    left_repo.eq_ignore_ascii_case(right_repo) && left_issue_number == right_issue_number
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

    #[test]
    fn github_token_is_bound_to_the_validated_https_api_origin() {
        assert!(github_request_may_receive_token(
            "https://api.github.com/repos/owner/repo/issues?page=2",
            "https://api.github.com"
        ));
        assert!(github_request_may_receive_token(
            "https://ghe.example/api/v3/repos/owner/repo",
            "https://ghe.example/api/v3"
        ));
        assert!(!github_request_may_receive_token(
            "https://attacker.invalid/repos/owner/repo?page=2",
            "https://api.github.com"
        ));
        assert!(!github_request_may_receive_token(
            "http://127.0.0.1:43123/repos/owner/repo",
            "http://127.0.0.1:43123"
        ));

        let client = reqwest::Client::new();
        let loopback = github_get(
            &client,
            "http://127.0.0.1:43123/repos/owner/repo",
            "PRIVATE_LOOPBACK_TOKEN",
            "http://127.0.0.1:43123",
        )
        .unwrap()
        .build()
        .unwrap();
        assert!(loopback
            .headers()
            .get(reqwest::header::AUTHORIZATION)
            .is_none());

        let off_origin = github_get(
            &client,
            "http://127.0.0.1:43123/metadata",
            "PRIVATE_OFF_ORIGIN_TOKEN",
            "https://api.github.com",
        )
        .unwrap_err();
        assert_eq!(off_origin.code, "github.request_failed");
        let serialized = serde_json::to_string(&off_origin).unwrap();
        assert!(!serialized.contains("PRIVATE_"), "{serialized}");

        let off_base_path = github_get(
            &client,
            "https://ghe.example/PRIVATE_OUTSIDE_API",
            "PRIVATE_PATH_TOKEN",
            "https://ghe.example/api/v3",
        )
        .unwrap_err();
        assert_eq!(off_base_path.code, "github.request_failed");
        assert!(!serde_json::to_string(&off_base_path)
            .unwrap()
            .contains("PRIVATE_"));
    }

    #[test]
    fn off_origin_pagination_is_rejected_before_request_build() {
        let mut headers = HeaderMap::new();
        headers.insert(
            LINK,
            HeaderValue::from_static("<http://127.0.0.1:43123/metadata>; rel=\"next\""),
        );
        let next = next_link(&headers).unwrap();
        let error = github_get(
            &reqwest::Client::new(),
            &next,
            "PRIVATE_PAGINATION_TOKEN",
            "https://api.github.com",
        )
        .unwrap_err();

        assert_eq!(error.code, "github.request_failed");
        assert!(!serde_json::to_string(&error).unwrap().contains("PRIVATE_"));
    }

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

    fn spawn_redirect_then_timeout() -> (String, Arc<AtomicUsize>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let count = Arc::new(AtomicUsize::new(0));
        let thread_count = Arc::clone(&count);
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept redirect request");
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request);
            thread_count.fetch_add(1, Ordering::SeqCst);
            stream
                .write_all(
                    b"HTTP/1.1 301 Moved Permanently\r\nlocation: /repos/owner/b/issues/2\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
                )
                .expect("write redirect response");
            let (mut stream, _) = listener.accept().expect("accept timeout request");
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request);
            thread_count.fetch_add(1, Ordering::SeqCst);
            thread::sleep(StdDuration::from_millis(100));
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
        test_profile_with_repos(api_base_url, vec![test_repo()])
    }

    fn test_profile_with_repos(api_base_url: &str, repos: Vec<RepoRef>) -> Profile {
        let root = PathBuf::from("/tmp/qgh-github-classification-test");
        Profile {
            id: "test".to_string(),
            host: "example.test".to_string(),
            api_base_url: api_base_url.to_string(),
            web_base_url: "https://example.test".to_string(),
            repos,
            embedding: None,
            reranker: None,
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

    fn two_repo_profile(api_base_url: &str) -> Profile {
        test_profile_with_repos(
            api_base_url,
            vec![
                RepoRef {
                    owner: "owner".to_string(),
                    name: "a".to_string(),
                },
                RepoRef {
                    owner: "owner".to_string(),
                    name: "b".to_string(),
                },
            ],
        )
    }

    fn repo_a() -> RepoRef {
        RepoRef {
            owner: "owner".to_string(),
            name: "a".to_string(),
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

    fn empty_fetch_result() -> FetchResult {
        FetchResult {
            issues: 0,
            comments: 0,
            skipped_pull_requests: 0,
            confirmed_permission_lost_repos: Vec::new(),
            confirmed_source_deletions: Vec::new(),
        }
    }

    fn target_outcome_for_test(
        terminal: ClassifiedTargetIssueTerminal,
    ) -> ClassifiedTargetIssueFetchOutcome {
        ClassifiedTargetIssueFetchOutcome {
            confirmed_transitions: Vec::new(),
            terminal,
        }
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
    fn backoff_headers_normalize_extreme_and_negative_values() {
        let mut primary_headers = HeaderMap::new();
        primary_headers.insert("x-ratelimit-remaining", HeaderValue::from_static("0"));
        primary_headers.insert(
            "x-ratelimit-reset",
            HeaderValue::from_static("-9223372036854775808"),
        );
        let primary = backoff_from_response(StatusCode::FORBIDDEN, &primary_headers, "scope")
            .expect("primary rate-limit evidence");
        assert_eq!(primary.retry_after_seconds, 0);
        assert_eq!(primary.reset_at, None);

        let mut negative_headers = HeaderMap::new();
        negative_headers.insert(RETRY_AFTER, HeaderValue::from_static("-5"));
        let negative =
            backoff_from_response(StatusCode::TOO_MANY_REQUESTS, &negative_headers, "scope")
                .expect("secondary rate-limit evidence");
        assert_eq!(negative.retry_after_seconds, 0);

        let mut huge_headers = HeaderMap::new();
        huge_headers.insert(RETRY_AFTER, HeaderValue::from_static("9223372036854775807"));
        let huge = backoff_from_response(StatusCode::TOO_MANY_REQUESTS, &huge_headers, "scope")
            .expect("secondary rate-limit evidence");
        assert_eq!(huge.retry_after_seconds, i64::MAX);
    }

    #[test]
    fn transient_unavailable_error_is_retryable() {
        let error = github_unavailable();
        assert_eq!(error.code, "github.request_failed");
        assert_eq!(error.exit_code, 3);
        assert!(error.retryable);
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
            ClassifiedTargetIssueTerminal::AuthenticationFailed,
            ClassifiedTargetIssueTerminal::Transient(GitHubTransientKind::Timeout),
            ClassifiedTargetIssueTerminal::Transient(GitHubTransientKind::Network),
            ClassifiedTargetIssueTerminal::Transient(GitHubTransientKind::Server),
            ClassifiedTargetIssueTerminal::AmbiguousForbidden,
        ] {
            assert!(legacy_target_issue_outcome(target_outcome_for_test(outcome)).is_err());
        }
        let backoff = legacy_target_issue_outcome(target_outcome_for_test(
            ClassifiedTargetIssueTerminal::Backoff(BackoffPlan {
                reason: "secondary_rate_limit".to_string(),
                scope: "content-free".to_string(),
                retry_after_seconds: 1,
                reset_at: None,
            }),
        ))
        .unwrap();
        assert!(matches!(backoff, TargetIssueFetchOutcome::Backoff(_)));

        for interruption in [
            LifecycleInterruption::AuthenticationFailed,
            LifecycleInterruption::Transient(GitHubTransientKind::Timeout),
            LifecycleInterruption::Transient(GitHubTransientKind::Network),
            LifecycleInterruption::Transient(GitHubTransientKind::Server),
            LifecycleInterruption::AmbiguousForbidden,
        ] {
            assert!(legacy_fetch_outcome(ClassifiedFetchOutcome {
                result: empty_fetch_result(),
                interruption: Some(interruption),
                terminal_error: None,
            })
            .is_err());
        }
        let fetch_backoff = legacy_fetch_outcome(ClassifiedFetchOutcome {
            result: empty_fetch_result(),
            interruption: Some(LifecycleInterruption::Backoff(BackoffPlan {
                reason: "primary_rate_limit".to_string(),
                scope: "content-free".to_string(),
                retry_after_seconds: 1,
                reset_at: None,
            })),
            terminal_error: None,
        })
        .unwrap();
        assert!(matches!(fetch_backoff, FetchOutcome::Backoff(_)));

        let Err(terminal_error) = legacy_fetch_outcome(ClassifiedFetchOutcome {
            result: empty_fetch_result(),
            interruption: None,
            terminal_error: Some(invalid_issue_response()),
        }) else {
            panic!("evidence-empty terminal error must surface unchanged");
        };
        assert_eq!(terminal_error.code, "github.invalid_issue_json");
    }

    #[test]
    fn compatibility_wrappers_require_typed_handling_for_all_confirmed_states() {
        let Err(lifecycle_error) = legacy_lifecycle_check(ClassifiedLifecycleCheck::Confirmed {
            state: ConfirmedRemoteState::SourceDeleted,
            http_status: 404,
        }) else {
            panic!("confirmed lifecycle must require typed handling");
        };
        assert_eq!(
            lifecycle_error.code,
            "github.confirmed_lifecycle_requires_typed_handling"
        );

        let Err(target_error) = legacy_target_issue_outcome(target_outcome_for_test(
            ClassifiedTargetIssueTerminal::Confirmed {
                state: ConfirmedRemoteState::SourceDeleted,
                repo: "owner/repo".to_string(),
                issue_number: 42,
                lifecycle: TargetIssueLifecycle {
                    status: "deleted".to_string(),
                    reason: Some("deleted".to_string()),
                    http_status: Some(404),
                    alias_chain: Vec::new(),
                },
            },
        )) else {
            panic!("confirmed target must require typed handling");
        };
        assert_eq!(
            target_error.code,
            "github.confirmed_lifecycle_requires_typed_handling"
        );

        let Err(lifecycle_error) = legacy_lifecycle_check(ClassifiedLifecycleCheck::Confirmed {
            state: ConfirmedRemoteState::RepositoryPermissionLoss,
            http_status: 404,
        }) else {
            panic!("repository loss must require typed handling");
        };
        assert_eq!(
            lifecycle_error.code,
            "github.confirmed_lifecycle_requires_typed_handling"
        );

        let Err(target_error) = legacy_target_issue_outcome(target_outcome_for_test(
            ClassifiedTargetIssueTerminal::Confirmed {
                state: ConfirmedRemoteState::RepositoryPermissionLoss,
                repo: "owner/repo".to_string(),
                issue_number: 42,
                lifecycle: TargetIssueLifecycle {
                    status: "permission_loss".to_string(),
                    reason: Some("permission_loss".to_string()),
                    http_status: Some(404),
                    alias_chain: Vec::new(),
                },
            },
        )) else {
            panic!("repository loss must not become source unavailable");
        };
        assert_eq!(
            target_error.code,
            "github.confirmed_lifecycle_requires_typed_handling"
        );
    }

    #[test]
    fn full_and_backfill_legacy_wrappers_reject_confirmed_repo_evidence() {
        let confirmed = ConfirmedRepositoryPermissionLoss {
            repo: "owner/repo".to_string(),
            http_status: 404,
        };
        let Err(fetched) = legacy_fetch_outcome(ClassifiedFetchOutcome {
            result: FetchResult {
                issues: 0,
                comments: 0,
                skipped_pull_requests: 0,
                confirmed_permission_lost_repos: vec![confirmed.clone()],
                confirmed_source_deletions: Vec::new(),
            },
            interruption: None,
            terminal_error: Some(invalid_issue_response()),
        }) else {
            panic!("confirmed full-fetch evidence must require typed handling");
        };
        assert_eq!(
            fetched.code,
            "github.confirmed_lifecycle_requires_typed_handling"
        );

        let Err(backfill_error) = legacy_backfill_outcome(BackfillOutcome {
            issues: 0,
            comments: 0,
            skipped_pull_requests: 0,
            confirmed_permission_lost_repos: vec![confirmed.clone()],
            confirmed_source_deletions: Vec::new(),
            all_reached_end: false,
            backoff: None,
            interruption: Some(LifecycleInterruption::Transient(
                GitHubTransientKind::Server,
            )),
            terminal_error: None,
        }) else {
            panic!("confirmed backfill evidence must require typed handling");
        };
        assert_eq!(
            backfill_error.code,
            "github.confirmed_lifecycle_requires_typed_handling"
        );

        let Err(repo_comments_error) = legacy_repo_comments_outcome(RepoCommentsResult {
            comments: Vec::new(),
            cursor_updates: Vec::new(),
            skipped_pr_comments: 0,
            deferred_comments: 0,
            backoff: None,
            confirmed_permission_lost_repos: vec![confirmed],
            interruption: Some(LifecycleInterruption::Transient(
                GitHubTransientKind::Server,
            )),
            terminal_error: None,
        }) else {
            panic!("confirmed repo-comments evidence must require typed handling");
        };
        assert_eq!(
            repo_comments_error.code,
            "github.confirmed_lifecycle_requires_typed_handling"
        );

        let Err(target_error) = legacy_target_issue_outcome(ClassifiedTargetIssueFetchOutcome {
            confirmed_transitions: vec![ConfirmedIssueTransition {
                source_repo: "owner/a".to_string(),
                source_issue_number: 1,
                target_repo: "owner/b".to_string(),
                target_issue_number: 2,
                state: ConfirmedRemoteState::SourceTransferred,
                http_status: 301,
            }],
            terminal: ClassifiedTargetIssueTerminal::Failed(invalid_issue_response()),
        }) else {
            panic!("confirmed target transition must require typed handling");
        };
        assert_eq!(
            target_error.code,
            "github.confirmed_lifecycle_requires_typed_handling"
        );
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
        assert!(outcome.interruption.is_none());
        assert!(outcome.terminal_error.is_none());
        let fetched = outcome.result;
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
        assert!(outcome.interruption.is_none());
        assert!(outcome.terminal_error.is_none());
        let fetched = outcome.result;
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
    async fn full_fetch_stops_before_an_unrelated_repo_after_confirmation() {
        let (base_url, request_count, handle) = spawn_owned_responses(vec![
            NOT_FOUND_RESPONSE.to_string(),
            NOT_FOUND_RESPONSE.to_string(),
        ]);
        let profile = two_repo_profile(&base_url);
        let mut commit = |_page: FetchPage| Ok(());

        let outcome =
            fetch_issues_classified(&profile, "test-token", &[], false, None, &mut commit)
                .await
                .unwrap();

        handle.join().unwrap();
        assert_eq!(request_count.load(Ordering::SeqCst), 2);
        assert!(outcome.interruption.is_none());
        assert!(outcome.terminal_error.is_none());
        assert_eq!(outcome.result.confirmed_permission_lost_repos.len(), 1);
        assert_eq!(
            outcome.result.confirmed_permission_lost_repos[0].repo,
            "owner/a"
        );
    }

    #[tokio::test]
    async fn full_fetch_stops_before_an_unrelated_repo_after_source_deletion() {
        const ISSUE_PAGE: &str = r#"[{"id":1,"node_id":"I_A","number":1,"title":"Public title","body":"Public body","state":"open","labels":[],"milestone":null,"assignees":[],"user":{"login":"alice"},"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-02T00:00:00Z","closed_at":null,"html_url":"https://example.test/owner/a/issues/1"}]"#;
        let responses = vec![
            json_response("200 OK", "", ISSUE_PAGE),
            json_response("404 Not Found", "", "{}"),
            json_response("200 OK", "", "{}"),
        ];
        let (base_url, request_count, handle) = spawn_owned_responses(responses);
        let profile = two_repo_profile(&base_url);
        let mut commit = |_page: FetchPage| Ok(());

        let outcome = fetch_issues_classified(&profile, "test-token", &[], true, None, &mut commit)
            .await
            .unwrap();

        handle.join().unwrap();
        assert_eq!(request_count.load(Ordering::SeqCst), 3);
        assert!(outcome.interruption.is_none());
        assert!(outcome.terminal_error.is_none());
        assert_eq!(outcome.result.confirmed_source_deletions.len(), 1);
        assert_eq!(
            outcome.result.confirmed_source_deletions[0].source_id,
            "qgh://example.test/issue/I_A"
        );
    }

    #[tokio::test]
    async fn full_fetch_confirmation_short_circuits_page_commit() {
        let (base_url, request_count, handle) = spawn_owned_responses(vec![
            NOT_FOUND_RESPONSE.to_string(),
            NOT_FOUND_RESPONSE.to_string(),
        ]);
        let profile = two_repo_profile(&base_url);
        let mut commit_calls = 0usize;
        let mut commit = |_page: FetchPage| {
            commit_calls += 1;
            Ok(())
        };
        let outcome =
            fetch_issues_classified(&profile, "test-token", &[], false, None, &mut commit)
                .await
                .unwrap();
        handle.join().unwrap();
        assert_eq!(request_count.load(Ordering::SeqCst), 2);
        assert_eq!(commit_calls, 0);
        assert_eq!(outcome.result.confirmed_permission_lost_repos.len(), 1);
        assert!(outcome.interruption.is_none());
        assert!(outcome.terminal_error.is_none());
    }

    #[tokio::test]
    async fn full_fetch_commits_lifecycle_before_continuing_other_repositories() {
        let responses = vec![
            NOT_FOUND_RESPONSE.to_string(),
            NOT_FOUND_RESPONSE.to_string(),
            json_response("200 OK", "", "[]"),
        ];
        let (base_url, request_count, handle) = spawn_owned_responses(responses);
        let profile = two_repo_profile(&base_url);
        let lifecycle_committed = std::cell::Cell::new(false);
        let mut commit_lifecycle = |evidence: &ConfirmedFetchLifecycle| {
            assert!(matches!(
                evidence,
                ConfirmedFetchLifecycle::RepositoryPermissionLoss(confirmed)
                    if confirmed.repo == "owner/a"
            ));
            lifecycle_committed.set(true);
            Ok(())
        };
        let mut commit_page = |_page: FetchPage| {
            assert!(
                lifecycle_committed.get(),
                "the next repository page must follow the durable lifecycle callback"
            );
            Ok(())
        };

        let outcome = fetch_issues_classified_with_lifecycle_commit(
            &profile,
            "test-token",
            &[],
            false,
            None,
            &mut commit_page,
            &mut commit_lifecycle,
        )
        .await
        .unwrap();

        handle.join().unwrap();
        assert!(lifecycle_committed.get());
        assert_eq!(request_count.load(Ordering::SeqCst), 3);
        assert_eq!(outcome.result.confirmed_permission_lost_repos.len(), 1);
        assert!(outcome.terminal_error.is_none());
        assert!(outcome.interruption.is_none());
    }

    #[tokio::test]
    async fn classified_bulk_paths_stop_immediately_after_confirmation() {
        let responses = vec![
            NOT_FOUND_RESPONSE.to_string(),
            NOT_FOUND_RESPONSE.to_string(),
        ];
        let (base_url, _, handle) = spawn_owned_responses(responses);
        let profile = two_repo_profile(&base_url);
        let mut commit = |_page: FetchPage| Ok(());
        let backfill = fetch_backfill_issues_classified(
            &profile,
            "test-token",
            &[],
            None,
            None,
            None,
            &mut commit,
        )
        .await
        .unwrap();
        handle.join().unwrap();
        assert_eq!(backfill.confirmed_permission_lost_repos.len(), 1);
        assert!(backfill.terminal_error.is_none());
        assert!(backfill.interruption.is_none());

        let responses = vec![
            NOT_FOUND_RESPONSE.to_string(),
            NOT_FOUND_RESPONSE.to_string(),
        ];
        let (base_url, _, handle) = spawn_owned_responses(responses);
        let profile = two_repo_profile(&base_url);
        let repo_comments =
            fetch_repo_comments_classified(&profile, "test-token", &[], 0, &|_, _| None, None)
                .await
                .unwrap();
        handle.join().unwrap();
        assert_eq!(repo_comments.confirmed_permission_lost_repos.len(), 1);
        assert!(repo_comments.terminal_error.is_none());
        assert!(repo_comments.interruption.is_none());

        let (base_url, _, handle) = spawn_owned_responses(vec![
            NOT_FOUND_RESPONSE.to_string(),
            OK_RESPONSE.to_string(),
        ]);
        let profile = two_repo_profile(&base_url);
        let candidates = vec![
            ReconciliationCandidate {
                source_id: "qgh://example.test/issue/I_A".to_string(),
                entity_type: "issue".to_string(),
                repo: "owner/a".to_string(),
                issue_number: 1,
                github_id: 1,
            },
            ReconciliationCandidate {
                source_id: "qgh://example.test/unsupported/X".to_string(),
                entity_type: "unsupported".to_string(),
                repo: "owner/b".to_string(),
                issue_number: 2,
                github_id: 2,
            },
        ];
        let reconciliation = reconcile_sources(&profile, "test-token", &candidates, None)
            .await
            .unwrap();
        handle.join().unwrap();
        assert_eq!(reconciliation.unavailable_sources.len(), 1);
        assert!(reconciliation.terminal_error.is_none());
        assert!(reconciliation.interruption.is_none());
    }

    #[tokio::test]
    async fn temporary_or_invalid_redirect_never_confirms_transfer() {
        let responses = [
            "HTTP/1.1 302 Found\r\nlocation: /repos/owner/repo/issues/99\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
            "HTTP/1.1 307 Temporary Redirect\r\nlocation: /repos/owner/repo/issues/99\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
            "HTTP/1.1 301 Moved Permanently\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
            "HTTP/1.1 301 Moved Permanently\r\nlocation: /repos/owner/repo/issues/42\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
            "HTTP/1.1 301 Moved Permanently\r\nlocation: /repos/OWNER/REPO/issues/42\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
            "HTTP/1.1 308 Permanent Redirect\r\nlocation: /repos/owner/repo/issues/99\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
            "HTTP/1.1 308 Permanent Redirect\r\nlocation: not a url\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
            "HTTP/1.1 301 Moved Permanently\r\nlocation: /repos/owner/repo/pulls/99\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
            "HTTP/1.1 308 Permanent Redirect\r\nlocation: https://other.example/repos/owner/repo/issues/99\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
        ];
        for response in responses {
            let (base_url, request_count, handle) = spawn_responses(vec![response]);
            let profile = test_profile(&base_url);

            let outcome =
                fetch_target_issue_classified(&profile, "test-token", &test_repo(), 42, None)
                    .await
                    .unwrap();

            handle.join().unwrap();
            assert!(matches!(
                outcome.terminal,
                ClassifiedTargetIssueTerminal::Transient(GitHubTransientKind::UnexpectedResponse)
            ));
            assert_eq!(request_count.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn followed_permanent_transfer_carries_typed_transition() {
        const REDIRECT: &str = "HTTP/1.1 301 Moved Permanently\r\nlocation: /repos/owner/repo/issues/99\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}";
        let issue = r#"{"id":99,"node_id":"I_MOVED","number":99,"title":"Moved","body":"Public","state":"open","labels":[],"milestone":null,"assignees":[],"user":{"login":"alice"},"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-02T00:00:00Z","closed_at":null,"html_url":"https://example.test/owner/repo/issues/99"}"#;
        let responses = vec![
            REDIRECT.to_string(),
            json_response("200 OK", "", issue),
            json_response("200 OK", "", "[]"),
        ];
        let (base_url, request_count, handle) = spawn_owned_responses(responses);
        let profile = test_profile(&base_url);

        let outcome = fetch_target_issue_classified(&profile, "test-token", &test_repo(), 42, None)
            .await
            .unwrap();

        handle.join().unwrap();
        assert_eq!(outcome.confirmed_transitions.len(), 1);
        let ClassifiedTargetIssueTerminal::Fetched(fetched) = outcome.terminal else {
            panic!("permanent in-scope transfer should be followed");
        };
        assert_eq!(
            fetched.confirmed_transition,
            Some(ConfirmedRemoteState::SourceTransferred)
        );
        assert_eq!(fetched.lifecycle.status, "transferred");
        assert_eq!(fetched.lifecycle.http_status, Some(301));
        assert_eq!(request_count.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn permanent_transfer_commits_purge_before_following_target() {
        const REDIRECT: &str = "HTTP/1.1 301 Moved Permanently\r\nlocation: /repos/owner/repo/issues/99\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}";
        let (base_url, request_count, handle) = spawn_responses(vec![REDIRECT]);
        let profile = test_profile(&base_url);
        let mut committed = Vec::new();
        let mut commit = |transition: &ConfirmedIssueTransition| {
            committed.push(transition.clone());
            Err(QghError::new(
                "purge.fixture_commit_failed",
                "sensitive fixture detail must be redacted",
                6,
            ))
        };

        let outcome = fetch_target_issue_classified_with_transition_commit(
            &profile,
            "test-token",
            &test_repo(),
            42,
            None,
            &mut commit,
        )
        .await
        .unwrap();

        handle.join().unwrap();
        assert_eq!(request_count.load(Ordering::SeqCst), 1);
        assert_eq!(committed.len(), 1);
        assert_eq!(outcome.confirmed_transitions, committed);
        let ClassifiedTargetIssueTerminal::Failed(error) = outcome.terminal else {
            panic!("failed purge commit must stop before the transfer target request");
        };
        assert_eq!(error.code, "purge.fixture_commit_failed");
        assert_eq!(error.message, "Local fetch checkpoint commit failed.");
        assert!(!error.message.contains("sensitive fixture detail"));
    }

    #[tokio::test]
    async fn followed_transfer_retains_transition_across_parse_and_validation_failures() {
        let redirect = "HTTP/1.1 301 Moved Permanently\r\nlocation: /repos/owner/b/issues/2\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}".to_string();
        let issue = r#"{"id":2,"node_id":"I_B","number":2,"title":"Moved","body":"Public","state":"open","labels":[],"milestone":null,"assignees":[],"user":{"login":"alice"},"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-02T00:00:00Z","closed_at":null,"html_url":"https://example.test/owner/b/issues/2"}"#;
        let pull_request = issue.replace("\"html_url\"", "\"pull_request\":{},\"html_url\"");
        let cases = vec![
            (
                vec![redirect.clone(), json_response("200 OK", "", "{}")],
                "github.invalid_issue_json",
            ),
            (
                vec![
                    redirect.clone(),
                    json_response("200 OK", "", issue),
                    json_response("200 OK", "", "{}"),
                ],
                "github.invalid_comment_json",
            ),
            (
                vec![redirect, json_response("200 OK", "", &pull_request)],
                "validation.unsupported_source_type",
            ),
        ];

        for (responses, expected_code) in cases {
            let (base_url, _, handle) = spawn_owned_responses(responses);
            let profile = two_repo_profile(&base_url);
            let outcome = fetch_target_issue_classified(&profile, "test-token", &repo_a(), 1, None)
                .await
                .unwrap();
            handle.join().unwrap();
            assert_eq!(outcome.confirmed_transitions.len(), 1);
            let ClassifiedTargetIssueTerminal::Failed(error) = outcome.terminal else {
                panic!("post-transfer failure must remain typed");
            };
            assert_eq!(error.code, expected_code);
            assert!(error
                .details
                .as_object()
                .is_some_and(serde_json::Map::is_empty));
        }
    }

    #[tokio::test]
    async fn followed_transfer_retains_all_transitions_on_cycle_and_limit_failure() {
        let responses = vec![
            "HTTP/1.1 301 Moved Permanently\r\nlocation: /repos/owner/b/issues/2\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}".to_string(),
            "HTTP/1.1 301 Moved Permanently\r\nlocation: /repos/owner/a/issues/1\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}".to_string(),
        ];
        let (base_url, request_count, handle) = spawn_owned_responses(responses);
        let profile = two_repo_profile(&base_url);
        let outcome = fetch_target_issue_classified(&profile, "test-token", &repo_a(), 1, None)
            .await
            .unwrap();
        handle.join().unwrap();
        assert_eq!(request_count.load(Ordering::SeqCst), 2);
        assert_eq!(outcome.confirmed_transitions.len(), 2);
        let ClassifiedTargetIssueTerminal::Failed(error) = outcome.terminal else {
            panic!("transfer cycle must remain typed");
        };
        assert_eq!(error.code, "sync.transfer_cycle");
        assert_eq!(error.details["repo"], "owner/a");
        assert_eq!(error.details["issue_number"], 1);
        assert_eq!(error.details["alias_chain"].as_array().unwrap().len(), 2);

        let repos = (0..=8)
            .map(|index| RepoRef {
                owner: "owner".to_string(),
                name: format!("r{index}"),
            })
            .collect::<Vec<_>>();
        let start_repo = repos[0].clone();
        let responses = (1..=8)
            .map(|index| {
                format!(
                    "HTTP/1.1 301 Moved Permanently\r\nlocation: /repos/owner/r{index}/issues/{index}\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{{}}"
                )
            })
            .collect::<Vec<_>>();
        let (base_url, request_count, handle) = spawn_owned_responses(responses);
        let profile = test_profile_with_repos(&base_url, repos);
        let outcome = fetch_target_issue_classified(&profile, "test-token", &start_repo, 1, None)
            .await
            .unwrap();
        handle.join().unwrap();
        assert_eq!(request_count.load(Ordering::SeqCst), 8);
        assert_eq!(outcome.confirmed_transitions.len(), 8);
        let ClassifiedTargetIssueTerminal::Failed(error) = outcome.terminal else {
            panic!("transfer follow limit must remain typed");
        };
        assert_eq!(error.code, "sync.transfer_chain_too_long");
        assert_eq!(error.details["repo"], "owner/r0");
        assert_eq!(error.details["issue_number"], 1);
        assert_eq!(error.details["alias_chain"].as_array().unwrap().len(), 8);
        assert_eq!(error.details["max_redirects"], 8);
    }

    #[tokio::test]
    async fn followed_transfer_preserves_transition_with_final_delete_or_repo_loss() {
        const REDIRECT: &str = "HTTP/1.1 301 Moved Permanently\r\nlocation: /repos/owner/b/issues/2\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}";
        for (confirmation, expected_state) in [
            (OK_RESPONSE, ConfirmedRemoteState::SourceDeleted),
            (
                NOT_FOUND_RESPONSE,
                ConfirmedRemoteState::RepositoryPermissionLoss,
            ),
        ] {
            let (base_url, _, handle) =
                spawn_responses(vec![REDIRECT, NOT_FOUND_RESPONSE, confirmation]);
            let profile = two_repo_profile(&base_url);

            let outcome = fetch_target_issue_classified(&profile, "test-token", &repo_a(), 1, None)
                .await
                .unwrap();

            handle.join().unwrap();
            assert_eq!(outcome.confirmed_transitions.len(), 1);
            assert_eq!(
                outcome.confirmed_transitions[0],
                ConfirmedIssueTransition {
                    source_repo: "owner/a".to_string(),
                    source_issue_number: 1,
                    target_repo: "owner/b".to_string(),
                    target_issue_number: 2,
                    state: ConfirmedRemoteState::SourceTransferred,
                    http_status: 301,
                }
            );
            let ClassifiedTargetIssueTerminal::Confirmed { state, repo, .. } = outcome.terminal
            else {
                panic!("final denial must stay typed");
            };
            assert_eq!(state, expected_state);
            assert_eq!(repo, "owner/b");
        }
    }

    #[tokio::test]
    async fn followed_transfer_preserves_transition_with_comment_repo_loss() {
        const REDIRECT: &str = "HTTP/1.1 301 Moved Permanently\r\nlocation: /repos/owner/b/issues/2\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}";
        let issue = r#"{"id":2,"node_id":"I_B","number":2,"title":"Moved","body":"Public","state":"open","labels":[],"milestone":null,"assignees":[],"user":{"login":"alice"},"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-02T00:00:00Z","closed_at":null,"html_url":"https://example.test/owner/b/issues/2"}"#;
        let responses = vec![
            REDIRECT.to_string(),
            json_response("200 OK", "", issue),
            json_response("404 Not Found", "", "{}"),
            json_response("404 Not Found", "", "{}"),
        ];
        let (base_url, _, handle) = spawn_owned_responses(responses);
        let profile = two_repo_profile(&base_url);

        let outcome = fetch_target_issue_classified(&profile, "test-token", &repo_a(), 1, None)
            .await
            .unwrap();

        handle.join().unwrap();
        assert_eq!(outcome.confirmed_transitions.len(), 1);
        assert_eq!(outcome.confirmed_transitions[0].source_repo, "owner/a");
        assert_eq!(outcome.confirmed_transitions[0].target_repo, "owner/b");
        assert!(matches!(
            outcome.terminal,
            ClassifiedTargetIssueTerminal::Confirmed {
                state: ConfirmedRemoteState::RepositoryPermissionLoss,
                ref repo,
                ..
            } if repo == "owner/b"
        ));
    }

    #[tokio::test]
    async fn followed_transfer_preserves_transition_before_timeout_or_backoff() {
        let (base_url, request_count, handle) = spawn_redirect_then_timeout();
        let profile = two_repo_profile(&base_url);
        let client = lifecycle_client_with_timeout(StdDuration::from_millis(10)).unwrap();

        let timeout = fetch_target_issue_classified_with_client(
            &client,
            &profile,
            "test-token",
            &repo_a(),
            1,
            None,
        )
        .await
        .unwrap();

        handle.join().unwrap();
        assert_eq!(request_count.load(Ordering::SeqCst), 2);
        assert_eq!(timeout.confirmed_transitions.len(), 1);
        assert!(matches!(
            timeout.terminal,
            ClassifiedTargetIssueTerminal::Transient(GitHubTransientKind::Timeout)
        ));

        let responses = vec![
            "HTTP/1.1 301 Moved Permanently\r\nlocation: /repos/owner/b/issues/2\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}".to_string(),
            "HTTP/1.1 301 Moved Permanently\r\nlocation: /repos/owner/b/issues/3\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}".to_string(),
            json_response("429 Too Many Requests", "retry-after: 0\r\n", "{}"),
        ];
        let (base_url, _, handle) = spawn_owned_responses(responses);
        let profile = two_repo_profile(&base_url);
        let backoff = fetch_target_issue_classified(&profile, "test-token", &repo_a(), 1, None)
            .await
            .unwrap();
        handle.join().unwrap();
        assert_eq!(backoff.confirmed_transitions.len(), 2);
        assert_eq!(backoff.confirmed_transitions[0].source_issue_number, 1);
        assert_eq!(backoff.confirmed_transitions[0].target_issue_number, 2);
        assert_eq!(backoff.confirmed_transitions[1].source_issue_number, 2);
        assert_eq!(backoff.confirmed_transitions[1].target_issue_number, 3);
        assert!(matches!(
            backoff.terminal,
            ClassifiedTargetIssueTerminal::Backoff(_)
        ));
    }

    #[tokio::test]
    async fn direct_active_fetch_has_no_confirmed_transition() {
        let issue = r#"{"id":42,"node_id":"I_ACTIVE","number":42,"title":"Active","body":"Public","state":"open","labels":[],"milestone":null,"assignees":[],"user":{"login":"alice"},"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-02T00:00:00Z","closed_at":null,"html_url":"https://example.test/owner/repo/issues/42"}"#;
        let (base_url, _, handle) = spawn_owned_responses(vec![
            json_response("200 OK", "", issue),
            json_response("200 OK", "", "[]"),
        ]);
        let profile = test_profile(&base_url);

        let outcome = fetch_target_issue_classified(&profile, "test-token", &test_repo(), 42, None)
            .await
            .unwrap();

        handle.join().unwrap();
        assert!(outcome.confirmed_transitions.is_empty());
        let ClassifiedTargetIssueTerminal::Fetched(fetched) = outcome.terminal else {
            panic!("active issue should be fetched");
        };
        assert_eq!(fetched.confirmed_transition, None);
        assert_eq!(fetched.lifecycle.status, "active");
    }

    #[tokio::test]
    async fn reconciliation_requires_permanent_canonical_issue_redirect() {
        let candidate = ReconciliationCandidate {
            source_id: "qgh://example.test/issue/I_REDIRECT".to_string(),
            entity_type: "issue".to_string(),
            repo: "owner/repo".to_string(),
            issue_number: 42,
            github_id: 42,
        };
        for response in [
            "HTTP/1.1 302 Found\r\nlocation: /repos/owner/repo/issues/99\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
            "HTTP/1.1 308 Permanent Redirect\r\nlocation: /repos/owner/repo/issues/99\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
            "HTTP/1.1 301 Moved Permanently\r\nlocation: /repos/owner/repo/issues/42\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
            "HTTP/1.1 301 Moved Permanently\r\nlocation: /repos/OWNER/REPO/issues/42\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
            "HTTP/1.1 301 Moved Permanently\r\nlocation: https://other.example/repos/owner/repo/issues/99\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
        ] {
            let (base_url, _, handle) = spawn_responses(vec![response]);
            let profile = test_profile(&base_url);
            let outcome = check_source_lifecycle_classified(
                &profile,
                "test-token",
                &candidate,
                None,
            )
            .await
            .unwrap();
            handle.join().unwrap();
            assert_eq!(
                outcome,
                ClassifiedLifecycleCheck::Transient(
                    GitHubTransientKind::UnexpectedResponse
                )
            );
        }

        let comment_candidate = ReconciliationCandidate {
            source_id: "qgh://example.test/issue-comment/IC_REDIRECT".to_string(),
            entity_type: "issue_comment".to_string(),
            repo: "owner/repo".to_string(),
            issue_number: 42,
            github_id: 420,
        };
        let (base_url, _, handle) = spawn_responses(vec![
            "HTTP/1.1 301 Moved Permanently\r\nlocation: /repos/owner/repo/issues/99\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
        ]);
        let profile = test_profile(&base_url);
        let comment_outcome =
            check_source_lifecycle_classified(&profile, "test-token", &comment_candidate, None)
                .await
                .unwrap();
        handle.join().unwrap();
        assert_eq!(
            comment_outcome,
            ClassifiedLifecycleCheck::Transient(GitHubTransientKind::UnexpectedResponse)
        );

        let (base_url, _, handle) = spawn_responses(vec![
            "HTTP/1.1 301 Moved Permanently\r\nlocation: /repos/owner/repo/issues/99\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
        ]);
        let profile = test_profile(&base_url);
        let outcome = check_source_lifecycle_classified(&profile, "test-token", &candidate, None)
            .await
            .unwrap();
        handle.join().unwrap();
        assert_eq!(
            outcome,
            ClassifiedLifecycleCheck::Confirmed {
                state: ConfirmedRemoteState::SourceTransferred,
                http_status: 301,
            }
        );
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
        assert_eq!(outcome.confirmed_transitions.len(), 1);
        let ClassifiedTargetIssueTerminal::Confirmed {
            state,
            repo,
            issue_number,
            lifecycle,
        } = outcome.terminal
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
