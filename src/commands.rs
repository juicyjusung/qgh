use crate::chunking::chunk_markdown;
use crate::cli::{EmbedArgs, InitArgs, InitRepoArgs, InitTokenSourceArg, QueryArgs, ReconcileMode};
use crate::config::{
    bootstrap_profile_repo, current_git_worktree_root, discover_repo_policy,
    git_remote_defaults_for_root, load_profile, load_repo_policy_at, parse_repo, resolve_token,
    CommentsMode, EmbeddingConfig, EmbeddingProviderKind, GitRemote, Profile,
    ProfileBootstrapInput, RepoPolicy, RepoRef, TokenSource,
};
use crate::coverage;
#[cfg(feature = "fastembed-provider")]
use crate::embedding::FastembedTokenizer;
use crate::embedding::{
    default_hf_model_reference, parse_hf_model_reference, EmbeddingFingerprintExpectation,
    EmbeddingFingerprintSeed, EmbeddingProvider, EmbeddingProviderError, EmbeddingTokenizer,
    EmbeddingVector, LOCAL_MODEL_REVISION,
};
#[cfg(feature = "fastembed-provider")]
use crate::embedding::{
    resolve_fastembed_snapshot, FastembedEngine, LocalEmbeddingProvider, ResolvedModelSnapshot,
};
use crate::error::QghError;
use crate::freshness::{self, FreshnessContext, FreshnessOverrides};
use crate::github;
use crate::index;
use crate::model::{
    ReconciliationCandidate, StoredComment, StoredIssue, StoredSource, SyncSummary,
    TargetedSyncSummary,
};
use crate::paths::ProfilePaths;
use crate::resolution::ResolvedRepoScope;
use crate::store::Store;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

const GET_BATCH_SIZE_CAP: usize = 20;

/// Default `--if-stale` threshold when neither the flag nor `[sync].max_age`
/// provides one: 30 minutes.
const DEFAULT_SYNC_MAX_AGE_SECONDS: i64 = 30 * 60;

/// Default `--reconcile recent` window when neither `--window` nor
/// `[profile].reconcile_after` provides one: 7 days.
const DEFAULT_RECONCILE_WINDOW_SECONDS: i64 = 7 * 24 * 60 * 60;

pub struct InitCommandOutcome {
    pub data: Value,
    pub warnings: Vec<Value>,
    pub meta: Value,
}

pub struct LocalReadOutcome {
    pub data: Value,
    pub warnings: Vec<Value>,
}

fn local_read_outcome(data: Value, warnings: Vec<Value>) -> LocalReadOutcome {
    LocalReadOutcome { data, warnings }
}

