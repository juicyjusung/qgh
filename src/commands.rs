use crate::chunking::{chunk_markdown, CHUNKER_FINGERPRINT};
use crate::cli::{EmbedArgs, InitArgs, InitRepoArgs, InitTokenSourceArg, QueryArgs, ReconcileMode};
use crate::config::{
    bootstrap_profile_repo, current_git_worktree_root, discover_repo_policy,
    git_remote_defaults_for_root, load_profile, load_repo_policy_at, parse_repo, resolve_token,
    suggest_init_profile_id, CommentsMode, EmbeddingConfig, EmbeddingProviderKind, GitRemote,
    Profile, ProfileBootstrapInput, RepoPolicy, RepoRef, TokenSource,
};
use crate::coverage;
use crate::embedding::{
    builtin_preset_hf_reference, default_hf_model_reference, parse_hf_model_reference,
    EmbeddingFingerprint, EmbeddingFingerprintExpectation, EmbeddingFingerprintSeed,
    EmbeddingProvider, EmbeddingProviderError, EmbeddingTokenizer, EmbeddingVector,
    LOCAL_MODEL_REVISION,
};
#[cfg(feature = "fastembed-provider")]
use crate::embedding::{
    default_prepared_model_store, validate_batch_comparability, FastembedEngine,
    FastembedTokenizer, LocalEmbeddingProvider, ModelManifestV1, ModelSourceV1,
    PreparedModelInspection, PreparedModelSnapshot, PreparedModelStore,
};
#[cfg(debug_assertions)]
use crate::embedding::{PoolingKind, TokenSpan, DEFAULT_QUERY_PREFIX};
use crate::error::QghError;
use crate::freshness::{self, FreshnessContext, FreshnessOverrides};
use crate::github;
use crate::index;
use crate::model::{
    ReconciliationCandidate, StoredChunk, StoredComment, StoredIssue, StoredSource, SyncSummary,
    TargetedSyncSummary, VectorSearchFilters,
};
use crate::paths::ProfilePaths;
use crate::resolution::ResolvedRepoScope;
use crate::store::{
    PendingPurgeView, PurgeTarget, PurgeTrigger, RetrievalBuildSnapshot, RetrievalPublicationView,
    Store,
};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
#[cfg(feature = "fastembed-provider")]
use std::sync::{Arc, Mutex, OnceLock};