#[allow(clippy::too_many_arguments)]
pub async fn sync(
    profile_id: &str,
    reconcile: Option<ReconcileMode>,
    window: Option<&str>,
    if_stale: bool,
    max_age: Option<&str>,
    backfill: bool,
    max_requests: Option<usize>,
    max_duration: Option<&str>,
    repo_scope: Option<&ResolvedRepoScope>,
    show_progress: bool,
) -> Result<LocalReadOutcome, QghError> {
    let progress = StderrSyncProgress::new(show_progress);
    progress.line(format_args!(
        "qgh sync: loading profile profile={profile_id}"
    ));
    let profile = load_profile(profile_id)?;
    let token = resolve_token(&profile)?;
    let mut store = Store::open(&profile.paths)?;

    if window.is_some() && reconcile != Some(ReconcileMode::Recent) {
        return Err(QghError::validation(
            "validation.window_requires_recent",
            "--window is only valid with --reconcile recent.",
        ));
    }
    if backfill && (reconcile.is_some() || window.is_some() || if_stale) {
        return Err(QghError::validation(
            "validation.backfill_conflicts",
            "--backfill cannot be combined with --reconcile, --window, or --if-stale.",
        ));
    }
    if !backfill && (max_requests.is_some() || max_duration.is_some()) {
        return Err(QghError::validation(
            "validation.requires_backfill",
            "--max-requests and --max-duration require --backfill.",
        ));
    }

    // `--if-stale`: skip the network sync entirely when the local snapshot is
    // still within max-age. Never-synced always proceeds.
    if if_stale {
        let max_age_seconds = match max_age {
            Some(value) => freshness::parse_duration_seconds("max_age", value)?,
            None => profile
                .sync_max_age_seconds
                .unwrap_or(DEFAULT_SYNC_MAX_AGE_SECONDS),
        };
        let last_sync = store.status()?.last_sync_at;
        if let Some(last_sync_at) = last_sync.as_deref() {
            let snapshot_age_seconds = freshness::snapshot_age_seconds(last_sync_at)?;
            if snapshot_age_seconds <= max_age_seconds {
                progress.line(format_args!(
                    "qgh sync: skipped, snapshot fresh age={snapshot_age_seconds}s max_age={max_age_seconds}s"
                ));
                let warnings =
                    refresh_embedding_for_sync_if_enabled(&profile, &mut store, &progress);
                return Ok(local_read_outcome(
                    json!({
                        "profile_id": profile.id,
                        "sync_state": "skipped_fresh",
                        "sync": {
                            "last_successful_sync": last_sync,
                            "snapshot_age_seconds": snapshot_age_seconds,
                            "max_age_seconds": max_age_seconds
                        }
                    }),
                    warnings,
                ));
            }
        }
    }

    let cursors = store.sync_cursors()?;
    let per_issue_comments = profile.comments_mode == CommentsMode::PerIssue;
    let fetch_profile = profile_scoped_to_repo(&profile, repo_scope)?;

    if backfill {
        return backfill_sync(
            &profile,
            &fetch_profile,
            &token,
            &mut store,
            max_requests,
            max_duration,
            &progress,
        )
        .await;
    }

    progress.line(format_args!(
        "qgh sync: fetching GitHub issues/comments repos={}",
        fetch_profile.repos.len()
    ));
    let sync_run_id = Store::new_sync_run_id();
    let mut summary = None;
    let fetched = {
        let mut commit_page = |page: github::FetchPage| -> Result<(), QghError> {
            let page_summary = store.upsert_sources_for_run(
                &sync_run_id,
                &page.issues,
                &page.comments,
                page.skipped_pull_requests,
                &page.cursor_updates,
            )?;
            merge_sync_summary(&mut summary, page_summary);
            Ok(())
        };
        github::fetch_issues(
            &fetch_profile,
            &token,
            &cursors,
            per_issue_comments,
            Some(&progress),
            &mut commit_page,
        )
        .await?
    };
    let fetched = match fetched {
        github::FetchOutcome::Fetched(fetched) => fetched,
        github::FetchOutcome::Backoff(backoff) => {
            progress.line(format_args!(
                "qgh sync: backoff reason={} scope={} retry_after_seconds={}",
                backoff.reason, backoff.scope, backoff.retry_after_seconds
            ));
            let backoff = store.record_backoff_state(
                &backoff.reason,
                &backoff.scope,
                backoff.retry_after_seconds,
                backoff.reset_at.as_deref(),
            )?;
            let status = store.status()?;
            return Ok(local_read_outcome(
                json!({
                    "profile_id": profile.id,
                    "sync_state": "backoff",
                    "backoff": backoff,
                    "sync": {
                        "last_successful_sync": status.last_sync_at,
                        "scheduler": {
                            "max_in_flight_requests": profile.max_in_flight_requests,
                            "hard_cap": 16
                        }
                    },
                    "sources": {
                        "issue_count": status.issue_count,
                        "comment_count": status.comment_count,
                        "tombstone_count": status.tombstone_count
                    },
                    "index": {
                        "active_generation": status.active_generation,
                        "dirty_task_count": status.dirty_task_count
                    }
                }),
                Vec::new(),
            ));
        }
    };
    progress.line(format_args!(
        "qgh sync: fetched issues={} comments={} skipped_pull_requests={}",
        fetched.issues, fetched.comments, fetched.skipped_pull_requests
    ));

    // Repo-level comment listing (opt-in). Fetch fresh comments repo-wide, then
    // upsert the ones whose parent resolves locally; PR-parent comments are
    // skipped and unresolved parents deferred as a coverage gap.
    let mut repo_comment_stats = None;
    if profile.comments_mode == CommentsMode::RepoListing {
        progress.line(format_args!("qgh sync: repo-level comment listing"));
        let comment_cursors = store.sync_cursors()?;
        let budget = profile.comment_parent_resolution_budget;
        let outcome = {
            let resolve = |repo_name: &str, number: i64| -> Option<github::CommentParent> {
                store
                    .find_issue_by_repo_number(repo_name, number)
                    .ok()
                    .flatten()
                    .map(|issue| github::CommentParent {
                        source_id: issue.source_id,
                        number: issue.number,
                        title: issue.title,
                        canonical_url: issue.canonical_url,
                    })
            };
            github::fetch_repo_comments(
                &fetch_profile,
                &token,
                &comment_cursors,
                budget,
                &resolve,
                Some(&progress),
            )
            .await?
        };
        // Commit whatever was fetched (possibly partial) before handling backoff,
        // so progress is never discarded under rate limiting.
        let page_summary = store.upsert_sources_for_run(
            &sync_run_id,
            &[],
            &outcome.comments,
            0,
            &outcome.cursor_updates,
        )?;
        merge_sync_summary(&mut summary, page_summary);
        repo_comment_stats = Some((outcome.skipped_pr_comments, outcome.deferred_comments));
        if let Some(backoff) = outcome.backoff {
            progress.line(format_args!(
                "qgh sync: comment backoff reason={} scope={} retry_after_seconds={}",
                backoff.reason, backoff.scope, backoff.retry_after_seconds
            ));
            let backoff = store.record_backoff_state(
                &backoff.reason,
                &backoff.scope,
                backoff.retry_after_seconds,
                backoff.reset_at.as_deref(),
            )?;
            let status = store.status()?;
            return Ok(local_read_outcome(
                json!({
                    "profile_id": profile.id,
                    "sync_state": "backoff",
                    "backoff": backoff,
                    "sync": {
                        "last_successful_sync": status.last_sync_at,
                        "scheduler": {
                            "max_in_flight_requests": profile.max_in_flight_requests,
                            "hard_cap": 16
                        }
                    },
                    "sources": {
                        "issue_count": status.issue_count,
                        "comment_count": status.comment_count,
                        "tombstone_count": status.tombstone_count
                    },
                    "index": {
                        "active_generation": status.active_generation,
                        "dirty_task_count": status.dirty_task_count
                    }
                }),
                Vec::new(),
            ));
        }
    }

    let summary = match summary {
        Some(summary) => summary,
        None => store.upsert_sources_for_run(&sync_run_id, &[], &[], 0, &[])?,
    };
    progress.line(format_args!(
        "qgh sync: stored upserted_issues={} upserted_comments={} cursor_updates={}",
        summary.upserted_issues,
        summary.upserted_comments,
        summary.cursor_updates.len()
    ));

    // Seed corpus coverage metadata from a full-profile sync only. A repo-scoped
    // sync must not claim corpus-wide completion for repos it never touched.
    if repo_scope.is_none() {
        let mut coverage = store.coverage_snapshot()?;
        if coverage.recent_bootstrap_floor.is_none() {
            // Fixed once at first seed; never re-derived from `now`. checked_sub
            // avoids a panic on an absurdly large configured lookback.
            coverage.recent_bootstrap_floor = Utc::now()
                .checked_sub_signed(chrono::Duration::seconds(
                    profile.bootstrap.lookback_seconds,
                ))
                .map(|floor| floor.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
        }
        // Oldest reach only extends backward: tombstoning the oldest issue must
        // not make coverage report a more recent floor than was actually synced.
        if let Some(corpus_oldest) = store.oldest_active_issue_updated_at()? {
            coverage.oldest_synced_updated_at = Some(match coverage.oldest_synced_updated_at {
                Some(existing) => existing.min(corpus_oldest),
                None => corpus_oldest,
            });
        }
        // A full-profile Fetched sync paginated every repo to the end (a mid-pass
        // backoff returns early), so open issues are covered up to now.
        coverage.open_backfill_complete = true;
        if let Some(watermark) = summary
            .cursor_updates
            .iter()
            .filter(|cursor| cursor.endpoint.starts_with("issues:"))
            .filter_map(|cursor| cursor.watermark.clone())
            .max()
        {
            coverage.open_cursor = Some(watermark);
        }
        store.update_coverage(&coverage)?;
    }

    let reconciliation = match reconcile {
        Some(mode) => {
            let (mode_str, candidates) = match mode {
                ReconcileMode::Full => ("full", store.active_reconciliation_candidates()?),
                ReconcileMode::Recent => {
                    // Default to a dedicated window constant, not reconcile_after
                    // (which is the status staleness threshold, a different axis).
                    let window_seconds = match window {
                        Some(value) => freshness::parse_duration_seconds("window", value)?,
                        None => DEFAULT_RECONCILE_WINDOW_SECONDS,
                    };
                    let updated_since = Utc::now()
                        .checked_sub_signed(chrono::Duration::seconds(window_seconds))
                        .map(|floor| floor.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
                        .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string());
                    (
                        "recent",
                        store.recent_reconciliation_candidates(&updated_since)?,
                    )
                }
            };
            let candidates = reconciliation_candidates_scoped_to_repo(candidates, repo_scope);
            let estimated_api_cost_class = estimate_api_cost_class(candidates.len());
            progress.line(format_args!(
                "qgh sync: reconciling sources={} mode={mode_str}",
                candidates.len()
            ));
            let result =
                github::reconcile_sources(&fetch_profile, &token, &candidates, Some(&progress))
                    .await?;
            let mut tombstoned_sources = 0;
            for unavailable in result.unavailable_sources {
                store.tombstone_source(&unavailable.source_id, &unavailable.reason)?;
                tombstoned_sources += 1;
            }
            store.record_reconciliation_run(
                mode_str,
                result.checked_sources,
                tombstoned_sources,
                estimated_api_cost_class,
            )?;
            json!({
                "mode": mode_str,
                "checked_sources": result.checked_sources,
                "tombstoned_sources": tombstoned_sources,
                "estimated_api_cost_class": estimated_api_cost_class
            })
        }
        None => json!({ "mode": "none" }),
    };
    store.clear_backoff_state()?;
    let index = rebuild_bm25_index(&profile, &mut store, &progress)?;
    store.mark_sync_run_completed(&summary.sync_run_id)?;
    let comment_listing = match repo_comment_stats {
        Some((skipped_pr_comments, deferred_comments)) => json!({
            "mode": "repo_listing",
            "skipped_pr_comments": skipped_pr_comments,
            "deferred_comments": deferred_comments
        }),
        None => json!({ "mode": "per_issue" }),
    };
    let watermarks = summary
        .cursor_updates
        .iter()
        .map(|cursor| (cursor.endpoint.clone(), json!(cursor.watermark)))
        .collect::<serde_json::Map<_, _>>();
    progress.line(format_args!(
        "qgh sync: complete sync_run_id={}",
        summary.sync_run_id
    ));
    Ok(local_read_outcome(
        json!({
            "profile_id": profile.id,
            "sync_state": "ok",
            "sync_run_id": summary.sync_run_id,
            "scheduler": {
                "max_in_flight_requests": profile.max_in_flight_requests,
                "hard_cap": 16
            },
            "issues": {
                "fetched": summary.fetched_issues,
                "upserted": summary.upserted_issues,
                "skipped_pull_requests": summary.skipped_pull_requests
            },
            "comments": {
                "fetched": summary.fetched_comments,
                "upserted": summary.upserted_comments
            },
            "comment_listing": comment_listing,
            "cursors": {
                "updated": summary.cursor_updates.len(),
                "not_modified_endpoints": summary.not_modified_endpoints,
                "watermarks": watermarks
            },
            "index": {
                "active_generation": index.generation,
                "dirty_task_count": index.dirty_task_count
            },
            "reconciliation": reconciliation
        }),
        index.warnings,
    ))
}

async fn backfill_sync(
    profile: &Profile,
    fetch_profile: &Profile,
    token: &str,
    store: &mut Store,
    max_requests: Option<usize>,
    max_duration: Option<&str>,
    progress: &StderrSyncProgress,
) -> Result<LocalReadOutcome, QghError> {
    progress.line(format_args!("qgh sync: historical backfill"));
    let max_duration_seconds = max_duration
        .map(|value| freshness::parse_duration_seconds("max_duration", value))
        .transpose()?;
    let backfill_run_id = Store::new_sync_run_id();
    let cursors = store.sync_cursors()?;
    let mut summary = None;
    let outcome = {
        let mut commit_page = |page: github::FetchPage| -> Result<(), QghError> {
            let page_summary = store.upsert_sources_for_run(
                &backfill_run_id,
                &page.issues,
                &page.comments,
                page.skipped_pull_requests,
                &page.cursor_updates,
            )?;
            merge_sync_summary(&mut summary, page_summary);
            Ok(())
        };
        github::fetch_backfill_issues(
            fetch_profile,
            token,
            &cursors,
            max_requests,
            max_duration_seconds,
            Some(progress),
            &mut commit_page,
        )
        .await?
    };

    // History cursors are per-repo; the coverage envelope reports the
    // least-advanced (min) one, and historical coverage is complete only when
    // every repo paginated to the end this run (never on a budget/backoff cut).
    let mut coverage = store.coverage_snapshot()?;
    coverage.history_cursor = store
        .sync_cursors()?
        .into_iter()
        .filter(|cursor| cursor.endpoint.starts_with("history:"))
        .filter_map(|cursor| cursor.cursor)
        .min();
    if let Some(corpus_oldest) = store.oldest_active_issue_updated_at()? {
        coverage.oldest_synced_updated_at = Some(match coverage.oldest_synced_updated_at {
            Some(existing) => existing.min(corpus_oldest),
            None => corpus_oldest,
        });
    }
    if outcome.all_reached_end {
        coverage.historical_backfill_complete = true;
    }
    store.update_coverage(&coverage)?;

    if let Some(backoff) = outcome.backoff {
        progress.line(format_args!(
            "qgh sync: backfill backoff reason={} scope={} retry_after_seconds={}",
            backoff.reason, backoff.scope, backoff.retry_after_seconds
        ));
        let backoff = store.record_backoff_state(
            &backoff.reason,
            &backoff.scope,
            backoff.retry_after_seconds,
            backoff.reset_at.as_deref(),
        )?;
        let index = rebuild_bm25_index(profile, store, progress)?;
        let status = store.status()?;
        return Ok(local_read_outcome(
            json!({
                "profile_id": profile.id,
                "sync_state": "backoff",
                "backoff": backoff,
                "backfill": {
                    "issues": outcome.issues,
                    "comments": outcome.comments,
                    "skipped_pull_requests": outcome.skipped_pull_requests,
                    "reached_end": false,
                    "history_cursor": coverage.history_cursor,
                    "historical_backfill_complete": coverage.historical_backfill_complete
                },
                "sources": {
                    "issue_count": status.issue_count,
                    "comment_count": status.comment_count,
                    "tombstone_count": status.tombstone_count
                }
            }),
            index.warnings,
        ));
    }

    store.clear_backoff_state()?;
    let index = rebuild_bm25_index(profile, store, progress)?;
    if let Some(summary) = &summary {
        store.mark_sync_run_completed(&summary.sync_run_id)?;
    }
    Ok(local_read_outcome(
        json!({
            "profile_id": profile.id,
            "sync_state": "ok",
            "backfill": {
                "issues": outcome.issues,
                "comments": outcome.comments,
                "skipped_pull_requests": outcome.skipped_pull_requests,
                "reached_end": outcome.all_reached_end,
                "history_cursor": coverage.history_cursor,
                "historical_backfill_complete": coverage.historical_backfill_complete
            },
            "index": {
                "active_generation": index.generation,
                "dirty_task_count": index.dirty_task_count
            }
        }),
        index.warnings,
    ))
}

fn merge_sync_summary(total: &mut Option<SyncSummary>, page: SyncSummary) {
    match total {
        Some(total) => {
            total.sync_run_id = page.sync_run_id;
            total.fetched_issues += page.fetched_issues;
            total.upserted_issues += page.upserted_issues;
            total.fetched_comments += page.fetched_comments;
            total.upserted_comments += page.upserted_comments;
            total.skipped_pull_requests += page.skipped_pull_requests;
            total.cursor_updates.extend(page.cursor_updates);
            total.not_modified_endpoints += page.not_modified_endpoints;
        }
        None => *total = Some(page),
    }
}

pub async fn sync_issue(
    profile_id: &str,
    issue_number: i64,
    repo_scope: Option<&ResolvedRepoScope>,
    show_progress: bool,
) -> Result<LocalReadOutcome, QghError> {
    if issue_number < 1 {
        return Err(QghError::validation(
            "validation.invalid_issue_number",
            "Issue number must be a positive integer.",
        )
        .with_details(json!({ "issue_number": issue_number })));
    }

    let progress = StderrSyncProgress::new(show_progress);
    progress.line(format_args!(
        "qgh sync issue: loading profile profile={profile_id}"
    ));
    let profile = load_profile(profile_id)?;
    let repo = target_issue_repo(&profile, repo_scope)?;
    let token = resolve_token(&profile)?;
    let mut store = Store::open(&profile.paths)?;
    progress.line(format_args!(
        "qgh sync issue: fetching repo={} issue_number={issue_number}",
        repo.full_name()
    ));

    let outcome =
        github::fetch_target_issue(&profile, &token, &repo, issue_number, Some(&progress)).await?;
    match outcome {
        github::TargetIssueFetchOutcome::Backoff(backoff) => {
            progress.line(format_args!(
                "qgh sync issue: backoff reason={} scope={} retry_after_seconds={}",
                backoff.reason, backoff.scope, backoff.retry_after_seconds
            ));
            let backoff = store.record_backoff_state(
                &backoff.reason,
                &backoff.scope,
                backoff.retry_after_seconds,
                backoff.reset_at.as_deref(),
            )?;
            let status = store.status()?;
            Ok(local_read_outcome(
                json!({
                    "profile_id": profile.id,
                    "sync_state": "backoff",
                    "target": {
                        "kind": "issue",
                        "repo": repo.full_name(),
                        "issue_number": issue_number
                    },
                    "backoff": backoff,
                    "sync": {
                        "last_successful_sync": status.last_sync_at,
                        "scheduler": {
                            "max_in_flight_requests": profile.max_in_flight_requests,
                            "hard_cap": 16
                        }
                    },
                    "sources": {
                        "issue_count": status.issue_count,
                        "comment_count": status.comment_count,
                        "tombstone_count": status.tombstone_count
                    },
                    "index": {
                        "active_generation": status.active_generation,
                        "dirty_task_count": status.dirty_task_count
                    }
                }),
                Vec::new(),
            ))
        }
        github::TargetIssueFetchOutcome::Fetched(fetched) => {
            progress.line(format_args!(
                "qgh sync issue: fetched issue=1 comments={}",
                fetched.comments.len()
            ));
            let mut summary =
                store.upsert_target_issue_refresh(&fetched.issue, &fetched.comments)?;
            if fetched.lifecycle.reason.as_deref() == Some("transferred")
                && (fetched.issue.repo != repo.full_name() || fetched.issue.number != issue_number)
            {
                let (tombstoned_issues, tombstoned_comments) = store
                    .tombstone_target_issue_sources(
                        &repo.full_name(),
                        issue_number,
                        "transferred",
                    )?;
                summary.tombstoned_issues += tombstoned_issues;
                summary.tombstoned_comments += tombstoned_comments;
                summary.deleted_comments += tombstoned_comments;
            }
            progress.line(format_args!(
                "qgh sync issue: stored comments added={} updated={} deleted={}",
                summary.added_comments, summary.updated_comments, summary.deleted_comments
            ));
            store.clear_backoff_state()?;
            let index = rebuild_bm25_index(&profile, &mut store, &progress)?;
            progress.line(format_args!(
                "qgh sync issue: complete sync_run_id={}",
                summary.sync_run_id
            ));
            Ok(local_read_outcome(
                target_issue_sync_json(
                    &profile,
                    &repo,
                    issue_number,
                    &summary,
                    &fetched.lifecycle,
                    index.generation,
                    index.dirty_task_count,
                ),
                index.warnings,
            ))
        }
        github::TargetIssueFetchOutcome::Unavailable(lifecycle) => {
            let reason = lifecycle.reason.as_deref().unwrap_or("unavailable");
            progress.line(format_args!(
                "qgh sync issue: lifecycle status={} reason={}",
                lifecycle.status, reason
            ));
            let summary =
                store.tombstone_target_issue_refresh(&repo.full_name(), issue_number, reason)?;
            store.clear_backoff_state()?;
            let index = rebuild_bm25_index(&profile, &mut store, &progress)?;
            progress.line(format_args!(
                "qgh sync issue: complete sync_run_id={}",
                summary.sync_run_id
            ));
            Ok(local_read_outcome(
                target_issue_sync_json(
                    &profile,
                    &repo,
                    issue_number,
                    &summary,
                    &lifecycle,
                    index.generation,
                    index.dirty_task_count,
                ),
                index.warnings,
            ))
        }
    }
}

struct StderrSyncProgress {
    enabled: bool,
}

impl StderrSyncProgress {
    fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    fn line(&self, args: fmt::Arguments<'_>) {
        if self.enabled {
            eprintln!("{args}");
        }
    }
}

impl github::ProgressReporter for StderrSyncProgress {
    fn report(&self, event: github::ProgressEvent) {
        match event {
            github::ProgressEvent::RepoStarted { repo } => {
                self.line(format_args!("qgh sync: fetching repo={repo}"));
            }
            github::ProgressEvent::IssuePageFetched { repo, item_count } => {
                self.line(format_args!(
                    "qgh sync: received issue page repo={repo} items={item_count}"
                ));
            }
            github::ProgressEvent::RepoProgress {
                repo,
                issues,
                comments,
                skipped_pull_requests,
            } => {
                self.line(format_args!(
                    "qgh sync: processed repo={repo} issues={issues} comments={comments} skipped_pull_requests={skipped_pull_requests}"
                ));
            }
            github::ProgressEvent::IssueEndpointNotModified { repo } => {
                self.line(format_args!("qgh sync: issues unchanged repo={repo}"));
            }
            github::ProgressEvent::CommentPageFetched {
                repo,
                issue_number,
                item_count,
            } => {
                self.line(format_args!(
                    "qgh sync: received comment page repo={repo} issue=#{issue_number} items={item_count}"
                ));
            }
            github::ProgressEvent::Backoff {
                reason,
                scope,
                retry_after_seconds,
            } => {
                self.line(format_args!(
                    "qgh sync: GitHub backoff reason={reason} scope={scope} retry_after_seconds={retry_after_seconds}"
                ));
            }
            github::ProgressEvent::ReconciliationProgress { checked, total } => {
                self.line(format_args!(
                    "qgh sync: reconciled checked_sources={checked}/{total}"
                ));
            }
        }
    }
}

fn rebuild_bm25_index(
    profile: &Profile,
    store: &mut Store,
    progress: &StderrSyncProgress,
) -> Result<IndexRebuildOutcome, QghError> {
    let warnings = refresh_embedding_for_sync_if_enabled(profile, store, progress);
    let sources = store.active_index_sources()?;
    progress.line(format_args!(
        "qgh sync: rebuilding BM25 index sources={}",
        sources.len()
    ));
    let (generation, reserved_generation_path) =
        store.reserve_index_generation(&profile.paths.index_root, sources.len())?;
    let generation_path = index::rebuild(&profile.paths.index_root, generation, &sources)?;
    debug_assert_eq!(generation_path, reserved_generation_path);
    store.mark_index_published(
        generation,
        &generation_path.to_string_lossy(),
        sources.len(),
    )?;
    progress.line(format_args!(
        "qgh sync: published BM25 index generation={} sources={}",
        generation,
        sources.len()
    ));
    let status = store.status()?;
    Ok(IndexRebuildOutcome {
        generation,
        dirty_task_count: status.dirty_task_count,
        warnings,
    })
}

struct IndexRebuildOutcome {
    generation: i64,
    dirty_task_count: i64,
    warnings: Vec<Value>,
}

#[derive(Default)]
struct ChunkRefreshStats {
    refreshed_chunks: usize,
    skipped_sources: usize,
}

fn refresh_embedding_for_sync_if_enabled(
    profile: &Profile,
    store: &mut Store,
    progress: &StderrSyncProgress,
) -> Vec<Value> {
    let Some(embedding) = profile.embedding.as_ref() else {
        return Vec::new();
    };

    let mut warnings = Vec::new();
    match store.cleanup_inactive_embedding_artifacts() {
        Ok(cleaned_chunks) if cleaned_chunks > 0 => progress.line(format_args!(
            "qgh sync: cleaned stale embedding artifacts chunks={cleaned_chunks}"
        )),
        Ok(_) => {}
        Err(error) => warnings.push(embedding_sync_warning(&error)),
    }

    let tokenizer = match embedding_tokenizer(embedding) {
        Ok(tokenizer) => tokenizer,
        Err(error) => {
            warnings.push(embedding_sync_warning(&error));
            return warnings;
        }
    };
    if let Err(error) = refresh_embedding_chunks(store, tokenizer.as_ref(), progress) {
        warnings.push(embedding_sync_warning(&error));
        return warnings;
    }

    match refresh_incremental_chunk_embeddings(store, embedding) {
        Ok(embedded_chunks) => progress.line(format_args!(
            "qgh sync: refreshed chunk embeddings embedded={embedded_chunks}"
        )),
        Err(error) => warnings.push(embedding_sync_warning(&error)),
    }
    warnings
}

fn refresh_embedding_chunks(
    store: &mut Store,
    tokenizer: &dyn EmbeddingTokenizer,
    progress: &StderrSyncProgress,
) -> Result<ChunkRefreshStats, QghError> {
    let mut stats = ChunkRefreshStats::default();
    for source in store.active_index_sources()? {
        let source_version_id = store
            .latest_source_version_id(&source.source_id)?
            .ok_or_else(|| {
                QghError::storage(format!(
                    "Cannot chunk source `{}` without an active source version.",
                    source.source_id
                ))
            })?;
        if store.source_version_has_chunks(source_version_id)? {
            stats.skipped_sources += 1;
            continue;
        }
        let chunks = chunk_markdown(&source.body, tokenizer).map_err(|error| {
            QghError::storage(format!(
                "Failed to chunk source `{}` with embedding tokenizer: {error}",
                source.source_id
            ))
        })?;
        stats.refreshed_chunks += store
            .replace_chunks_for_source_version(&source.source_id, source_version_id, &chunks)?
            .len();
    }
    progress.line(format_args!(
        "qgh sync: refreshed embedding chunks chunks={} skipped_sources={}",
        stats.refreshed_chunks, stats.skipped_sources
    ));
    Ok(stats)
}

fn refresh_incremental_chunk_embeddings(
    store: &mut Store,
    embedding: &EmbeddingConfig,
) -> Result<usize, QghError> {
    let runtime = embedding_runtime(embedding)?;
    let expectation = embedding_fingerprint_expectation(embedding);
    let matching_active_fingerprint = store
        .active_embedding_fingerprint()?
        .filter(|fingerprint| fingerprint.matches_expectation(&expectation));
    let chunks = match matching_active_fingerprint.as_ref() {
        Some(fingerprint) => store.active_chunks_missing_embedding_for_fingerprint(fingerprint)?,
        None => store.active_embedding_chunks()?,
    };
    if chunks.is_empty() {
        return Ok(0);
    }

    let texts = chunks
        .iter()
        .map(|chunk| chunk.body.as_str())
        .collect::<Vec<_>>();
    let vectors = runtime
        .provider
        .embed_documents(&texts)
        .map_err(embedding_error)?;
    if vectors.len() != chunks.len() {
        return Err(QghError::validation(
            "embedding.vector_count_mismatch",
            "Embedding provider returned a different number of vectors than input chunks.",
        )
        .with_details(json!({
            "chunk_count": chunks.len(),
            "vector_count": vectors.len()
        })));
    }
    let dimension = embedding_dimension(&vectors)?;
    let fingerprint = match matching_active_fingerprint {
        Some(fingerprint) => {
            if fingerprint.dimension != dimension {
                return Err(QghError::validation(
                    "embedding.dimension_mismatch",
                    "Embedding provider returned a different vector dimension than the active fingerprint.",
                )
                .with_details(json!({
                    "active_dimension": fingerprint.dimension,
                    "provider_dimension": dimension
                }))
                .with_hint("Run `qgh embed --force` to recompute local embeddings."));
            }
            fingerprint
        }
        None => runtime.fingerprint_seed.with_dimension(dimension),
    };
    let embeddings = chunks
        .iter()
        .zip(vectors)
        .map(|(chunk, vector)| (chunk.chunk_id, vector))
        .collect::<Vec<_>>();
    store.upsert_chunk_embeddings(&fingerprint, &embeddings)
}

fn embedding_sync_warning(error: &QghError) -> Value {
    let mut warning = json!({
        "code": "embedding.sync_failed",
        "severity": "warn",
        "message": "Embedding refresh failed during sync. BM25 index refresh completed without vector updates.",
        "details": {
            "cause_code": error.code
        }
    });
    if let Some(hint) = &error.hint {
        warning["hint"] = json!(hint);
    }
    warning
}

#[cfg(feature = "fastembed-provider")]
fn embedding_tokenizer(
    embedding: &EmbeddingConfig,
) -> Result<Box<dyn EmbeddingTokenizer>, QghError> {
    match embedding.provider {
        EmbeddingProviderKind::Local => {
            FastembedTokenizer::from_options(embedding.fastembed_options())
                .map(|tokenizer| Box::new(tokenizer) as Box<dyn EmbeddingTokenizer>)
                .map_err(embedding_error)
        }
    }
}

#[cfg(not(feature = "fastembed-provider"))]
fn embedding_tokenizer(
    embedding: &EmbeddingConfig,
) -> Result<Box<dyn EmbeddingTokenizer>, QghError> {
    match embedding.provider {
        EmbeddingProviderKind::Local => Err(QghError::validation(
            "embedding.provider_unavailable",
            "This qgh binary was built without the fastembed-provider feature.",
        )
        .with_hint("Rebuild with `--features fastembed-provider` or remove `[embedding]`.")),
    }
}

fn embedding_error(error: EmbeddingProviderError) -> QghError {
    let mut qgh_error =
        QghError::validation(error.code(), error.message()).with_details(error.details().clone());
    if let Some(hint) = error.hint() {
        qgh_error = qgh_error.with_hint(hint);
    }
    qgh_error
}

fn target_issue_repo(
    profile: &Profile,
    repo_scope: Option<&ResolvedRepoScope>,
) -> Result<RepoRef, QghError> {
    if let Some(repo_scope) = repo_scope {
        return profile
            .repos
            .iter()
            .find(|repo| repo.full_name() == repo_scope.repo)
            .cloned()
            .ok_or_else(|| {
                QghError::validation(
                    "validation.invalid_repo",
                    format!(
                        "Repo `{}` is outside profile `{}` allowlist.",
                        repo_scope.repo, profile.id
                    ),
                )
                .with_details(json!({
                    "profile_id": &profile.id,
                    "repo": &repo_scope.repo
                }))
                .with_hint("Use a repo from the profile allowlist or update the profile config.")
            });
    }
    if profile.repos.len() == 1 {
        return Ok(profile.repos[0].clone());
    }
    Err(QghError::validation(
        "validation.repo_required",
        "sync issue requires a single repo scope.",
    )
    .with_details(json!({
        "profile_id": profile.id,
        "repo_count": profile.repos.len()
    }))
    .with_hint("Run from a repo worktree, create .qgh.toml with qgh init repo, or pass sync issue --repo owner/repo."))
}

fn target_issue_sync_json(
    profile: &Profile,
    repo: &RepoRef,
    issue_number: i64,
    summary: &TargetedSyncSummary,
    lifecycle: &github::TargetIssueLifecycle,
    generation: i64,
    dirty_task_count: i64,
) -> Value {
    json!({
        "profile_id": &profile.id,
        "sync_state": "ok",
        "sync_run_id": &summary.sync_run_id,
        "target": {
            "kind": "issue",
            "repo": repo.full_name(),
            "issue_number": issue_number
        },
        "lifecycle": {
            "status": &lifecycle.status,
            "reason": &lifecycle.reason,
            "http_status": lifecycle.http_status,
            "alias_chain": &lifecycle.alias_chain
        },
        "issues": {
            "fetched": summary.fetched_issues,
            "upserted": summary.upserted_issues,
            "tombstoned": summary.tombstoned_issues,
            "skipped_pull_requests": 0
        },
        "comments": {
            "fetched": summary.fetched_comments,
            "upserted": summary.upserted_comments,
            "added": summary.added_comments,
            "updated": summary.updated_comments,
            "deleted": summary.deleted_comments,
            "tombstoned": summary.tombstoned_comments
        },
        "cursors": {
            "updated": 0,
            "not_modified_endpoints": 0,
            "watermarks": {}
        },
        "index": {
            "active_generation": generation,
            "dirty_task_count": dirty_task_count
        },
        "reconciliation": {
            "mode": "targeted_issue"
        }
    })
}

pub fn embed(profile_id: &str, args: &EmbedArgs) -> Result<LocalReadOutcome, QghError> {
    if !args.force {
        return Err(QghError::validation(
            "embedding.force_required",
            "`qgh embed` requires --force for this full-refresh slice.",
        )
        .with_hint("Run `qgh embed --force` to recompute every stored chunk embedding."));
    }

    let profile = load_profile(profile_id)?;
    let Some(embedding) = profile.embedding.as_ref() else {
        return Err(QghError::validation(
            "embedding.not_configured",
            "Embedding is not configured for this profile.",
        )
        .with_hint("Add an [embedding] section before running `qgh embed --force`."));
    };
    let mut store = Store::open(&profile.paths)?;
    let runtime = embedding_runtime(embedding)?;
    let progress = StderrSyncProgress::new(false);
    let chunk_stats = refresh_embedding_chunks(&mut store, runtime.tokenizer.as_ref(), &progress)?;
    let data = refresh_chunk_embeddings(
        &mut store,
        runtime.provider.as_ref(),
        runtime.fingerprint_seed,
    )?;
    Ok(LocalReadOutcome {
        data: json!({
            "profile_id": profile.id,
            "embedding_state": "refreshed",
            "chunks": {
                "refreshed": chunk_stats.refreshed_chunks,
                "embedded": data["embedded_chunks"]
            }
        }),
        warnings: Vec::new(),
    })
}

struct EmbeddingRuntime {
    tokenizer: Box<dyn EmbeddingTokenizer>,
    provider: Box<dyn EmbeddingProvider>,
    fingerprint_seed: EmbeddingFingerprintSeed,
}

#[cfg(feature = "fastembed-provider")]
fn embedding_runtime(embedding: &EmbeddingConfig) -> Result<EmbeddingRuntime, QghError> {
    match embedding.provider {
        EmbeddingProviderKind::Local => {
            let snapshot = resolve_fastembed_snapshot(embedding.fastembed_options())
                .map_err(embedding_error)?;
            let tokenizer = FastembedTokenizer::from_snapshot(&snapshot)
                .map(|tokenizer| Box::new(tokenizer) as Box<dyn EmbeddingTokenizer>)
                .map_err(embedding_error)?;
            let engine = FastembedEngine::from_snapshot(&snapshot).map_err(embedding_error)?;
            let provider = LocalEmbeddingProvider::new(engine, snapshot.query_prefix.clone());
            Ok(EmbeddingRuntime {
                tokenizer,
                provider: Box::new(provider),
                fingerprint_seed: embedding_fingerprint_seed(embedding, &snapshot),
            })
        }
    }
}

#[cfg(not(feature = "fastembed-provider"))]
fn embedding_runtime(embedding: &EmbeddingConfig) -> Result<EmbeddingRuntime, QghError> {
    match embedding.provider {
        EmbeddingProviderKind::Local => Err(QghError::validation(
            "embedding.provider_unavailable",
            "This qgh binary was built without the fastembed-provider feature.",
        )
        .with_hint("Rebuild with `--features fastembed-provider` or remove `[embedding]`.")),
    }
}

#[cfg(feature = "fastembed-provider")]
fn embedding_fingerprint_seed(
    embedding: &EmbeddingConfig,
    snapshot: &ResolvedModelSnapshot,
) -> EmbeddingFingerprintSeed {
    EmbeddingFingerprintSeed {
        provider: embedding_provider_name(embedding.provider).to_string(),
        model_id: embedding_model_id(embedding, snapshot),
        model_revision: snapshot.model_revision.clone(),
        pooling: snapshot.pooling,
        query_prefix: snapshot.query_prefix.clone(),
    }
}

#[cfg(feature = "fastembed-provider")]
fn embedding_model_id(embedding: &EmbeddingConfig, snapshot: &ResolvedModelSnapshot) -> String {
    if let Some(model_id) = &snapshot.model_id {
        return model_id.clone();
    }
    embedding
        .model_path
        .as_ref()
        .map(|path| format!("model_path:{}", path.to_string_lossy()))
        .unwrap_or_else(|| format!("model_file:{}", snapshot.model_file))
}

fn refresh_chunk_embeddings(
    store: &mut Store,
    provider: &dyn EmbeddingProvider,
    fingerprint_seed: EmbeddingFingerprintSeed,
) -> Result<Value, QghError> {
    let chunks = store.active_embedding_chunks()?;
    if chunks.is_empty() {
        return Err(QghError::validation(
            "embedding.no_chunks",
            "No active chunks are available to embed.",
        )
        .with_hint("Run `qgh sync` with [embedding] configured before `qgh embed --force`."));
    }

    let texts = chunks
        .iter()
        .map(|chunk| chunk.body.as_str())
        .collect::<Vec<_>>();
    let vectors = provider.embed_documents(&texts).map_err(embedding_error)?;
    if vectors.len() != chunks.len() {
        return Err(QghError::validation(
            "embedding.vector_count_mismatch",
            "Embedding provider returned a different number of vectors than input chunks.",
        )
        .with_details(json!({
            "chunk_count": chunks.len(),
            "vector_count": vectors.len()
        })));
    }
    let dimension = embedding_dimension(&vectors)?;
    let fingerprint = fingerprint_seed.with_dimension(dimension);
    let embeddings = chunks
        .iter()
        .zip(vectors)
        .map(|(chunk, vector)| (chunk.chunk_id, vector))
        .collect::<Vec<_>>();
    let embedded_chunks = store.replace_all_chunk_embeddings(&fingerprint, &embeddings)?;
    let usable_embeddings = store.current_chunk_embedding_count_for_fingerprint(&fingerprint)?;
    Ok(json!({
        "embedded_chunks": embedded_chunks,
        "usable_embeddings": usable_embeddings
    }))
}

fn embedding_dimension(vectors: &[EmbeddingVector]) -> Result<usize, QghError> {
    let Some(first) = vectors.first() else {
        return Err(QghError::validation(
            "embedding.empty_result",
            "Embedding provider returned no vectors.",
        ));
    };
    if first.is_empty() {
        return Err(QghError::validation(
            "embedding.empty_vector",
            "Embedding provider returned an empty vector.",
        ));
    }
    let dimension = first.len();
    if vectors.iter().any(|vector| vector.len() != dimension) {
        return Err(QghError::validation(
            "embedding.dimension_mismatch",
            "Embedding provider returned inconsistent vector dimensions.",
        )
        .with_details(json!({ "expected_dimension": dimension })));
    }
    Ok(dimension)
}

fn profile_scoped_to_repo(
    profile: &Profile,
    repo_scope: Option<&ResolvedRepoScope>,
) -> Result<Profile, QghError> {
    let Some(repo_scope) = repo_scope else {
        return Ok(profile.clone());
    };
    let Some(repo) = profile
        .repos
        .iter()
        .find(|repo| repo.full_name() == repo_scope.repo)
        .cloned()
    else {
        return Err(QghError::validation(
            "validation.invalid_repo",
            format!(
                "Repo `{}` is outside profile `{}` allowlist.",
                repo_scope.repo, profile.id
            ),
        )
        .with_details(json!({
            "profile_id": profile.id,
            "repo": repo_scope.repo
        }))
        .with_hint("Use a repo from the profile allowlist or update the profile config."));
    };
    let mut scoped = profile.clone();
    scoped.repos = vec![repo];
    Ok(scoped)
}

fn reconciliation_candidates_scoped_to_repo(
    candidates: Vec<ReconciliationCandidate>,
    repo_scope: Option<&ResolvedRepoScope>,
) -> Vec<ReconciliationCandidate> {
    let Some(repo_scope) = repo_scope else {
        return candidates;
    };
    candidates
        .into_iter()
        .filter(|candidate| candidate.repo == repo_scope.repo)
        .collect()
}

pub fn init(profile_arg: Option<&str>, args: &InitArgs) -> Result<InitCommandOutcome, QghError> {
    if let Some(crate::cli::InitTarget::Repo(repo_args)) = &args.target {
        return init_repo_policy(profile_arg, repo_args.clone());
    }
    let Some(root) = current_git_worktree_root() else {
        return Err(QghError::validation(
            "config.no_git_worktree",
            "qgh init must be run inside a git worktree.",
        )
        .with_hint("Run qgh init from a git worktree."));
    };
    let remote = optional_git_remote_defaults(&root, args)?;
    let preset = init_preset(profile_arg, args, &root, remote.as_ref())?;
    if args.yes {
        return finish_init_preset(&root, preset);
    }
    write_init_preset_preview(&preset)?;
    if prompt_use_defaults()? {
        finish_init_preset(&root, preset)
    } else {
        init_custom_interactive(&root, remote.as_ref(), profile_arg, args)
    }
}

pub fn init_repo_policy(
    profile_arg: Option<&str>,
    args: InitRepoArgs,
) -> Result<InitCommandOutcome, QghError> {
    let Some(root) = current_git_worktree_root() else {
        return Err(QghError::validation(
            "config.no_git_worktree",
            "qgh init must be run inside a git worktree.",
        )
        .with_hint("Run qgh init from a git worktree or pass --repo after initializing git."));
    };

    let (repo, repo_source) = match args.repo.as_deref() {
        Some(repo) => {
            parse_repo(repo).map_err(|message| {
                QghError::validation(
                    "validation.invalid_repo",
                    format!("Repo `{repo}` {message}"),
                )
                .with_details(json!({ "repo": repo }))
                .with_hint("Use explicit owner/repo format.")
            })?;
            (repo.to_string(), "cli")
        }
        None => (repo_from_origin_remote(&root)?, "git_remote"),
    };

    let explicit_profile = explicit_profile_for_init(profile_arg);
    let (profile_validation, warnings, meta_profile_id, meta_profile_source) =
        match explicit_profile {
            Some((profile_id, profile_source)) => {
                let profile = load_profile(&profile_id)?;
                if !profile.allows_repo(&repo) {
                    return Err(QghError::validation(
                        "validation.invalid_repo",
                        format!(
                            "Repo `{repo}` is outside profile `{}` allowlist.",
                            profile.id
                        ),
                    )
                    .with_details(json!({
                        "profile_id": profile.id,
                        "repo": repo
                    }))
                    .with_hint(
                        "Use a repo from the profile allowlist or update the profile config.",
                    ));
                }
                (
                    json!({
                        "status": "validated",
                        "profile_id": profile.id,
                        "profile_source": profile_source,
                        "allowlist_match": true
                    }),
                    Vec::new(),
                    Some(profile_id),
                    Some(profile_source),
                )
            }
            None => (
                json!({
                    "status": "not_checked",
                    "profile_id": Value::Null,
                    "profile_source": Value::Null,
                    "allowlist_match": Value::Null
                }),
                vec![json!({
                    "code": "config.profile_not_checked",
                    "severity": "warn",
                    "message": "Profile allowlist was not checked because no profile was explicit."
                })],
                None,
                None,
            ),
        };

    let path = root.join(".qgh.toml");
    let overwritten = path.exists();
    if overwritten && !args.force {
        return Err(QghError::validation(
            "config.repo_policy_exists",
            "Repo policy already exists.",
        )
        .with_details(json!({ "path": path.to_string_lossy() }))
        .with_hint("Use --force to overwrite the existing .qgh.toml."));
    }

    fs::write(&path, repo_policy_toml(&repo)).map_err(|error| {
        QghError::storage(format!(
            "Failed to write repo policy at {}: {error}",
            path.display()
        ))
    })?;
    load_repo_policy_at(&path)?;

    let meta_repo_source = if repo_source == "cli" {
        Some("cli")
    } else {
        None
    };
    Ok(InitCommandOutcome {
        data: json!({
            "path": path.to_string_lossy(),
            "repo": repo,
            "repo_source": repo_source,
            "overwritten": overwritten,
            "profile_validation": profile_validation
        }),
        warnings,
        meta: json!({
            "profile_id": meta_profile_id,
            "profile_source": meta_profile_source,
            "repo": repo,
            "repo_source": meta_repo_source,
            "repo_policy_path": Value::Null
        }),
    })
}

fn init_custom_interactive(
    root: &std::path::Path,
    remote: Option<&GitRemote>,
    profile_arg: Option<&str>,
    args: &InitArgs,
) -> Result<InitCommandOutcome, QghError> {
    let repo = match args.repo.as_deref() {
        Some(repo) => {
            parse_repo(repo).map_err(|message| {
                QghError::validation(
                    "validation.invalid_repo",
                    format!("Repo `{repo}` {message}"),
                )
            })?;
            repo.to_string()
        }
        None => remote
            .map(|remote| remote.repo.clone())
            .ok_or_else(|| missing_init_value("--repo"))?,
    };
    let profile_default = profile_arg.unwrap_or("work");
    let profile_id = prompt_line("profile id", profile_default)?;
    let host_default = args
        .host
        .clone()
        .or_else(|| remote.map(|remote| remote.host.clone()))
        .ok_or_else(|| missing_init_value("--host"))?;
    let host = prompt_line("host", &host_default)?;
    let api_default = args
        .api_base_url
        .clone()
        .or_else(|| remote.map(|remote| remote.api_base_url.clone()))
        .unwrap_or_else(|| default_api_base_url(&host));
    let api_base_url = prompt_line(
        "api base url",
        args.api_base_url.as_deref().unwrap_or(&api_default),
    )?;
    let web_default = args
        .web_base_url
        .clone()
        .or_else(|| remote.map(|remote| remote.web_base_url.clone()))
        .unwrap_or_else(|| default_web_base_url(&host));
    let web_base_url = prompt_line(
        "web base url",
        args.web_base_url.as_deref().unwrap_or(&web_default),
    )?;
    let token_source_name = match args.token_source {
        Some(InitTokenSourceArg::GithubCli) => "github_cli".to_string(),
        Some(InitTokenSourceArg::Env) => "env".to_string(),
        None => prompt_line("token source (github_cli/env)", "github_cli")?,
    };
    let token_source = match token_source_name.as_str() {
        "github_cli" => TokenSource::GithubCli,
        "env" => {
            let env = match args.token_env.as_deref() {
                Some(env) => env.to_string(),
                None => prompt_line("token env var", "GITHUB_TOKEN")?,
            };
            TokenSource::Env { env }
        }
        _ => {
            return Err(QghError::validation(
                "validation.invalid_token_source",
                "Token source must be `github_cli` or `env`.",
            ));
        }
    };
    let write_repo_policy = prompt_bool("create .qgh.toml", true)?;
    finish_profile_init(
        root,
        ProfileInitPlan {
            profile_id,
            repo,
            host,
            api_base_url,
            web_base_url,
            token_source,
            write_repo_policy,
            force_repo_policy: args.force,
        },
    )
}

struct InitPreset {
    profile_id: String,
    repo: String,
    host: String,
    api_base_url: String,
    web_base_url: String,
    token_source: TokenSource,
    write_repo_policy: bool,
    force_repo_policy: bool,
    config_path: PathBuf,
    repo_policy_path: PathBuf,
    db_path: PathBuf,
}

fn init_preset(
    profile_arg: Option<&str>,
    args: &InitArgs,
    root: &std::path::Path,
    remote: Option<&GitRemote>,
) -> Result<InitPreset, QghError> {
    let repo = match args.repo.as_deref() {
        Some(repo) => {
            parse_repo(repo).map_err(|message| {
                QghError::validation(
                    "validation.invalid_repo",
                    format!("Repo `{repo}` {message}"),
                )
            })?;
            repo.to_string()
        }
        None => remote
            .map(|remote| remote.repo.clone())
            .ok_or_else(|| missing_init_value("--repo"))?,
    };
    let profile_id = profile_arg.unwrap_or("work").to_string();
    let host = args
        .host
        .clone()
        .or_else(|| remote.map(|remote| remote.host.clone()))
        .ok_or_else(|| missing_init_value("--host"))?;
    let api_base_url = args
        .api_base_url
        .clone()
        .or_else(|| remote.map(|remote| remote.api_base_url.clone()))
        .unwrap_or_else(|| default_api_base_url(&host));
    let web_base_url = args
        .web_base_url
        .clone()
        .or_else(|| remote.map(|remote| remote.web_base_url.clone()))
        .unwrap_or_else(|| default_web_base_url(&host));
    let token_source = init_token_source_or_default(args)?;
    let paths = ProfilePaths::resolve(&profile_id)?;
    Ok(InitPreset {
        profile_id,
        repo,
        host,
        api_base_url,
        web_base_url,
        token_source,
        write_repo_policy: true,
        force_repo_policy: args.force,
        config_path: paths.config_file,
        repo_policy_path: root.join(".qgh.toml"),
        db_path: paths.db_path,
    })
}

fn finish_init_preset(
    root: &std::path::Path,
    preset: InitPreset,
) -> Result<InitCommandOutcome, QghError> {
    finish_profile_init(
        root,
        ProfileInitPlan {
            profile_id: preset.profile_id,
            repo: preset.repo,
            host: preset.host,
            api_base_url: preset.api_base_url,
            web_base_url: preset.web_base_url,
            token_source: preset.token_source,
            write_repo_policy: preset.write_repo_policy,
            force_repo_policy: preset.force_repo_policy,
        },
    )
}

fn write_init_preset_preview(preset: &InitPreset) -> Result<(), QghError> {
    let mut stderr = io::stderr();
    writeln!(stderr, "Detected qgh init defaults:")?;
    writeln!(stderr, "  repo: {}", preset.repo)?;
    writeln!(stderr, "  host: {}", preset.host)?;
    writeln!(stderr, "  profile id: {}", preset.profile_id)?;
    writeln!(
        stderr,
        "  token source: {}",
        token_source_display(&preset.token_source)
    )?;
    writeln!(stderr, "  config path: {}", preset.config_path.display())?;
    writeln!(stderr, "  repo policy: create")?;
    writeln!(
        stderr,
        "  repo policy path: {}",
        preset.repo_policy_path.display()
    )?;
    writeln!(stderr, "  db path: {}", preset.db_path.display())?;
    Ok(())
}

struct ProfileInitPlan {
    profile_id: String,
    repo: String,
    host: String,
    api_base_url: String,
    web_base_url: String,
    token_source: TokenSource,
    write_repo_policy: bool,
    force_repo_policy: bool,
}

fn finish_profile_init(
    root: &std::path::Path,
    plan: ProfileInitPlan,
) -> Result<InitCommandOutcome, QghError> {
    let policy_path = root.join(".qgh.toml");
    let repo_policy_action = plan_repo_policy_action(
        &policy_path,
        &plan.repo,
        plan.write_repo_policy,
        plan.force_repo_policy,
    )?;

    let bootstrap = bootstrap_profile_repo(ProfileBootstrapInput {
        profile_id: plan.profile_id.clone(),
        host: plan.host,
        api_base_url: plan.api_base_url,
        web_base_url: plan.web_base_url,
        repo: plan.repo.clone(),
        token_source: plan.token_source,
    })?;

    apply_repo_policy_action(&policy_path, &plan.repo, repo_policy_action)?;

    let profile_id = plan.profile_id;
    let repo = plan.repo;
    let repo_policy_path = if plan.write_repo_policy {
        Value::String(policy_path.to_string_lossy().to_string())
    } else {
        Value::Null
    };
    Ok(InitCommandOutcome {
        data: json!({
            "profile_config_path": bootstrap.config_path.to_string_lossy(),
            "profile_id": profile_id.clone(),
            "profile_action": bootstrap.profile_action,
            "repo": repo.clone(),
            "repo_allowlist_action": bootstrap.repo_allowlist_action,
            "repo_policy_action": repo_policy_action,
            "repo_policy_path": repo_policy_path.clone(),
            "token_source": {
                "kind": bootstrap.token_source_kind
            },
            "next_steps": ["qgh sync", "qgh query <terms>"]
        }),
        warnings: Vec::new(),
        meta: json!({
            "profile_id": profile_id,
            "profile_source": "cli",
            "repo": repo,
            "repo_source": "cli",
            "repo_policy_path": repo_policy_path
        }),
    })
}

fn plan_repo_policy_action(
    policy_path: &std::path::Path,
    requested_repo: &str,
    write_repo_policy: bool,
    force_repo_policy: bool,
) -> Result<&'static str, QghError> {
    if !write_repo_policy {
        return Ok("skipped");
    }
    if !policy_path.exists() {
        return Ok("created");
    }
    if force_repo_policy {
        return Ok("overwritten");
    }
    let existing_policy = load_repo_policy_at(policy_path)?;
    let existing_repo = existing_policy.repo.full_name();
    if existing_repo == requested_repo {
        return Ok("already_exists");
    }
    Err(QghError::validation(
        "config.repo_policy_exists",
        "Repo policy already exists for a different repo.",
    )
    .with_details(json!({
        "path": policy_path.to_string_lossy(),
        "existing_repo": existing_repo,
        "requested_repo": requested_repo
    }))
    .with_hint("Use --force to overwrite the existing .qgh.toml."))
}

fn apply_repo_policy_action(
    policy_path: &std::path::Path,
    repo: &str,
    action: &'static str,
) -> Result<(), QghError> {
    if !matches!(action, "created" | "overwritten") {
        return Ok(());
    }
    fs::write(policy_path, repo_policy_toml(repo)).map_err(|error| {
        QghError::storage(format!(
            "Failed to write repo policy at {}: {error}",
            policy_path.display()
        ))
    })?;
    load_repo_policy_at(policy_path)?;
    Ok(())
}

fn prompt_line(label: &str, default: &str) -> Result<String, QghError> {
    let mut stderr = io::stderr();
    write!(stderr, "{label} [{default}]: ")?;
    stderr.flush()?;
    let mut line = String::new();
    let bytes = io::stdin().read_line(&mut line)?;
    if bytes == 0 {
        writeln!(stderr, "\nqgh init canceled; no files changed.")?;
        return Err(init_cancelled());
    }
    let value = line.trim();
    if value.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(value.to_string())
    }
}

fn prompt_use_defaults() -> Result<bool, QghError> {
    let answer = prompt_line("Use these defaults?", "Y/n")?;
    if answer == "Y/n" {
        return Ok(true);
    }
    match answer.to_ascii_lowercase().as_str() {
        "y" | "yes" => Ok(true),
        "n" | "no" => Ok(false),
        _ => Err(QghError::validation(
            "validation.invalid_init_answer",
            "Use these defaults? expects yes or no.",
        )),
    }
}