const GET_BATCH_SIZE_CAP: usize = 20;
const HYBRID_RRF_K: f32 = 60.0;
const STALE_BUILDING_RETENTION_HOURS: i64 = 24;
const PREVIOUS_READY_RETENTION_DAYS: i64 = 7;
const HYBRID_OVERFETCH_FACTOR: usize = 4;

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
    let parsed_window_seconds = window
        .map(|value| freshness::parse_duration_seconds("window", value))
        .transpose()?;
    if let Some(value) = max_duration {
        freshness::parse_duration_seconds("max_duration", value)?;
    }
    let if_stale_max_age_seconds = if if_stale {
        Some(match max_age {
            Some(value) => freshness::parse_duration_seconds("max_age", value)?,
            None => profile
                .sync_max_age_seconds
                .unwrap_or(DEFAULT_SYNC_MAX_AGE_SECONDS),
        })
    } else {
        None
    };
    let fetch_profile = profile_scoped_to_repo(&profile, repo_scope)?;
    let mut store = Store::open(&profile.paths)?;
    run_sync_purge_preflight(&profile, &mut store)?;
    let token = resolve_token(&profile)?;

    // `--if-stale`: skip the network sync entirely when the local snapshot is
    // still within max-age. Never-synced always proceeds.
    if let Some(max_age_seconds) = if_stale_max_age_seconds {
        let last_sync = store.status()?.last_sync_at;
        if let Some(last_sync_at) = last_sync.as_deref() {
            let snapshot_age_seconds = freshness::snapshot_age_seconds(last_sync_at)?;
            if snapshot_age_seconds <= max_age_seconds {
                match store.resolve_active_tantivy_artifact() {
                    Ok(_) => {
                        progress.line(format_args!(
                            "qgh sync: skipped, snapshot fresh age={snapshot_age_seconds}s max_age={max_age_seconds}s"
                        ));
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
                            Vec::new(),
                        ));
                    }
                    Err(error)
                        if matches!(
                            error.code.as_str(),
                            "publication.source_snapshot_incomplete"
                                | "publication.source_snapshot_changed"
                                | "publication.embedding_snapshot_mismatch"
                                | "publication.tantivy_artifact_not_ready"
                                | "publication.source_inventory_mismatch"
                        ) =>
                    {
                        progress.line(format_args!(
                            "qgh sync: fresh remote snapshot requires retrieval publication repair"
                        ));
                    }
                    Err(error) => return Err(error),
                }
            }
        }
    }

    let cursors = store.sync_cursors()?;
    let per_issue_comments = profile.comments_mode == CommentsMode::PerIssue;

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
        github::fetch_issues_classified(
            &fetch_profile,
            &token,
            &cursors,
            per_issue_comments,
            Some(&progress),
            &mut commit_page,
        )
        .await?
    };
    let github::ClassifiedFetchOutcome {
        result: fetched,
        interruption,
        terminal_error,
    } = fetched;
    let purge_evidence = confirmed_fetch_purge_requests(
        &fetched.confirmed_permission_lost_repos,
        &fetched.confirmed_source_deletions,
    );
    queue_and_finish_purges(&mut store, &purge_evidence.requests)?;
    if let Some(error) = purge_evidence.deferred_error {
        repair_lexical_successor_if_required(&profile, &mut store)?;
        return Err(error);
    }
    if let Some(error) = terminal_error {
        repair_lexical_successor_if_required(&profile, &mut store)?;
        return Err(error);
    }
    if let Some(interruption) = interruption {
        repair_lexical_successor_if_required(&profile, &mut store)?;
        match interruption_disposition(interruption) {
            InterruptionDisposition::Error(error) => return Err(error),
            InterruptionDisposition::Backoff(backoff) => {
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
                let warnings = if summary.is_some() {
                    vec![incomplete_snapshot_publication_warning()]
                } else {
                    Vec::new()
                };
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
                    warnings,
                ));
            }
        }
    }
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
            github::fetch_repo_comments_classified(
                &fetch_profile,
                &token,
                &comment_cursors,
                budget,
                &resolve,
                Some(&progress),
            )
            .await?
        };
        let purge_evidence =
            confirmed_fetch_purge_requests(&outcome.confirmed_permission_lost_repos, &[]);
        let queued_purge_requests = queue_purge_requests(&mut store, &purge_evidence.requests)?;
        // Process whatever was fetched (possibly partial) after confirmed
        // lifecycle evidence is durable. A pending guard keeps same-repo rows
        // fail closed until the refreshed target mapping is purged below.
        let page_summary = store.upsert_sources_for_run_under_pending_purge(
            &sync_run_id,
            &[],
            &outcome.comments,
            0,
            &outcome.cursor_updates,
        )?;
        merge_sync_summary(&mut summary, page_summary);
        repo_comment_stats = Some((outcome.skipped_pr_comments, outcome.deferred_comments));
        queue_purge_requests(&mut store, &queued_purge_requests)?;
        finish_pending_purges(&mut store)?;
        if let Some(error) = purge_evidence.deferred_error {
            repair_lexical_successor_if_required(&profile, &mut store)?;
            return Err(error);
        }
        if let Some(error) = outcome.terminal_error {
            repair_lexical_successor_if_required(&profile, &mut store)?;
            return Err(error);
        }
        let mut backoff = outcome.backoff;
        if let Some(interruption) = outcome.interruption {
            repair_lexical_successor_if_required(&profile, &mut store)?;
            match interruption_disposition(interruption) {
                InterruptionDisposition::Backoff(plan) => backoff = Some(plan),
                InterruptionDisposition::Error(error) => return Err(error),
            }
        }
        if let Some(backoff) = backoff {
            repair_lexical_successor_if_required(&profile, &mut store)?;
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
                vec![incomplete_snapshot_publication_warning()],
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
                    let window_seconds =
                        parsed_window_seconds.unwrap_or(DEFAULT_RECONCILE_WINDOW_SECONDS);
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
            let purge_evidence = reconciliation_purge_requests(
                &result.unavailable_sources,
                &result.confirmed_permission_lost_repos,
            );
            let purge = queue_and_finish_purges(&mut store, &purge_evidence.requests)?;
            if let Some(error) = purge_evidence.deferred_error {
                repair_lexical_successor_if_required(&profile, &mut store)?;
                return Err(error);
            }
            if let Some(error) = result.terminal_error {
                repair_lexical_successor_if_required(&profile, &mut store)?;
                return Err(error);
            }
            if let Some(interruption) = result.interruption {
                repair_lexical_successor_if_required(&profile, &mut store)?;
                match interruption_disposition(interruption) {
                    InterruptionDisposition::Error(error) => return Err(error),
                    InterruptionDisposition::Backoff(backoff) => {
                        return sync_backoff_outcome(
                            &profile,
                            &mut store,
                            backoff,
                            vec![incomplete_snapshot_publication_warning()],
                        )
                    }
                }
            }
            let tombstoned_sources = purge.purged_sources;
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
    store.mark_sync_run_completed(&summary.sync_run_id)?;
    let repair = repair_lexical_successor_if_required(&profile, &mut store)?;
    let index = rebuild_after_successor_repair(&profile, &mut store, &progress, repair)?;
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

fn sync_backoff_outcome(
    profile: &Profile,
    store: &mut Store,
    backoff: github::BackoffPlan,
    warnings: Vec<Value>,
) -> Result<LocalReadOutcome, QghError> {
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
        warnings,
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
        github::fetch_backfill_issues_classified(
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
    let purge_evidence = confirmed_fetch_purge_requests(
        &outcome.confirmed_permission_lost_repos,
        &outcome.confirmed_source_deletions,
    );
    queue_and_finish_purges(store, &purge_evidence.requests)?;
    if let Some(error) = purge_evidence.deferred_error {
        repair_lexical_successor_if_required(profile, store)?;
        return Err(error);
    }
    if let Some(error) = outcome.terminal_error.clone() {
        repair_lexical_successor_if_required(profile, store)?;
        return Err(error);
    }
    let interruption_backoff = match outcome.interruption.clone() {
        None => None,
        Some(interruption) => {
            repair_lexical_successor_if_required(profile, store)?;
            match interruption_disposition(interruption) {
                InterruptionDisposition::Backoff(plan) => Some(plan),
                InterruptionDisposition::Error(error) => return Err(error),
            }
        }
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

    if let Some(backoff) = outcome.backoff.or(interruption_backoff) {
        repair_lexical_successor_if_required(profile, store)?;
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
        let warnings = vec![incomplete_snapshot_publication_warning()];
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
            warnings,
        ));
    }

    store.clear_backoff_state()?;
    if let Some(summary) = &summary {
        store.mark_sync_run_completed(&summary.sync_run_id)?;
    }
    let repair = repair_lexical_successor_if_required(profile, store)?;
    let index = rebuild_after_successor_repair(profile, store, progress, repair)?;
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
    let mut store = Store::open(&profile.paths)?;
    run_sync_purge_preflight(&profile, &mut store)?;
    let token = resolve_token(&profile)?;
    progress.line(format_args!(
        "qgh sync issue: fetching repo={} issue_number={issue_number}",
        repo.full_name()
    ));

    let outcome = github::fetch_target_issue_classified(
        &profile,
        &token,
        &repo,
        issue_number,
        Some(&progress),
    )
    .await?;
    let transition_evidence = target_transition_purge_requests(&outcome.confirmed_transitions);
    let mut purge_requests = transition_evidence.requests;
    if let github::ClassifiedTargetIssueTerminal::Confirmed {
        state,
        repo: confirmed_repo,
        issue_number: confirmed_issue_number,
        ..
    } = &outcome.terminal
    {
        purge_requests.push(terminal_confirmed_purge_request(
            *state,
            confirmed_repo,
            *confirmed_issue_number,
        ));
    }
    queue_purge_requests(&mut store, &purge_requests)?;
    if let Some(error) = transition_evidence.deferred_error {
        finish_confirmed_target_purges(&mut store)?;
        repair_lexical_successor_if_required(&profile, &mut store)?;
        return Err(error);
    }
    match outcome.terminal {
        github::ClassifiedTargetIssueTerminal::Backoff(backoff) => {
            finish_confirmed_target_purges(&mut store)?;
            repair_lexical_successor_if_required(&profile, &mut store)?;
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
        github::ClassifiedTargetIssueTerminal::AuthenticationFailed => {
            finish_confirmed_target_purges(&mut store)?;
            repair_lexical_successor_if_required(&profile, &mut store)?;
            Err(QghError::auth(
                "GitHub authentication failed during lifecycle verification.",
            ))
        }
        github::ClassifiedTargetIssueTerminal::Transient(_)
        | github::ClassifiedTargetIssueTerminal::AmbiguousForbidden => {
            finish_confirmed_target_purges(&mut store)?;
            repair_lexical_successor_if_required(&profile, &mut store)?;
            Err(QghError::github(
                "GitHub request ended without a confirmed destructive lifecycle state.",
            ))
        }
        github::ClassifiedTargetIssueTerminal::Failed(error) => {
            finish_confirmed_target_purges(&mut store)?;
            repair_lexical_successor_if_required(&profile, &mut store)?;
            Err(error)
        }
        github::ClassifiedTargetIssueTerminal::Fetched(fetched) => {
            progress.line(format_args!(
                "qgh sync issue: fetched issue=1 comments={}",
                fetched.comments.len()
            ));
            let incoming_comment_ids = fetched
                .comments
                .iter()
                .map(|comment| comment.source_id.as_str())
                .collect::<BTreeSet<_>>();
            let deleted_comment_ids = store
                .active_comment_source_ids_for_issue(&fetched.issue.repo, fetched.issue.number)?
                .into_iter()
                .filter(|source_id| !incoming_comment_ids.contains(source_id.as_str()))
                .collect::<Vec<_>>();
            purge_requests.extend(deleted_comment_ids.iter().map(|source_id| {
                (
                    PurgeTarget::Source {
                        source_id: source_id.clone(),
                    },
                    PurgeTrigger::ConfirmedDelete,
                )
            }));
            queue_purge_requests(&mut store, &purge_requests)?;
            let purged = finish_confirmed_target_purges(&mut store)?;
            let mut summary =
                store.upsert_target_issue_refresh(&fetched.issue, &fetched.comments)?;
            summary.deleted_comments += purged.purged_comments;
            summary.tombstoned_issues += purged.purged_issues;
            summary.tombstoned_comments += purged.purged_comments;
            let repair = repair_lexical_successor_if_required(&profile, &mut store)?;
            progress.line(format_args!(
                "qgh sync issue: stored comments added={} updated={} deleted={}",
                summary.added_comments, summary.updated_comments, summary.deleted_comments
            ));
            store.clear_backoff_state()?;
            let index = rebuild_after_successor_repair(&profile, &mut store, &progress, repair)?;
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
        github::ClassifiedTargetIssueTerminal::Confirmed {
            state,
            repo: _,
            issue_number: _,
            lifecycle,
        } => {
            let reason = state.reason();
            progress.line(format_args!(
                "qgh sync issue: lifecycle status={} reason={}",
                lifecycle.status, reason
            ));
            let purged = finish_confirmed_target_purges(&mut store)?;
            let successor = repair_lexical_successor_if_required(&profile, &mut store)?;
            let sync_run_id = match &successor {
                SuccessorRepairOutcome::Repaired {
                    source_snapshot_sync_run_id,
                    ..
                } => source_snapshot_sync_run_id.clone(),
                SuccessorRepairOutcome::NotRequired => store
                    .active_retrieval_publication()?
                    .map(|publication| publication.source_snapshot_sync_run_id)
                    .or(store.latest_successful_sync_run_id()?)
                    .ok_or_else(|| {
                        QghError::new(
                            "purge.successor_repair_state_invalid",
                            "Completed lifecycle cleanup has no published source snapshot.",
                            6,
                        )
                    })?,
            };
            let summary = TargetedSyncSummary {
                sync_run_id,
                fetched_issues: 0,
                upserted_issues: 0,
                fetched_comments: 0,
                upserted_comments: 0,
                added_comments: 0,
                updated_comments: 0,
                deleted_comments: purged.purged_comments,
                tombstoned_issues: purged.purged_issues,
                tombstoned_comments: purged.purged_comments,
            };
            store.clear_backoff_state()?;
            let index = rebuild_after_successor_repair(&profile, &mut store, &progress, successor)?;
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
    let Some(source_snapshot_sync_run_id) = store.latest_successful_sync_run_id()? else {
        let status = store.status()?;
        let generation = store
            .active_retrieval_publication()?
            .map(|publication| publication.tantivy_generation)
            .unwrap_or(status.active_generation);
        return Ok(IndexRebuildOutcome {
            generation,
            dirty_task_count: status.dirty_task_count,
            warnings: vec![incomplete_snapshot_publication_warning()],
        });
    };
    let (mut warnings, embedding_prepared) =
        prepare_embedding_chunks_for_sync_if_enabled(profile, store, progress);
    store.mark_sync_run_completed(&source_snapshot_sync_run_id)?;
    let Some(snapshot) = store.capture_retrieval_build_snapshot()? else {
        let status = store.status()?;
        return Ok(IndexRebuildOutcome {
            generation: status.active_generation,
            dirty_task_count: status.dirty_task_count,
            warnings: vec![incomplete_snapshot_publication_warning()],
        });
    };
    let embedding_generation_id = if embedding_prepared {
        match refresh_incremental_chunk_embeddings_for_snapshot(
            store,
            profile
                .embedding
                .as_ref()
                .expect("prepared embedding config"),
            &snapshot,
        ) {
            Ok((embedded_chunks, generation_id)) => {
                progress.line(format_args!(
                    "qgh sync: refreshed chunk embeddings embedded={embedded_chunks}"
                ));
                generation_id
            }
            Err(_) => {
                warnings.push(embedding_sync_warning(
                    "embedding.sync_refresh_failed",
                    "Embedding refresh failed during sync. BM25 index refresh remains available.",
                ));
                None
            }
        }
    } else {
        None
    };
    let sources = snapshot.sources();
    progress.line(format_args!(
        "qgh sync: rebuilding BM25 index sources={}",
        sources.len()
    ));
    let (generation, reserved_generation_path) =
        store.reserve_index_generation_for_snapshot(&profile.paths.index_root, &snapshot)?;
    let generation_path = store.rebuild_reserved_index_generation(generation, sources)?;
    debug_assert_eq!(generation_path, reserved_generation_path);
    match store.activate_retrieval_publication(
        snapshot.identity().sync_run_id(),
        generation,
        embedding_generation_id,
        snapshot.expected_publication_id(),
    ) {
        Ok(_) if embedding_generation_id.is_some() => {
            match cleanup_old_embedding_generations(store) {
                Ok(removed) if removed > 0 => progress.line(format_args!(
                    "qgh sync: cleaned expired embedding generations count={removed}"
                )),
                Ok(_) => {}
                Err(_) => warnings.push(embedding_sync_warning(
                    "embedding.generation_cleanup_failed",
                    "Expired embedding generation cleanup failed after publication. Retrieval remains available.",
                )),
            }
        }
        Ok(_) => {}
        Err(error) => {
            let valid_previous = snapshot.expected_publication_id().is_some_and(|expected| {
                store
                    .active_retrieval_publication()
                    .ok()
                    .flatten()
                    .is_some_and(|publication| publication.publication_id == expected)
                    && matches!(store.resolve_active_tantivy_artifact(), Ok(Some(_)))
            });
            if !valid_previous {
                return Err(error);
            }
            warnings.push(embedding_sync_warning(
                "publication.activation_failed",
                "Retrieval publication activation failed after BM25 rebuild. The previous publication remains active.",
            ));
        }
    }
    let status = store.status()?;
    let active_generation = store
        .active_retrieval_publication()?
        .map(|publication| publication.tantivy_generation)
        .unwrap_or(status.active_generation);
    progress.line(format_args!(
        "qgh sync: active BM25 index generation={} built_generation={} sources={}",
        active_generation,
        generation,
        sources.len()
    ));
    Ok(IndexRebuildOutcome {
        generation: active_generation,
        dirty_task_count: status.dirty_task_count,
        warnings,
    })
}

fn incomplete_snapshot_publication_warning() -> Value {
    embedding_sync_warning(
        "publication.incomplete_snapshot_deferred",
        "Retrieval publication was deferred because the current source sync is incomplete. The previous publication remains active.",
    )
}

fn incomplete_source_snapshot_error_for_command() -> QghError {
    QghError::new(
        "publication.source_snapshot_incomplete",
        "A retrieval build requires a completed source snapshot at the current epoch.",
        6,
    )
}

/// Publishes a source-safe lexical successor after destructive lifecycle
/// cleanup. This path never initializes embedding/model/vector capability and
/// supports an empty authoritative corpus.
#[derive(Clone, Debug, PartialEq, Eq)]
enum SuccessorRepairOutcome {
    NotRequired,
    Repaired {
        generation: i64,
        source_snapshot_sync_run_id: String,
    },
}

fn repair_lexical_successor_if_required(
    profile: &Profile,
    store: &mut Store,
) -> Result<SuccessorRepairOutcome, QghError> {
    if !store.pending_purges()?.is_empty() {
        return Err(QghError::new(
            "purge.successor_blocked",
            "A lexical successor cannot be published while purge work is pending.",
            6,
        ));
    }
    if !store.successor_repair_required()? {
        return Ok(SuccessorRepairOutcome::NotRequired);
    }
    let source_snapshot_sync_run_id =
        store.record_purge_successor_snapshot()?.ok_or_else(|| {
            QghError::new(
                "purge.successor_repair_state_invalid",
                "Purge successor repair state changed before snapshot creation.",
                6,
            )
        })?;
    let snapshot = store
        .capture_retrieval_build_snapshot()?
        .ok_or_else(incomplete_source_snapshot_error_for_command)?;
    if snapshot.identity().sync_run_id() != source_snapshot_sync_run_id {
        return Err(incomplete_source_snapshot_error_for_command());
    }
    let sources = snapshot.sources();
    let (generation, reserved_generation_path) =
        store.reserve_index_generation_for_snapshot(&profile.paths.index_root, &snapshot)?;
    let generation_path = store.rebuild_reserved_index_generation(generation, sources)?;
    debug_assert_eq!(generation_path, reserved_generation_path);
    store.activate_retrieval_publication(
        &source_snapshot_sync_run_id,
        generation,
        None,
        snapshot.expected_publication_id(),
    )?;
    Ok(SuccessorRepairOutcome::Repaired {
        generation,
        source_snapshot_sync_run_id,
    })
}

fn run_sync_purge_preflight(profile: &Profile, store: &mut Store) -> Result<(), QghError> {
    let mut failed = false;
    match store.retry_pending_purges() {
        Ok(_) => {}
        Err(_) => failed = true,
    }

    let configured = configured_repository_identity_keys(profile);
    let remaining_before_reconciliation = store
        .pending_purges()?
        .into_iter()
        .filter_map(|pending| match pending.target {
            PurgeTarget::Repository { repo } => Some(github_repo_identity_key(&repo)),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let removed_repositories = store
        .known_repositories()?
        .into_iter()
        .filter(|repo| !configured.contains(&github_repo_identity_key(repo)))
        .filter(|repo| !remaining_before_reconciliation.contains(&github_repo_identity_key(repo)))
        .map(|repo| {
            (
                PurgeTarget::Repository { repo },
                PurgeTrigger::AllowlistRemoval,
            )
        })
        .collect::<Vec<_>>();
    if store.queue_purges(&removed_repositories).is_err()
        || (!failed && store.retry_pending_purges().is_err())
    {
        failed = true;
    }

    let remaining = store.pending_purges()?;
    if failed || !remaining.is_empty() {
        return Err(purge_retry_error(&remaining));
    }
    repair_lexical_successor_if_required(profile, store)?;
    Ok(())
}

fn rebuild_after_successor_repair(
    profile: &Profile,
    store: &mut Store,
    progress: &StderrSyncProgress,
    repair: SuccessorRepairOutcome,
) -> Result<IndexRebuildOutcome, QghError> {
    if let SuccessorRepairOutcome::Repaired { generation, .. } = repair {
        let status = store.status()?;
        return Ok(IndexRebuildOutcome {
            generation,
            dirty_task_count: status.dirty_task_count,
            warnings: Vec::new(),
        });
    }
    rebuild_bm25_index(profile, store, progress)
}

#[derive(Default)]
struct PurgeBatchOutcome {
    purged_sources: usize,
    purged_issues: usize,
    purged_comments: usize,
}

fn queue_and_finish_purges(
    store: &mut Store,
    requests: &[(PurgeTarget, PurgeTrigger)],
) -> Result<PurgeBatchOutcome, QghError> {
    queue_purge_requests(store, requests)?;
    finish_pending_purges(store)
}

fn queue_purge_requests(
    store: &mut Store,
    requests: &[(PurgeTarget, PurgeTrigger)],
) -> Result<Vec<(PurgeTarget, PurgeTrigger)>, QghError> {
    let requests = canonicalize_purge_requests(requests);
    store.queue_purges(&requests)?;
    Ok(requests)
}

fn finish_pending_purges(store: &mut Store) -> Result<PurgeBatchOutcome, QghError> {
    let outcomes = match store.retry_pending_purges() {
        Ok(outcomes) => outcomes,
        Err(_) => {
            let remaining = store.pending_purges()?;
            return Err(purge_retry_error(&remaining));
        }
    };
    let remaining = store.pending_purges()?;
    if !remaining.is_empty() {
        return Err(purge_retry_error(&remaining));
    }
    Ok(PurgeBatchOutcome {
        purged_sources: outcomes.iter().map(|outcome| outcome.purged_sources).sum(),
        purged_issues: outcomes.iter().map(|outcome| outcome.purged_issues).sum(),
        purged_comments: outcomes.iter().map(|outcome| outcome.purged_comments).sum(),
    })
}

fn canonicalize_purge_requests(
    requests: &[(PurgeTarget, PurgeTrigger)],
) -> Vec<(PurgeTarget, PurgeTrigger)> {
    let repository_targets = requests
        .iter()
        .filter_map(|(target, _)| match target {
            PurgeTarget::Repository { repo } => Some(github_repo_identity_key(repo)),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let mut canonical = Vec::new();
    for (target, trigger) in requests {
        let subsumed = match target {
            PurgeTarget::Repository { .. } => false,
            PurgeTarget::Issue { repo, .. } => {
                repository_targets.contains(&github_repo_identity_key(repo))
            }
            PurgeTarget::Source { .. } => false,
        };
        if !subsumed {
            canonical.push((target.clone(), *trigger));
        }
    }
    canonical
}

fn github_repo_identity_key(repo: &str) -> String {
    repo.to_ascii_lowercase()
}

fn configured_repository_identity_keys(profile: &Profile) -> BTreeSet<String> {
    profile
        .repos
        .iter()
        .map(RepoRef::full_name)
        .map(|repo| github_repo_identity_key(&repo))
        .collect()
}

fn purge_retry_error(remaining: &[PendingPurgeView]) -> QghError {
    let target_kinds = remaining
        .iter()
        .map(|pending| pending.target.kind())
        .collect::<BTreeSet<_>>();
    let triggers = remaining
        .iter()
        .map(|pending| pending.trigger.as_str())
        .collect::<BTreeSet<_>>();
    let current_stages = remaining
        .iter()
        .map(|pending| pending.current_stage.as_str())
        .collect::<BTreeSet<_>>();
    let failure_stages = remaining
        .iter()
        .filter_map(|pending| pending.failure_stage.map(|stage| stage.as_str()))
        .collect::<BTreeSet<_>>();
    QghError::new(
        "purge.retry_failed",
        "One or more lifecycle purges remain pending after all targets were attempted.",
        6,
    )
    .with_details(json!({
        "pending_count": remaining.len(),
        "target_kinds": target_kinds,
        "triggers": triggers,
        "current_stages": current_stages,
        "failure_stages": failure_stages
    }))
}

struct PurgeEvidenceRequests {
    requests: Vec<(PurgeTarget, PurgeTrigger)>,
    deferred_error: Option<QghError>,
}

fn confirmed_fetch_purge_requests(
    permission_losses: &[github::ConfirmedRepositoryPermissionLoss],
    source_deletions: &[github::ConfirmedSourceDeletion],
) -> PurgeEvidenceRequests {
    let permission_lost_repos = permission_losses
        .iter()
        .map(|confirmed| github_repo_identity_key(&confirmed.repo))
        .collect::<BTreeSet<_>>();
    let mut requests = permission_losses
        .iter()
        .map(|confirmed| {
            (
                PurgeTarget::Repository {
                    repo: confirmed.repo.clone(),
                },
                PurgeTrigger::PermissionLoss,
            )
        })
        .collect::<Vec<_>>();
    let mut deferred_error = None;
    for confirmed in source_deletions {
        if permission_lost_repos.contains(&github_repo_identity_key(&confirmed.repo)) {
            continue;
        }
        let target = match confirmed.entity_type.as_str() {
            "issue" => PurgeTarget::Issue {
                repo: confirmed.repo.clone(),
                issue_number: confirmed.issue_number,
            },
            "issue_comment" => PurgeTarget::Source {
                source_id: confirmed.source_id.clone(),
            },
            _ => {
                deferred_error.get_or_insert_with(|| {
                    QghError::new(
                        "purge.lifecycle_candidate_missing",
                        "Confirmed lifecycle evidence has an unsupported source type.",
                        6,
                    )
                });
                continue;
            }
        };
        requests.push((target, PurgeTrigger::ConfirmedDelete));
    }
    PurgeEvidenceRequests {
        requests,
        deferred_error,
    }
}

enum InterruptionDisposition {
    Backoff(github::BackoffPlan),
    Error(QghError),
}

fn interruption_disposition(
    interruption: github::LifecycleInterruption,
) -> InterruptionDisposition {
    match interruption {
        github::LifecycleInterruption::AuthenticationFailed => InterruptionDisposition::Error(
            QghError::auth("GitHub authentication failed during lifecycle verification."),
        ),
        github::LifecycleInterruption::Transient(_)
        | github::LifecycleInterruption::AmbiguousForbidden => {
            InterruptionDisposition::Error(QghError::github(
                "GitHub request ended without a confirmed destructive lifecycle state.",
            ))
        }
        github::LifecycleInterruption::Backoff(plan) => InterruptionDisposition::Backoff(plan),
    }
}

fn reconciliation_purge_requests(
    failures: &[github::LifecycleFailure],
    permission_losses: &[github::ConfirmedRepositoryPermissionLoss],
) -> PurgeEvidenceRequests {
    let mut requests = permission_losses
        .iter()
        .map(|confirmed| {
            (
                PurgeTarget::Repository {
                    repo: confirmed.repo.clone(),
                },
                PurgeTrigger::PermissionLoss,
            )
        })
        .collect::<Vec<_>>();
    let mut deferred_error = None;
    for failure in failures {
        let target = match failure.entity_type.as_str() {
            "issue" => PurgeTarget::Issue {
                repo: failure.repo.clone(),
                issue_number: failure.issue_number,
            },
            "issue_comment" => PurgeTarget::Source {
                source_id: failure.source_id.clone(),
            },
            _ => {
                deferred_error.get_or_insert_with(|| {
                    QghError::new(
                        "purge.lifecycle_candidate_missing",
                        "Confirmed lifecycle evidence has an unsupported source type.",
                        6,
                    )
                });
                continue;
            }
        };
        let trigger = match failure.state {
            github::ConfirmedRemoteState::SourceDeleted => PurgeTrigger::ConfirmedDelete,
            github::ConfirmedRemoteState::SourceTransferred => PurgeTrigger::ConfirmedTombstone,
            github::ConfirmedRemoteState::RepositoryPermissionLoss => {
                deferred_error.get_or_insert_with(|| {
                    QghError::new(
                        "purge.lifecycle_candidate_missing",
                        "Repository permission evidence must identify the repository target.",
                        6,
                    )
                });
                requests.push((
                    PurgeTarget::Repository {
                        repo: failure.repo.clone(),
                    },
                    PurgeTrigger::PermissionLoss,
                ));
                continue;
            }
        };
        requests.push((target, trigger));
    }
    PurgeEvidenceRequests {
        requests,
        deferred_error,
    }
}

fn target_transition_purge_requests(
    transitions: &[github::ConfirmedIssueTransition],
) -> PurgeEvidenceRequests {
    let mut requests = Vec::with_capacity(transitions.len());
    let mut deferred_error = None;
    for transition in transitions {
        let request = match transition.state {
            github::ConfirmedRemoteState::SourceTransferred => (
                PurgeTarget::Issue {
                    repo: transition.source_repo.clone(),
                    issue_number: transition.source_issue_number,
                },
                PurgeTrigger::ConfirmedTombstone,
            ),
            github::ConfirmedRemoteState::SourceDeleted => (
                PurgeTarget::Issue {
                    repo: transition.source_repo.clone(),
                    issue_number: transition.source_issue_number,
                },
                PurgeTrigger::ConfirmedDelete,
            ),
            github::ConfirmedRemoteState::RepositoryPermissionLoss => (
                PurgeTarget::Repository {
                    repo: transition.source_repo.clone(),
                },
                PurgeTrigger::PermissionLoss,
            ),
        };
        if transition.state != github::ConfirmedRemoteState::SourceTransferred {
            deferred_error.get_or_insert_with(|| {
                QghError::new(
                    "purge.lifecycle_candidate_missing",
                    "Confirmed issue transition has an invalid lifecycle state.",
                    6,
                )
            });
        }
        requests.push(request);
    }
    PurgeEvidenceRequests {
        requests,
        deferred_error,
    }
}

fn terminal_confirmed_purge_request(
    state: github::ConfirmedRemoteState,
    repo: &str,
    issue_number: i64,
) -> (PurgeTarget, PurgeTrigger) {
    match state {
        github::ConfirmedRemoteState::SourceDeleted => (
            PurgeTarget::Issue {
                repo: repo.to_string(),
                issue_number,
            },
            PurgeTrigger::ConfirmedDelete,
        ),
        github::ConfirmedRemoteState::SourceTransferred => (
            PurgeTarget::Issue {
                repo: repo.to_string(),
                issue_number,
            },
            PurgeTrigger::ConfirmedTombstone,
        ),
        github::ConfirmedRemoteState::RepositoryPermissionLoss => (
            PurgeTarget::Repository {
                repo: repo.to_string(),
            },
            PurgeTrigger::PermissionLoss,
        ),
    }
}

fn candidate_confirmed_purge_request(
    candidate: &ReconciliationCandidate,
    state: github::ConfirmedRemoteState,
) -> (PurgeTarget, PurgeTrigger) {
    match state {
        github::ConfirmedRemoteState::RepositoryPermissionLoss => (
            PurgeTarget::Repository {
                repo: candidate.repo.clone(),
            },
            PurgeTrigger::PermissionLoss,
        ),
        github::ConfirmedRemoteState::SourceDeleted
        | github::ConfirmedRemoteState::SourceTransferred => {
            let target = if candidate.entity_type == "issue" {
                PurgeTarget::Issue {
                    repo: candidate.repo.clone(),
                    issue_number: candidate.issue_number,
                }
            } else {
                PurgeTarget::Source {
                    source_id: candidate.source_id.clone(),
                }
            };
            let trigger = if state == github::ConfirmedRemoteState::SourceTransferred {
                PurgeTrigger::ConfirmedTombstone
            } else {
                PurgeTrigger::ConfirmedDelete
            };
            (target, trigger)
        }
    }
}

fn finish_confirmed_target_purges(store: &mut Store) -> Result<PurgeBatchOutcome, QghError> {
    let outcome = finish_pending_purges(store)?;
    if outcome.purged_sources != outcome.purged_issues + outcome.purged_comments {
        return Err(QghError::new(
            "purge.retry_failed",
            "Confirmed lifecycle cleanup completed with an inconsistent source count.",
            6,
        )
        .with_details(json!({
            "expected_source_count": outcome.purged_issues + outcome.purged_comments,
            "purged_source_count": outcome.purged_sources
        })));
    }
    Ok(outcome)
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

fn prepare_embedding_chunks_for_sync_if_enabled(
    profile: &Profile,
    store: &mut Store,
    progress: &StderrSyncProgress,
) -> (Vec<Value>, bool) {
    let Some(embedding) = profile.embedding.as_ref() else {
        return (Vec::new(), false);
    };

    let mut warnings = Vec::new();
    if store.enable_vector().is_err() {
        warnings.push(embedding_sync_warning(
            "embedding.sync_vector_init_failed",
            "Local vector storage initialization failed during sync. BM25 index refresh remains available.",
        ));
        return (warnings, false);
    }
    match store.cleanup_tombstoned_embedding_artifacts() {
        Ok(cleaned_chunks) if cleaned_chunks > 0 => progress.line(format_args!(
            "qgh sync: purged tombstoned embedding artifacts chunks={cleaned_chunks}"
        )),
        Ok(_) => {}
        Err(_) => warnings.push(embedding_sync_warning(
            "embedding.tombstone_cleanup_failed",
            "Tombstoned embedding artifact cleanup failed during sync. BM25 results remain available.",
        )),
    }
    let tokenizer = match embedding_tokenizer(embedding) {
        Ok(tokenizer) => tokenizer,
        Err(_) => {
            warnings.push(embedding_sync_warning(
                "embedding.sync_tokenizer_failed",
                "Prepared embedding model acquisition or tokenizer initialization failed during sync. BM25 index refresh remains available.",
            ));
            return (warnings, false);
        }
    };
    if refresh_embedding_chunks(store, tokenizer.as_ref(), progress).is_err() {
        warnings.push(embedding_sync_warning(
            "embedding.sync_chunking_failed",
            "Embedding chunk refresh failed during sync. BM25 index refresh remains available.",
        ));
        return (warnings, false);
    }
    (warnings, true)
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
        if store.source_version_chunks_match_fingerprint(source_version_id, CHUNKER_FINGERPRINT)? {
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
    Ok(refresh_incremental_chunk_embeddings_with_generation(store, embedding)?.0)
}

fn refresh_incremental_chunk_embeddings_with_generation(
    store: &mut Store,
    embedding: &EmbeddingConfig,
) -> Result<(usize, Option<i64>), QghError> {
    let Some(snapshot) = store.capture_retrieval_build_snapshot()? else {
        return Ok((0, None));
    };
    refresh_incremental_chunk_embeddings_for_snapshot(store, embedding, &snapshot)
}

fn refresh_incremental_chunk_embeddings_for_snapshot(
    store: &mut Store,
    embedding: &EmbeddingConfig,
    snapshot: &RetrievalBuildSnapshot,
) -> Result<(usize, Option<i64>), QghError> {
    let runtime = embedding_runtime_local_only(embedding, None)?;
    let expectation = embedding_fingerprint_expectation(embedding);
    refresh_incremental_chunk_embeddings_with_provider_and_generation(
        store,
        runtime.provider.as_ref(),
        runtime.model_manifest_hash.clone(),
        runtime.fingerprint_seed.clone(),
        &expectation,
        snapshot,
    )
}

fn refresh_incremental_chunk_embeddings_with_provider(
    store: &mut Store,
    provider: &dyn EmbeddingProvider,
    model_manifest_hash: String,
    fingerprint_seed: EmbeddingFingerprintSeed,
    expectation: &EmbeddingFingerprintExpectation,
) -> Result<usize, QghError> {
    let Some(snapshot) = store.capture_retrieval_build_snapshot()? else {
        return Ok(0);
    };
    Ok(
        refresh_incremental_chunk_embeddings_with_provider_and_generation(
            store,
            provider,
            model_manifest_hash,
            fingerprint_seed,
            expectation,
            &snapshot,
        )?
        .0,
    )
}

fn refresh_incremental_chunk_embeddings_with_provider_and_generation(
    store: &mut Store,
    provider: &dyn EmbeddingProvider,
    model_manifest_hash: String,
    fingerprint_seed: EmbeddingFingerprintSeed,
    _expectation: &EmbeddingFingerprintExpectation,
    snapshot: &RetrievalBuildSnapshot,
) -> Result<(usize, Option<i64>), QghError> {
    let chunks = snapshot.embedding_chunks();
    if chunks.is_empty() {
        return Ok((0, None));
    }

    let texts = chunks
        .iter()
        .map(|chunk| chunk.prepared_input.as_str())
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
    let runtime_fingerprint_hash = fingerprint.hash();
    let context_template_version = crate::context::METADATA_CONTEXT_TEMPLATE_VERSION.to_string();
    let spec = crate::store::EmbeddingGenerationSpec {
        model_manifest_hash: model_manifest_hash.clone(),
        runtime_fingerprint_hash,
        chunker_fingerprint: chunks
            .first()
            .map(|chunk| chunk.chunk.chunker_fingerprint.clone())
            .unwrap_or_else(|| "none".to_string()),
        context_template_version: context_template_version.clone(),
        output_dimension: dimension,
    };
    let generation_id = store.begin_embedding_generation(snapshot, &spec)?;
    let embeddings = chunks.iter().zip(vectors).collect::<Vec<_>>();
    for batch in embeddings.chunks(32) {
        let staged = batch
            .iter()
            .map(|(chunk, vector)| {
                Ok(crate::store::EmbeddingGenerationChunk {
                    chunk_id: chunk.chunk.chunk_id,
                    source_version_id: chunk.chunk.source_version_id,
                    source_version_hash: store
                        .source_version_hash(chunk.chunk.source_version_id)?
                        .ok_or_else(|| QghError::storage("Missing source version hash."))?,
                    context_hash: chunk
                        .prepared_input
                        .context_hash(&model_manifest_hash, &chunk.chunk.chunker_fingerprint),
                    vector: (*vector).clone(),
                })
            })
            .collect::<Result<Vec<_>, QghError>>()?;
        store.stage_embedding_generation_batch(generation_id, &staged)?;
    }
    store.validate_embedding_generation(generation_id)?;
    Ok((chunks.len(), Some(generation_id)))
}

fn embedding_sync_warning(code: &'static str, message: &'static str) -> Value {
    json!({
        "code": code,
        "severity": "warn",
        "message": message
    })
}

fn cleanup_old_embedding_generations(store: &mut Store) -> Result<usize, QghError> {
    let now = Utc::now();
    let stale_building_before = (now - Duration::hours(STALE_BUILDING_RETENTION_HOURS))
        .to_rfc3339_opts(SecondsFormat::Secs, true);
    let previous_ready_before = (now - Duration::days(PREVIOUS_READY_RETENTION_DAYS))
        .to_rfc3339_opts(SecondsFormat::Secs, true);
    let removed_generations =
        store.cleanup_embedding_generations(&stale_building_before, &previous_ready_before)?;
    let removed_chunks = store.cleanup_inactive_embedding_artifacts()?;
    Ok(removed_generations + removed_chunks)
}

#[cfg(feature = "fastembed-provider")]
fn embedding_tokenizer(
    embedding: &EmbeddingConfig,
) -> Result<Box<dyn EmbeddingTokenizer>, QghError> {
    match embedding.provider {
        EmbeddingProviderKind::Local => {
            let options = embedding.fastembed_options();
            let prepared_store = default_prepared_model_store().map_err(embedding_error)?;
            let snapshot = prepared_store.acquire(&options).map_err(embedding_error)?;
            FastembedTokenizer::from_prepared_snapshot(&snapshot)
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
    let profile = load_profile(profile_id)?;
    let Some(embedding) = profile.embedding.as_ref() else {
        return Err(QghError::validation(
            "embedding.not_configured",
            "Embedding is not configured for this profile.",
        )
        .with_hint("Add an [embedding] section before running `qgh embed --force`."));
    };

    if !args.force {
        return Err(QghError::validation(
            "embedding.force_required",
            "`qgh embed` requires --force for this full-refresh slice.",
        )
        .with_hint("Run `qgh embed --force` to recompute every stored chunk embedding."));
    }

    let mut store = Store::open(&profile.paths)?;
    store.enable_vector()?;
    let runtime = embedding_runtime_for_acquisition(embedding)?;
    let progress = StderrSyncProgress::new(false);
    let chunk_stats = refresh_embedding_chunks(&mut store, runtime.tokenizer.as_ref(), &progress)?;
    let source_snapshot_sync_run_id = if store.successor_repair_required()? {
        store
            .record_purge_successor_snapshot()?
            .ok_or_else(incomplete_source_snapshot_error_for_command)?
    } else {
        store
            .record_local_rebuild_snapshot()?
            .sync_run_id()
            .to_string()
    };
    let snapshot = store
        .capture_retrieval_build_snapshot()?
        .ok_or_else(incomplete_source_snapshot_error_for_command)?;
    if snapshot.identity().sync_run_id() != source_snapshot_sync_run_id {
        return Err(incomplete_source_snapshot_error_for_command());
    }
    let data = refresh_chunk_embeddings(
        &mut store,
        &profile.paths,
        runtime.provider.as_ref(),
        runtime.model_manifest_hash.clone(),
        runtime.fingerprint_seed.clone(),
        &snapshot,
    )?;
    let mut warnings = Vec::new();
    let publication_activated = store
        .active_retrieval_publication()?
        .and_then(|publication| publication.embedding_generation_id)
        .is_some();
    if publication_activated {
        match cleanup_old_embedding_generations(&mut store) {
            Ok(removed) if removed > 0 => progress.line(format_args!(
                "qgh embed: cleaned expired embedding generations count={removed}"
            )),
            Ok(_) => {}
            Err(_) => warnings.push(embedding_sync_warning(
                "embedding.generation_cleanup_failed",
                "Expired embedding generation cleanup failed after publication. Retrieval remains available.",
            )),
        }
    }
    Ok(LocalReadOutcome {
        data: json!({
            "profile_id": profile.id,
            "embedding_state": "refreshed",
            "chunks": {
                "refreshed": chunk_stats.refreshed_chunks,
                "embedded": data["embedded_chunks"]
            }
        }),
        warnings,
    })
}

struct EmbeddingRuntime {
    tokenizer: Box<dyn EmbeddingTokenizer>,
    provider: Box<dyn EmbeddingProvider>,
    model_manifest_hash: String,
    fingerprint_seed: EmbeddingFingerprintSeed,
}

#[cfg(feature = "fastembed-provider")]
static EMBEDDING_RUNTIME_CACHE: OnceLock<Mutex<HashMap<String, Arc<EmbeddingRuntime>>>> =
    OnceLock::new();

#[cfg(debug_assertions)]
const TEST_EMBEDDING_QUERY_VECTORS_ENV: &str = "QGH_TEST_EMBEDDING_QUERY_VECTORS";
#[cfg(debug_assertions)]
const TEST_EMBEDDING_DOCUMENT_VECTORS_ENV: &str = "QGH_TEST_EMBEDDING_DOCUMENT_VECTORS";

#[cfg(debug_assertions)]
struct TestEmbeddingProvider {
    document_vectors: HashMap<String, EmbeddingVector>,
    query_vectors: HashMap<String, EmbeddingVector>,
}

#[cfg(debug_assertions)]
impl EmbeddingProvider for TestEmbeddingProvider {
    fn embed_documents(
        &self,
        texts: &[&str],
    ) -> Result<Vec<EmbeddingVector>, EmbeddingProviderError> {
        texts
            .iter()
            .map(|text| {
                self.document_vectors.get(*text).cloned().ok_or_else(|| {
                    EmbeddingProviderError::structured(
                        "embedding.test_document_vector_missing",
                        "Test embedding provider has no vector for this document.",
                    )
                })
            })
            .collect()
    }

    fn embed_query(&self, text: &str) -> Result<EmbeddingVector, EmbeddingProviderError> {
        self.query_vectors.get(text).cloned().ok_or_else(|| {
            EmbeddingProviderError::structured(
                "embedding.test_query_vector_missing",
                "Test embedding provider has no vector for this query.",
            )
        })
    }
}

#[cfg(debug_assertions)]
struct TestEmbeddingTokenizer;

#[cfg(debug_assertions)]
impl EmbeddingTokenizer for TestEmbeddingTokenizer {
    fn tokenize(&self, text: &str) -> Result<Vec<TokenSpan>, EmbeddingProviderError> {
        if text.is_empty() {
            Ok(Vec::new())
        } else {
            Ok(vec![TokenSpan {
                start: 0,
                end: text.len(),
            }])
        }
    }
}

#[cfg(debug_assertions)]
fn test_embedding_runtime(
    embedding: &EmbeddingConfig,
) -> Result<Option<EmbeddingRuntime>, QghError> {
    let query_vectors = test_embedding_vectors_from_env(TEST_EMBEDDING_QUERY_VECTORS_ENV)?;
    let document_vectors = test_embedding_vectors_from_env(TEST_EMBEDDING_DOCUMENT_VECTORS_ENV)?;
    if query_vectors.is_none() && document_vectors.is_none() {
        return Ok(None);
    }
    let query_vectors = query_vectors.unwrap_or_default();
    let document_vectors = document_vectors.unwrap_or_default();
    let Some(dimension) = query_vectors
        .values()
        .chain(document_vectors.values())
        .next()
        .map(Vec::len)
    else {
        return Err(QghError::validation(
            "embedding.test_vectors_empty",
            "Test embedding vector env vars must contain at least one vector.",
        ));
    };
    if dimension == 0
        || query_vectors
            .values()
            .chain(document_vectors.values())
            .any(|vector| vector.len() != dimension)
    {
        return Err(QghError::validation(
            "embedding.test_vectors_dimension_mismatch",
            "Test embedding vectors must be non-empty and share one dimension.",
        ));
    }
    let configured = configured_embedding_snapshot(embedding);
    Ok(Some(EmbeddingRuntime {
        tokenizer: Box::new(TestEmbeddingTokenizer),
        provider: Box::new(TestEmbeddingProvider {
            document_vectors,
            query_vectors,
        }),
        model_manifest_hash: "f4f58582f743f03f94eb63915d7bb93f54328dd9a9b0c258eb6eb578456f7946"
            .to_string(),
        fingerprint_seed: EmbeddingFingerprintSeed {
            provider: embedding_provider_name(embedding.provider).to_string(),
            model_id: configured
                .model_id
                .unwrap_or_else(|| "qgh-test-embedding".to_string()),
            model_revision: configured
                .model_revision
                .unwrap_or_else(|| LOCAL_MODEL_REVISION.to_string()),
            pooling: embedding.pooling.unwrap_or(PoolingKind::Cls),
            query_prefix: embedding
                .query_prefix
                .clone()
                .unwrap_or_else(|| DEFAULT_QUERY_PREFIX.to_string()),
        },
    }))
}

#[cfg(debug_assertions)]
fn test_embedding_vectors_from_env(
    env: &str,
) -> Result<Option<HashMap<String, EmbeddingVector>>, QghError> {
    let raw_vectors = match std::env::var(env) {
        Ok(raw_vectors) => raw_vectors,
        Err(std::env::VarError::NotPresent) => return Ok(None),
        Err(error) => {
            return Err(QghError::validation(
                "embedding.test_vectors_invalid",
                format!("{env} must be valid UTF-8: {error}"),
            ));
        }
    };
    serde_json::from_str(&raw_vectors)
        .map(Some)
        .map_err(|error| {
            QghError::validation(
                "embedding.test_vectors_invalid",
                format!("{env} must be a JSON object of vectors: {error}"),
            )
        })
}

#[cfg(not(debug_assertions))]
fn test_embedding_runtime(
    _embedding: &EmbeddingConfig,
) -> Result<Option<EmbeddingRuntime>, QghError> {
    Ok(None)
}

#[cfg(feature = "fastembed-provider")]
#[derive(Clone, Copy)]
enum PreparedModelAccess {
    Acquire,
    LoadLocal,
}

#[cfg(feature = "fastembed-provider")]
fn embedding_runtime_for_acquisition(
    embedding: &EmbeddingConfig,
) -> Result<Arc<EmbeddingRuntime>, QghError> {
    embedding_runtime_with_access(embedding, PreparedModelAccess::Acquire, None)
}

#[cfg(feature = "fastembed-provider")]
fn embedding_runtime_local_only(
    embedding: &EmbeddingConfig,
    cache_profile_id: Option<&str>,
) -> Result<Arc<EmbeddingRuntime>, QghError> {
    embedding_runtime_with_access(embedding, PreparedModelAccess::LoadLocal, cache_profile_id)
}

#[cfg(feature = "fastembed-provider")]
fn embedding_runtime_with_access(
    embedding: &EmbeddingConfig,
    access: PreparedModelAccess,
    cache_profile_id: Option<&str>,
) -> Result<Arc<EmbeddingRuntime>, QghError> {
    if let Some(runtime) = test_embedding_runtime(embedding)? {
        return Ok(Arc::new(runtime));
    }
    match embedding.provider {
        EmbeddingProviderKind::Local => {
            let options = embedding.fastembed_options();
            let prepared_store = default_prepared_model_store().map_err(embedding_error)?;
            match access {
                PreparedModelAccess::Acquire => {
                    let snapshot = prepared_store.acquire(&options).map_err(embedding_error)?;
                    build_embedding_runtime(embedding, &snapshot)
                }
                PreparedModelAccess::LoadLocal => {
                    let inspection = prepared_store.inspect(&options).map_err(embedding_error)?;
                    if let Some(profile_id) = cache_profile_id {
                        let cache_key = prepared_runtime_cache_key(profile_id, &inspection);
                        let profile_prefix = format!("{profile_id}:");
                        return runtime_cache_get_or_try_init(
                            EMBEDDING_RUNTIME_CACHE.get_or_init(|| Mutex::new(HashMap::new())),
                            &cache_key,
                            &profile_prefix,
                            || {
                                let snapshot =
                                    prepared_store.verify(inspection).map_err(embedding_error)?;
                                build_embedding_runtime(embedding, &snapshot)
                            },
                        );
                    }
                    let snapshot = prepared_store.verify(inspection).map_err(embedding_error)?;
                    build_embedding_runtime(embedding, &snapshot)
                }
            }
        }
    }
}

#[cfg(feature = "fastembed-provider")]
fn prepared_runtime_cache_key(profile_id: &str, inspection: &PreparedModelInspection) -> String {
    format!(
        "{profile_id}:{}:{}",
        inspection.manifest_hash(),
        inspection.artifact_stamp()
    )
}

#[cfg(feature = "fastembed-provider")]
fn runtime_cache_get_or_try_init<T, E>(
    cache: &Mutex<HashMap<String, Arc<T>>>,
    cache_key: &str,
    profile_prefix: &str,
    initialize: impl FnOnce() -> Result<Arc<T>, E>,
) -> Result<Arc<T>, E> {
    if let Some(runtime) = cache
        .lock()
        .expect("embedding runtime cache mutex poisoned")
        .get(cache_key)
        .cloned()
    {
        return Ok(runtime);
    }
    let runtime = initialize()?;
    let mut cache = cache
        .lock()
        .expect("embedding runtime cache mutex poisoned");
    if let Some(existing) = cache.get(cache_key).cloned() {
        return Ok(existing);
    }
    cache.retain(|key, _| !key.starts_with(profile_prefix));
    cache.insert(cache_key.to_string(), Arc::clone(&runtime));
    Ok(runtime)
}

#[cfg(feature = "fastembed-provider")]
fn build_embedding_runtime(
    embedding: &EmbeddingConfig,
    snapshot: &PreparedModelSnapshot,
) -> Result<Arc<EmbeddingRuntime>, QghError> {
    let tokenizer = FastembedTokenizer::from_prepared_snapshot(snapshot)
        .map(|tokenizer| Box::new(tokenizer) as Box<dyn EmbeddingTokenizer>)
        .map_err(embedding_error)?;
    let engine = FastembedEngine::from_prepared_snapshot(snapshot).map_err(embedding_error)?;
    let provider =
        LocalEmbeddingProvider::with_contract(engine, snapshot.manifest.runtime_contract())
            .map_err(embedding_error)?;
    validate_batch_comparability(&provider, "qgh prepared model smoke").map_err(embedding_error)?;
    Ok(Arc::new(EmbeddingRuntime {
        tokenizer,
        provider: Box::new(provider),
        model_manifest_hash: snapshot.manifest.hash(),
        fingerprint_seed: embedding_fingerprint_seed(embedding, snapshot),
    }))
}

#[cfg(not(feature = "fastembed-provider"))]
fn embedding_runtime_for_acquisition(
    embedding: &EmbeddingConfig,
) -> Result<std::sync::Arc<EmbeddingRuntime>, QghError> {
    embedding_runtime_unavailable(embedding)
}

#[cfg(not(feature = "fastembed-provider"))]
fn embedding_runtime_local_only(
    embedding: &EmbeddingConfig,
    _cache_profile_id: Option<&str>,
) -> Result<std::sync::Arc<EmbeddingRuntime>, QghError> {
    embedding_runtime_unavailable(embedding)
}

#[cfg(not(feature = "fastembed-provider"))]
fn embedding_runtime_unavailable(
    embedding: &EmbeddingConfig,
) -> Result<std::sync::Arc<EmbeddingRuntime>, QghError> {
    if let Some(runtime) = test_embedding_runtime(embedding)? {
        return Ok(std::sync::Arc::new(runtime));
    }
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
    snapshot: &PreparedModelSnapshot,
) -> EmbeddingFingerprintSeed {
    EmbeddingFingerprintSeed {
        provider: embedding_provider_name(embedding.provider).to_string(),
        model_id: prepared_model_id(snapshot),
        model_revision: snapshot.manifest.hash(),
        pooling: snapshot.manifest.pooling,
        query_prefix: snapshot.manifest.query_prefix.clone().unwrap_or_default(),
    }
}

#[cfg(feature = "fastembed-provider")]
fn prepared_model_id(snapshot: &PreparedModelSnapshot) -> String {
    prepared_manifest_model_id(&snapshot.manifest)
}

#[cfg(feature = "fastembed-provider")]
fn prepared_manifest_model_id(manifest: &ModelManifestV1) -> String {
    match &manifest.model_source {
        ModelSourceV1::Hf { model_id, .. } => model_id.clone(),
        ModelSourceV1::Local { declared_id } => format!("local:{declared_id}"),
    }
}

fn refresh_chunk_embeddings(
    store: &mut Store,
    paths: &ProfilePaths,
    provider: &dyn EmbeddingProvider,
    model_manifest_hash: String,
    fingerprint_seed: EmbeddingFingerprintSeed,
    snapshot: &RetrievalBuildSnapshot,
) -> Result<Value, QghError> {
    let chunks = snapshot.embedding_chunks();
    if chunks.is_empty() {
        return Err(QghError::validation(
            "embedding.no_chunks",
            "No active chunks are available to embed.",
        )
        .with_hint("Run `qgh sync` with [embedding] configured before `qgh embed --force`."));
    }

    let texts = chunks
        .iter()
        .map(|chunk| chunk.prepared_input.as_str())
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
    let runtime_fingerprint_hash = fingerprint.hash();
    let embeddings = chunks.iter().zip(vectors).collect::<Vec<_>>();
    let source_sync_run_id = snapshot.identity().sync_run_id().to_string();
    let context_template_version = crate::context::METADATA_CONTEXT_TEMPLATE_VERSION.to_string();
    let spec = crate::store::EmbeddingGenerationSpec {
        model_manifest_hash: model_manifest_hash.clone(),
        runtime_fingerprint_hash,
        chunker_fingerprint: chunks
            .first()
            .map(|chunk| chunk.chunk.chunker_fingerprint.clone())
            .unwrap_or_else(|| "none".to_string()),
        context_template_version: context_template_version.clone(),
        output_dimension: dimension,
    };
    let generation_id = store.begin_embedding_generation(snapshot, &spec)?;
    for batch in embeddings.chunks(32) {
        let staged = batch
            .iter()
            .map(|(chunk, vector)| {
                Ok(crate::store::EmbeddingGenerationChunk {
                    chunk_id: chunk.chunk.chunk_id,
                    source_version_id: chunk.chunk.source_version_id,
                    source_version_hash: store
                        .source_version_hash(chunk.chunk.source_version_id)?
                        .ok_or_else(|| QghError::storage("Missing source version hash."))?,
                    context_hash: chunk
                        .prepared_input
                        .context_hash(&model_manifest_hash, &chunk.chunk.chunker_fingerprint),
                    vector: vector.clone(),
                })
            })
            .collect::<Result<Vec<_>, QghError>>()?;
        store.stage_embedding_generation_batch(generation_id, &staged)?;
    }
    store.validate_embedding_generation(generation_id)?;
    let (tantivy_generation, reserved_path) =
        store.reserve_index_generation_for_snapshot(&paths.index_root, snapshot)?;
    let built_path =
        store.rebuild_reserved_index_generation(tantivy_generation, snapshot.sources())?;
    debug_assert_eq!(reserved_path, built_path);
    store.activate_retrieval_publication(
        &source_sync_run_id,
        tantivy_generation,
        Some(generation_id),
        snapshot.expected_publication_id(),
    )?;
    let embedded_chunks = embeddings.len();
    let usable_embeddings = embedded_chunks;
    Ok(json!({
        "embedded_chunks": embedded_chunks,
        "usable_embeddings": usable_embeddings
    }))
}

fn ensure_vector_only_smoke(
    store: &Store,
    profile_id: &str,
    query_vector: &[f32],
) -> Result<(), QghError> {
    let hits = store.vector_only_search(query_vector, &VectorSearchFilters::default(), 1)?;
    let Some(hit) = hits.first() else {
        return Err(QghError::storage(
            "Vector-only smoke returned no source candidates.",
        ));
    };
    let source = store.get_source(&hit.source_id)?.ok_or_else(|| {
        QghError::storage(format!(
            "Vector-only smoke hit `{}` could not round-trip through local get.",
            hit.source_id
        ))
    })?;
    let result = source_result(
        source,
        Ranking::Vector {
            vector_distance: hit.vector_distance,
        },
        profile_id,
        None,
    );
    let get_source = get_source_base(store, &hit.source_id, None)?;
    for key in ["source_id", "canonical_url", "source_version"] {
        if result[key] != get_source[key] {
            return Err(QghError::storage(format!(
                "Vector-only smoke hit `{}` lost {key} round-trip metadata.",
                hit.source_id
            )));
        }
    }
    Ok(())
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
    let host_default = args
        .host
        .clone()
        .or_else(|| remote.map(|remote| remote.host.clone()))
        .ok_or_else(|| missing_init_value("--host"))?;
    let profile_default = match profile_arg {
        Some(profile_id) => profile_id.to_string(),
        None => suggest_init_profile_id(&repo, &host_default)?,
    };
    let profile_id = prompt_line("profile id", &profile_default)?;
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
    let host = args
        .host
        .clone()
        .or_else(|| remote.map(|remote| remote.host.clone()))
        .ok_or_else(|| missing_init_value("--host"))?;
    let profile_id = match profile_arg {
        Some(profile_id) => profile_id.to_string(),
        None => suggest_init_profile_id(&repo, &host)?,
    };
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
    let mut warnings = Vec::new();
    if !bootstrap.duplicate_profile_ids.is_empty() {
        warnings.push(json!({
            "code": "config.duplicate_repo_allowlist",
            "severity": "warn",
            "message": format!(
                "Repo `{}` is also allowlisted in profile(s): {}. Profile auto-resolution will be ambiguous.",
                repo,
                bootstrap.duplicate_profile_ids.join(", ")
            )
        }));
    }
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
        warnings,
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
    let mut store = Store::open(&profile.paths)?;
    let allowed_repository_keys = configured_repository_identity_keys(&profile);
    store.validate_profile_read_allowlist(&allowed_repository_keys)?;
    let mut vector_open_warnings = Vec::new();
    let vector_enabled = if profile.embedding.is_some() {
        match store.enable_vector() {
            Ok(()) => true,
            Err(_) => {
                vector_open_warnings.push(embedding_warning(
                    "embedding.vector_init_failed",
                    "Local vector storage initialization failed. BM25 results are still returned.",
                ));
                false
            }
        }
    } else {
        false
    };
    let fence = store.begin_profile_read_snapshot(&allowed_repository_keys)?;
    let outcome = (|| -> Result<LocalReadOutcome, QghError> {
        let overrides = freshness_overrides(args.max_age.as_deref(), args.require_fresh)?;
        let publication = store.active_retrieval_publication()?;
        let active_index_path = ensure_query_publication_is_safe(&store)?;
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
            warnings.append(&mut vector_open_warnings);
            if vector_enabled {
                warnings.extend(embedding_warnings(&profile, &store)?);
            }
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
        let (hybrid_vector_hits, mut hybrid_warnings) = if vector_enabled {
            hybrid_vector_hits(
                &profile,
                &store,
                publication.as_ref(),
                &args.query,
                &filters,
                limit,
            )?
        } else {
            (None, vector_open_warnings)
        };
        let lexical_limit = if hybrid_vector_hits.is_some() {
            hybrid_candidate_limit(limit)
        } else {
            limit
        };
        let lexical_hits = match active_index_path.as_deref() {
            Some(active_index_path) => index::search_with_filters(
                active_index_path,
                &args.query,
                &filters.search_filters(),
                lexical_limit,
            )?,
            None => Vec::new(),
        };
        let hits = match hybrid_vector_hits {
            Some(vector_hits) => fuse_hybrid_hits(lexical_hits, vector_hits, limit),
            None => lexical_hits
                .into_iter()
                .map(QueryHit::from_bm25)
                .take(limit)
                .collect(),
        };
        let mut results = QueryResults::default();
        let mut unresolvable_hits = 0;
        for mut hit in hits {
            let Some(source) = store.get_source(&hit.source_id)? else {
                unresolvable_hits += 1;
                continue;
            };
            let lexical_version_matches = hit
                .lexical_source_updated_at
                .as_ref()
                .is_none_or(|indexed| indexed == stored_source_updated_at(&source));
            let vector_version_matches = match hit.vector_evidence.as_ref() {
                Some(evidence) => {
                    store.latest_source_version_id(&hit.source_id)?
                        == Some(evidence.chunk.source_version_id)
                }
                None => true,
            };
            if !lexical_version_matches || !vector_version_matches {
                unresolvable_hits += 1;
                continue;
            }
            if !filters.matches(&source) {
                continue;
            }
            hit.lexical_evidence = lexical_evidence(&store, &source, &args.query)?;
            let evidence = hit
                .vector_evidence
                .as_ref()
                .or(hit.lexical_evidence.as_ref());
            results.push(source, hit.ranking, &profile.id, evidence);
        }
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
        let coverage = coverage::evaluate(&store.coverage_snapshot()?, results.items.is_empty());
        let mut warnings = freshness.warnings;
        warnings.extend(coverage.warnings);
        warnings.append(&mut hybrid_warnings);
        if vector_enabled {
            warnings.extend(embedding_warnings(&profile, &store)?);
        }
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
    })();
    match outcome {
        Ok(outcome) => {
            store.end_read_snapshot_and_validate(fence)?;
            Ok(outcome)
        }
        Err(error) => {
            let _ = store.rollback_read_snapshot();
            Err(error)
        }
    }
}

fn stored_source_updated_at(source: &StoredSource) -> &str {
    match source {
        StoredSource::Issue(issue) => &issue.source_version.github_updated_at,
        StoredSource::Comment(comment) => &comment.source_version.github_updated_at,
    }
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

    fn search_filters(&self) -> index::SearchFilters {
        index::SearchFilters {
            repo: self.repo.clone(),
            labels: self.labels.clone(),
            state: self.state.clone(),
            author: self.author.clone(),
            issue: self.issue,
            source_types: self.source_types.clone(),
        }
    }

    fn vector_search_filters(&self) -> VectorSearchFilters {
        VectorSearchFilters {
            repo: self.repo.clone(),
            labels: self.labels.clone(),
            state: self.state.clone(),
            author: self.author.clone(),
            issue: self.issue,
            source_types: self.source_types.clone(),
        }
    }
}

#[derive(Debug)]
struct QueryHit {
    source_id: String,
    lexical_source_updated_at: Option<String>,
    ranking: Ranking,
    vector_evidence: Option<MatchedChunkEvidence>,
    lexical_evidence: Option<MatchedChunkEvidence>,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
struct MatchedChunkEvidence {
    chunk: StoredChunk,
    source_version_hash: String,
    retriever_kind: &'static str,
    rank: usize,
    score_or_distance: f32,
}

impl QueryHit {
    fn from_bm25(hit: index::SearchHit) -> Self {
        Self {
            source_id: hit.source_id,
            lexical_source_updated_at: hit.source_updated_at,
            ranking: Ranking::Bm25(hit.score),
            vector_evidence: None,
            lexical_evidence: None,
        }
    }
}

#[derive(Debug, Default)]
struct HybridAccumulator {
    source_id: String,
    bm25_rank: Option<usize>,
    bm25_score: Option<f32>,
    lexical_source_updated_at: Option<String>,
    vector_rank: Option<usize>,
    vector_distance: Option<f32>,
    vector_evidence: Option<MatchedChunkEvidence>,
}

impl HybridAccumulator {
    fn new(source_id: String) -> Self {
        Self {
            source_id,
            ..Self::default()
        }
    }

    fn record_bm25(&mut self, rank: usize, score: f32, source_updated_at: Option<String>) {
        if self.bm25_rank.is_none_or(|current| rank < current) {
            self.bm25_rank = Some(rank);
            self.bm25_score = Some(score);
            self.lexical_source_updated_at = source_updated_at;
        }
    }

    fn record_vector(&mut self, rank: usize, hit: crate::model::VectorSearchHit) {
        let vector_distance = hit.vector_distance;
        if self.vector_rank.is_none_or(|current| rank < current) {
            self.vector_rank = Some(rank);
            self.vector_distance = Some(vector_distance);
            self.vector_evidence = Some(MatchedChunkEvidence {
                chunk: hit.chunk,
                source_version_hash: hit.source_version_hash,
                retriever_kind: "vector",
                rank,
                score_or_distance: vector_distance,
            });
        }
    }

    fn rrf_score(&self) -> f32 {
        rrf_component(self.bm25_rank) + rrf_component(self.vector_rank)
    }

    fn best_rank(&self) -> usize {
        self.bm25_rank
            .into_iter()
            .chain(self.vector_rank)
            .min()
            .unwrap_or(usize::MAX)
    }

    fn into_query_hit(self) -> QueryHit {
        // A candidate only carries genuine hybrid evidence when it actually
        // received a vector contribution. Fusion still runs whenever the
        // hybrid path is eligible (config + coverage), even for a query
        // where the vector search legitimately returns zero hits (e.g. no
        // sqlite-vec table yet) — those candidates are BM25-only in
        // substance and must not report ranking.kind = hybrid, or eval/A-B
        // evidence cannot distinguish real fusion from a BM25 fallback.
        let ranking = if self.vector_rank.is_some() {
            let rrf_rank_score = self.rrf_score();
            Ranking::Hybrid {
                lexical_score: self.bm25_score,
                vector_distance: self.vector_distance,
                rrf_rank_score,
                final_order_score: rrf_rank_score,
            }
        } else {
            Ranking::Bm25(self.bm25_score.unwrap_or(0.0))
        };
        QueryHit {
            source_id: self.source_id,
            lexical_source_updated_at: self.lexical_source_updated_at,
            ranking,
            vector_evidence: self.vector_evidence,
            lexical_evidence: None,
        }
    }
}

fn rrf_component(rank: Option<usize>) -> f32 {
    rank.map(|rank| 1.0 / (HYBRID_RRF_K + rank as f32))
        .unwrap_or(0.0)
}

fn hybrid_candidate_limit(limit: usize) -> usize {
    limit.saturating_mul(HYBRID_OVERFETCH_FACTOR).max(limit)
}

fn fuse_hybrid_hits(
    bm25_hits: Vec<index::SearchHit>,
    vector_hits: Vec<crate::model::VectorSearchHit>,
    limit: usize,
) -> Vec<QueryHit> {
    let mut candidates = HashMap::<String, HybridAccumulator>::new();
    for (rank, hit) in bm25_hits.into_iter().enumerate() {
        candidates
            .entry(hit.source_id.clone())
            .or_insert_with(|| HybridAccumulator::new(hit.source_id))
            .record_bm25(rank + 1, hit.score, hit.source_updated_at);
    }
    for (rank, hit) in vector_hits.into_iter().enumerate() {
        let source_id = hit.source_id.clone();
        candidates
            .entry(source_id)
            .or_insert_with_key(|source_id| HybridAccumulator::new(source_id.clone()))
            .record_vector(rank + 1, hit);
    }

    let mut candidates = candidates.into_values().collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        right
            .rrf_score()
            .total_cmp(&left.rrf_score())
            .then_with(|| left.best_rank().cmp(&right.best_rank()))
            .then_with(|| left.source_id.cmp(&right.source_id))
    });
    candidates
        .into_iter()
        .take(limit)
        .map(HybridAccumulator::into_query_hit)
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HybridQueryEncodingError {
    FingerprintMismatch,
    EncodingFailed,
    DimensionMismatch,
}

fn encode_hybrid_query(
    runtime: &EmbeddingRuntime,
    publication: Option<&RetrievalPublicationView>,
    query_text: &str,
) -> Result<EmbeddingVector, HybridQueryEncodingError> {
    let publication = publication.ok_or(HybridQueryEncodingError::FingerprintMismatch)?;
    let dimension = publication
        .output_dimension
        .ok_or(HybridQueryEncodingError::FingerprintMismatch)?;
    let expected_runtime_hash = runtime
        .fingerprint_seed
        .clone()
        .with_dimension(dimension)
        .hash();
    if publication.model_manifest_hash.as_deref() != Some(runtime.model_manifest_hash.as_str())
        || publication.runtime_fingerprint_hash.as_deref() != Some(expected_runtime_hash.as_str())
    {
        return Err(HybridQueryEncodingError::FingerprintMismatch);
    }
    let vector = runtime
        .provider
        .embed_query(query_text)
        .map_err(|_| HybridQueryEncodingError::EncodingFailed)?;
    if vector.len() != dimension {
        return Err(HybridQueryEncodingError::DimensionMismatch);
    }
    Ok(vector)
}

fn hybrid_vector_hits(
    profile: &Profile,
    store: &Store,
    publication: Option<&RetrievalPublicationView>,
    query_text: &str,
    filters: &QueryFilters,
    limit: usize,
) -> Result<(Option<Vec<crate::model::VectorSearchHit>>, Vec<Value>), QghError> {
    let Some(embedding) = profile.embedding.as_ref() else {
        return Ok((None, Vec::new()));
    };
    let Some(coverage) = embedding_coverage_state(profile, store)? else {
        return Ok((None, Vec::new()));
    };
    if !coverage.hybrid_ready() {
        return Ok((None, Vec::new()));
    }
    let runtime = match embedding_runtime_local_only(embedding, Some(&profile.id)) {
        Ok(runtime) => runtime,
        Err(_) => {
            return Ok((
                None,
                vec![embedding_warning(
                    "embedding.runtime_unavailable",
                    "Local embedding runtime was unavailable. BM25 results are still returned.",
                )],
            ));
        }
    };
    let query_vector = match encode_hybrid_query(&runtime, publication, query_text) {
        Ok(vector) => vector,
        Err(HybridQueryEncodingError::FingerprintMismatch) => {
            return Ok((
                None,
                vec![json!({
                    "code": "embedding.fingerprint_mismatch",
                    "severity": "warn_strong",
                    "message": "Stored embeddings were created with a different embedding fingerprint and will not be used for vector search. BM25 results are still returned."
                })],
            ));
        }
        Err(HybridQueryEncodingError::EncodingFailed) => {
            return Ok((
                None,
                vec![embedding_warning(
                    "embedding.query_encoding_failed",
                    "Local query embedding failed. BM25 results are still returned.",
                )],
            ));
        }
        Err(HybridQueryEncodingError::DimensionMismatch) => {
            return Ok((
                None,
                vec![embedding_warning(
                    "embedding.query_dimension_mismatch",
                    "Query embedding dimension did not match the active generation. BM25 results are still returned.",
                )],
            ));
        }
    };
    let Some(generation_id) =
        publication.and_then(|publication| publication.embedding_generation_id)
    else {
        return Ok((None, Vec::new()));
    };
    match store.generation_vector_search(
        generation_id,
        &query_vector,
        &filters.vector_search_filters(),
        hybrid_candidate_limit(limit),
    ) {
        Ok(hits) => Ok((Some(hits), Vec::new())),
        Err(_) => Ok((
            None,
            vec![embedding_warning(
                "embedding.vector_search_failed",
                "Local vector search failed. BM25 results are still returned.",
            )],
        )),
    }
}

struct EmbeddingCoverageState {
    active_fingerprint: Option<EmbeddingFingerprint>,
    generation_active: bool,
    active_matches_config: bool,
    artifact_corrupt: bool,
    total_chunks: i64,
    completed_chunks: i64,
    missing_chunks: i64,
    mismatched_chunks: i64,
    prepared_runtime: PreparedRuntimeAvailability,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreparedRuntimeAvailability {
    Available,
    Missing,
    Corrupt,
}

struct ConfiguredEmbeddingSnapshot {
    model_id: Option<String>,
    model_revision: Option<String>,
    pooling: Option<crate::embedding::PoolingKind>,
    query_prefix: Option<String>,
    prepared_runtime: PreparedRuntimeAvailability,
}

impl EmbeddingCoverageState {
    fn state(&self) -> &'static str {
        if self.artifact_corrupt {
            return "corrupt";
        }
        if !self.active_matches_config && self.mismatched_chunks > 0 {
            return "fingerprint_mismatch";
        }
        match (
            self.has_active_embedding(),
            self.active_matches_config,
            self.missing_chunks,
        ) {
            (false, _, _) => "missing",
            (true, false, _) => "fingerprint_mismatch",
            (true, true, 0) => "complete",
            (true, true, _) => "partial",
        }
    }

    fn status_state(&self) -> &'static str {
        match self.prepared_runtime {
            PreparedRuntimeAvailability::Corrupt => "corrupt",
            PreparedRuntimeAvailability::Missing if !self.artifact_corrupt => "missing",
            PreparedRuntimeAvailability::Available | PreparedRuntimeAvailability::Missing => {
                self.state()
            }
        }
    }

    fn has_active_embedding(&self) -> bool {
        self.generation_active || self.active_fingerprint.is_some()
    }

    fn hybrid_ready(&self) -> bool {
        !self.artifact_corrupt
            && self.active_matches_config
            && self.total_chunks > 0
            && self.missing_chunks == 0
    }
}

fn embedding_coverage_state(
    profile: &Profile,
    store: &Store,
) -> Result<Option<EmbeddingCoverageState>, QghError> {
    let Some(embedding) = profile.embedding.as_ref() else {
        return Ok(None);
    };
    let configured = configured_embedding_snapshot(embedding);
    embedding_coverage_state_for_config(embedding, store, &configured).map(Some)
}

fn embedding_coverage_state_for_config(
    embedding: &EmbeddingConfig,
    store: &Store,
    configured: &ConfiguredEmbeddingSnapshot,
) -> Result<EmbeddingCoverageState, QghError> {
    if let Some((_generation_id, total_chunks, completed_chunks, generation_valid)) =
        store.active_embedding_generation_coverage()?
    {
        if !generation_valid {
            return Ok(EmbeddingCoverageState {
                active_fingerprint: None,
                generation_active: true,
                active_matches_config: true,
                artifact_corrupt: true,
                total_chunks,
                completed_chunks,
                missing_chunks: total_chunks.saturating_sub(completed_chunks),
                mismatched_chunks: 0,
                prepared_runtime: configured.prepared_runtime,
            });
        }
        // Publication validation proves structural coverage. When the config
        // fully pins the fingerprint inputs, status can compare offline. A
        // mutable revision remains structurally complete and is verified
        // after the local runtime resolves it in `hybrid_vector_hits`.
        let expectation = embedding_fingerprint_expectation_from_snapshot(embedding, configured);
        let comparable_seed = expectation
            .model_id
            .zip(expectation.model_revision)
            .zip(expectation.pooling)
            .zip(expectation.query_prefix)
            .map(|(((model_id, model_revision), pooling), query_prefix)| {
                EmbeddingFingerprintSeed {
                    provider: expectation.provider,
                    model_id,
                    model_revision,
                    pooling,
                    query_prefix,
                }
            });
        let active_matches_config = comparable_seed.is_none_or(|seed| {
            store
                .active_retrieval_publication()
                .ok()
                .flatten()
                .and_then(|publication| {
                    publication
                        .output_dimension
                        .zip(publication.runtime_fingerprint_hash)
                })
                .is_some_and(|(dimension, hash)| hash == seed.with_dimension(dimension).hash())
        });
        return Ok(EmbeddingCoverageState {
            active_fingerprint: None,
            generation_active: true,
            active_matches_config,
            artifact_corrupt: false,
            total_chunks,
            completed_chunks: if active_matches_config {
                completed_chunks
            } else {
                0
            },
            missing_chunks: if active_matches_config {
                0
            } else {
                completed_chunks
            },
            mismatched_chunks: if active_matches_config {
                0
            } else {
                completed_chunks
            },
            prepared_runtime: configured.prepared_runtime,
        });
    }
    let expectation = embedding_fingerprint_expectation_from_snapshot(embedding, configured);
    let total_chunks = match store.active_embedding_chunk_count() {
        Ok(total_chunks) => total_chunks,
        Err(_) => {
            return Ok(corrupt_embedding_coverage(
                None,
                false,
                0,
                configured.prepared_runtime,
            ));
        }
    };
    let active_fingerprint = match store.active_embedding_fingerprint() {
        Ok(active_fingerprint) => active_fingerprint,
        Err(_) => {
            return Ok(corrupt_embedding_coverage(
                None,
                false,
                total_chunks,
                configured.prepared_runtime,
            ));
        }
    };
    let active_matches_config = active_fingerprint
        .as_ref()
        .is_some_and(|fingerprint| fingerprint.matches_expectation(&expectation));
    let active_embedding_count = match active_fingerprint.as_ref() {
        Some(fingerprint) => match store.current_chunk_embedding_count_for_fingerprint(fingerprint)
        {
            Ok(count) => count,
            Err(_) => {
                return Ok(corrupt_embedding_coverage(
                    active_fingerprint,
                    active_matches_config,
                    total_chunks,
                    configured.prepared_runtime,
                ));
            }
        },
        None => 0,
    };
    if let Some(fingerprint) = active_fingerprint.as_ref() {
        if active_embedding_count > 0
            && !matches!(
                store.vector_index_ready_for_fingerprint(fingerprint, active_embedding_count),
                Ok(true)
            )
        {
            return Ok(corrupt_embedding_coverage(
                active_fingerprint,
                active_matches_config,
                total_chunks,
                configured.prepared_runtime,
            ));
        }
    }
    let completed_chunks = if active_matches_config {
        active_embedding_count
    } else {
        0
    };
    let missing_chunks = total_chunks.saturating_sub(completed_chunks);
    let mismatched_chunks = if active_fingerprint.is_some() && !active_matches_config {
        active_embedding_count
    } else {
        0
    };

    Ok(EmbeddingCoverageState {
        active_fingerprint,
        generation_active: false,
        active_matches_config,
        artifact_corrupt: false,
        total_chunks,
        completed_chunks,
        missing_chunks,
        mismatched_chunks,
        prepared_runtime: configured.prepared_runtime,
    })
}

fn corrupt_embedding_coverage(
    active_fingerprint: Option<EmbeddingFingerprint>,
    active_matches_config: bool,
    total_chunks: i64,
    prepared_runtime: PreparedRuntimeAvailability,
) -> EmbeddingCoverageState {
    EmbeddingCoverageState {
        active_fingerprint,
        generation_active: false,
        active_matches_config,
        artifact_corrupt: true,
        total_chunks,
        completed_chunks: 0,
        missing_chunks: total_chunks,
        mismatched_chunks: 0,
        prepared_runtime,
    }
}

fn embedding_warning(code: &'static str, message: &'static str) -> Value {
    json!({
        "code": code,
        "severity": "warn",
        "message": message
    })
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
    fn push(
        &mut self,
        source: StoredSource,
        ranking: Ranking,
        profile_id: &str,
        evidence: Option<&MatchedChunkEvidence>,
    ) {
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
        self.items
            .push(source_result(source, ranking, profile_id, evidence));
    }
}

fn embedding_warnings(profile: &Profile, store: &Store) -> Result<Vec<Value>, QghError> {
    let Some(coverage) = embedding_coverage_state(profile, store)? else {
        return Ok(Vec::new());
    };
    Ok(embedding_warnings_for_coverage(&coverage))
}

fn embedding_warnings_for_coverage(coverage: &EmbeddingCoverageState) -> Vec<Value> {
    embedding_warnings_for_state(coverage.state())
}

fn embedding_warnings_for_state(state: &str) -> Vec<Value> {
    match state {
        "missing" => vec![json!({
            "code": "embedding.coverage_missing",
            "severity": "warn",
            "message": "No usable embeddings are available. Hybrid retrieval is disabled and BM25 results are still returned."
        })],
        "partial" => vec![json!({
            "code": "embedding.coverage_partial",
            "severity": "warn",
            "message": "Embedding coverage is incomplete. Hybrid retrieval is disabled and BM25 results are still returned."
        })],
        "fingerprint_mismatch" => vec![json!({
            "code": "embedding.fingerprint_mismatch",
            "severity": "warn_strong",
            "message": "Stored embeddings were created with a different embedding fingerprint and will not be used for vector search. BM25 results are still returned."
        })],
        "corrupt" => vec![embedding_warning(
            "embedding.artifact_corrupt",
            "Stored embedding artifacts are corrupt or incomplete. Hybrid retrieval is disabled and BM25 results are still returned.",
        )],
        "complete" => Vec::new(),
        _ => unreachable!("embedding coverage state is closed"),
    }
}

fn embedding_status_report(
    profile: &Profile,
    store: &Store,
) -> Result<(Option<Value>, Vec<Value>), QghError> {
    let Some(embedding) = profile.embedding.as_ref() else {
        return Ok((None, Vec::new()));
    };
    let configured = configured_embedding_snapshot(embedding);
    let coverage = embedding_coverage_state_for_config(embedding, store, &configured)?;
    let status_state = coverage.status_state();
    let warnings = embedding_warnings_for_state(status_state);

    Ok((
        Some(json!({
            "state": status_state,
            "coverage": {
                "total_chunks": coverage.total_chunks,
                "completed_chunks": coverage.completed_chunks,
                "missing_chunks": coverage.missing_chunks,
                "mismatched_chunks": coverage.mismatched_chunks
            },
            "configured_model": configured_embedding_model_json(embedding, &configured),
            "fingerprint": coverage
                .active_fingerprint
                .as_ref()
                .map(|fingerprint| embedding_fingerprint_status_json(
                    fingerprint,
                    coverage.active_matches_config
                ))
        })),
        warnings,
    ))
}

fn configured_embedding_model_json(
    embedding: &EmbeddingConfig,
    configured: &ConfiguredEmbeddingSnapshot,
) -> Value {
    let model =
        if embedding.model_path.is_some() || embedding.manifest_path.is_some() {
            None
        } else {
            Some(embedding.model.clone().unwrap_or_else(|| {
                format!("hf:{}", configured_hf_model_reference(embedding).model_id)
            }))
        };
    json!({
        "provider": embedding_provider_name(embedding.provider),
        "model": model,
        "model_id": configured.model_id,
        "model_revision": configured.model_revision,
        "model_path": embedding
            .model_path
            .as_ref()
            .or(embedding.manifest_path.as_ref())
            .map(|path| path.to_string_lossy().into_owned())
    })
}

fn embedding_fingerprint_status_json(
    fingerprint: &EmbeddingFingerprint,
    matches_config: bool,
) -> Value {
    json!({
        "hash": fingerprint.hash(),
        "schema_version": fingerprint.schema_version,
        "provider": fingerprint.provider,
        "model_id": fingerprint.model_id,
        "model_revision": fingerprint.model_revision,
        "dimension": fingerprint.dimension,
        "pooling": fingerprint.pooling.as_str(),
        "query_prefix": fingerprint.query_prefix,
        "chunker_version": fingerprint.chunker_version,
        "source_schema_version": fingerprint.source_schema_version,
        "matches_config": matches_config
    })
}

fn embedding_fingerprint_expectation(
    embedding: &EmbeddingConfig,
) -> EmbeddingFingerprintExpectation {
    let configured = configured_embedding_snapshot(embedding);
    embedding_fingerprint_expectation_from_snapshot(embedding, &configured)
}

fn embedding_fingerprint_expectation_from_snapshot(
    embedding: &EmbeddingConfig,
    configured: &ConfiguredEmbeddingSnapshot,
) -> EmbeddingFingerprintExpectation {
    EmbeddingFingerprintExpectation {
        provider: embedding_provider_name(embedding.provider).to_string(),
        model_id: configured.model_id.clone(),
        model_revision: configured.model_revision.clone(),
        pooling: configured.pooling,
        query_prefix: configured.query_prefix.clone(),
    }
}

fn configured_embedding_snapshot(embedding: &EmbeddingConfig) -> ConfiguredEmbeddingSnapshot {
    #[cfg(feature = "fastembed-provider")]
    let prepared_runtime = {
        let mut prepared_runtime = PreparedRuntimeAvailability::Missing;
        if let Ok(store) = default_prepared_model_store() {
            let options = embedding.fastembed_options();
            match store.inspect(&options) {
                Ok(inspection) => {
                    return configured_snapshot_from_inspection(
                        &inspection,
                        PreparedRuntimeAvailability::Available,
                    );
                }
                Err(error) => {
                    let availability = if error.code() == "embedding.prepared_snapshot_missing" {
                        PreparedRuntimeAvailability::Missing
                    } else {
                        PreparedRuntimeAvailability::Corrupt
                    };
                    prepared_runtime = availability;
                    if let Some(manifest_path) = options.manifest_path.as_deref() {
                        if let Ok(inspection) =
                            PreparedModelStore::new(PathBuf::new()).inspect_manifest(manifest_path)
                        {
                            return configured_snapshot_from_inspection(&inspection, availability);
                        }
                    }
                }
            }
        }
        prepared_runtime
    };
    #[cfg(not(feature = "fastembed-provider"))]
    let prepared_runtime = PreparedRuntimeAvailability::Missing;

    #[cfg(debug_assertions)]
    let prepared_runtime = if std::env::var_os(TEST_EMBEDDING_QUERY_VECTORS_ENV).is_some()
        || std::env::var_os(TEST_EMBEDDING_DOCUMENT_VECTORS_ENV).is_some()
    {
        PreparedRuntimeAvailability::Available
    } else {
        prepared_runtime
    };

    ConfiguredEmbeddingSnapshot {
        model_id: if embedding.model_path.is_some() {
            embedding
                .model_path
                .as_ref()
                .map(|path| format!("model_path:{}", path.to_string_lossy()))
        } else {
            Some(configured_hf_model_reference(embedding).model_id)
        },
        model_revision: configured_embedding_model_revision_without_snapshot(embedding),
        pooling: embedding.pooling,
        query_prefix: embedding.query_prefix.clone(),
        prepared_runtime,
    }
}

#[cfg(feature = "fastembed-provider")]
fn configured_snapshot_from_inspection(
    inspection: &PreparedModelInspection,
    prepared_runtime: PreparedRuntimeAvailability,
) -> ConfiguredEmbeddingSnapshot {
    let manifest = inspection.manifest();
    ConfiguredEmbeddingSnapshot {
        model_id: Some(prepared_manifest_model_id(manifest)),
        model_revision: Some(inspection.manifest_hash().to_string()),
        pooling: Some(manifest.pooling),
        query_prefix: Some(manifest.query_prefix.clone().unwrap_or_default()),
        prepared_runtime,
    }
}

fn configured_embedding_model_revision_without_snapshot(
    embedding: &EmbeddingConfig,
) -> Option<String> {
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
        .map(|model| {
            parse_hf_model_reference(model)
                .or_else(|| builtin_preset_hf_reference(model))
                .expect("validated embedding model reference or preset")
        })
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
            results.push(source, Ranking::Exact, profile_id, None);
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
            results.push(source, Ranking::Exact, profile_id, None);
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
    let allowed_repository_keys = configured_repository_identity_keys(&profile);
    store.validate_profile_read_allowlist(&allowed_repository_keys)?;
    let lifecycle_check = if verify_lifecycle {
        lifecycle_check_for_get(&profile, &mut store, source_id).await?
    } else {
        lifecycle_not_requested()
    };
    let fence = store.begin_profile_read_snapshot(&allowed_repository_keys)?;
    let outcome =
        get_source_for_get(&store, source_id, repo_scope, lifecycle_check).map(|source| {
            json!({
            "profile_id": profile.id,
            "source": source
            })
        });
    match outcome {
        Ok(outcome) => {
            store.end_read_snapshot_and_validate(fence)?;
            Ok(outcome)
        }
        Err(error) => {
            let _ = store.rollback_read_snapshot();
            Err(error)
        }
    }
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
    let allowed_repository_keys = configured_repository_identity_keys(&profile);
    store.validate_profile_read_allowlist(&allowed_repository_keys)?;
    let mut lifecycle_checks = Vec::with_capacity(source_ids.len());
    for source_id in source_ids {
        let check = if verify_lifecycle {
            lifecycle_check_for_get(&profile, &mut store, source_id).await
        } else {
            Ok(lifecycle_not_requested())
        };
        match check {
            Ok(check) => lifecycle_checks.push(Ok(check)),
            Err(error) if is_get_item_error(&error) => lifecycle_checks.push(Err(error)),
            Err(error) => return Err(error),
        }
    }

    let fence = store.begin_profile_read_snapshot(&allowed_repository_keys)?;
    let mut items = Vec::with_capacity(source_ids.len());
    let mut returned = 0;
    let mut failed = 0;
    for (input_index, (source_id, lifecycle_check)) in
        source_ids.iter().zip(lifecycle_checks).enumerate()
    {
        let source = match lifecycle_check {
            Ok(lifecycle_check) => {
                get_source_for_get(&store, source_id, repo_scope, lifecycle_check)
            }
            Err(error) => Err(error),
        };
        match source {
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
            Err(error) => {
                let _ = store.rollback_read_snapshot();
                return Err(error);
            }
        }
    }

    let outcome = json!({
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
    });
    store.end_read_snapshot_and_validate(fence)?;
    Ok(outcome)
}

fn get_source_for_get(
    store: &Store,
    source_id: &str,
    repo_scope: Option<&ResolvedRepoScope>,
    lifecycle_check: Value,
) -> Result<Value, QghError> {
    let mut source_json = get_source_base(store, source_id, repo_scope)?;
    source_json["lifecycle_check"] = lifecycle_check;
    Ok(source_json)
}

fn lifecycle_not_requested() -> Value {
    json!({
        "status": "not_checked",
        "reason": "not_requested",
        "remote_checked": false
    })
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
                match github::check_source_lifecycle_classified(profile, &token, &candidate, None)
                    .await
                {
                    Ok(github::ClassifiedLifecycleCheck::Active) => json!({
                        "status": "active",
                        "remote_checked": true
                    }),
                    Ok(github::ClassifiedLifecycleCheck::Confirmed { state, .. }) => {
                        queue_and_finish_purges(
                            store,
                            &[candidate_confirmed_purge_request(&candidate, state)],
                        )?;
                        repair_lexical_successor_if_required(profile, store)?;
                        let tombstone = store.get_tombstone(source_id)?.ok_or_else(|| {
                            QghError::new(
                                "purge.tombstone_missing",
                                "Confirmed lifecycle cleanup did not retain a tombstone.",
                                6,
                            )
                        })?;
                        return Err(QghError::source_tombstoned(
                            &tombstone.source_id,
                            &tombstone.reason,
                            &tombstone.observed_at,
                        ));
                    }
                    Ok(github::ClassifiedLifecycleCheck::AuthenticationFailed) => json!({
                        "status": "not_checked",
                        "error_code": QghError::auth(
                            "GitHub authentication failed during lifecycle verification."
                        ).code,
                        "remote_checked": false
                    }),
                    Ok(
                        github::ClassifiedLifecycleCheck::Backoff(_)
                        | github::ClassifiedLifecycleCheck::Transient(_)
                        | github::ClassifiedLifecycleCheck::AmbiguousForbidden,
                    ) => json!({
                        "status": "not_checked",
                        "error_code": QghError::github(
                            "GitHub request ended without a confirmed destructive lifecycle state."
                        ).code,
                        "remote_checked": false
                    }),
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
    let purge = purge_report(&store)?;
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
    let (embedding, embedding_warnings) = embedding_status_report(&profile, &store)?;
    warnings.extend(embedding_warnings);
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
    let mut outcome = LocalReadOutcome {
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
        "purge": purge,
        "privacy": {
            "classification": "sensitive_derivative_data",
            "default_network_egress": "configured_github_host_only",
            "hosted_provider_egress": "disabled",
            "local_paths_may_contain_private_content": true,
            "single_user_permissions": "0600_files_0700_dirs_where_supported"
        }
        }),
        warnings,
    };
    if let Some(embedding) = embedding {
        outcome.data["embedding"] = embedding;
    }
    Ok(outcome)
}

pub async fn doctor(profile_id: &str) -> Result<Value, QghError> {
    let profile = load_profile(profile_id)?;
    let store = Store::open(&profile.paths)?;
    let status = store.status()?;
    let purge = purge_report(&store)?;
    let purge_ok = purge["pending_count"].as_u64() == Some(0)
        && purge["retrieval_blocked"].as_bool() == Some(false);
    let permissions_ok = private_paths_ok(&profile.paths);
    let sqlite_ok = status.active_generation >= 0;
    let tantivy_ok = store.resolve_active_tantivy_artifact().is_ok();
    let (github_ok, rate_limit_ok, rate_limit_headers) = match resolve_token(&profile) {
        Ok(token) => doctor_github_probe(&profile, &token).await,
        Err(_) => (false, false, rate_limit_headers_json(None, None)),
    };
    let mut checks = vec![
        json!({
            "name": "config",
            "ok": true
        }),
        json!({
            "name": "file_permissions",
            "ok": permissions_ok
        }),
        json!({
            "name": "sqlite",
            "ok": sqlite_ok
        }),
        json!({
            "name": "tantivy",
            "ok": tantivy_ok
        }),
    ];
    if let Some(embedding) = profile.embedding.as_ref() {
        checks.extend(doctor_embedding_checks(embedding, &store));
    }
    checks.extend([
        json!({
            "name": "github_auth_reachability",
            "ok": github_ok
        }),
        json!({
            "name": "rate_limit_headers",
            "ok": rate_limit_ok,
            "headers": rate_limit_headers
        }),
        json!({
            "name": "purge",
            "ok": purge_ok
        }),
    ]);
    Ok(json!({
        "profile_id": profile.id,
        "checks": checks,
        "purge": {
            "pending_count": purge["pending_count"],
            "successor_repair_required": purge["successor_repair_required"],
            "retrieval_blocked": purge["retrieval_blocked"],
            "target_kinds": purge["target_kinds"],
            "triggers": purge["triggers"],
            "current_stages": purge["current_stages"],
            "failure_stages": purge["failure_stages"],
            "unmanaged_filesystem_backups": "not_deleted_by_qgh"
        },
        "mcp": {
            "doctor_exposed": false,
            "tools": ["query", "get", "status"]
        }
    }))
}

fn doctor_embedding_checks(embedding: &EmbeddingConfig, store: &Store) -> [Value; 3] {
    #[cfg(feature = "fastembed-provider")]
    let (artifacts_ok, runtime_ok) = {
        let snapshot = default_prepared_model_store().and_then(|prepared_store| {
            prepared_store
                .inspect(&embedding.fastembed_options())
                .and_then(|inspection| prepared_store.verify(inspection))
        });
        let artifacts_ok = snapshot.is_ok();
        let runtime_ok = snapshot
            .as_ref()
            .is_ok_and(|snapshot| build_embedding_runtime(embedding, snapshot).is_ok());
        (artifacts_ok, runtime_ok)
    };
    #[cfg(not(feature = "fastembed-provider"))]
    let (artifacts_ok, runtime_ok) = {
        let _ = embedding;
        (false, false)
    };
    let generation_ok = store
        .validate_active_embedding_generation_artifacts()
        .unwrap_or(false);
    [
        json!({"name": "embedding_artifacts", "ok": artifacts_ok}),
        json!({"name": "embedding_runtime", "ok": runtime_ok}),
        json!({"name": "embedding_generation", "ok": generation_ok}),
    ]
}

fn purge_report(store: &Store) -> Result<Value, QghError> {
    let pending = store.pending_purges()?;
    let successor_repair_required = store.successor_repair_required()?;
    let retrieval_blocked = !pending.is_empty() || successor_repair_required;
    let target_kinds = pending
        .iter()
        .map(|request| request.target.kind())
        .collect::<BTreeSet<_>>();
    let triggers = pending
        .iter()
        .map(|request| request.trigger.as_str())
        .collect::<BTreeSet<_>>();
    let current_stages = pending
        .iter()
        .map(|request| request.current_stage.as_str())
        .collect::<BTreeSet<_>>();
    let failure_stages = pending
        .iter()
        .filter_map(|request| request.failure_stage.map(|stage| stage.as_str()))
        .collect::<BTreeSet<_>>();
    Ok(json!({
        "pending_count": pending.len(),
        "successor_repair_required": successor_repair_required,
        "retrieval_blocked": retrieval_blocked,
        "target_kinds": target_kinds,
        "triggers": triggers,
        "current_stages": current_stages,
        "failure_stages": failure_stages
    }))
}

fn active_index_path(store: &Store, fallback: &std::path::Path) -> Result<PathBuf, QghError> {
    Ok(store
        .active_index_path()?
        .map(PathBuf::from)
        .unwrap_or_else(|| fallback.to_path_buf()))
}

fn ensure_query_publication_is_safe(store: &Store) -> Result<Option<PathBuf>, QghError> {
    if store.successor_repair_required()? {
        return Err(successor_repair_required_error());
    }
    store.resolve_active_tantivy_artifact()
}

fn successor_repair_required_error() -> QghError {
    QghError::new(
        "purge.successor_repair_required",
        "Retrieval is blocked until a clean lexical successor is published.",
        6,
    )
    .with_details(json!({
        "successor_repair_required": true
    }))
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

#[derive(Debug)]
enum Ranking {
    Bm25(f32),
    Vector {
        vector_distance: f32,
    },
    Hybrid {
        lexical_score: Option<f32>,
        vector_distance: Option<f32>,
        rrf_rank_score: f32,
        final_order_score: f32,
    },
    Exact,
}

fn source_result(
    source: StoredSource,
    ranking: Ranking,
    profile_id: &str,
    evidence: Option<&MatchedChunkEvidence>,
) -> Value {
    match source {
        StoredSource::Issue(issue) => issue_result(issue, ranking, profile_id, evidence),
        StoredSource::Comment(comment) => comment_result(comment, ranking, profile_id, evidence),
    }
}

fn issue_result(
    issue: StoredIssue,
    ranking: Ranking,
    profile_id: &str,
    evidence: Option<&MatchedChunkEvidence>,
) -> Value {
    let source_id = issue.source_id;
    json!({
        "source_id": source_id,
        "entity_type": "issue",
        "repo": issue.repo,
        "issue_number": issue.number,
        "title": issue.title,
        "canonical_url": issue.canonical_url,
        "snippet": matched_snippet(&issue.body, &issue.source_version, evidence),
        "get_args": {
            "source_id": source_id,
            "profile_id": profile_id
        },
        "parent_issue": Value::Null,
        "source_version": issue.source_version,
        "ranking": ranking_json(ranking)
    })
}

fn comment_result(
    comment: StoredComment,
    ranking: Ranking,
    profile_id: &str,
    evidence: Option<&MatchedChunkEvidence>,
) -> Value {
    let source_id = comment.source_id;
    json!({
        "source_id": source_id,
        "entity_type": "issue_comment",
        "repo": comment.repo,
        "issue_number": comment.issue_number,
        "author": comment.author,
        "canonical_url": comment.canonical_url,
        "parent_issue": comment.parent_issue,
        "snippet": matched_snippet(&comment.body, &comment.source_version, evidence),
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
            "lexical_score": score,
            "vector_distance": Value::Null
        }),
        Ranking::Vector { vector_distance } => json!({
            "kind": "vector",
            "lexical_score": Value::Null,
            "vector_distance": vector_distance
        }),
        Ranking::Hybrid {
            lexical_score,
            vector_distance,
            rrf_rank_score,
            final_order_score,
        } => json!({
            "kind": "hybrid",
            "lexical_score": lexical_score,
            "vector_distance": vector_distance,
            "rrf_rank_score": rrf_rank_score,
            "final_order_score": final_order_score
        }),
        Ranking::Exact => json!({
            "kind": "exact",
            "lexical_score": Value::Null,
            "vector_distance": Value::Null
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

fn matched_snippet(
    body: &str,
    source_version: &crate::model::SourceVersionView,
    evidence: Option<&MatchedChunkEvidence>,
) -> String {
    let Some(evidence) = evidence else {
        return snippet(body);
    };
    if evidence.source_version_hash != source_version.body_hash {
        return snippet(body);
    }
    let Some(matched) = body.get(evidence.chunk.byte_start..evidence.chunk.byte_end) else {
        return snippet(body);
    };
    if matched.is_empty() {
        return snippet(body);
    }
    snippet(matched)
}

fn lexical_evidence(
    store: &Store,
    source: &StoredSource,
    query: &str,
) -> Result<Option<MatchedChunkEvidence>, QghError> {
    let source_id = match source {
        StoredSource::Issue(issue) => &issue.source_id,
        StoredSource::Comment(comment) => &comment.source_id,
    };
    let source_version_hash = match source {
        StoredSource::Issue(issue) => issue.source_version.body_hash.clone(),
        StoredSource::Comment(comment) => comment.source_version.body_hash.clone(),
    };
    let Some(version_id) = store.latest_source_version_id(source_id)? else {
        return Ok(None);
    };
    let terms = query
        .split_whitespace()
        .map(str::to_lowercase)
        .filter(|term| !term.is_empty())
        .collect::<Vec<_>>();
    if terms.is_empty() {
        return Ok(None);
    }
    let mut best: Option<(usize, StoredChunk)> = None;
    for chunk in store.chunks_for_source_version(version_id)? {
        let body = chunk.body.to_lowercase();
        let matched = terms
            .iter()
            .filter(|term| body.contains(term.as_str()))
            .count();
        if matched == 0 {
            continue;
        }
        if best.as_ref().is_none_or(|(current, current_chunk)| {
            matched > *current
                || (matched == *current && chunk.chunk_index < current_chunk.chunk_index)
        }) {
            best = Some((matched, chunk));
        }
    }
    Ok(best.map(|(_, chunk)| MatchedChunkEvidence {
        chunk,
        source_version_hash,
        retriever_kind: "lexical",
        rank: 0,
        score_or_distance: 0.0,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{IssueRecord, SourceVersionView};

    #[cfg(feature = "fastembed-provider")]
    #[test]
    fn warm_runtime_cache_hit_skips_verifier_and_runtime_factory() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let cache = Mutex::new(HashMap::from([(
            "work:manifest:stamp".to_string(),
            Arc::new(7_usize),
        )]));
        let builds = AtomicUsize::new(0);

        let runtime = runtime_cache_get_or_try_init(
            &cache,
            "work:manifest:stamp",
            "work:",
            || -> Result<Arc<usize>, ()> {
                builds.fetch_add(1, Ordering::SeqCst);
                Ok(Arc::new(9))
            },
        )
        .unwrap();

        assert_eq!(*runtime, 7);
        assert_eq!(builds.load(Ordering::SeqCst), 0);
    }

    #[cfg(feature = "fastembed-provider")]
    #[test]
    fn changed_runtime_cache_stamp_verifies_before_replacing_profile_entry() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let cache = Mutex::new(HashMap::from([(
            "work:manifest:old-stamp".to_string(),
            Arc::new(7_usize),
        )]));
        let builds = AtomicUsize::new(0);

        let runtime = runtime_cache_get_or_try_init(
            &cache,
            "work:manifest:new-stamp",
            "work:",
            || -> Result<Arc<usize>, ()> {
                builds.fetch_add(1, Ordering::SeqCst);
                Ok(Arc::new(9))
            },
        )
        .unwrap();

        assert_eq!(*runtime, 9);
        assert_eq!(builds.load(Ordering::SeqCst), 1);
        let cache = cache.lock().unwrap();
        assert!(!cache.contains_key("work:manifest:old-stamp"));
        assert!(cache.contains_key("work:manifest:new-stamp"));
    }

    fn test_stored_chunk(source_id: &str) -> StoredChunk {
        StoredChunk {
            chunk_id: 1,
            source_id: source_id.to_string(),
            source_version_id: 1,
            body: "matched evidence".to_string(),
            chunk_index: 0,
            token_start: 0,
            token_end: 2,
            byte_start: 0,
            byte_end: 16,
            chunker_version: crate::chunking::CHUNKER_VERSION.to_string(),
            chunker_fingerprint: crate::chunking::CHUNKER_FINGERPRINT.to_string(),
            heading_path: Vec::new(),
        }
    }
    #[cfg(feature = "vector-search")]
    use crate::chunking::MarkdownChunk;
    #[cfg(feature = "vector-search")]
    use crate::embedding::PoolingKind;
    #[cfg(feature = "vector-search")]
    use crate::model::{CommentRecord, VectorSearchFilters};
    use crate::paths::ProfilePaths;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[cfg(feature = "vector-search")]
    struct MockEmbeddingProvider;

    #[cfg(feature = "vector-search")]
    #[derive(Default)]
    struct RecordingEmbeddingProvider {
        documents: std::sync::Mutex<Vec<String>>,
    }

    #[cfg(feature = "vector-search")]
    fn pinned_embedding_profile(paths: &ProfilePaths, model_revision: &str) -> Profile {
        Profile {
            id: "identity-test-profile".to_string(),
            host: "github.com".to_string(),
            api_base_url: "https://api.github.com".to_string(),
            web_base_url: "https://github.com".to_string(),
            repos: vec![crate::config::RepoRef {
                owner: "owner".to_string(),
                name: "repo".to_string(),
            }],
            embedding: Some(EmbeddingConfig {
                provider: EmbeddingProviderKind::Local,
                manifest_path: None,
                model: Some(format!("hf:fixture/model@{model_revision}")),
                model_path: None,
                file: None,
                pooling: Some(PoolingKind::Cls),
                query_prefix: Some(crate::embedding::DEFAULT_QUERY_PREFIX.to_string()),
                quantization: None,
                token_source: None,
            }),
            reconcile_after_seconds: None,
            freshness: crate::config::FreshnessSettings {
                query_max_age_seconds: 1,
                query_stale_behavior: crate::config::StaleBehavior::Warn,
                active_issue_max_age_seconds: None,
            },
            bootstrap: crate::config::BootstrapSettings {
                lookback_seconds: 1,
            },
            sync_max_age_seconds: None,
            comments_mode: CommentsMode::PerIssue,
            comment_parent_resolution_budget: 1,
            max_in_flight_requests: 1,
            token_source: TokenSource::GithubCli,
            paths: paths.clone(),
        }
    }

    #[cfg(feature = "vector-search")]
    impl EmbeddingProvider for RecordingEmbeddingProvider {
        fn embed_documents(
            &self,
            texts: &[&str],
        ) -> Result<Vec<EmbeddingVector>, EmbeddingProviderError> {
            self.documents
                .lock()
                .unwrap()
                .extend(texts.iter().map(|text| (*text).to_string()));
            Ok(texts.iter().map(|_| vec![1.0, 2.0, 3.0]).collect())
        }

        fn embed_query(&self, _text: &str) -> Result<EmbeddingVector, EmbeddingProviderError> {
            Ok(vec![1.0, 2.0, 3.0])
        }
    }

    #[test]
    fn first_publication_activation_failure_returns_original_error() {
        let paths = temp_profile_paths("first-publication-activation-failure");
        let profile = bm25_test_profile(&paths);
        let mut store = Store::open(&paths).unwrap();
        seed_bm25_snapshot(
            &mut store,
            "sync-first-publication-activation-failure",
            "I_FIRST_PUBLICATION_ACTIVATION_FAILURE",
        );
        store.fail_next_retrieval_publication_activation(QghError::new(
            "publication.test_activation_failed",
            "Fixture activation failed.",
            6,
        ));

        let error = match rebuild_bm25_index(&profile, &mut store, &StderrSyncProgress::new(false))
        {
            Ok(_) => panic!("first activation failure must be fatal"),
            Err(error) => error,
        };
        assert_eq!(error.code, "publication.test_activation_failed");
        assert!(store.active_retrieval_publication().unwrap().is_none());

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn activation_failure_warns_only_when_exact_previous_publication_is_valid() {
        let paths = temp_profile_paths("valid-previous-publication-activation-failure");
        let profile = bm25_test_profile(&paths);
        let mut store = Store::open(&paths).unwrap();
        seed_bm25_snapshot(
            &mut store,
            "sync-valid-previous-publication",
            "I_VALID_PREVIOUS_PUBLICATION",
        );
        rebuild_bm25_index(&profile, &mut store, &StderrSyncProgress::new(false)).unwrap();
        let previous_id = store
            .active_retrieval_publication()
            .unwrap()
            .unwrap()
            .publication_id;
        store.fail_next_retrieval_publication_activation(QghError::new(
            "publication.test_activation_failed",
            "Fixture activation failed.",
            6,
        ));

        let outcome =
            rebuild_bm25_index(&profile, &mut store, &StderrSyncProgress::new(false)).unwrap();
        assert!(outcome
            .warnings
            .iter()
            .any(|warning| warning["code"] == "publication.activation_failed"));
        assert_eq!(
            store
                .active_retrieval_publication()
                .unwrap()
                .unwrap()
                .publication_id,
            previous_id
        );
        assert!(matches!(
            store.resolve_active_tantivy_artifact(),
            Ok(Some(_))
        ));

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn activation_failure_is_fatal_when_previous_artifact_is_invalid() {
        let paths = temp_profile_paths("invalid-previous-publication-activation-failure");
        let profile = bm25_test_profile(&paths);
        let mut store = Store::open(&paths).unwrap();
        seed_bm25_snapshot(
            &mut store,
            "sync-invalid-previous-publication",
            "I_INVALID_PREVIOUS_PUBLICATION",
        );
        rebuild_bm25_index(&profile, &mut store, &StderrSyncProgress::new(false)).unwrap();
        let previous_path = store.resolve_active_tantivy_artifact().unwrap().unwrap();
        fs::remove_dir_all(previous_path).unwrap();
        store.fail_next_retrieval_publication_activation(QghError::new(
            "publication.test_activation_failed",
            "Fixture activation failed.",
            6,
        ));

        let error = match rebuild_bm25_index(&profile, &mut store, &StderrSyncProgress::new(false))
        {
            Ok(_) => panic!("invalid previous publication cannot justify fallback success"),
            Err(error) => error,
        };
        assert_eq!(error.code, "publication.test_activation_failed");

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn repository_purge_subsumes_only_same_repo_targets() {
        let requests = vec![
            (
                PurgeTarget::Issue {
                    repo: "owner/repo".to_string(),
                    issue_number: 42,
                },
                PurgeTrigger::ConfirmedDelete,
            ),
            (
                PurgeTarget::Repository {
                    repo: "owner/repo".to_string(),
                },
                PurgeTrigger::PermissionLoss,
            ),
            (
                PurgeTarget::Issue {
                    repo: "other/repo".to_string(),
                    issue_number: 7,
                },
                PurgeTrigger::ConfirmedTombstone,
            ),
        ];

        let canonical = canonicalize_purge_requests(&requests);
        assert_eq!(
            canonical,
            vec![
                (
                    PurgeTarget::Repository {
                        repo: "owner/repo".to_string(),
                    },
                    PurgeTrigger::PermissionLoss,
                ),
                (
                    PurgeTarget::Issue {
                        repo: "other/repo".to_string(),
                        issue_number: 7,
                    },
                    PurgeTrigger::ConfirmedTombstone,
                ),
            ]
        );
    }

    #[test]
    fn lifecycle_request_builders_keep_valid_evidence_before_strict_error() {
        let fetch = confirmed_fetch_purge_requests(
            &[github::ConfirmedRepositoryPermissionLoss {
                repo: "owner/repo".to_string(),
                http_status: 403,
            }],
            &[
                github::ConfirmedSourceDeletion {
                    source_id: "qgh://github.com/issue-comment/IC_VALID".to_string(),
                    repo: "other/repo".to_string(),
                    entity_type: "issue_comment".to_string(),
                    issue_number: 7,
                    http_status: 404,
                },
                github::ConfirmedSourceDeletion {
                    source_id: "qgh://github.com/issue-comment/IC_UNKNOWN".to_string(),
                    repo: "third/repo".to_string(),
                    entity_type: "unknown".to_string(),
                    issue_number: 9,
                    http_status: 404,
                },
            ],
        );
        assert_eq!(fetch.requests.len(), 2);
        assert_eq!(
            fetch.deferred_error.as_ref().unwrap().code,
            "purge.lifecycle_candidate_missing"
        );

        let reconciliation = reconciliation_purge_requests(
            &[
                github::LifecycleFailure {
                    source_id: "qgh://github.com/issue/I_VALID".to_string(),
                    repo: "owner/repo".to_string(),
                    entity_type: "issue".to_string(),
                    issue_number: 42,
                    reason: "deleted".to_string(),
                    state: github::ConfirmedRemoteState::SourceDeleted,
                    http_status: 404,
                },
                github::LifecycleFailure {
                    source_id: "qgh://github.com/issue/I_UNKNOWN".to_string(),
                    repo: "owner/repo".to_string(),
                    entity_type: "unknown".to_string(),
                    issue_number: 43,
                    reason: "deleted".to_string(),
                    state: github::ConfirmedRemoteState::SourceDeleted,
                    http_status: 404,
                },
            ],
            &[],
        );
        assert_eq!(reconciliation.requests.len(), 1);
        assert_eq!(
            reconciliation.deferred_error.as_ref().unwrap().code,
            "purge.lifecycle_candidate_missing"
        );

        let transition = target_transition_purge_requests(&[github::ConfirmedIssueTransition {
            source_repo: "owner/repo".to_string(),
            source_issue_number: 42,
            target_repo: "owner/repo".to_string(),
            target_issue_number: 43,
            state: github::ConfirmedRemoteState::SourceDeleted,
            http_status: 404,
        }]);
        assert_eq!(transition.requests.len(), 1);
        assert_eq!(
            transition.deferred_error.as_ref().unwrap().code,
            "purge.lifecycle_candidate_missing"
        );
    }

    #[cfg(feature = "vector-search")]
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

    #[cfg(feature = "vector-search")]
    struct PanicQueryEmbeddingProvider;

    #[cfg(feature = "vector-search")]
    impl EmbeddingProvider for PanicQueryEmbeddingProvider {
        fn embed_documents(
            &self,
            _texts: &[&str],
        ) -> Result<Vec<EmbeddingVector>, EmbeddingProviderError> {
            panic!("document encoding is not part of this query test")
        }

        fn embed_query(&self, _text: &str) -> Result<EmbeddingVector, EmbeddingProviderError> {
            panic!("runtime mismatch must be rejected before query encoding")
        }
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn runtime_fingerprint_mismatch_blocks_query_encoding_before_bm25_fallback() {
        let runtime = EmbeddingRuntime {
            tokenizer: Box::new(TestEmbeddingTokenizer),
            provider: Box::new(PanicQueryEmbeddingProvider),
            model_manifest_hash: "manifest-query-runtime-check".to_string(),
            fingerprint_seed: EmbeddingFingerprintSeed {
                provider: "local".to_string(),
                model_id: "fixture/model".to_string(),
                model_revision: "fixture-sha".to_string(),
                pooling: PoolingKind::Cls,
                query_prefix: crate::embedding::DEFAULT_QUERY_PREFIX.to_string(),
            },
        };
        let publication = RetrievalPublicationView {
            publication_id: 1,
            source_snapshot_sync_run_id: "sync-query-runtime-check".to_string(),
            source_snapshot_epoch: 1,
            tantivy_generation: 1,
            embedding_generation_id: Some(1),
            model_manifest_hash: Some("manifest-query-runtime-check".to_string()),
            runtime_fingerprint_hash: Some("wrong-runtime-fingerprint".to_string()),
            chunker_fingerprint: Some(crate::chunking::CHUNKER_FINGERPRINT.to_string()),
            context_template_version: Some(
                crate::context::METADATA_CONTEXT_TEMPLATE_VERSION.to_string(),
            ),
            output_dimension: Some(3),
        };

        let error = encode_hybrid_query(&runtime, Some(&publication), "query-not-logged")
            .expect_err("runtime mismatch must fail closed");
        assert_eq!(error, HybridQueryEncodingError::FingerprintMismatch);
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn fresh_distinct_identity_reaches_hybrid_query_encoding() {
        let fingerprint_seed = EmbeddingFingerprintSeed {
            provider: "local".to_string(),
            model_id: "fixture/model".to_string(),
            model_revision: "fixture-sha".to_string(),
            pooling: PoolingKind::Cls,
            query_prefix: crate::embedding::DEFAULT_QUERY_PREFIX.to_string(),
        };
        let runtime_fingerprint_hash = fingerprint_seed.clone().with_dimension(3).hash();
        let model_manifest_hash = "manifest-distinct-from-runtime";
        assert_ne!(model_manifest_hash, runtime_fingerprint_hash);
        let runtime = EmbeddingRuntime {
            tokenizer: Box::new(TestEmbeddingTokenizer),
            provider: Box::new(RecordingEmbeddingProvider::default()),
            model_manifest_hash: model_manifest_hash.to_string(),
            fingerprint_seed,
        };
        let publication = RetrievalPublicationView {
            publication_id: 1,
            source_snapshot_sync_run_id: "sync-fresh-hybrid-identity".to_string(),
            source_snapshot_epoch: 1,
            tantivy_generation: 1,
            embedding_generation_id: Some(1),
            model_manifest_hash: Some(model_manifest_hash.to_string()),
            runtime_fingerprint_hash: Some(runtime_fingerprint_hash),
            chunker_fingerprint: Some(crate::chunking::CHUNKER_FINGERPRINT.to_string()),
            context_template_version: Some(
                crate::context::METADATA_CONTEXT_TEMPLATE_VERSION.to_string(),
            ),
            output_dimension: Some(3),
        };

        assert_eq!(
            encode_hybrid_query(&runtime, Some(&publication), "query-not-logged").unwrap(),
            vec![1.0, 2.0, 3.0]
        );
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn sync_embedding_provider_receives_production_issue_context() {
        let paths = temp_profile_paths("sync-embedding-context");
        let mut store = Store::open(&paths).unwrap();
        store.enable_vector().unwrap();
        let issue = vector_issue("I_CONTEXT_SYNC", "owner/repo", 47, "open", "alice", &[]);
        let source_id = issue.source_id.clone();
        store
            .upsert_sources_for_run("sync-context", &[issue], &[], 0, &[])
            .unwrap();
        store.mark_sync_run_completed("sync-context").unwrap();
        let source_version_id = store.latest_source_version_id(&source_id).unwrap().unwrap();
        store
            .replace_chunks_for_source_version(
                &source_id,
                source_version_id,
                &[test_chunk("unchanged authoritative chunk".to_string())],
            )
            .unwrap();
        store.mark_sync_run_completed("sync-context").unwrap();
        let provider = RecordingEmbeddingProvider::default();
        let seed = EmbeddingFingerprintSeed {
            provider: "local".to_string(),
            model_id: "fixture/model".to_string(),
            model_revision: "fixture-sha".to_string(),
            pooling: PoolingKind::Cls,
            query_prefix: crate::embedding::DEFAULT_QUERY_PREFIX.to_string(),
        };
        let expectation = EmbeddingFingerprintExpectation {
            provider: "local".to_string(),
            model_id: Some("fixture/model".to_string()),
            model_revision: Some("fixture-sha".to_string()),
            pooling: Some(PoolingKind::Cls),
            query_prefix: Some(crate::embedding::DEFAULT_QUERY_PREFIX.to_string()),
        };

        refresh_incremental_chunk_embeddings_with_provider(
            &mut store,
            &provider,
            "manifest-context-sync".to_string(),
            seed,
            &expectation,
        )
        .unwrap();

        assert_eq!(
            *provider.documents.lock().unwrap(),
            vec!["Repository: github.com/owner/repo\nIssue #47: Vector smoke I_CONTEXT_SYNC\n\nunchanged authoritative chunk"]
        );
        assert_eq!(
            store.active_contextual_embedding_chunks().unwrap()[0]
                .chunk
                .body,
            "unchanged authoritative chunk"
        );

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn force_embedding_provider_and_persisted_hash_share_production_comment_context() {
        let paths = temp_profile_paths("force-embedding-comment-context");
        let mut store = Store::open(&paths).unwrap();
        store.enable_vector().unwrap();
        let issue = vector_issue("I_CONTEXT_FORCE", "owner/repo", 48, "open", "alice", &[]);
        let comment = CommentRecord {
            source_id: "qgh://github.com/issue-comment/IC_CONTEXT_FORCE".to_string(),
            host: "github.com".to_string(),
            repo: "owner/repo".to_string(),
            node_id: "IC_CONTEXT_FORCE".to_string(),
            github_id: 4801,
            body: "authoritative comment body".to_string(),
            author: Some("bob".to_string()),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-02T00:00:00Z".to_string(),
            canonical_url: "https://github.com/owner/repo/issues/48#issuecomment-4801".to_string(),
            body_hash: "comment-body-hash-context-force".to_string(),
            indexed_at: "2026-01-02T00:00:01Z".to_string(),
            parent_issue_source_id: issue.source_id.clone(),
            parent_issue_number: issue.number,
            parent_issue_title: issue.title.clone(),
            parent_issue_canonical_url: issue.canonical_url.clone(),
        };
        store
            .upsert_sources_for_run(
                "sync-force-context",
                &[issue],
                std::slice::from_ref(&comment),
                0,
                &[],
            )
            .unwrap();
        store.mark_sync_run_completed("sync-force-context").unwrap();
        let source_version_id = store
            .latest_source_version_id(&comment.source_id)
            .unwrap()
            .unwrap();
        store
            .replace_chunks_for_source_version(
                &comment.source_id,
                source_version_id,
                &[test_chunk("unchanged comment chunk".to_string())],
            )
            .unwrap();
        store.mark_sync_run_completed("sync-force-context").unwrap();
        let snapshot = store.capture_retrieval_build_snapshot().unwrap().unwrap();
        let provider = RecordingEmbeddingProvider::default();
        let model_revision = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let seed = EmbeddingFingerprintSeed {
            provider: "local".to_string(),
            model_id: "fixture/model".to_string(),
            model_revision: model_revision.to_string(),
            pooling: PoolingKind::Cls,
            query_prefix: crate::embedding::DEFAULT_QUERY_PREFIX.to_string(),
        };
        let model_manifest_hash = "manifest-context-force";
        let expected_runtime_fingerprint_hash = seed.clone().with_dimension(3).hash();

        refresh_chunk_embeddings(
            &mut store,
            &paths,
            &provider,
            model_manifest_hash.to_string(),
            seed,
            &snapshot,
        )
        .unwrap();

        let expected_input = "Repository: github.com/owner/repo\nComment on issue #48: Vector smoke I_CONTEXT_FORCE\n\nunchanged comment chunk";
        assert_eq!(
            *provider.documents.lock().unwrap(),
            vec![expected_input.to_string()]
        );
        assert_eq!(
            store.active_contextual_embedding_chunks().unwrap()[0]
                .chunk
                .body,
            "unchanged comment chunk"
        );
        let connection = rusqlite::Connection::open(&paths.db_path).unwrap();
        let (
            stored_hash,
            manifest_hash,
            runtime_fingerprint_hash,
            chunker_fingerprint,
            template_version,
        ): (String, String, String, String, String) = connection
            .query_row(
                "SELECT egc.context_hash, eg.model_manifest_hash,
                        eg.runtime_fingerprint_hash,
                        eg.chunker_fingerprint, eg.context_template_version
                 FROM embedding_generation_chunks egc
                 JOIN embedding_generations eg ON eg.id = egc.generation_id
                 ORDER BY eg.id DESC LIMIT 1",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(manifest_hash, model_manifest_hash);
        assert_eq!(runtime_fingerprint_hash, expected_runtime_fingerprint_hash);
        assert_ne!(manifest_hash, runtime_fingerprint_hash);
        assert_eq!(
            stored_hash,
            crate::context::embedding_context_hash(
                &manifest_hash,
                &chunker_fingerprint,
                &template_version,
                expected_input,
            )
        );
        assert_ne!(
            stored_hash,
            crate::context::embedding_context_hash(
                &runtime_fingerprint_hash,
                &chunker_fingerprint,
                &template_version,
                expected_input,
            )
        );
        let profile = pinned_embedding_profile(&paths, model_revision);
        let embedding = profile.embedding.as_ref().unwrap();
        let mut configured = configured_embedding_snapshot(embedding);
        configured.prepared_runtime = PreparedRuntimeAvailability::Available;
        let coverage = embedding_coverage_state_for_config(embedding, &store, &configured).unwrap();
        assert_eq!(coverage.state(), "complete");
        assert!(coverage.hybrid_ready());

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn hybrid_rrf_overfetch_snapshot_differs_from_bm25_and_dedupes_sources() {
        let bm25_hits = vec![
            index::SearchHit {
                source_id: "source-a".to_string(),
                source_updated_at: Some("2026-01-01T00:00:00Z".to_string()),
                score: 10.0,
            },
            index::SearchHit {
                source_id: "source-b".to_string(),
                source_updated_at: Some("2026-01-01T00:00:00Z".to_string()),
                score: 9.0,
            },
            index::SearchHit {
                source_id: "source-d".to_string(),
                source_updated_at: Some("2026-01-01T00:00:00Z".to_string()),
                score: 8.0,
            },
            index::SearchHit {
                source_id: "source-a".to_string(),
                source_updated_at: Some("2026-01-01T00:00:00Z".to_string()),
                score: 1.0,
            },
        ];
        let bm25_snapshot = bm25_hits
            .iter()
            .take(3)
            .map(|hit| hit.source_id.clone())
            .collect::<Vec<_>>();
        let vector_hits = vec![
            crate::model::VectorSearchHit {
                source_id: "source-c".to_string(),
                chunk: test_stored_chunk("source-c"),
                source_version_hash: "test-version".to_string(),
                vector_distance: 0.01,
            },
            crate::model::VectorSearchHit {
                source_id: "source-a".to_string(),
                chunk: test_stored_chunk("source-a"),
                source_version_hash: "test-version".to_string(),
                vector_distance: 0.02,
            },
            crate::model::VectorSearchHit {
                source_id: "source-c".to_string(),
                chunk: test_stored_chunk("source-c"),
                source_version_hash: "test-version".to_string(),
                vector_distance: 0.03,
            },
        ];

        let hits = fuse_hybrid_hits(bm25_hits, vector_hits, 3);
        let hybrid_snapshot = hits
            .iter()
            .map(|hit| hit.source_id.clone())
            .collect::<Vec<_>>();

        assert_eq!(bm25_snapshot, vec!["source-a", "source-b", "source-d"]);
        assert_eq!(hybrid_snapshot, vec!["source-a", "source-c", "source-b"]);
        assert_ne!(hybrid_snapshot, bm25_snapshot);
        assert_eq!(
            hits.iter()
                .filter(|hit| hit.source_id == "source-c")
                .count(),
            1
        );
        match &hits[0].ranking {
            Ranking::Hybrid {
                lexical_score,
                vector_distance,
                rrf_rank_score,
                final_order_score,
            } => {
                assert_eq!(*lexical_score, Some(10.0));
                assert_eq!(*vector_distance, Some(0.02));
                let expected = rrf_component(Some(1)) + rrf_component(Some(2));
                assert_eq!(*rrf_rank_score, expected);
                assert_eq!(*final_order_score, expected);
            }
            _ => panic!("hybrid sources must expose fused ranking evidence"),
        }
        match &hits[1].ranking {
            Ranking::Hybrid {
                lexical_score,
                vector_distance,
                rrf_rank_score,
                final_order_score,
            } => {
                assert_eq!(*lexical_score, None);
                assert_eq!(*vector_distance, Some(0.01));
                let expected = rrf_component(Some(1));
                assert_eq!(*rrf_rank_score, expected);
                assert_eq!(*final_order_score, expected);
            }
            _ => panic!("vector-only hybrid sources must expose fused ranking evidence"),
        }
        let ranking = ranking_json(Ranking::Hybrid {
            lexical_score: Some(10.0),
            vector_distance: Some(0.02),
            rrf_rank_score: 0.032,
            final_order_score: 0.032,
        });
        assert_eq!(ranking["kind"], "hybrid");
        assert_eq!(ranking["lexical_score"], json!(10.0));
        assert!((ranking["vector_distance"].as_f64().unwrap() - 0.02).abs() < 1e-6);
        assert!((ranking["rrf_rank_score"].as_f64().unwrap() - 0.032).abs() < 1e-6);
        assert!((ranking["final_order_score"].as_f64().unwrap() - 0.032).abs() < 1e-6);
        assert!(ranking.get("confidence").is_none());
        assert!(ranking.get("probability").is_none());
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn force_refresh_persists_vectors_under_new_fingerprint() {
        let paths = temp_profile_paths("command-embed-force");
        let mut store = Store::open(&paths).unwrap();
        store.enable_vector().unwrap();
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
                        chunker_version: crate::chunking::CHUNKER_VERSION.to_string(),
                        chunker_fingerprint: crate::chunking::CHUNKER_FINGERPRINT.to_string(),
                        heading_path: Vec::new(),
                    },
                    MarkdownChunk {
                        chunk_index: 1,
                        byte_start: 11,
                        byte_end: 22,
                        token_start: 2,
                        token_end: 4,
                        token_count: 2,
                        body: "gamma delta".to_string(),
                        chunker_version: crate::chunking::CHUNKER_VERSION.to_string(),
                        chunker_fingerprint: crate::chunking::CHUNKER_FINGERPRINT.to_string(),
                        heading_path: Vec::new(),
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

        let missing_snapshot = store.capture_retrieval_build_snapshot().unwrap_err();
        assert_eq!(
            missing_snapshot.code,
            "publication.source_snapshot_incomplete"
        );
        store.mark_sync_run_completed("sync-command-embed").unwrap();
        let first_snapshot = store.capture_retrieval_build_snapshot().unwrap().unwrap();
        let (generation, reserved_path) = store
            .reserve_index_generation_for_snapshot(&paths.index_root, &first_snapshot)
            .unwrap();
        let generation_path =
            index::rebuild(&paths.index_root, generation, first_snapshot.sources()).unwrap();
        assert_eq!(generation_path, reserved_path);
        store
            .activate_retrieval_publication("sync-command-embed", generation, None, None)
            .unwrap();
        let force_snapshot = store.capture_retrieval_build_snapshot().unwrap().unwrap();
        let outcome = refresh_chunk_embeddings(
            &mut store,
            &paths,
            &MockEmbeddingProvider,
            "manifest-command-embed".to_string(),
            seed,
            &force_snapshot,
        )
        .unwrap();

        assert_eq!(outcome["embedded_chunks"], 2);
        assert_eq!(outcome["usable_embeddings"], 2);
        assert_eq!(
            store
                .latest_embedding_generation_state()
                .unwrap()
                .as_deref(),
            Some("active")
        );
        assert!(store
            .active_retrieval_publication()
            .unwrap()
            .unwrap()
            .embedding_generation_id
            .is_some());

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn chunk_refresh_replaces_stale_fingerprint_and_is_idempotent() {
        let paths = temp_profile_paths("command-refresh-stale-chunk-fingerprint");
        let mut store = Store::open(&paths).unwrap();
        store.enable_vector().unwrap();
        let source_id = "qgh://github.com/issue/I_STALE_CHUNK_FINGERPRINT";
        let issue = IssueRecord {
            source_id: source_id.to_string(),
            host: "github.com".to_string(),
            repo: "owner/repo".to_string(),
            node_id: "I_STALE_CHUNK_FINGERPRINT".to_string(),
            github_id: 906,
            number: 906,
            title: "Stale chunk fingerprint".to_string(),
            body: "raw body must remain byte-for-byte stable".to_string(),
            state: "open".to_string(),
            labels: Vec::new(),
            milestone: None,
            assignees: Vec::new(),
            author: Some("alice".to_string()),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-02T00:00:00Z".to_string(),
            closed_at: None,
            canonical_url: "https://github.com/owner/repo/issues/906".to_string(),
            body_hash: "body-hash-stale-chunk-fingerprint".to_string(),
            indexed_at: "2026-01-02T00:00:01Z".to_string(),
        };
        store
            .upsert_sources_for_run(
                "sync-stale-chunk-fingerprint",
                std::slice::from_ref(&issue),
                &[],
                0,
                &[],
            )
            .unwrap();
        let progress = StderrSyncProgress::new(false);
        refresh_embedding_chunks(&mut store, &TestEmbeddingTokenizer, &progress).unwrap();
        let source_version_id = store.latest_source_version_id(source_id).unwrap().unwrap();
        rusqlite::Connection::open(&paths.db_path)
            .unwrap()
            .execute(
                "UPDATE chunks SET chunker_fingerprint = 'legacy-stale-fingerprint'
                 WHERE source_version_id = ?1",
                rusqlite::params![source_version_id],
            )
            .unwrap();
        let raw_body_before = store.get_issue(source_id).unwrap().unwrap().body;

        let refreshed =
            refresh_embedding_chunks(&mut store, &TestEmbeddingTokenizer, &progress).unwrap();

        assert_eq!(refreshed.skipped_sources, 0);
        assert!(refreshed.refreshed_chunks > 0);
        let refreshed_chunks = store.chunks_for_source_version(source_version_id).unwrap();
        assert!(refreshed_chunks
            .iter()
            .all(|chunk| { chunk.chunker_fingerprint == crate::chunking::CHUNKER_FINGERPRINT }));
        assert_eq!(
            store.get_issue(source_id).unwrap().unwrap().body,
            raw_body_before
        );
        let refreshed_ids = refreshed_chunks
            .iter()
            .map(|chunk| chunk.chunk_id)
            .collect::<Vec<_>>();

        let second =
            refresh_embedding_chunks(&mut store, &TestEmbeddingTokenizer, &progress).unwrap();

        assert_eq!(second.refreshed_chunks, 0);
        assert_eq!(second.skipped_sources, 1);
        assert_eq!(
            store
                .chunks_for_source_version(source_version_id)
                .unwrap()
                .iter()
                .map(|chunk| chunk.chunk_id)
                .collect::<Vec<_>>(),
            refreshed_ids
        );

        rusqlite::Connection::open(&paths.db_path)
            .unwrap()
            .execute(
                "INSERT INTO chunks
                    (source_id, source_version_id, body, chunk_index, token_start,
                     token_end, byte_start, byte_end, chunker_version,
                     chunker_fingerprint, heading_path_json)
                 SELECT source_id, source_version_id, body, chunk_index + 1, token_start,
                        token_end, byte_start, byte_end, chunker_version,
                        'legacy-mixed-fingerprint', heading_path_json
                 FROM chunks WHERE source_version_id = ?1 LIMIT 1",
                rusqlite::params![source_version_id],
            )
            .unwrap();
        let mixed =
            refresh_embedding_chunks(&mut store, &TestEmbeddingTokenizer, &progress).unwrap();
        assert!(mixed.refreshed_chunks > 0);
        assert!(store
            .chunks_for_source_version(source_version_id)
            .unwrap()
            .iter()
            .all(|chunk| chunk.chunker_fingerprint == CHUNKER_FINGERPRINT));

        let conn = rusqlite::Connection::open(&paths.db_path).unwrap();
        conn.execute_batch(
            "ALTER TABLE chunks RENAME TO chunks_with_strict_fingerprint;
             CREATE TABLE chunks (
                id INTEGER PRIMARY KEY,
                source_id TEXT NOT NULL,
                source_version_id INTEGER NOT NULL,
                body TEXT NOT NULL,
                chunk_index INTEGER NOT NULL DEFAULT 0,
                token_start INTEGER NOT NULL DEFAULT 0,
                token_end INTEGER NOT NULL DEFAULT 0,
                byte_start INTEGER NOT NULL DEFAULT 0,
                byte_end INTEGER NOT NULL DEFAULT 0,
                chunker_version TEXT NOT NULL DEFAULT 'markdown-token-v1',
                chunker_fingerprint TEXT,
                heading_path_json TEXT NOT NULL DEFAULT '[]'
             );
             INSERT INTO chunks
                (id, source_id, source_version_id, body, chunk_index, token_start,
                 token_end, byte_start, byte_end, chunker_version,
                 chunker_fingerprint, heading_path_json)
             SELECT id, source_id, source_version_id, body, chunk_index, token_start,
                    token_end, byte_start, byte_end, chunker_version,
                    NULL, heading_path_json
             FROM chunks_with_strict_fingerprint;
             DROP TABLE chunks_with_strict_fingerprint;",
        )
        .unwrap();
        drop(conn);

        let null_fingerprint =
            refresh_embedding_chunks(&mut store, &TestEmbeddingTokenizer, &progress).unwrap();
        assert!(null_fingerprint.refreshed_chunks > 0);
        assert!(store
            .chunks_for_source_version(source_version_id)
            .unwrap()
            .iter()
            .all(|chunk| chunk.chunker_fingerprint == CHUNKER_FINGERPRINT));
        assert_eq!(
            store.get_issue(source_id).unwrap().unwrap().body,
            raw_body_before
        );

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn sync_incremental_embedding_persists_vectors_and_skips_completed_chunks() {
        let paths = temp_profile_paths("sync-incremental-embedding");
        let mut store = Store::open(&paths).unwrap();
        store.enable_vector().unwrap();
        let source_id = "qgh://github.com/issue/I_SYNC_EMBED";
        let issue = IssueRecord {
            source_id: source_id.to_string(),
            host: "github.com".to_string(),
            repo: "owner/repo".to_string(),
            node_id: "I_SYNC_EMBED".to_string(),
            github_id: 404,
            number: 10,
            title: "Sync embed".to_string(),
            body: "sync alpha beta gamma delta".to_string(),
            state: "open".to_string(),
            labels: Vec::new(),
            milestone: None,
            assignees: Vec::new(),
            author: Some("alice".to_string()),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-02T00:00:00Z".to_string(),
            closed_at: None,
            canonical_url: "https://github.com/owner/repo/issues/10".to_string(),
            body_hash: "body-hash-sync-embed".to_string(),
            indexed_at: "2026-01-02T00:00:01Z".to_string(),
        };
        store
            .upsert_sources_for_run("sync-incremental-embed", &[issue], &[], 0, &[])
            .unwrap();
        store
            .mark_sync_run_completed("sync-incremental-embed")
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
                        byte_end: 15,
                        token_start: 0,
                        token_end: 3,
                        token_count: 3,
                        body: "sync alpha beta".to_string(),
                        chunker_version: crate::chunking::CHUNKER_VERSION.to_string(),
                        chunker_fingerprint: crate::chunking::CHUNKER_FINGERPRINT.to_string(),
                        heading_path: Vec::new(),
                    },
                    MarkdownChunk {
                        chunk_index: 1,
                        byte_start: 16,
                        byte_end: 27,
                        token_start: 3,
                        token_end: 5,
                        token_count: 2,
                        body: "gamma delta".to_string(),
                        chunker_version: crate::chunking::CHUNKER_VERSION.to_string(),
                        chunker_fingerprint: crate::chunking::CHUNKER_FINGERPRINT.to_string(),
                        heading_path: Vec::new(),
                    },
                ],
            )
            .unwrap();
        store
            .mark_sync_run_completed("sync-incremental-embed")
            .unwrap();
        let seed = EmbeddingFingerprintSeed {
            provider: "local".to_string(),
            model_id: "Snowflake/snowflake-arctic-embed-l-v2.0".to_string(),
            model_revision: "fixture-sha".to_string(),
            pooling: PoolingKind::Cls,
            query_prefix: crate::embedding::DEFAULT_QUERY_PREFIX.to_string(),
        };
        let expectation = EmbeddingFingerprintExpectation {
            provider: "local".to_string(),
            model_id: Some("Snowflake/snowflake-arctic-embed-l-v2.0".to_string()),
            model_revision: Some("fixture-sha".to_string()),
            pooling: Some(PoolingKind::Cls),
            query_prefix: Some(crate::embedding::DEFAULT_QUERY_PREFIX.to_string()),
        };

        let embedded = refresh_incremental_chunk_embeddings_with_provider(
            &mut store,
            &MockEmbeddingProvider,
            "manifest-sync-embed".to_string(),
            seed.clone(),
            &expectation,
        )
        .unwrap();
        assert_eq!(embedded, 2);
        assert_eq!(
            store
                .latest_embedding_generation_state()
                .unwrap()
                .as_deref(),
            Some("ready")
        );

        let skipped = refresh_incremental_chunk_embeddings_with_provider(
            &mut store,
            &MockEmbeddingProvider,
            "manifest-sync-embed".to_string(),
            seed,
            &expectation,
        )
        .unwrap();
        assert_eq!(skipped, 2);

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn vector_only_smoke_prefilters_and_round_trips_sources() {
        let paths = temp_profile_paths("vector-only-smoke");
        let mut store = Store::open(&paths).unwrap();
        store.enable_vector().unwrap();
        let issues = vec![
            vector_issue("I_VECTOR_REPO", "other/repo", 1, "open", "bob", &["bug"]),
            vector_issue(
                "I_VECTOR_LABEL",
                "owner/repo",
                2,
                "open",
                "bob",
                &["enhancement"],
            ),
            vector_issue("I_VECTOR_STATE", "owner/repo", 3, "closed", "bob", &["bug"]),
            vector_issue(
                "I_VECTOR_AUTHOR",
                "owner/repo",
                4,
                "open",
                "alice",
                &["bug"],
            ),
            vector_issue("I_VECTOR_ALLOWED", "owner/repo", 5, "open", "bob", &["bug"]),
        ];
        store.upsert_sources(&issues, &[], 0, &[]).unwrap();

        let vectors = [
            vec![0.0, 0.0, 0.0],
            vec![0.01, 0.0, 0.0],
            vec![0.02, 0.0, 0.0],
            vec![0.03, 0.0, 0.0],
            vec![10.0, 0.0, 0.0],
        ];
        let mut embeddings = Vec::new();
        for (issue, vector) in issues.iter().zip(vectors) {
            let source_version_id = store
                .latest_source_version_id(&issue.source_id)
                .unwrap()
                .unwrap();
            let chunk_bodies = if issue.node_id == "I_VECTOR_ALLOWED" {
                vec![
                    test_chunk("far allowed chunk".to_string()),
                    test_chunk("best allowed chunk".to_string()),
                ]
            } else {
                vec![test_chunk(format!("vector smoke chunk {}", issue.node_id))]
            };
            let chunks = store
                .replace_chunks_for_source_version(
                    &issue.source_id,
                    source_version_id,
                    &chunk_bodies,
                )
                .unwrap();
            embeddings.push((chunks[0].chunk_id, vector));
            if issue.node_id == "I_VECTOR_ALLOWED" {
                embeddings.push((chunks[1].chunk_id, vec![0.0, 0.0, 0.0]));
            }
        }
        let fingerprint = EmbeddingFingerprintSeed {
            provider: "local".to_string(),
            model_id: "Snowflake/snowflake-arctic-embed-l-v2.0".to_string(),
            model_revision: "fixture-sha".to_string(),
            pooling: PoolingKind::Cls,
            query_prefix: crate::embedding::DEFAULT_QUERY_PREFIX.to_string(),
        }
        .with_dimension(3);
        store
            .replace_all_chunk_embeddings(&fingerprint, &embeddings)
            .unwrap();

        let filters = VectorSearchFilters {
            repo: Some("owner/repo".to_string()),
            labels: vec!["bug".to_string()],
            state: Some("open".to_string()),
            author: Some("bob".to_string()),
            issue: None,
            source_types: vec!["issue".to_string()],
        };
        let hits = store
            .vector_only_search(&[0.0, 0.0, 0.0], &filters, 1)
            .unwrap();

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].source_id, "qgh://github.com/issue/I_VECTOR_ALLOWED");
        assert_eq!(hits[0].vector_distance, 0.0);
        assert_eq!(hits[0].chunk.body, "best allowed chunk");
        assert_eq!(hits[0].source_version_hash, "body-hash-I_VECTOR_ALLOWED");
        assert!(
            hits[0].vector_distance.is_finite(),
            "vector_distance must be finite"
        );

        let mut round_trip_successes = 0;
        for hit in &hits {
            let source = store.get_source(&hit.source_id).unwrap().unwrap();
            let result = source_result(
                source,
                Ranking::Vector {
                    vector_distance: hit.vector_distance,
                },
                "work",
                None,
            );
            let get_source = get_source_base(&store, &hit.source_id, None).unwrap();
            assert_eq!(result["source_id"], get_source["source_id"]);
            assert_eq!(result["canonical_url"], get_source["canonical_url"]);
            assert_eq!(result["source_version"], get_source["source_version"]);
            assert_eq!(result["get_args"]["source_id"], hit.source_id);
            assert_eq!(result["get_args"]["profile_id"], "work");
            assert_eq!(result["ranking"]["kind"], "vector");
            assert!(result["ranking"]["lexical_score"].is_null());
            assert_eq!(
                result["ranking"]["vector_distance"],
                json!(hit.vector_distance)
            );
            round_trip_successes += 1;
        }
        assert_eq!(round_trip_successes, hits.len());

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn matched_snippet_uses_valid_current_span_and_rejects_stale_span() {
        let source_version = SourceVersionView {
            body_hash: "current".to_string(),
            github_updated_at: "2026-01-01T00:00:00Z".to_string(),
            indexed_at: "2026-01-01T00:00:00Z".to_string(),
            sync_run_id: "sync-test".to_string(),
            lifecycle_state: "active".to_string(),
        };
        let mut chunk = test_stored_chunk("source");
        chunk.byte_start = 7;
        chunk.byte_end = 14;
        let evidence = MatchedChunkEvidence {
            chunk,
            source_version_hash: "current".to_string(),
            retriever_kind: "vector",
            rank: 1,
            score_or_distance: 0.1,
        };
        assert_eq!(
            matched_snippet("prefix matched suffix", &source_version, Some(&evidence)),
            "matched"
        );

        let stale = MatchedChunkEvidence {
            source_version_hash: "old".to_string(),
            ..evidence
        };
        assert_eq!(
            matched_snippet("prefix matched suffix", &source_version, Some(&stale)),
            "prefix matched suffix"
        );
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

    fn bm25_test_profile(paths: &ProfilePaths) -> Profile {
        Profile {
            id: "bm25-test-profile".to_string(),
            host: "github.com".to_string(),
            api_base_url: "https://api.github.com".to_string(),
            web_base_url: "https://github.com".to_string(),
            repos: vec![crate::config::RepoRef {
                owner: "owner".to_string(),
                name: "repo".to_string(),
            }],
            embedding: None,
            reconcile_after_seconds: None,
            freshness: crate::config::FreshnessSettings {
                query_max_age_seconds: 1,
                query_stale_behavior: crate::config::StaleBehavior::Warn,
                active_issue_max_age_seconds: None,
            },
            bootstrap: crate::config::BootstrapSettings {
                lookback_seconds: 1,
            },
            sync_max_age_seconds: None,
            comments_mode: CommentsMode::PerIssue,
            comment_parent_resolution_budget: 1,
            max_in_flight_requests: 1,
            token_source: TokenSource::GithubCli,
            paths: paths.clone(),
        }
    }

    fn seed_bm25_snapshot(store: &mut Store, sync_run_id: &str, node_id: &str) {
        store
            .upsert_sources_for_run(
                sync_run_id,
                &[IssueRecord {
                    source_id: format!("qgh://github.com/issue/{node_id}"),
                    host: "github.com".to_string(),
                    repo: "owner/repo".to_string(),
                    node_id: node_id.to_string(),
                    github_id: 47,
                    number: 47,
                    title: "Fixture publication".to_string(),
                    body: "Fixture body not emitted by activation errors.".to_string(),
                    state: "open".to_string(),
                    labels: Vec::new(),
                    milestone: None,
                    assignees: Vec::new(),
                    author: Some("fixture".to_string()),
                    created_at: "2026-01-01T00:00:00Z".to_string(),
                    updated_at: "2026-01-02T00:00:00Z".to_string(),
                    closed_at: None,
                    canonical_url: "https://github.com/owner/repo/issues/47".to_string(),
                    body_hash: "fixture-body-hash".to_string(),
                    indexed_at: "2026-01-02T00:00:01Z".to_string(),
                }],
                &[],
                0,
                &[],
            )
            .unwrap();
        store.mark_sync_run_completed(sync_run_id).unwrap();
    }

    #[cfg(feature = "vector-search")]
    fn vector_issue(
        node_id: &str,
        repo: &str,
        number: i64,
        state: &str,
        author: &str,
        labels: &[&str],
    ) -> IssueRecord {
        IssueRecord {
            source_id: format!("qgh://github.com/issue/{node_id}"),
            host: "github.com".to_string(),
            repo: repo.to_string(),
            node_id: node_id.to_string(),
            github_id: number,
            number,
            title: format!("Vector smoke {node_id}"),
            body: format!("Vector-only smoke body for {node_id}"),
            state: state.to_string(),
            labels: labels.iter().map(|label| label.to_string()).collect(),
            milestone: None,
            assignees: Vec::new(),
            author: Some(author.to_string()),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-02T00:00:00Z".to_string(),
            closed_at: None,
            canonical_url: format!("https://github.com/{repo}/issues/{number}"),
            body_hash: format!("body-hash-{node_id}"),
            indexed_at: "2026-01-02T00:00:01Z".to_string(),
        }
    }

    #[cfg(feature = "vector-search")]
    fn test_chunk(body: String) -> MarkdownChunk {
        MarkdownChunk {
            chunk_index: 0,
            byte_start: 0,
            byte_end: body.len(),
            token_start: 0,
            token_end: 1,
            token_count: 1,
            body,
            chunker_version: crate::chunking::CHUNKER_VERSION.to_string(),
            chunker_fingerprint: crate::chunking::CHUNKER_FINGERPRINT.to_string(),
            heading_path: Vec::new(),
        }
    }
}