fn prompt_bool(label: &str, default: bool) -> Result<bool, QghError> {
    let default_text = if default { "Y/n" } else { "y/N" };
    let answer = prompt_line(label, default_text)?;
    if answer == default_text {
        return Ok(default);
    }
    match answer.to_ascii_lowercase().as_str() {
        "y" | "yes" => Ok(true),
        "n" | "no" => Ok(false),
        _ => Err(QghError::validation(
            "validation.invalid_init_answer",
            format!("{label} expects yes or no."),
        )),
    }
}

fn init_cancelled() -> QghError {
    QghError::validation(
        "validation.init_cancelled",
        "qgh init canceled before writing files.",
    )
    .with_hint("Run qgh init again, or use qgh init -y for non-interactive setup.")
}

pub fn query(
    profile_id: &str,
    args: QueryArgs,
    repo_scope: Option<&ResolvedRepoScope>,
) -> Result<LocalReadOutcome, QghError> {
    let profile = load_profile(profile_id)?;
    let repo_policy = discover_repo_policy()?;
    let filters = QueryFilters::from_args(&args, &profile, repo_policy.as_ref(), repo_scope)?;
    let limit = effective_limit(&args, repo_policy.as_ref())?;
    let store = Store::open(&profile.paths)?;
    let overrides = freshness_overrides(args.max_age.as_deref(), args.require_fresh)?;
    if let Some(results) = exact_results(&store, &args.query, &filters, &profile.id)? {
        let last_successful_sync_at =
            query_freshness_sync_time(&store, &profile, &filters, &results)?;
        let freshness = freshness::evaluate(
            profile.freshness_settings(repo_policy.as_ref()),
            FreshnessContext {
                last_successful_sync_at: last_successful_sync_at.as_deref(),
                includes_open_issue: results.includes_open_issue,
                overrides,
            },
        )?;
        if freshness.fails {
            return Err(freshness_error(freshness.block, freshness.warnings));
        }
        // Exact-locator resolution is not an FTS coverage scenario: an empty
        // result here means the locator was filtered out or did not resolve, not
        // that historical backfill is incomplete. Expose the coverage block but
        // do not fire the partial-coverage backfill warning.
        let coverage = coverage::evaluate(&store.coverage_snapshot()?, false);
        let mut warnings = freshness.warnings;
        warnings.extend(coverage.warnings);
        warnings.extend(embedding_warnings(&profile, &store)?);
        return Ok(LocalReadOutcome {
            data: json!({
            "profile_id": profile.id,
            "freshness": freshness.block,
            "coverage": coverage.block,
            "result_filtering": {
                "unresolvable_hits": 0
            },
            "results": results.items
            }),
            warnings,
        });
    }
    let active_index_path = active_index_path(&store, &profile.paths.index_active)?;
    let hits = index::search(&active_index_path, &args.query, limit)?;
    let mut results = QueryResults::default();
    let mut unresolvable_hits = 0;
    for hit in hits {
        let Some(source) = store.get_source(&hit.source_id)? else {
            unresolvable_hits += 1;
            continue;
        };
        if !filters.matches(&source) {
            continue;
        }
        results.push(source, Ranking::Bm25(hit.score), &profile.id);
    }
    let last_successful_sync_at = query_freshness_sync_time(&store, &profile, &filters, &results)?;
    let freshness = freshness::evaluate(
        profile.freshness_settings(repo_policy.as_ref()),
        FreshnessContext {
            last_successful_sync_at: last_successful_sync_at.as_deref(),
            includes_open_issue: results.includes_open_issue,
            overrides,
        },
    )?;
    if freshness.fails {
        return Err(freshness_error(freshness.block, freshness.warnings));
    }
    let coverage = coverage::evaluate(&store.coverage_snapshot()?, results.items.is_empty());
    let mut warnings = freshness.warnings;
    warnings.extend(coverage.warnings);
    warnings.extend(embedding_warnings(&profile, &store)?);
    Ok(LocalReadOutcome {
        data: json!({
        "profile_id": profile.id,
        "freshness": freshness.block,
        "coverage": coverage.block,
        "result_filtering": {
            "unresolvable_hits": unresolvable_hits
        },
        "results": results.items
        }),
        warnings,
    })
}

fn explicit_profile_for_init(profile_arg: Option<&str>) -> Option<(String, &'static str)> {
    if let Some(profile_id) = profile_arg {
        return Some((profile_id.to_string(), "cli"));
    }
    std::env::var("QGH_PROFILE")
        .ok()
        .map(|profile_id| (profile_id, "env"))
}

fn init_token_source_or_default(args: &InitArgs) -> Result<TokenSource, QghError> {
    match args.token_source {
        Some(InitTokenSourceArg::GithubCli) => {
            if args.token_env.is_some() {
                return Err(QghError::validation(
                    "validation.invalid_token_source",
                    "--token-env can only be used with --token-source env.",
                ));
            }
            Ok(TokenSource::GithubCli)
        }
        Some(InitTokenSourceArg::Env) => {
            let env = match args.token_env.clone() {
                Some(env) => env,
                None if args.yes => return Err(missing_init_value("--token-env")),
                None => prompt_line("token env var", "GITHUB_TOKEN")?,
            };
            Ok(TokenSource::Env { env })
        }
        None => {
            if args.token_env.is_some() {
                return Err(missing_init_value("--token-source"));
            }
            Ok(TokenSource::GithubCli)
        }
    }
}

fn token_source_display(token_source: &TokenSource) -> String {
    match token_source {
        TokenSource::GithubCli => "github_cli".to_string(),
        TokenSource::Env { env } => format!("env ({env})"),
        TokenSource::Unsupported => "unsupported".to_string(),
    }
}

fn optional_git_remote_defaults(
    root: &std::path::Path,
    args: &InitArgs,
) -> Result<Option<GitRemote>, QghError> {
    match git_remote_defaults_for_root(root) {
        Ok(remote) => Ok(Some(remote)),
        Err(_error) if args.repo.is_some() && args.host.is_some() => Ok(None),
        Err(error) => Err(error),
    }
}

fn default_api_base_url(host: &str) -> String {
    if host == "github.com" {
        "https://api.github.com".to_string()
    } else {
        format!("https://{host}/api/v3")
    }
}

fn default_web_base_url(host: &str) -> String {
    format!("https://{host}")
}

fn missing_init_value(flag: &str) -> QghError {
    QghError::validation(
        "validation.missing_init_value",
        format!("{flag} is required for non-interactive qgh init."),
    )
    .with_hint("Provide all required init flags with --yes.")
}

fn repo_from_origin_remote(root: &std::path::Path) -> Result<String, QghError> {
    Ok(git_remote_defaults_for_root(root)?.repo)
}

fn repo_policy_toml(repo: &str) -> String {
    format!(
        r#"schema_version = "qgh.repo.v1"

[repo]
github = "{repo}"

[defaults]
scope = "repo"
state = "all"
source_types = ["issue", "issue_comment"]
labels = []

[query]
limit = 10
"#
    )
}

#[derive(Debug)]
struct QueryFilters {
    repo: Option<String>,
    labels: Vec<String>,
    state: Option<String>,
    author: Option<String>,
    issue: Option<i64>,
    source_types: Vec<String>,
}

impl QueryFilters {
    fn from_args(
        args: &QueryArgs,
        profile: &Profile,
        repo_policy: Option<&RepoPolicy>,
        repo_scope: Option<&ResolvedRepoScope>,
    ) -> Result<Self, QghError> {
        if args.wiki.is_some() {
            return Err(QghError::validation(
                "validation.unsupported_filter",
                "Wiki filters are post-MVP and unsupported.",
            ));
        }
        let repo = effective_repo(args, profile, repo_policy, repo_scope)?;
        let state = effective_state(args, repo_policy)?;
        let issue = effective_issue(args.issue)?;
        let labels = effective_labels(args, repo_policy);
        let source_types = effective_source_types(repo_policy);
        Ok(Self {
            repo,
            labels,
            state,
            author: args.author.clone(),
            issue,
            source_types,
        })
    }

    fn matches(&self, source: &StoredSource) -> bool {
        match source {
            StoredSource::Issue(issue) => {
                self.source_type_matches("issue")
                    && self.repo_matches(&issue.repo)
                    && self.issue_matches(issue.number)
                    && self.author_matches(issue.author.as_deref())
                    && self.state_matches(Some(&issue.state))
                    && self.labels.iter().all(|label| issue.labels.contains(label))
            }
            StoredSource::Comment(comment) => {
                self.source_type_matches("issue_comment")
                    && self.repo_matches(&comment.repo)
                    && self.issue_matches(comment.issue_number)
                    && self.author_matches(comment.author.as_deref())
                    && self.state.is_none()
                    && self.labels.is_empty()
            }
        }
    }

    fn repo_matches(&self, repo: &str) -> bool {
        self.repo.as_deref().is_none_or(|expected| expected == repo)
    }

    fn issue_matches(&self, issue_number: i64) -> bool {
        self.issue.is_none_or(|expected| expected == issue_number)
    }

    fn author_matches(&self, author: Option<&str>) -> bool {
        self.author
            .as_deref()
            .is_none_or(|expected| author == Some(expected))
    }

    fn state_matches(&self, state: Option<&String>) -> bool {
        self.state
            .as_ref()
            .is_none_or(|expected| state == Some(expected))
    }

    fn source_type_matches(&self, source_type: &str) -> bool {
        self.source_types
            .iter()
            .any(|allowed| allowed == source_type)
    }
}

fn effective_repo(
    args: &QueryArgs,
    profile: &Profile,
    repo_policy: Option<&RepoPolicy>,
    repo_scope: Option<&ResolvedRepoScope>,
) -> Result<Option<String>, QghError> {
    if let Some(repo) = &args.repo {
        validate_repo(repo)?;
        if !profile.allows_repo(repo) {
            return Err(QghError::validation(
                "validation.invalid_repo",
                format!(
                    "Repo `{repo}` is outside profile `{}` allowlist.",
                    profile.id
                ),
            )
            .with_details(json!({
                "profile_id": profile.id,
                "repo": repo
            }))
            .with_hint("Use a repo from the profile allowlist or update the profile config."));
        }
        return Ok(Some(repo.clone()));
    }

    let Some(scope) = repo_scope else {
        return Ok(None);
    };
    let repo = scope.repo.clone();
    if !profile.allows_repo(&repo) {
        if let Some(repo_policy) = repo_policy {
            return Err(QghError::invalid_repo_policy(format!(
                "Repo policy repo `{repo}` is outside profile `{}` allowlist.",
                profile.id
            ))
            .with_details(json!({
                "profile_id": profile.id,
                "repo": repo,
                "repo_policy_path": repo_policy.path
            }))
            .with_hint("Update `.qgh.toml` or the profile repo allowlist."));
        }
        return Err(QghError::validation(
            "validation.invalid_repo",
            format!(
                "Repo `{repo}` is outside profile `{}` allowlist.",
                profile.id
            ),
        )
        .with_details(json!({
            "profile_id": profile.id,
            "repo": repo
        }))
        .with_hint("Use a repo from the profile allowlist or update the profile config."));
    }
    Ok(Some(repo))
}

fn effective_state(
    args: &QueryArgs,
    repo_policy: Option<&RepoPolicy>,
) -> Result<Option<String>, QghError> {
    if let Some(state) = &args.state {
        if !matches!(state.as_str(), "open" | "closed") {
            return Err(QghError::validation(
                "validation.invalid_state",
                "State filter must be `open` or `closed`.",
            ));
        }
        return Ok(Some(state.clone()));
    }
    Ok(repo_policy.and_then(|policy| policy.defaults.state.clone()))
}

fn effective_issue(issue: Option<i64>) -> Result<Option<i64>, QghError> {
    if issue.is_some_and(|issue| issue < 1) {
        return Err(QghError::validation(
            "validation.invalid_issue_number",
            "Issue number must be a positive integer.",
        )
        .with_details(json!({ "issue_number": issue })));
    }
    Ok(issue)
}

fn effective_labels(args: &QueryArgs, repo_policy: Option<&RepoPolicy>) -> Vec<String> {
    if !args.label.is_empty() {
        return args.label.clone();
    }
    repo_policy
        .map(|policy| policy.defaults.labels.clone())
        .unwrap_or_default()
}

fn effective_source_types(repo_policy: Option<&RepoPolicy>) -> Vec<String> {
    repo_policy
        .map(|policy| policy.defaults.source_types.clone())
        .unwrap_or_else(|| vec!["issue".to_string(), "issue_comment".to_string()])
}

fn effective_limit(args: &QueryArgs, repo_policy: Option<&RepoPolicy>) -> Result<usize, QghError> {
    let limit = args
        .limit
        .or_else(|| repo_policy.and_then(|policy| policy.query.limit))
        .unwrap_or(10);
    if limit == 0 {
        return Err(QghError::validation(
            "validation.invalid_query",
            "Query limit must be greater than zero.",
        ));
    }
    Ok(limit)
}

#[derive(Default)]
struct QueryResults {
    items: Vec<Value>,
    includes_open_issue: bool,
    repos: Vec<String>,
}

impl QueryResults {
    fn push(&mut self, source: StoredSource, ranking: Ranking, profile_id: &str) {
        if matches!(&source, StoredSource::Issue(issue) if issue.state == "open") {
            self.includes_open_issue = true;
        }
        let repo = match &source {
            StoredSource::Issue(issue) => &issue.repo,
            StoredSource::Comment(comment) => &comment.repo,
        };
        if !self.repos.contains(repo) {
            self.repos.push(repo.clone());
        }
        self.items.push(source_result(source, ranking, profile_id));
    }
}

fn embedding_warnings(profile: &Profile, store: &Store) -> Result<Vec<Value>, QghError> {
    let Some(embedding) = profile.embedding.as_ref() else {
        return Ok(Vec::new());
    };
    let Some(active) = store.active_embedding_fingerprint()? else {
        return Ok(Vec::new());
    };
    let expectation = embedding_fingerprint_expectation(embedding);
    if active.matches_expectation(&expectation) {
        return Ok(Vec::new());
    }
    Ok(vec![json!({
        "code": "embedding.fingerprint_mismatch",
        "severity": "warn_strong",
        "message": "Stored embeddings were created with a different embedding fingerprint and will not be used for vector search. BM25 results are still returned.",
        "hint": "Run `qgh embed --force` to recompute local embeddings."
    })])
}

fn embedding_fingerprint_expectation(
    embedding: &EmbeddingConfig,
) -> EmbeddingFingerprintExpectation {
    EmbeddingFingerprintExpectation {
        provider: embedding_provider_name(embedding.provider).to_string(),
        model_id: configured_embedding_model_id(embedding),
        model_revision: configured_embedding_model_revision(embedding),
        pooling: embedding.pooling,
        query_prefix: embedding.query_prefix.clone(),
    }
}

fn configured_embedding_model_id(embedding: &EmbeddingConfig) -> Option<String> {
    if embedding.model_path.is_some() {
        return embedding
            .model_path
            .as_ref()
            .map(|path| format!("model_path:{}", path.to_string_lossy()));
    }
    Some(configured_hf_model_reference(embedding).model_id)
}

fn configured_embedding_model_revision(embedding: &EmbeddingConfig) -> Option<String> {
    if embedding.model_path.is_some() {
        return Some(LOCAL_MODEL_REVISION.to_string());
    }
    // Stored fingerprints record the resolved commit sha. The query path
    // loads no model files and must stay offline, so a mutable configured
    // revision ("main", tags) cannot be asserted here; only an immutable
    // sha-pinned revision is comparable. Mutable revisions are resolved
    // and re-checked whenever the provider runtime loads (embed/search).
    let revision = configured_hf_model_reference(embedding).revision;
    if revision.len() == 40 && revision.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(revision)
    } else {
        None
    }
}

fn configured_hf_model_reference(
    embedding: &EmbeddingConfig,
) -> crate::embedding::HfModelReference {
    embedding
        .model
        .as_deref()
        .map(|model| parse_hf_model_reference(model).expect("validated embedding model reference"))
        .unwrap_or_else(default_hf_model_reference)
}

fn embedding_provider_name(provider: EmbeddingProviderKind) -> &'static str {
    match provider {
        EmbeddingProviderKind::Local => "local",
    }
}

fn query_freshness_sync_time(
    store: &Store,
    profile: &Profile,
    filters: &QueryFilters,
    results: &QueryResults,
) -> Result<Option<String>, QghError> {
    let repos = query_freshness_repos(profile, filters, results);
    store.oldest_successful_sync_at_for_repos(&repos)
}

fn query_freshness_repos(
    profile: &Profile,
    filters: &QueryFilters,
    results: &QueryResults,
) -> Vec<String> {
    if let Some(repo) = &filters.repo {
        return vec![repo.clone()];
    }
    if !results.repos.is_empty() {
        return results.repos.clone();
    }
    profile.repos.iter().map(|repo| repo.full_name()).collect()
}

fn exact_results(
    store: &Store,
    query_text: &str,
    filters: &QueryFilters,
    profile_id: &str,
) -> Result<Option<QueryResults>, QghError> {
    if let Some(source) = exact_url_result(store, query_text)? {
        let mut results = QueryResults::default();
        if filters.matches(&source) {
            results.push(source, Ranking::Exact, profile_id);
        }
        return Ok(Some(results));
    }
    let issue_number = filters.issue.or_else(|| parse_issue_number(query_text));
    let Some(issue_number) = issue_number else {
        return Ok(None);
    };
    let matches = if let Some(repo) = &filters.repo {
        store
            .find_issue_by_repo_number(repo, issue_number)?
            .into_iter()
            .collect::<Vec<_>>()
    } else {
        store.find_issues_by_number(issue_number)?
    };
    if matches.len() > 1 {
        return Err(QghError::validation(
            "validation.ambiguous_locator",
            "Issue number matches multiple repos; add --repo.",
        ));
    }
    let mut results = QueryResults::default();
    for source in matches.into_iter().map(StoredSource::Issue) {
        if filters.matches(&source) {
            results.push(source, Ranking::Exact, profile_id);
        }
    }
    Ok(Some(results))
}

fn exact_url_result(store: &Store, query_text: &str) -> Result<Option<StoredSource>, QghError> {
    if !query_text.starts_with("https://github.com/") {
        return Ok(None);
    }
    if query_text.contains("#issuecomment-") {
        return store
            .find_comment_by_canonical_url(query_text)
            .map(|comment| comment.map(StoredSource::Comment));
    }
    store
        .find_issue_by_canonical_url(query_text)
        .map(|issue| issue.map(StoredSource::Issue))
}

fn parse_issue_number(query_text: &str) -> Option<i64> {
    query_text
        .strip_prefix('#')
        .unwrap_or(query_text)
        .parse::<i64>()
        .ok()
}

fn validate_repo(repo: &str) -> Result<(), QghError> {
    let Some((owner, name)) = repo.split_once('/') else {
        return Err(QghError::validation(
            "validation.invalid_repo",
            "Repo filter must use owner/repo format.",
        ));
    };
    if owner.is_empty() || name.is_empty() || name.contains('/') || repo.contains('*') {
        return Err(QghError::validation(
            "validation.invalid_repo",
            "Repo filter must use explicit owner/repo format.",
        ));
    }
    Ok(())
}

fn enforce_source_scope(
    source_id: &str,
    source: &StoredSource,
    repo_scope: Option<&ResolvedRepoScope>,
) -> Result<(), QghError> {
    let Some(repo_scope) = repo_scope else {
        return Ok(());
    };
    let source_repo = match source {
        StoredSource::Issue(issue) => &issue.repo,
        StoredSource::Comment(comment) => &comment.repo,
    };
    if source_repo == &repo_scope.repo {
        return Ok(());
    }
    Err(QghError::source_outside_effective_scope(
        source_id,
        source_repo,
        &repo_scope.repo,
    ))
}

pub async fn get(
    profile_id: &str,
    source_id: &str,
    repo_scope: Option<&ResolvedRepoScope>,
    verify_lifecycle: bool,
) -> Result<Value, QghError> {
    let profile = load_profile(profile_id)?;
    let mut store = Store::open(&profile.paths)?;
    let source = get_source_for_get(
        &profile,
        &mut store,
        source_id,
        repo_scope,
        verify_lifecycle,
    )
    .await?;
    Ok(json!({
        "profile_id": profile.id,
        "source": source
    }))
}

pub async fn get_cli(
    profile_id: &str,
    source_ids: &[String],
    repo_scope: Option<&ResolvedRepoScope>,
    verify_lifecycle: bool,
) -> Result<Value, QghError> {
    if source_ids.len() == 1 {
        return get(profile_id, &source_ids[0], repo_scope, verify_lifecycle).await;
    }
    if source_ids.len() > GET_BATCH_SIZE_CAP {
        return Err(QghError::validation(
            "validation.batch_size",
            format!("get accepts at most {GET_BATCH_SIZE_CAP} source_id values per batch."),
        )
        .with_details(json!({
            "requested": source_ids.len(),
            "batch_size_cap": GET_BATCH_SIZE_CAP
        }))
        .with_hint("Split the source_id list into smaller qgh get batches."));
    }

    let profile = load_profile(profile_id)?;
    let mut store = Store::open(&profile.paths)?;
    let mut items = Vec::with_capacity(source_ids.len());
    let mut returned = 0;
    let mut failed = 0;
    for (input_index, source_id) in source_ids.iter().enumerate() {
        match get_source_for_get(
            &profile,
            &mut store,
            source_id,
            repo_scope,
            verify_lifecycle,
        )
        .await
        {
            Ok(source) => {
                returned += 1;
                items.push(json!({
                    "input_index": input_index,
                    "source_id": source_id,
                    "ok": true,
                    "source": source
                }));
            }
            Err(error) if is_get_item_error(&error) => {
                failed += 1;
                items.push(json!({
                    "input_index": input_index,
                    "source_id": source_id,
                    "ok": false,
                    "error": error
                }));
            }
            Err(error) => return Err(error),
        }
    }

    Ok(json!({
        "profile_id": profile.id,
        "summary": {
            "requested": source_ids.len(),
            "returned": returned,
            "failed": failed,
            "batch_size_cap": GET_BATCH_SIZE_CAP
        },
        "lifecycle_check_policy": {
            "verify_lifecycle": verify_lifecycle,
            "mode": if verify_lifecycle { "sequential" } else { "not_requested" },
            "max_in_flight_requests": if verify_lifecycle { 1 } else { 0 },
            "profile_max_in_flight_requests": profile.max_in_flight_requests,
            "hard_cap": 16
        },
        "items": items
    }))
}

async fn get_source_for_get(
    profile: &Profile,
    store: &mut Store,
    source_id: &str,
    repo_scope: Option<&ResolvedRepoScope>,
    verify_lifecycle: bool,
) -> Result<Value, QghError> {
    let mut source_json = get_source_base(store, source_id, repo_scope)?;
    source_json["lifecycle_check"] = if verify_lifecycle {
        lifecycle_check_for_get(profile, store, source_id).await?
    } else {
        json!({
            "status": "not_checked",
            "reason": "not_requested",
            "remote_checked": false
        })
    };
    Ok(source_json)
}

fn get_source_base(
    store: &Store,
    source_id: &str,
    repo_scope: Option<&ResolvedRepoScope>,
) -> Result<Value, QghError> {
    if let Some(tombstone) = store.get_tombstone(source_id)? {
        return Err(QghError::source_tombstoned(
            &tombstone.source_id,
            &tombstone.reason,
            &tombstone.observed_at,
        ));
    }
    let Some(source) = store.get_source(source_id)? else {
        return Err(QghError::source_not_found(source_id));
    };
    enforce_source_scope(source_id, &source, repo_scope)?;
    let source_json = match source {
        StoredSource::Issue(issue) => issue_source(issue),
        StoredSource::Comment(comment) => comment_source(comment),
    };
    Ok(source_json)
}

async fn lifecycle_check_for_get(
    profile: &Profile,
    store: &mut Store,
    source_id: &str,
) -> Result<Value, QghError> {
    Ok(match resolve_token(profile) {
        Ok(token) => {
            if let Some(candidate) = store.get_reconciliation_candidate(source_id)? {
                match github::check_source_lifecycle(profile, &token, &candidate).await {
                    Ok(github::LifecycleCheck::Active) => json!({
                        "status": "active",
                        "remote_checked": true
                    }),
                    Ok(github::LifecycleCheck::Unavailable { reason }) => {
                        let tombstone = store.tombstone_source(source_id, &reason)?;
                        return Err(QghError::source_tombstoned(
                            &tombstone.source_id,
                            &tombstone.reason,
                            &tombstone.observed_at,
                        ));
                    }
                    Err(error) => json!({
                        "status": "not_checked",
                        "error_code": error.code,
                        "remote_checked": false
                    }),
                }
            } else {
                json!({
                    "status": "not_checked",
                    "reason": "missing_candidate",
                    "remote_checked": false
                })
            }
        }
        Err(error) => json!({
            "status": "not_checked",
            "error_code": error.code,
            "remote_checked": false
        }),
    })
}

fn is_get_item_error(error: &QghError) -> bool {
    matches!(
        error.code.as_str(),
        "source.not_found" | "source.tombstoned" | "source.outside_effective_scope"
    )
}

pub fn status(
    profile_id: &str,
    args: &crate::cli::StatusArgs,
    repo_scope: Option<&ResolvedRepoScope>,
) -> Result<LocalReadOutcome, QghError> {
    let profile = load_profile(profile_id)?;
    let repo_policy = discover_repo_policy()?;
    let store = Store::open(&profile.paths)?;
    let status = store.status()?;
    let coverage = coverage::evaluate(&store.coverage_snapshot()?, false);
    let active_index_path = active_index_path(&store, &profile.paths.index_active)?;
    let last_successful_sync_at = match repo_scope {
        Some(scope) => {
            store.oldest_successful_sync_at_for_repos(std::slice::from_ref(&scope.repo))?
        }
        None => status.last_sync_at.clone(),
    };
    let freshness = freshness::evaluate(
        profile.freshness_settings(repo_policy.as_ref()),
        FreshnessContext {
            last_successful_sync_at: last_successful_sync_at.as_deref(),
            includes_open_issue: false,
            overrides: freshness_overrides(args.max_age.as_deref(), args.require_fresh)?,
        },
    )?;
    if freshness.fails {
        return Err(freshness_error(freshness.block, freshness.warnings));
    }
    let mut warnings = freshness.warnings;
    warnings.extend(embedding_warnings(&profile, &store)?);
    let source_count = (status.issue_count + status.comment_count) as usize;
    let age_days = status
        .last_reconciliation
        .as_ref()
        .and_then(|run| age_days(&run.completed_at));
    let age_seconds = status
        .last_reconciliation
        .as_ref()
        .and_then(|run| age_seconds(&run.completed_at));
    let stale = profile
        .reconcile_after_seconds
        .is_some_and(|seconds| age_seconds.is_none_or(|age| age > seconds));
    let stale_warning = if stale {
        json!("reconciliation.stale")
    } else {
        Value::Null
    };
    let last_reconciliation = status.last_reconciliation.as_ref();
    let cursors = status
        .cursors
        .iter()
        .map(|cursor| {
            (
                cursor.endpoint.clone(),
                json!({
                    "watermark": cursor.watermark,
                    "has_etag": cursor.has_etag
                }),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    Ok(LocalReadOutcome {
        data: json!({
        "profile_id": profile.id,
        "freshness": freshness.block,
        "coverage": coverage.block,
        "github": {
            "host": profile.host,
            "api_base_url": profile.api_base_url,
            "web_base_url": profile.web_base_url
        },
        "paths": {
            "config": profile.paths.config_file,
            "profile_data": profile.paths.profile_dir,
            "database": profile.paths.db_path,
            "tantivy_index": active_index_path,
            "cache": profile.paths.cache_dir,
            "logs": profile.paths.log_dir
        },
        "sources": {
            "issue_count": status.issue_count,
            "comment_count": status.comment_count,
            "tombstone_count": status.tombstone_count
        },
        "database": {
            "schema_version": "qgh.db.v1"
        },
        "index": {
            "active_generation": status.active_generation,
            "dirty_task_count": status.dirty_task_count
        },
        "sync": {
            "last_sync_at": status.last_sync_at,
            "cursors": cursors,
            "backoff": status.backoff,
            "scheduler": {
                "max_in_flight_requests": profile.max_in_flight_requests,
                "hard_cap": 16
            }
        },
        "reconciliation": {
            "last_full_at": last_reconciliation.map(|run| run.completed_at.clone()),
            "age_days": age_days,
            "stale": stale,
            "stale_warning": stale_warning,
            "estimated_api_cost_class": estimate_api_cost_class(source_count),
            "last_checked_source_count": last_reconciliation.map(|run| run.checked_source_count),
            "last_tombstoned_count": last_reconciliation.map(|run| run.tombstoned_count),
            "last_estimated_api_cost_class": last_reconciliation.map(|run| run.estimated_api_cost_class.clone())
        },
        "privacy": {
            "classification": "sensitive_derivative_data",
            "default_network_egress": "configured_github_host_only",
            "hosted_provider_egress": "disabled",
            "local_paths_may_contain_private_content": true,
            "single_user_permissions": "0600_files_0700_dirs_where_supported"
        }
        }),
        warnings,
    })
}

pub async fn doctor(profile_id: &str) -> Result<Value, QghError> {
    let profile = load_profile(profile_id)?;
    let store = Store::open(&profile.paths)?;
    let status = store.status()?;
    let permissions_ok = private_paths_ok(&profile.paths);
    let sqlite_ok = status.active_generation >= 0;
    let active_index_path = active_index_path(&store, &profile.paths.index_active)?;
    let tantivy_ok = !active_index_path.exists()
        || index::search(&active_index_path, "__qgh_doctor_probe__", 1).is_ok();
    let (github_ok, rate_limit_ok, rate_limit_headers) = match resolve_token(&profile) {
        Ok(token) => doctor_github_probe(&profile, &token).await,
        Err(_) => (false, false, rate_limit_headers_json(None, None)),
    };
    Ok(json!({
        "profile_id": profile.id,
        "checks": [
            {
                "name": "config",
                "ok": true
            },
            {
                "name": "file_permissions",
                "ok": permissions_ok
            },
            {
                "name": "sqlite",
                "ok": sqlite_ok
            },
            {
                "name": "tantivy",
                "ok": tantivy_ok
            },
            {
                "name": "github_auth_reachability",
                "ok": github_ok
            },
            {
                "name": "rate_limit_headers",
                "ok": rate_limit_ok,
                "headers": rate_limit_headers
            }
        ],
        "mcp": {
            "doctor_exposed": false,
            "tools": ["query", "get", "status"]
        }
    }))
}

fn active_index_path(store: &Store, fallback: &std::path::Path) -> Result<PathBuf, QghError> {
    Ok(store
        .active_index_path()?
        .map(PathBuf::from)
        .unwrap_or_else(|| fallback.to_path_buf()))
}

fn freshness_overrides(
    max_age: Option<&str>,
    require_fresh: bool,
) -> Result<FreshnessOverrides, QghError> {
    Ok(FreshnessOverrides {
        max_age_seconds: max_age
            .map(|value| {
                freshness::parse_duration_seconds("--max-age", value)
                    .map_err(|error| QghError::validation("validation.cli", error.message))
            })
            .transpose()?,
        require_fresh,
    })
}

fn freshness_error(freshness: Value, warnings: Vec<Value>) -> QghError {
    QghError::validation(
        "freshness.stale",
        "Local snapshot is stale under the active freshness policy.",
    )
    .with_details(json!({
        "freshness": freshness,
        "warnings": warnings
    }))
    .with_hint("Run qgh sync, increase --max-age for this run, or omit --require-fresh.")
}

enum Ranking {
    Bm25(f32),
    Exact,
}

fn source_result(source: StoredSource, ranking: Ranking, profile_id: &str) -> Value {
    match source {
        StoredSource::Issue(issue) => issue_result(issue, ranking, profile_id),
        StoredSource::Comment(comment) => comment_result(comment, ranking, profile_id),
    }
}

fn issue_result(issue: StoredIssue, ranking: Ranking, profile_id: &str) -> Value {
    let source_id = issue.source_id;
    json!({
        "source_id": source_id,
        "entity_type": "issue",
        "repo": issue.repo,
        "issue_number": issue.number,
        "title": issue.title,
        "canonical_url": issue.canonical_url,
        "snippet": snippet(&issue.body),
        "get_args": {
            "source_id": source_id,
            "profile_id": profile_id
        },
        "parent_issue": Value::Null,
        "source_version": issue.source_version,
        "ranking": ranking_json(ranking)
    })
}

fn comment_result(comment: StoredComment, ranking: Ranking, profile_id: &str) -> Value {
    let source_id = comment.source_id;
    json!({
        "source_id": source_id,
        "entity_type": "issue_comment",
        "repo": comment.repo,
        "issue_number": comment.issue_number,
        "author": comment.author,
        "canonical_url": comment.canonical_url,
        "parent_issue": comment.parent_issue,
        "snippet": snippet(&comment.body),
        "get_args": {
            "source_id": source_id,
            "profile_id": profile_id
        },
        "source_version": comment.source_version,
        "ranking": ranking_json(ranking)
    })
}

fn ranking_json(ranking: Ranking) -> Value {
    match ranking {
        Ranking::Bm25(score) => json!({
            "kind": "bm25",
            "lexical_score": score
        }),
        Ranking::Exact => json!({
            "kind": "exact",
            "lexical_score": Value::Null
        }),
    }
}

fn estimate_api_cost_class(source_count: usize) -> &'static str {
    match source_count {
        0 => "none",
        1..=100 => "low",
        101..=1000 => "medium",
        _ => "high",
    }
}

fn age_days(timestamp: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(timestamp).ok().map(|parsed| {
        Utc::now()
            .signed_duration_since(parsed.with_timezone(&Utc))
            .num_days()
            .max(0)
    })
}

fn age_seconds(timestamp: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(timestamp).ok().map(|parsed| {
        Utc::now()
            .signed_duration_since(parsed.with_timezone(&Utc))
            .num_seconds()
            .max(0)
    })
}

async fn doctor_github_probe(profile: &crate::config::Profile, token: &str) -> (bool, bool, Value) {
    let url = format!("{}/rate_limit", profile.api_base_url);
    let response = reqwest::Client::new()
        .get(url)
        .bearer_auth(token)
        .header("accept", "application/vnd.github+json")
        .header("user-agent", github::user_agent())
        .header("x-github-api-version", github::GITHUB_API_VERSION)
        .send()
        .await;
    let Ok(response) = response else {
        return (false, false, rate_limit_headers_json(None, None));
    };
    let headers = response.headers();
    let remaining = headers
        .get("x-ratelimit-remaining")
        .and_then(|value| value.to_str().ok())
        .map(ToString::to_string);
    let reset = headers
        .get("x-ratelimit-reset")
        .and_then(|value| value.to_str().ok())
        .map(ToString::to_string);
    let rate_limit_ok = remaining.is_some();
    (
        response.status().is_success(),
        rate_limit_ok,
        rate_limit_headers_json(remaining, reset),
    )
}

fn rate_limit_headers_json(remaining: Option<String>, reset: Option<String>) -> Value {
    json!({
        "x-ratelimit-remaining": remaining,
        "x-ratelimit-reset": reset
    })
}

fn private_paths_ok(paths: &crate::paths::ProfilePaths) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let dirs = [
            &paths.profile_dir,
            &paths.cache_dir,
            &paths.log_dir,
            &paths.index_active,
        ];
        for dir in dirs.into_iter().filter(|path| path.exists()) {
            let Ok(metadata) = std::fs::metadata(dir) else {
                return false;
            };
            if metadata.permissions().mode() & 0o077 != 0 {
                return false;
            }
        }
        if paths.db_path.exists() {
            let Ok(metadata) = std::fs::metadata(&paths.db_path) else {
                return false;
            };
            if metadata.permissions().mode() & 0o077 != 0 {
                return false;
            }
        }
    }
    true
}

fn issue_source(issue: StoredIssue) -> Value {
    json!({
        "source_id": issue.source_id,
        "entity_type": "issue",
        "repo": issue.repo,
        "issue_number": issue.number,
        "title": issue.title,
        "body": issue.body,
        "canonical_url": issue.canonical_url,
        "source_version": issue.source_version
    })
}

fn comment_source(comment: StoredComment) -> Value {
    json!({
        "source_id": comment.source_id,
        "entity_type": "issue_comment",
        "repo": comment.repo,
        "issue_number": comment.issue_number,
        "author": comment.author,
        "body": comment.body,
        "canonical_url": comment.canonical_url,
        "parent_issue": comment.parent_issue,
        "source_version": comment.source_version
    })
}

fn snippet(body: &str) -> String {
    const MAX: usize = 180;
    if body.len() <= MAX {
        return body.to_string();
    }
    let mut end = MAX;
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &body[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunking::MarkdownChunk;
    use crate::embedding::PoolingKind;
    use crate::model::IssueRecord;
    use crate::paths::ProfilePaths;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct MockEmbeddingProvider;

    impl EmbeddingProvider for MockEmbeddingProvider {
        fn embed_documents(
            &self,
            texts: &[&str],
        ) -> Result<Vec<EmbeddingVector>, EmbeddingProviderError> {
            Ok(texts
                .iter()
                .enumerate()
                .map(|(index, _)| vec![index as f32, 1.0, 2.0])
                .collect())
        }

        fn embed_query(&self, _text: &str) -> Result<EmbeddingVector, EmbeddingProviderError> {
            Ok(vec![0.0, 1.0, 2.0])
        }
    }

    #[test]
    fn force_refresh_persists_vectors_under_new_fingerprint() {
        let paths = temp_profile_paths("command-embed-force");
        let mut store = Store::open(&paths).unwrap();
        let source_id = "qgh://github.com/issue/I_COMMAND_EMBED";
        let issue = IssueRecord {
            source_id: source_id.to_string(),
            host: "github.com".to_string(),
            repo: "owner/repo".to_string(),
            node_id: "I_COMMAND_EMBED".to_string(),
            github_id: 303,
            number: 9,
            title: "Command embed".to_string(),
            body: "alpha beta gamma delta".to_string(),
            state: "open".to_string(),
            labels: Vec::new(),
            milestone: None,
            assignees: Vec::new(),
            author: Some("alice".to_string()),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-02T00:00:00Z".to_string(),
            closed_at: None,
            canonical_url: "https://github.com/owner/repo/issues/9".to_string(),
            body_hash: "body-hash-command-embed".to_string(),
            indexed_at: "2026-01-02T00:00:01Z".to_string(),
        };
        store
            .upsert_sources_for_run("sync-command-embed", &[issue], &[], 0, &[])
            .unwrap();
        let source_version_id = store.latest_source_version_id(source_id).unwrap().unwrap();
        store
            .replace_chunks_for_source_version(
                source_id,
                source_version_id,
                &[
                    MarkdownChunk {
                        chunk_index: 0,
                        byte_start: 0,
                        byte_end: 10,
                        token_start: 0,
                        token_end: 2,
                        token_count: 2,
                        body: "alpha beta".to_string(),
                    },
                    MarkdownChunk {
                        chunk_index: 1,
                        byte_start: 11,
                        byte_end: 22,
                        token_start: 2,
                        token_end: 4,
                        token_count: 2,
                        body: "gamma delta".to_string(),
                    },
                ],
            )
            .unwrap();
        let seed = EmbeddingFingerprintSeed {
            provider: "local".to_string(),
            model_id: "Snowflake/snowflake-arctic-embed-l-v2.0".to_string(),
            model_revision: "fixture-sha".to_string(),
            pooling: PoolingKind::Cls,
            query_prefix: crate::embedding::DEFAULT_QUERY_PREFIX.to_string(),
        };

        let outcome = refresh_chunk_embeddings(&mut store, &MockEmbeddingProvider, seed).unwrap();
        let active = store.active_embedding_fingerprint().unwrap().unwrap();

        assert_eq!(outcome["embedded_chunks"], 2);
        assert_eq!(outcome["usable_embeddings"], 2);
        assert_eq!(active.dimension, 3);
        assert_eq!(active.model_revision, "fixture-sha");
        assert_eq!(
            store
                .current_chunk_embedding_count_for_fingerprint(&active)
                .unwrap(),
            2
        );

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    fn temp_profile_paths(name: &str) -> ProfilePaths {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let profile_dir = std::env::temp_dir().join(format!("qgh-commands-{name}-{nanos}"));
        let cache_dir = profile_dir.join("cache");
        let log_dir = cache_dir.join("logs");
        let index_root = profile_dir.join("tantivy");
        ProfilePaths {
            config_file: profile_dir.join("config.toml"),
            db_path: profile_dir.join("qgh.sqlite3"),
            index_active: index_root.join("active"),
            index_root,
            log_dir,
            cache_dir,
            profile_dir,
        }
    }
}
