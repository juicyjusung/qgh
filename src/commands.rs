use crate::chunking::chunk_markdown_with_fingerprint;
#[cfg(any(feature = "fastembed-provider", all(test, feature = "vector-search")))]
use crate::chunking::chunker_fingerprint_for_tokenizer_identity;
use crate::cli::{
    EmbedArgs, InitArgs, InitRepoArgs, InitTokenSourceArg, ModelArgs, ModelCommand, QueryArgs,
    ReconcileMode,
};
use crate::config::{
    bootstrap_profile_repo, current_git_worktree_root, discover_repo_policy,
    git_remote_defaults_for_root, load_profile, load_profile_optional, parse_repo, resolve_token,
    resolve_token_with_mode, suggest_init_profile_id, CommentsMode, EmbeddingConfig,
    EmbeddingProviderKind, GitRemote, Profile, ProfileBootstrapInput, ProfileBootstrapTarget,
    RepoPolicy, RepoRef, TokenResolutionMode, TokenSource,
};
use crate::context::{prepare_embedding_input, EmbeddingSourceContext};
use crate::coverage;
#[cfg(any(debug_assertions, test))]
use crate::embedding::TokenSpan;
#[cfg(debug_assertions)]
use crate::embedding::DEFAULT_QUERY_PREFIX;
use crate::embedding::{
    builtin_preset_hf_reference, default_hf_model_reference, default_prepared_model_store,
    parse_hf_model_reference, EmbeddingFingerprint, EmbeddingFingerprintExpectation,
    EmbeddingFingerprintSeed, EmbeddingProvider, EmbeddingProviderError, EmbeddingTokenizer,
    EmbeddingVector, ModelManifestV1, ModelSourceV1, PoolingKind, PreparedManifestInspection,
    PreparedModelStore, LOCAL_MODEL_REVISION,
};
#[cfg(feature = "fastembed-provider")]
use crate::embedding::{
    tokenizer_contract_identity_from_manifest, validate_batch_comparability, FastembedEngine,
    FastembedTokenizer, LocalEmbeddingProvider, PreparedEmbeddingTokenizer,
    PreparedModelInspection, PreparedModelSnapshot,
};
use crate::error::QghError;
use crate::freshness::{self, FreshnessContext, FreshnessOverrides};
use crate::fusion::{self, LEXICAL_GUARD_V1};
use crate::github;
use crate::index;
use crate::lease::FileLease;
#[cfg(feature = "fastembed-provider")]
use crate::local_models::ModelSnapshotState;
use crate::local_models::{
    default_prepared_qwen_model_store, install_qwen_model, qwen_model_manifest_hash,
    qwen_model_spec, ModelInstallAction, QWEN_EMBEDDING_MODEL_ID, QWEN_EMBEDDING_PRESET_ID,
    QWEN_EMBEDDING_QUERY_PREFIX, QWEN_RERANKER_PRESET_ID,
};
use crate::model::{
    BackoffView, CommandAction, CoverageSnapshot, RateBudgetObservation, ReconciliationCandidate,
    StoredChunk, StoredComment, StoredIssue, StoredSource, SyncSummary, TargetedSyncSummary,
    VectorSearchFilters,
};
use crate::paths::ProfilePaths;
#[cfg(feature = "fastembed-provider")]
use crate::qwen::{
    load_qwen_embedding, load_qwen_embedding_tokenizer, load_qwen_reranker,
    qwen_embedding_runtime_profile_id, validate_qwen_embedding_device, QwenReranker,
    QWEN_RERANK_DEPTH, QWEN_RERANK_MAX_LENGTH,
};
use crate::rate_budget;
use crate::repo_policy_mutation::RepoPolicyMutationPlan;
use crate::resolution::ResolvedRepoScope;
use crate::store::{
    PendingPurgeView, PurgeTarget, PurgeTrigger, RetrievalBuildSnapshot, RetrievalPublicationView,
    Store, STORE_SCHEMA_VERSION,
};
use crate::terminal::TerminalUi;
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::io::{self, Write};
use std::path::PathBuf;
#[cfg(feature = "fastembed-provider")]
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

const GET_BATCH_SIZE_CAP: usize = 20;
const QWEN_EMBEDDING_INPUT_ADAPTER_REVISION: &str = "explicit-window-v2";
const QWEN_EMBEDDING_METAL_ADAPTER_REVISION: &str = "metal-sdpa-adaptive-batching-v2";
const STALE_BUILDING_RETENTION_HOURS: i64 = 24;
const PREVIOUS_READY_RETENTION_DAYS: i64 = 7;
const LOCAL_RERANK_DEPTH: usize = 10;
const LOCAL_RERANK_MAX_TOKENS: usize = 384;

/// Default `--if-stale` threshold when neither the flag nor `[sync].max_age`
/// provides one: 30 minutes.
pub(crate) const DEFAULT_SYNC_MAX_AGE_SECONDS: i64 = 30 * 60;

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

pub fn install_model(args: &ModelArgs) -> Result<LocalReadOutcome, QghError> {
    let ModelCommand::Install(args) = &args.command;
    let preset_id = args.model.as_str();
    let spec = qwen_model_spec(preset_id).expect("CLI model preset is registered");
    let outcome = install_qwen_model(preset_id, !args.json)?;
    let action = match outcome.action {
        ModelInstallAction::Installed => "installed",
        ModelInstallAction::AlreadyInstalled => "already_installed",
    };
    let warnings = if preset_id == QWEN_RERANKER_PRESET_ID {
        vec![reranker_warning(
            "reranker.experimental",
            "The local Qwen reranker is experimental and remains disabled unless each query explicitly requests reranking.",
        )]
    } else {
        Vec::new()
    };
    Ok(local_read_outcome(
        json!({
            "model": preset_id,
            "purpose": match spec.purpose {
                crate::local_models::ModelPurpose::Embedding => "embedding",
                crate::local_models::ModelPurpose::Reranker => "reranker",
            },
            "model_id": spec.model_id,
            "resolved_revision": spec.resolved_revision,
            "action": action,
            "artifact_count": spec.artifacts.len(),
            "verified_bytes": spec.artifacts.iter().map(|artifact| artifact.byte_size).sum::<u64>(),
            "manifest_hash": outcome.snapshot.manifest_hash,
            "weights_bundled": false
        }),
        warnings,
    ))
}

fn local_read_outcome(data: Value, warnings: Vec<Value>) -> LocalReadOutcome {
    LocalReadOutcome { data, warnings }
}

fn sync_scheduler_contract(profile: &Profile) -> Value {
    json!({
        "mode": "sequential",
        "max_in_flight_requests": 1,
        "hard_cap": 1,
        "configured_max_in_flight_requests": profile.max_in_flight_requests,
        "configuration_hard_cap": 16
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScheduleSyncDeferral {
    HostCooldown,
    RateBudgetReserve,
    UnknownBudgetLimit,
}

impl ScheduleSyncDeferral {
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::HostCooldown => "host_cooldown",
            Self::RateBudgetReserve => "rate_budget_reserve",
            Self::UnknownBudgetLimit => "unknown_budget_limit",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScheduledSyncCompletion {
    Synced,
    SkippedFresh,
}

pub(crate) enum ScheduledSyncResult {
    Completed(ScheduledSyncCompletion),
    Deferred {
        reason: ScheduleSyncDeferral,
        rate_budget: Vec<RateBudgetObservation>,
    },
}

pub(crate) struct ScheduledSyncExecution {
    pub(crate) result: Result<ScheduledSyncResult, QghError>,
    pub(crate) remote_started: bool,
    pub(crate) budget_uncertain: bool,
}

enum SyncBodyOutcome {
    Completed {
        outcome: LocalReadOutcome,
        scheduled_completion: ScheduledSyncCompletion,
    },
    ScheduledDeferred {
        reason: ScheduleSyncDeferral,
        rate_budget: Vec<RateBudgetObservation>,
    },
}

impl SyncBodyOutcome {
    fn synced(outcome: LocalReadOutcome) -> Self {
        Self::Completed {
            outcome,
            scheduled_completion: ScheduledSyncCompletion::Synced,
        }
    }

    fn skipped_fresh(outcome: LocalReadOutcome) -> Self {
        Self::Completed {
            outcome,
            scheduled_completion: ScheduledSyncCompletion::SkippedFresh,
        }
    }
}

enum SyncInvocation<'a> {
    Standard,
    Scheduled {
        host_profiles: &'a [Profile],
        host_attempts: usize,
        host_budget_unknown: bool,
        manager_invoked: bool,
    },
}

impl SyncInvocation<'_> {
    fn token_resolution_mode(&self) -> TokenResolutionMode {
        match self {
            Self::Scheduled {
                manager_invoked: true,
                ..
            } => TokenResolutionMode::ManagedSchedule,
            Self::Standard | Self::Scheduled { .. } => TokenResolutionMode::Standard,
        }
    }
}

fn validate_sync_request(
    reconcile: Option<ReconcileMode>,
    window: Option<&str>,
    if_stale: bool,
    max_age: Option<&str>,
    backfill: bool,
    max_requests: Option<usize>,
    max_duration: Option<&str>,
) -> Result<(), QghError> {
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
    if !if_stale && max_age.is_some() {
        return Err(QghError::validation(
            "validation.max_age_requires_if_stale",
            "--max-age requires --if-stale.",
        ));
    }
    Ok(())
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
    all_repos: bool,
    repo_scope: Option<&ResolvedRepoScope>,
    json_mode: bool,
    quiet: bool,
    show_progress: bool,
) -> Result<LocalReadOutcome, QghError> {
    validate_sync_request(
        reconcile,
        window,
        if_stale,
        max_age,
        backfill,
        max_requests,
        max_duration,
    )?;
    let profile = load_profile(profile_id)?;
    let mut remote_started = false;
    let mut budget_uncertain = false;
    match sync_profile(
        profile,
        reconcile,
        window,
        if_stale,
        max_age,
        backfill,
        max_requests,
        max_duration,
        all_repos,
        repo_scope,
        json_mode,
        quiet,
        show_progress,
        SyncInvocation::Standard,
        &mut remote_started,
        &mut budget_uncertain,
    )
    .await?
    {
        SyncBodyOutcome::Completed { outcome, .. } => Ok(outcome),
        SyncBodyOutcome::ScheduledDeferred { .. } => unreachable!("standard sync cannot defer"),
    }
}

pub(crate) async fn sync_scheduled(
    profile: Profile,
    host_profiles: &[Profile],
    host_attempts: usize,
    host_budget_unknown: bool,
    manager_invoked: bool,
) -> ScheduledSyncExecution {
    let mut remote_started = false;
    let mut budget_uncertain = false;
    let result = sync_profile(
        profile,
        None,
        None,
        true,
        None,
        false,
        None,
        None,
        true,
        None,
        true,
        false,
        false,
        SyncInvocation::Scheduled {
            host_profiles,
            host_attempts,
            host_budget_unknown,
            manager_invoked,
        },
        &mut remote_started,
        &mut budget_uncertain,
    )
    .await
    .map(|outcome| match outcome {
        SyncBodyOutcome::Completed {
            scheduled_completion,
            ..
        } => ScheduledSyncResult::Completed(scheduled_completion),
        SyncBodyOutcome::ScheduledDeferred {
            reason,
            rate_budget,
        } => ScheduledSyncResult::Deferred {
            reason,
            rate_budget,
        },
    });
    ScheduledSyncExecution {
        result,
        remote_started,
        budget_uncertain,
    }
}

#[allow(clippy::too_many_arguments)]
async fn sync_profile(
    profile: Profile,
    reconcile: Option<ReconcileMode>,
    window: Option<&str>,
    if_stale: bool,
    max_age: Option<&str>,
    backfill: bool,
    max_requests: Option<usize>,
    max_duration: Option<&str>,
    all_repos: bool,
    repo_scope: Option<&ResolvedRepoScope>,
    json_mode: bool,
    quiet: bool,
    show_progress: bool,
    invocation: SyncInvocation<'_>,
    remote_started: &mut bool,
    budget_uncertain: &mut bool,
) -> Result<SyncBodyOutcome, QghError> {
    let profile_id = profile.id.clone();
    let retry_command = sync_retry_command(
        &profile_id,
        reconcile,
        window,
        if_stale,
        max_age,
        backfill,
        max_requests,
        max_duration,
        all_repos,
        repo_scope.map(|scope| scope.repo.as_str()),
        json_mode,
        quiet,
    );
    let progress = StderrSyncProgress::new(show_progress);
    progress.line(format_args!(
        "qgh sync: loading profile profile={profile_id}"
    ));
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
    let full_profile_scope = fetch_profile.repos.len() == profile.repos.len();
    let _sync_lease = FileLease::acquire_profile_sync(&profile.id, &profile.paths)?;
    let mut store = Store::open(&profile.paths)?;
    run_sync_purge_preflight(&profile, &mut store)?;
    let scheduled_invocation = matches!(&invocation, SyncInvocation::Scheduled { .. });
    if scheduled_invocation {
        if let Some(outcome) =
            skipped_fresh_sync_outcome(&store, &profile, if_stale_max_age_seconds, &progress)?
        {
            return Ok(SyncBodyOutcome::skipped_fresh(outcome));
        }
    }
    if let SyncInvocation::Scheduled {
        host_profiles,
        host_attempts,
        host_budget_unknown,
        ..
    } = &invocation
    {
        if let Some((reason, rate_budget)) = revalidate_scheduled_host(
            &profile,
            &store,
            host_profiles,
            *host_attempts,
            *host_budget_unknown,
        )? {
            return Ok(SyncBodyOutcome::ScheduledDeferred {
                reason,
                rate_budget,
            });
        }
    }
    let request_budget = if let SyncInvocation::Scheduled { host_profiles, .. } = &invocation {
        let evidence = load_scheduled_rate_budget_observations(&profile, &store, host_profiles)?;
        Some(github::RequestBudgetGate::from_observations(
            &evidence.observations,
            evidence.every_profile_has_fresh_core,
        ))
    } else {
        None
    };
    let token = resolve_token_with_mode(&profile, invocation.token_resolution_mode())?;

    // `--if-stale`: skip the network sync entirely when the local snapshot is
    // still within max-age. Never-synced always proceeds.
    if !scheduled_invocation {
        if let Some(outcome) =
            skipped_fresh_sync_outcome(&store, &profile, if_stale_max_age_seconds, &progress)?
        {
            return Ok(SyncBodyOutcome::skipped_fresh(outcome));
        }
    }

    let cursors = store.sync_cursors()?;
    let per_issue_comments = profile.comments_mode == CommentsMode::PerIssue;

    if backfill {
        *remote_started = true;
        return backfill_sync(
            &profile,
            &fetch_profile,
            &token,
            &mut store,
            max_requests,
            max_duration,
            full_profile_scope,
            &retry_command,
            &progress,
        )
        .await
        .map(SyncBodyOutcome::synced);
    }

    progress.line(format_args!(
        "qgh sync: fetching GitHub issues/comments repos={}",
        fetch_profile.repos.len()
    ));
    let sync_run_id = Store::new_sync_run_id();
    let mut summary = None;
    if !scheduled_invocation {
        *remote_started = true;
    }
    let fetched = {
        let store_cell = std::cell::RefCell::new(&mut store);
        let lifecycle_pending = std::cell::Cell::new(false);
        let mut commit_page = |page: github::FetchPage| -> Result<(), QghError> {
            let mut store = store_cell.borrow_mut();
            let page_summary = if lifecycle_pending.get() {
                store.upsert_sources_for_run_under_pending_purge(
                    &sync_run_id,
                    &page.issues,
                    &page.comments,
                    page.skipped_pull_requests,
                    &page.cursor_updates,
                )?
            } else {
                store.upsert_sources_for_run(
                    &sync_run_id,
                    &page.issues,
                    &page.comments,
                    page.skipped_pull_requests,
                    &page.cursor_updates,
                )?
            };
            merge_sync_summary(&mut summary, page_summary);
            Ok(())
        };
        let mut commit_lifecycle = |evidence: &github::ConfirmedFetchLifecycle| {
            let mut store = store_cell.borrow_mut();
            queue_confirmed_fetch_lifecycle(&mut store, evidence)?;
            lifecycle_pending.set(true);
            Ok(())
        };
        github::fetch_issues_classified_with_lifecycle_commit(
            &fetch_profile,
            &token,
            &cursors,
            per_issue_comments,
            Some(&progress),
            request_budget.as_ref(),
            &mut commit_page,
            &mut commit_lifecycle,
        )
        .await
    };
    if let Some(request_budget) = request_budget.as_ref() {
        *remote_started = request_budget.started_any();
        *budget_uncertain = request_budget.has_unobserved_started_request();
    }
    persist_rate_budget_observations(&mut store, &progress)?;
    let fetched = fetched?;
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
                return sync_backoff_failure(
                    &mut store,
                    &profile.id,
                    &profile.host,
                    backoff,
                    &retry_command,
                )
                .map(SyncBodyOutcome::synced);
            }
            InterruptionDisposition::RequestBudget(reason) => {
                return Ok(SyncBodyOutcome::ScheduledDeferred {
                    reason,
                    rate_budget: store.rate_budget_observations(&profile.host)?,
                });
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
            let store_cell = std::cell::RefCell::new(&mut store);
            let resolve = |repo_name: &str, number: i64| -> Option<github::CommentParent> {
                store_cell
                    .borrow()
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
            let mut commit_lifecycle = |evidence: &github::ConfirmedFetchLifecycle| {
                queue_confirmed_fetch_lifecycle(&mut store_cell.borrow_mut(), evidence)
            };
            github::fetch_repo_comments_classified_with_lifecycle_commit(
                &fetch_profile,
                &token,
                &comment_cursors,
                budget,
                &resolve,
                Some(&progress),
                request_budget.as_ref(),
                &mut commit_lifecycle,
            )
            .await
        };
        if let Some(request_budget) = request_budget.as_ref() {
            *remote_started = request_budget.started_any();
            *budget_uncertain = request_budget.has_unobserved_started_request();
        }
        persist_rate_budget_observations(&mut store, &progress)?;
        let outcome = outcome?;
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
                InterruptionDisposition::RequestBudget(reason) => {
                    return Ok(SyncBodyOutcome::ScheduledDeferred {
                        reason,
                        rate_budget: store.rate_budget_observations(&profile.host)?,
                    });
                }
                InterruptionDisposition::Error(error) => return Err(error),
            }
        }
        if let Some(backoff) = backoff {
            repair_lexical_successor_if_required(&profile, &mut store)?;
            progress.line(format_args!(
                "qgh sync: comment backoff reason={} scope={} retry_after_seconds={}",
                backoff.reason, backoff.scope, backoff.retry_after_seconds
            ));
            return sync_backoff_failure(
                &mut store,
                &profile.id,
                &profile.host,
                backoff,
                &retry_command,
            )
            .map(SyncBodyOutcome::synced);
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

    // Seed corpus coverage metadata from a full-profile sync only. A scoped
    // single-repo sync is still full-profile when that is the profile's only
    // repository; a partial multi-repo sync must not claim corpus completion.
    if full_profile_scope {
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
        coverage.open_scope_fingerprint = Some(profile_coverage_scope_fingerprint(&profile));
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
            let result = {
                let mut commit_lifecycle = |evidence: &github::ConfirmedFetchLifecycle| {
                    queue_confirmed_fetch_lifecycle(&mut store, evidence)
                };
                github::reconcile_sources_with_lifecycle_commit(
                    &fetch_profile,
                    &token,
                    &candidates,
                    Some(&progress),
                    request_budget.as_ref(),
                    &mut commit_lifecycle,
                )
                .await
            };
            if let Some(request_budget) = request_budget.as_ref() {
                *remote_started = request_budget.started_any();
                *budget_uncertain = request_budget.has_unobserved_started_request();
            }
            persist_rate_budget_observations(&mut store, &progress)?;
            let result = result?;
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
                        return sync_backoff_failure(
                            &mut store,
                            &profile.id,
                            &profile.host,
                            backoff,
                            &retry_command,
                        )
                        .map(SyncBodyOutcome::synced)
                    }
                    InterruptionDisposition::RequestBudget(reason) => {
                        return Ok(SyncBodyOutcome::ScheduledDeferred {
                            reason,
                            rate_budget: store.rate_budget_observations(&profile.host)?,
                        });
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
    let coverage = coverage::evaluate(
        &profile_coverage_snapshot(&store, &profile)?,
        false,
        &profile.id,
    );
    let rate_budget = stored_rate_budget_block(&store, &profile.host)?;
    Ok(SyncBodyOutcome::synced(local_read_outcome(
        json!({
            "profile_id": profile.id,
            "sync_state": "ok",
            "sync_run_id": summary.sync_run_id,
            "rate_budget": rate_budget,
            "scheduler": sync_scheduler_contract(&profile),
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
            "reconciliation": reconciliation,
            "coverage": coverage.block
        }),
        index.warnings,
    )))
}

fn skipped_fresh_sync_outcome(
    store: &Store,
    profile: &Profile,
    max_age_seconds: Option<i64>,
    progress: &StderrSyncProgress,
) -> Result<Option<LocalReadOutcome>, QghError> {
    let Some(max_age_seconds) = max_age_seconds else {
        return Ok(None);
    };
    let last_sync = store.sync_planning_snapshot()?.last_sync_at;
    let Some(last_sync_at) = last_sync.as_deref() else {
        return Ok(None);
    };
    let snapshot_age_seconds = freshness::snapshot_age_seconds(last_sync_at)?;
    if snapshot_age_seconds > max_age_seconds {
        return Ok(None);
    }
    match store.resolve_active_tantivy_artifact() {
        Ok(_) => {
            let coverage = coverage::evaluate(
                &profile_coverage_snapshot(store, profile)?,
                false,
                &profile.id,
            );
            progress.line(format_args!(
                "qgh sync: skipped, snapshot fresh age={snapshot_age_seconds}s max_age={max_age_seconds}s"
            ));
            let rate_budget = stored_rate_budget_block(store, &profile.host)?;
            Ok(Some(local_read_outcome(
                json!({
                    "profile_id": profile.id,
                    "sync_state": "skipped_fresh",
                    "sync": {
                        "last_successful_sync": last_sync,
                        "snapshot_age_seconds": snapshot_age_seconds,
                        "max_age_seconds": max_age_seconds
                    },
                    "rate_budget": rate_budget,
                    "scheduler": sync_scheduler_contract(profile),
                    "coverage": coverage.block
                }),
                Vec::new(),
            )))
        }
        Err(error) if is_repairable_retrieval_publication_error(&error.code) => {
            progress.line(format_args!(
                "qgh sync: fresh remote snapshot requires retrieval publication repair"
            ));
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn revalidate_scheduled_host(
    current_profile: &Profile,
    current_store: &Store,
    host_profiles: &[Profile],
    host_attempts: usize,
    host_budget_unknown: bool,
) -> Result<Option<(ScheduleSyncDeferral, Vec<RateBudgetObservation>)>, QghError> {
    let mut observations = Vec::new();
    let mut core_budget = None;
    let mut complete = true;
    let mut active_backoff = false;
    for profile in host_profiles {
        if profile.id == current_profile.id {
            let planning = current_store.sync_planning_snapshot()?;
            active_backoff |= planning
                .backoff
                .as_ref()
                .is_some_and(sync_backoff_is_active);
            let profile_observations = current_store.rate_budget_observations(&profile.host)?;
            let profile_core = fresh_core_budget(&profile_observations);
            complete &= profile_core.is_some();
            if let Some(profile_core) = profile_core {
                core_budget = Some(match core_budget {
                    None => profile_core,
                    Some(current) => conservative_rate_observation(current, profile_core),
                });
            }
            observations.extend(profile_observations);
        } else if let Some(store) = Store::open_existing_for_read(&profile.paths)? {
            let planning = store.sync_planning_snapshot()?;
            active_backoff |= planning
                .backoff
                .as_ref()
                .is_some_and(sync_backoff_is_active);
            let profile_observations = store.rate_budget_observations(&profile.host)?;
            let profile_core = fresh_core_budget(&profile_observations);
            complete &= profile_core.is_some();
            if let Some(profile_core) = profile_core {
                core_budget = Some(match core_budget {
                    None => profile_core,
                    Some(current) => conservative_rate_observation(current, profile_core),
                });
            }
            observations.extend(profile_observations);
        } else {
            complete = false;
        }
    }

    if active_backoff {
        return Ok(Some((ScheduleSyncDeferral::HostCooldown, observations)));
    }
    if host_attempts >= 1 && (host_budget_unknown || !complete) {
        return Ok(Some((
            ScheduleSyncDeferral::UnknownBudgetLimit,
            observations,
        )));
    }
    if complete {
        let reserve_exhausted = core_budget.is_none_or(|observation| {
            rate_budget::scheduled_additional_requests(&observation).is_none_or(|value| value == 0)
        });
        if reserve_exhausted {
            return Ok(Some((
                ScheduleSyncDeferral::RateBudgetReserve,
                observations,
            )));
        }
    }
    Ok(None)
}

struct ScheduledRateBudgetEvidence {
    observations: Vec<RateBudgetObservation>,
    every_profile_has_fresh_core: bool,
}

fn load_scheduled_rate_budget_observations(
    current_profile: &Profile,
    current_store: &Store,
    host_profiles: &[Profile],
) -> Result<ScheduledRateBudgetEvidence, QghError> {
    let mut observations = Vec::new();
    let mut every_profile_has_fresh_core = true;
    for profile in host_profiles {
        let profile_observations = if profile.id == current_profile.id {
            current_store.rate_budget_observations(&profile.host)?
        } else if profile.paths.db_path.exists() {
            Store::open_for_read(&profile.paths)?.rate_budget_observations(&profile.host)?
        } else {
            Vec::new()
        };
        every_profile_has_fresh_core &= fresh_core_budget(&profile_observations).is_some();
        observations.extend(profile_observations);
    }
    Ok(ScheduledRateBudgetEvidence {
        observations,
        every_profile_has_fresh_core,
    })
}

fn fresh_core_budget(observations: &[RateBudgetObservation]) -> Option<RateBudgetObservation> {
    observations
        .iter()
        .filter(|observation| rate_budget::is_fresh_core(observation))
        .cloned()
        .reduce(conservative_rate_observation)
}

fn sync_backoff_is_active(backoff: &BackoffView) -> bool {
    let retry_at = backoff
        .reset_at
        .as_deref()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
        .or_else(|| {
            DateTime::parse_from_rfc3339(&backoff.observed_at)
                .ok()
                .map(|value| value.with_timezone(&Utc))
                .and_then(|observed_at| {
                    Duration::try_seconds(backoff.retry_after_seconds.max(0))
                        .and_then(|duration| observed_at.checked_add_signed(duration))
                })
        });
    retry_at.is_none_or(|value| value > Utc::now())
}

fn conservative_rate_observation(
    left: RateBudgetObservation,
    right: RateBudgetObservation,
) -> RateBudgetObservation {
    let left_allowance = rate_budget::scheduled_additional_requests(&left).unwrap_or(0);
    let right_allowance = rate_budget::scheduled_additional_requests(&right).unwrap_or(0);
    if left_allowance <= right_allowance {
        left
    } else {
        right
    }
}

#[allow(clippy::too_many_arguments)]
fn sync_retry_command(
    profile_id: &str,
    reconcile: Option<ReconcileMode>,
    window: Option<&str>,
    if_stale: bool,
    max_age: Option<&str>,
    backfill: bool,
    max_requests: Option<usize>,
    max_duration: Option<&str>,
    all_repos: bool,
    repo: Option<&str>,
    json_mode: bool,
    quiet: bool,
) -> String {
    let mut command = String::from("qgh sync");
    if all_repos || repo.is_none() {
        command.push_str(" --all");
    } else if let Some(repo) = repo {
        command.push_str(&format!(" --repo {repo}"));
    }
    if backfill {
        command.push_str(" --backfill");
        if let Some(max_requests) = max_requests {
            command.push_str(&format!(" --max-requests {max_requests}"));
        }
        if let Some(max_duration) = max_duration {
            command.push_str(&format!(" --max-duration {max_duration}"));
        }
    } else {
        if let Some(reconcile) = reconcile {
            let reconcile = match reconcile {
                ReconcileMode::Full => "full",
                ReconcileMode::Recent => "recent",
            };
            command.push_str(&format!(" --reconcile {reconcile}"));
        }
        if let Some(window) = window {
            command.push_str(&format!(" --window {window}"));
        }
        if if_stale {
            command.push_str(" --if-stale");
        }
        if let Some(max_age) = max_age {
            command.push_str(&format!(" --max-age {max_age}"));
        }
    }
    command.push_str(&format!(" --profile {profile_id}"));
    if quiet {
        command.push_str(" --quiet");
    }
    if json_mode {
        command.push_str(" --json");
    }
    command
}

fn sync_backoff_failure(
    store: &mut Store,
    profile_id: &str,
    profile_host: &str,
    backoff: github::BackoffPlan,
    retry_command: &str,
) -> Result<LocalReadOutcome, QghError> {
    let backoff = store.record_backoff_state(
        &backoff.reason,
        &backoff.scope,
        backoff.retry_after_seconds,
        backoff.reset_at.as_deref(),
        retry_command,
    )?;
    let local_query_available = matches!(store.successor_repair_required(), Ok(false))
        && matches!(store.resolve_active_tantivy_artifact(), Ok(Some(_)));
    let rate_budget = stored_rate_budget_block(store, profile_host)?;
    Err(sync_backoff_error(
        profile_id,
        &backoff,
        local_query_available,
        retry_command,
        rate_budget,
    ))
}

fn sync_backoff_error(
    profile_id: &str,
    backoff: &BackoffView,
    local_query_available: bool,
    retry_command: &str,
    rate_budget: Value,
) -> QghError {
    let retry_action = CommandAction::from_retry_command("sync_backoff", retry_command);
    let retry_at = backoff
        .reset_at
        .as_deref()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
        .or_else(|| {
            DateTime::parse_from_rfc3339(&backoff.observed_at)
                .ok()
                .map(|value| value.with_timezone(&Utc))
                .and_then(|observed_at| {
                    Duration::try_seconds(backoff.retry_after_seconds.max(0))
                        .and_then(|duration| observed_at.checked_add_signed(duration))
                })
        });
    let retry_instruction = if retry_at.is_some_and(|retry_at| retry_at <= Utc::now()) {
        format!("Retry now: {retry_command}.")
    } else if let Some(retry_at) = retry_at {
        format!("Retry {retry_command} after {}.", retry_at.to_rfc3339())
    } else {
        format!("Retry {retry_command} after GitHub permits requests again.")
    };
    let local_read_instruction = if local_query_available {
        "Existing local query, get, and status remain available."
    } else {
        "Local query is not currently ready; status remains available, and get may remain available for unfenced sources."
    };
    QghError::new(
        "sync.backoff",
        "GitHub sync paused because the remote host requested backoff.",
        5,
    )
    .with_details(json!({
        "profile_id": profile_id,
        "reason": backoff.reason,
        "scope": backoff.scope,
        "retry_after_seconds": backoff.retry_after_seconds,
        "reset_at": backoff.reset_at,
        "observed_at": backoff.observed_at,
        "last_successful_sync": backoff.last_successful_sync,
        "local_retrieval_available": local_query_available,
        "local_query_available": local_query_available,
        "local_status_available": true,
        "local_get_availability": "source_dependent",
        "retry_command": retry_command,
        "retry_action": retry_action,
        "retry_at": retry_at.map(|value| value.to_rfc3339()),
        "rate_budget": rate_budget
    }))
    .with_hint(format!("{retry_instruction} {local_read_instruction}"))
    .with_retryable(true)
}

#[allow(clippy::too_many_arguments)]
async fn backfill_sync(
    profile: &Profile,
    fetch_profile: &Profile,
    token: &str,
    store: &mut Store,
    max_requests: Option<usize>,
    max_duration: Option<&str>,
    full_profile_scope: bool,
    retry_command: &str,
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
        let store_cell = std::cell::RefCell::new(&mut *store);
        let lifecycle_pending = std::cell::Cell::new(false);
        let mut commit_page = |page: github::FetchPage| -> Result<(), QghError> {
            let mut store = store_cell.borrow_mut();
            let page_summary = if lifecycle_pending.get() {
                store.upsert_sources_for_run_under_pending_purge(
                    &backfill_run_id,
                    &page.issues,
                    &page.comments,
                    page.skipped_pull_requests,
                    &page.cursor_updates,
                )?
            } else {
                store.upsert_sources_for_run(
                    &backfill_run_id,
                    &page.issues,
                    &page.comments,
                    page.skipped_pull_requests,
                    &page.cursor_updates,
                )?
            };
            merge_sync_summary(&mut summary, page_summary);
            Ok(())
        };
        let mut commit_lifecycle = |evidence: &github::ConfirmedFetchLifecycle| {
            let mut store = store_cell.borrow_mut();
            queue_confirmed_fetch_lifecycle(&mut store, evidence)?;
            lifecycle_pending.set(true);
            Ok(())
        };
        github::fetch_backfill_issues_classified_with_lifecycle_commit(
            fetch_profile,
            token,
            &cursors,
            max_requests,
            max_duration_seconds,
            Some(progress),
            &mut commit_page,
            &mut commit_lifecycle,
        )
        .await
    };
    persist_rate_budget_observations(store, progress)?;
    let outcome = outcome?;
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
                InterruptionDisposition::RequestBudget(_) => {
                    return Err(github::github_unavailable());
                }
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
    if full_profile_scope && outcome.all_reached_end {
        coverage.historical_backfill_complete = true;
        coverage.historical_scope_fingerprint = Some(profile_coverage_scope_fingerprint(profile));
    }
    store.update_coverage(&coverage)?;

    if let Some(backoff) = outcome.backoff.or(interruption_backoff) {
        repair_lexical_successor_if_required(profile, store)?;
        progress.line(format_args!(
            "qgh sync: backfill backoff reason={} scope={} retry_after_seconds={}",
            backoff.reason, backoff.scope, backoff.retry_after_seconds
        ));
        return sync_backoff_failure(store, &profile.id, &profile.host, backoff, retry_command);
    }

    store.clear_backoff_state()?;
    if let Some(summary) = &summary {
        store.mark_sync_run_completed(&summary.sync_run_id)?;
    }
    let repair = repair_lexical_successor_if_required(profile, store)?;
    let index = rebuild_after_successor_repair(profile, store, progress, repair)?;
    let coverage = profile_coverage_snapshot(store, profile)?;
    let next_action = coverage::next_action(&coverage, &profile.id);
    let rate_budget = stored_rate_budget_block(store, &profile.host)?;
    Ok(local_read_outcome(
        json!({
            "profile_id": profile.id,
            "sync_state": "ok",
            "rate_budget": rate_budget,
            "scheduler": sync_scheduler_contract(profile),
            "backfill": {
                "issues": outcome.issues,
                "comments": outcome.comments,
                "skipped_pull_requests": outcome.skipped_pull_requests,
                "reached_end": outcome.all_reached_end,
                "history_cursor": coverage.history_cursor,
                "open_backfill_complete": coverage.open_backfill_complete,
                "historical_backfill_complete": coverage.historical_backfill_complete,
                "next_action": next_action
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

fn profile_coverage_scope_fingerprint(profile: &Profile) -> String {
    let repositories = profile
        .repos
        .iter()
        .map(RepoRef::full_name)
        .collect::<Vec<_>>();
    coverage::repository_scope_fingerprint(repositories.iter().map(String::as_str))
}

fn profile_coverage_snapshot(
    store: &Store,
    profile: &Profile,
) -> Result<CoverageSnapshot, QghError> {
    let mut snapshot = store.coverage_snapshot()?;
    let expected = profile_coverage_scope_fingerprint(profile);
    if snapshot.open_scope_fingerprint.as_deref() != Some(expected.as_str()) {
        snapshot.open_backfill_complete = false;
    }
    if snapshot.historical_scope_fingerprint.as_deref() != Some(expected.as_str()) {
        snapshot.historical_backfill_complete = false;
    }
    Ok(snapshot)
}

pub async fn sync_issue(
    profile_id: &str,
    issue_number: i64,
    repo_scope: Option<&ResolvedRepoScope>,
    json_mode: bool,
    quiet: bool,
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
    let mut retry_command = format!(
        "qgh sync issue {issue_number} --repo {} --profile {profile_id}",
        repo.full_name()
    );
    if quiet {
        retry_command.push_str(" --quiet");
    }
    if json_mode {
        retry_command.push_str(" --json");
    }
    let _sync_lease = FileLease::acquire_profile_sync(&profile.id, &profile.paths)?;
    let mut store = Store::open(&profile.paths)?;
    run_sync_purge_preflight(&profile, &mut store)?;
    let token = resolve_token(&profile)?;
    progress.line(format_args!(
        "qgh sync issue: fetching repo={} issue_number={issue_number}",
        repo.full_name()
    ));

    let outcome = {
        let mut commit_transition = |transition: &github::ConfirmedIssueTransition| {
            let evidence = target_transition_purge_requests(std::slice::from_ref(transition));
            if let Some(error) = evidence.deferred_error {
                return Err(error);
            }
            queue_purge_requests(&mut store, &evidence.requests).map(|_| ())
        };
        github::fetch_target_issue_classified_with_transition_commit(
            &profile,
            &token,
            &repo,
            issue_number,
            Some(&progress),
            &mut commit_transition,
        )
        .await
    };
    persist_rate_budget_observations(&mut store, &progress)?;
    let outcome = outcome?;
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
            sync_backoff_failure(
                &mut store,
                &profile.id,
                &profile.host,
                backoff,
                &retry_command,
            )
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
            Err(github::github_unavailable())
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
            let rate_budget = stored_rate_budget_block(&store, &profile.host)?;
            Ok(local_read_outcome(
                target_issue_sync_json(
                    &profile,
                    &repo,
                    issue_number,
                    &summary,
                    &fetched.lifecycle,
                    &index,
                    rate_budget,
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
            let rate_budget = stored_rate_budget_block(&store, &profile.host)?;
            Ok(local_read_outcome(
                target_issue_sync_json(
                    &profile,
                    &repo,
                    issue_number,
                    &summary,
                    &lifecycle,
                    &index,
                    rate_budget,
                ),
                index.warnings,
            ))
        }
    }
}

struct StderrSyncProgress {
    enabled: bool,
    operation: &'static str,
    rate_budget: RefCell<HashMap<String, RateBudgetObservation>>,
}

impl StderrSyncProgress {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            operation: "sync",
            rate_budget: RefCell::new(HashMap::new()),
        }
    }

    fn for_operation(enabled: bool, operation: &'static str) -> Self {
        Self {
            enabled,
            operation,
            rate_budget: RefCell::new(HashMap::new()),
        }
    }

    fn line(&self, args: fmt::Arguments<'_>) {
        if self.enabled {
            eprintln!("{}", TerminalUi::stderr().progress(&args.to_string()));
        }
    }

    fn phase(&self, args: fmt::Arguments<'_>) {
        self.line(format_args!("qgh {}: {args}", self.operation));
    }

    fn rate_budget_observations(&self) -> Vec<RateBudgetObservation> {
        let mut observations = self
            .rate_budget
            .borrow()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        observations.sort_by(|left, right| {
            (&left.host, &left.resource).cmp(&(&right.host, &right.resource))
        });
        observations
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
            github::ProgressEvent::RateBudgetObserved(observation) => {
                let mut rate_budget = self.rate_budget.borrow_mut();
                if observation.resource.is_none() {
                    rate_budget.retain(|_, prior| prior.host != observation.host);
                } else {
                    let unknown_key = format!("{}\0", observation.host);
                    rate_budget.remove(&unknown_key);
                }
                let key = format!(
                    "{}\0{}",
                    observation.host,
                    observation.resource.as_deref().unwrap_or("")
                );
                rate_budget.insert(key, observation);
            }
            github::ProgressEvent::ReconciliationProgress { checked, total } => {
                self.line(format_args!(
                    "qgh sync: reconciled checked_sources={checked}/{total}"
                ));
            }
        }
    }
}

fn persist_rate_budget_observations(
    store: &mut Store,
    progress: &StderrSyncProgress,
) -> Result<(), QghError> {
    store.record_rate_budget_observations(&progress.rate_budget_observations())
}

fn stored_rate_budget_block(store: &Store, host: &str) -> Result<Value, QghError> {
    Ok(rate_budget::block(&store.rate_budget_observations(host)?))
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
            progress,
        ) {
            Ok((embedded_chunks, generation_id)) => {
                progress.line(format_args!(
                    "qgh sync: refreshed chunk embeddings embedded={embedded_chunks}"
                ));
                generation_id
            }
            Err(error) => {
                warnings.push(embedding_refresh_failure_warning(&error));
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

fn queue_confirmed_fetch_lifecycle(
    store: &mut Store,
    evidence: &github::ConfirmedFetchLifecycle,
) -> Result<(), QghError> {
    let purge = match evidence {
        github::ConfirmedFetchLifecycle::RepositoryPermissionLoss(confirmed) => {
            confirmed_fetch_purge_requests(std::slice::from_ref(confirmed), &[])
        }
        github::ConfirmedFetchLifecycle::SourceDeletion(confirmed) => {
            confirmed_fetch_purge_requests(&[], std::slice::from_ref(confirmed))
        }
        github::ConfirmedFetchLifecycle::ReconciliationFailure(failure) => {
            reconciliation_purge_requests(std::slice::from_ref(failure), &[])
        }
    };
    if let Some(error) = purge.deferred_error {
        return Err(error);
    }
    queue_purge_requests(store, &purge.requests).map(|_| ())
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
    RequestBudget(ScheduleSyncDeferral),
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
            InterruptionDisposition::Error(github::github_unavailable())
        }
        github::LifecycleInterruption::Backoff(plan) => InterruptionDisposition::Backoff(plan),
        github::LifecycleInterruption::RequestBudget(reason) => {
            InterruptionDisposition::RequestBudget(match reason {
                github::RequestBudgetDeferral::RateBudgetReserve => {
                    ScheduleSyncDeferral::RateBudgetReserve
                }
                github::RequestBudgetDeferral::UnknownBudgetLimit => {
                    ScheduleSyncDeferral::UnknownBudgetLimit
                }
            })
        }
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

#[cfg(feature = "fastembed-provider")]
fn verified_embedding_chunk_nochange(
    store: &Store,
    expected_fingerprint: &str,
    progress: &StderrSyncProgress,
) -> Result<Option<ChunkRefreshStats>, QghError> {
    let Some(skipped_sources) =
        store.verified_active_source_chunk_manifest_count(expected_fingerprint)?
    else {
        return Ok(None);
    };
    progress.line(format_args!(
        "qgh sync: refreshed embedding chunks chunks=0 skipped_sources={skipped_sources}"
    ));
    Ok(Some(ChunkRefreshStats {
        refreshed_chunks: 0,
        skipped_sources,
    }))
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
    #[cfg(feature = "fastembed-provider")]
    if is_qwen_embedding_config(embedding) {
        let expected_fingerprint = configured_qwen_chunker_fingerprint();
        if let Ok(Some(_stats)) =
            verified_embedding_chunk_nochange(store, &expected_fingerprint, progress)
        {
            return (warnings, true);
        }
    }
    let tokenizer = match embedding_tokenizer(embedding) {
        Ok(tokenizer) => tokenizer,
        Err(error) => {
            warnings.push(embedding_tokenizer_failure_warning(embedding, &error));
            return (warnings, false);
        }
    };
    if refresh_embedding_chunks(
        store,
        tokenizer.tokenizer.as_ref(),
        &tokenizer.chunker_fingerprint,
        progress,
    )
    .is_err()
    {
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
    expected_fingerprint: &str,
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
        if store.source_version_chunks_match_fingerprint(source_version_id, expected_fingerprint)? {
            stats.skipped_sources += 1;
            continue;
        }
        let chunks = chunk_markdown_with_fingerprint(&source.body, tokenizer, expected_fingerprint)
            .map_err(|error| {
                QghError::storage(format!(
                    "Failed to chunk source `{}` with embedding tokenizer: {error}",
                    source.source_id
                ))
            })?;
        stats.refreshed_chunks += store
            .replace_chunks_for_source_version(&source.source_id, source_version_id, &chunks)?
            .len();
    }
    progress.phase(format_args!(
        "refreshed embedding chunks chunks={} skipped_sources={}",
        stats.refreshed_chunks, stats.skipped_sources
    ));
    Ok(stats)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EmbeddingGenerationContract {
    model_manifest_hash: String,
    fingerprint_seed: EmbeddingFingerprintSeed,
    chunker_fingerprint: String,
    output_dimension: usize,
}

impl EmbeddingGenerationContract {
    fn from_runtime(runtime: &EmbeddingRuntime) -> Self {
        Self {
            model_manifest_hash: runtime.model_manifest_hash.clone(),
            fingerprint_seed: runtime.fingerprint_seed.clone(),
            chunker_fingerprint: runtime.chunker_fingerprint.clone(),
            output_dimension: runtime.output_dimension,
        }
    }

    fn generation_spec(&self) -> crate::store::EmbeddingGenerationSpec {
        crate::store::EmbeddingGenerationSpec {
            model_manifest_hash: self.model_manifest_hash.clone(),
            runtime_fingerprint_hash: self
                .fingerprint_seed
                .clone()
                .with_dimension(self.output_dimension)
                .hash(),
            chunker_fingerprint: self.chunker_fingerprint.clone(),
            context_template_version: crate::context::METADATA_CONTEXT_TEMPLATE_VERSION.to_string(),
            output_dimension: self.output_dimension,
        }
    }
}

struct EmbeddingGenerationPlan {
    generation_id: i64,
    missing_chunk_ids: BTreeSet<i64>,
}

fn refresh_incremental_chunk_embeddings_for_snapshot(
    store: &mut Store,
    embedding: &EmbeddingConfig,
    snapshot: &RetrievalBuildSnapshot,
    progress: &StderrSyncProgress,
) -> Result<(usize, Option<i64>), QghError> {
    #[cfg(debug_assertions)]
    if let Some(runtime) = test_embedding_runtime(embedding)? {
        let contract = EmbeddingGenerationContract::from_runtime(&runtime);
        return refresh_incremental_chunk_embeddings_with_provider_and_contract(
            store,
            runtime.provider.as_ref(),
            &contract,
            snapshot,
            progress,
        );
    }

    #[cfg(feature = "fastembed-provider")]
    if is_qwen_embedding_config(embedding) {
        let contract = qwen_embedding_generation_contract_local_only(embedding)?;
        return refresh_incremental_chunk_embeddings_with_runtime_loader(
            store,
            &contract,
            snapshot,
            progress,
            || {
                embedding_runtime_local_only(
                    embedding,
                    None,
                    EmbeddingRuntimeValidation::BatchComparability,
                )
            },
        );
    }

    let runtime = embedding_runtime_local_only(
        embedding,
        None,
        EmbeddingRuntimeValidation::BatchComparability,
    )?;
    let contract = EmbeddingGenerationContract::from_runtime(&runtime);
    refresh_incremental_chunk_embeddings_with_provider_and_contract(
        store,
        runtime.provider.as_ref(),
        &contract,
        snapshot,
        progress,
    )
}

#[cfg(all(test, feature = "vector-search"))]
fn refresh_incremental_chunk_embeddings_with_provider(
    store: &mut Store,
    provider: &dyn EmbeddingProvider,
    model_manifest_hash: String,
    fingerprint_seed: EmbeddingFingerprintSeed,
    expected_chunker_fingerprint: String,
) -> Result<usize, QghError> {
    let Some(snapshot) = store.capture_retrieval_build_snapshot()? else {
        return Ok(0);
    };
    let contract = EmbeddingGenerationContract {
        model_manifest_hash,
        fingerprint_seed,
        chunker_fingerprint: expected_chunker_fingerprint,
        output_dimension: 3,
    };
    Ok(
        refresh_incremental_chunk_embeddings_with_provider_and_contract(
            store,
            provider,
            &contract,
            &snapshot,
            &StderrSyncProgress::new(false),
        )?
        .0,
    )
}

#[cfg(any(feature = "fastembed-provider", all(test, feature = "vector-search")))]
fn refresh_incremental_chunk_embeddings_with_runtime_loader(
    store: &mut Store,
    contract: &EmbeddingGenerationContract,
    snapshot: &RetrievalBuildSnapshot,
    progress: &StderrSyncProgress,
    load_runtime: impl FnOnce() -> Result<std::sync::Arc<EmbeddingRuntime>, QghError>,
) -> Result<(usize, Option<i64>), QghError> {
    let Some(plan) = plan_embedding_generation(store, contract, snapshot, progress)? else {
        return Ok((0, None));
    };
    if plan.missing_chunk_ids.is_empty() {
        store.validate_embedding_generation(plan.generation_id)?;
        return Ok((0, Some(plan.generation_id)));
    }
    let runtime = load_runtime()?;
    if EmbeddingGenerationContract::from_runtime(&runtime) != *contract {
        return Err(QghError::validation(
            "embedding.generation_runtime_mismatch",
            "Loaded embedding runtime does not match the planned generation contract.",
        ));
    }
    complete_embedding_generation(
        store,
        runtime.provider.as_ref(),
        contract,
        snapshot,
        plan,
        progress,
    )
}

fn refresh_incremental_chunk_embeddings_with_provider_and_contract(
    store: &mut Store,
    provider: &dyn EmbeddingProvider,
    contract: &EmbeddingGenerationContract,
    snapshot: &RetrievalBuildSnapshot,
    progress: &StderrSyncProgress,
) -> Result<(usize, Option<i64>), QghError> {
    let Some(plan) = plan_embedding_generation(store, contract, snapshot, progress)? else {
        return Ok((0, None));
    };
    complete_embedding_generation(store, provider, contract, snapshot, plan, progress)
}

fn plan_embedding_generation(
    store: &mut Store,
    contract: &EmbeddingGenerationContract,
    snapshot: &RetrievalBuildSnapshot,
    progress: &StderrSyncProgress,
) -> Result<Option<EmbeddingGenerationPlan>, QghError> {
    let chunks = snapshot.embedding_chunks();
    if chunks.is_empty() {
        return Ok(None);
    }
    if chunks
        .iter()
        .any(|chunk| chunk.chunk.chunker_fingerprint != contract.chunker_fingerprint)
    {
        return Err(QghError::validation(
            "embedding.generation_invalid_spec",
            "Stored chunks do not match the prepared tokenizer contract.",
        ));
    }

    let spec = contract.generation_spec();
    let build = store.start_or_resume_embedding_generation(snapshot, &spec)?;
    let missing_chunk_ids = build
        .missing_chunk_ids
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let missing_chunks = chunks
        .iter()
        .filter(|chunk| missing_chunk_ids.contains(&chunk.chunk.chunk_id))
        .collect::<Vec<_>>();
    if missing_chunks.len() != missing_chunk_ids.len() {
        return Err(QghError::validation(
            "embedding.generation_inventory_mismatch",
            "Embedding generation work does not match the captured chunk inventory.",
        ));
    }
    progress.line(format_args!(
        "qgh sync: embedding plan total={} staged={} reused={} missing={}",
        chunks.len(),
        build.already_staged,
        build.reused,
        missing_chunks.len()
    ));
    Ok(Some(EmbeddingGenerationPlan {
        generation_id: build.generation_id,
        missing_chunk_ids,
    }))
}

fn complete_embedding_generation(
    store: &mut Store,
    provider: &dyn EmbeddingProvider,
    contract: &EmbeddingGenerationContract,
    snapshot: &RetrievalBuildSnapshot,
    plan: EmbeddingGenerationPlan,
    progress: &StderrSyncProgress,
) -> Result<(usize, Option<i64>), QghError> {
    let missing_chunks = snapshot
        .embedding_chunks()
        .iter()
        .filter(|chunk| plan.missing_chunk_ids.contains(&chunk.chunk.chunk_id))
        .collect::<Vec<_>>();
    let started = Instant::now();
    let mut embedded = 0usize;
    for batch in missing_chunks.chunks(32) {
        let texts = batch
            .iter()
            .map(|chunk| chunk.prepared_input.as_str())
            .collect::<Vec<_>>();
        let vectors = provider.embed_documents(&texts).map_err(embedding_error)?;
        if vectors.len() != batch.len() {
            return Err(QghError::validation(
                "embedding.vector_count_mismatch",
                "Embedding provider returned a different number of vectors than input chunks.",
            )
            .with_details(json!({
                "chunk_count": batch.len(),
                "vector_count": vectors.len()
            })));
        }
        if embedding_dimension(&vectors)? != contract.output_dimension {
            return Err(QghError::validation(
                "embedding.vector_dimension_mismatch",
                "Embedding provider output does not match the configured generation dimension.",
            )
            .with_details(json!({
                "expected_dimension": contract.output_dimension,
                "actual_dimension": vectors.first().map(Vec::len)
            })));
        }
        let staged = batch
            .iter()
            .zip(vectors)
            .map(|(chunk, vector)| {
                Ok(crate::store::EmbeddingGenerationChunk {
                    chunk_id: chunk.chunk.chunk_id,
                    source_version_id: chunk.chunk.source_version_id,
                    source_version_hash: store
                        .source_version_hash(chunk.chunk.source_version_id)?
                        .ok_or_else(|| QghError::storage("Missing source version hash."))?,
                    context_hash: chunk
                        .prepared_input
                        .context_hash(&contract.model_manifest_hash, &contract.chunker_fingerprint),
                    vector,
                })
            })
            .collect::<Result<Vec<_>, QghError>>()?;
        let staged_count = store.stage_embedding_generation_batch(plan.generation_id, &staged)?;
        embedded = embedded.saturating_add(staged_count);
        let elapsed_seconds = started.elapsed().as_secs_f64().max(f64::EPSILON);
        let chunks_per_second = embedded as f64 / elapsed_seconds;
        let remaining = missing_chunks.len().saturating_sub(embedded);
        let eta_seconds = if chunks_per_second > 0.0 {
            (remaining as f64 / chunks_per_second).ceil() as u64
        } else {
            0
        };
        progress.line(format_args!(
            "qgh sync: embedded chunks={embedded}/{} elapsed_seconds={:.1} chunks_per_second={:.2} eta_seconds={eta_seconds}",
            missing_chunks.len(),
            elapsed_seconds,
            chunks_per_second,
        ));
    }
    store.validate_embedding_generation(plan.generation_id)?;
    Ok((embedded, Some(plan.generation_id)))
}

fn embedding_sync_warning(code: &str, message: &'static str) -> Value {
    json!({
        "code": code,
        "severity": "warn",
        "message": message
    })
}

fn embedding_refresh_failure_warning(error: &QghError) -> Value {
    let mut warning = embedding_sync_warning(
        &error.code,
        "Embedding refresh failed during sync. BM25 index refresh remains available.",
    );
    if matches!(
        error.code.as_str(),
        "embedding.model_not_installed" | "embedding.qwen_snapshot_invalid"
    ) {
        let reason = if error.code == "embedding.model_not_installed" {
            "embedding_model_not_installed"
        } else {
            "embedding_model_invalid"
        };
        warning["action"] = json!(CommandAction::new(
            reason,
            "qgh model install qwen3-embedding-0.6b",
        ));
    }
    warning
}

fn embedding_tokenizer_failure_warning(embedding: &EmbeddingConfig, error: &QghError) -> Value {
    if is_qwen_embedding_config(embedding) {
        let code = match error.code.as_str() {
            "model.not_installed" => Some("embedding.model_not_installed"),
            "model.snapshot_invalid" | "model.artifact_invalid" => {
                Some("embedding.qwen_snapshot_invalid")
            }
            _ => None,
        };
        if let Some(code) = code {
            return embedding_refresh_failure_warning(&QghError::validation(
                code,
                "The configured Qwen embedding snapshot is not ready.",
            ));
        }
    }
    embedding_sync_warning(
        "embedding.sync_tokenizer_failed",
        "Prepared embedding model acquisition or tokenizer initialization failed during sync. BM25 index refresh remains available.",
    )
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

struct EmbeddingChunkingRuntime {
    tokenizer: Box<dyn EmbeddingTokenizer>,
    chunker_fingerprint: String,
}

#[cfg(feature = "fastembed-provider")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmbeddingTokenizerRoute {
    PreparedQwenSnapshot,
    PreparedFastembedSnapshot,
}

#[cfg(feature = "fastembed-provider")]
fn embedding_tokenizer_route(embedding: &EmbeddingConfig) -> EmbeddingTokenizerRoute {
    if is_qwen_embedding_config(embedding) {
        EmbeddingTokenizerRoute::PreparedQwenSnapshot
    } else {
        EmbeddingTokenizerRoute::PreparedFastembedSnapshot
    }
}

#[cfg(feature = "fastembed-provider")]
fn embedding_tokenizer(embedding: &EmbeddingConfig) -> Result<EmbeddingChunkingRuntime, QghError> {
    match (embedding.provider, embedding_tokenizer_route(embedding)) {
        (EmbeddingProviderKind::Local, EmbeddingTokenizerRoute::PreparedQwenSnapshot) => {
            let spec = qwen_model_spec(QWEN_EMBEDDING_PRESET_ID)
                .expect("Qwen embedding preset is registered");
            let snapshot = default_prepared_qwen_model_store()?.inspect(&spec)?;
            qwen_embedding_tokenizer_from_snapshot(&snapshot)
        }
        (EmbeddingProviderKind::Local, EmbeddingTokenizerRoute::PreparedFastembedSnapshot) => {
            let options = embedding.fastembed_options();
            let prepared_store = default_prepared_model_store().map_err(embedding_error)?;
            let tokenizer: PreparedEmbeddingTokenizer = prepared_store
                .acquire_tokenizer_with_identity(&options)
                .map_err(embedding_error)?;
            let chunker_fingerprint =
                chunker_fingerprint_for_tokenizer_identity(tokenizer.contract_identity());
            Ok(EmbeddingChunkingRuntime {
                tokenizer: Box::new(tokenizer),
                chunker_fingerprint,
            })
        }
    }
}

#[cfg(feature = "fastembed-provider")]
fn qwen_embedding_tokenizer_from_snapshot(
    snapshot: &crate::local_models::PreparedQwenModelSnapshot,
) -> Result<EmbeddingChunkingRuntime, QghError> {
    let tokenizer = load_qwen_embedding_tokenizer(snapshot).map_err(embedding_error)?;
    Ok(EmbeddingChunkingRuntime {
        tokenizer: Box::new(tokenizer),
        chunker_fingerprint: qwen_embedding_chunker_fingerprint(snapshot),
    })
}

#[cfg(feature = "fastembed-provider")]
fn qwen_embedding_chunker_fingerprint(
    snapshot: &crate::local_models::PreparedQwenModelSnapshot,
) -> String {
    qwen_embedding_chunker_fingerprint_for_manifest(&snapshot.manifest_hash)
}

#[cfg(feature = "fastembed-provider")]
fn configured_qwen_chunker_fingerprint() -> String {
    let spec =
        qwen_model_spec(QWEN_EMBEDDING_PRESET_ID).expect("Qwen embedding preset is registered");
    qwen_embedding_chunker_fingerprint_for_manifest(&qwen_model_manifest_hash(&spec))
}

#[cfg(feature = "fastembed-provider")]
fn qwen_embedding_chunker_fingerprint_for_manifest(manifest_hash: &str) -> String {
    chunker_fingerprint_for_tokenizer_identity(&format!("qgh.qwen_tokenizer.v1:{manifest_hash}"))
}

#[cfg(not(feature = "fastembed-provider"))]
fn embedding_tokenizer(embedding: &EmbeddingConfig) -> Result<EmbeddingChunkingRuntime, QghError> {
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
    index: &IndexRebuildOutcome,
    rate_budget: Value,
) -> Value {
    json!({
        "profile_id": &profile.id,
        "sync_state": "ok",
        "sync_run_id": &summary.sync_run_id,
        "rate_budget": rate_budget,
        "scheduler": sync_scheduler_contract(profile),
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
            "active_generation": index.generation,
            "dirty_task_count": index.dirty_task_count
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

    let progress = StderrSyncProgress::for_operation(!args.json && !args.quiet, "embed");
    progress.phase(format_args!("loading configured local model"));
    let mut store = Store::open(&profile.paths)?;
    store.enable_vector()?;
    let runtime = embedding_runtime_for_acquisition(embedding)?;
    let chunk_stats = refresh_embedding_chunks(
        &mut store,
        runtime.tokenizer.as_ref(),
        &runtime.chunker_fingerprint,
        &progress,
    )?;
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
    let data = refresh_chunk_embeddings_with_progress(
        &mut store,
        &profile.paths,
        runtime.provider.as_ref(),
        runtime.model_manifest_hash.clone(),
        runtime.fingerprint_seed.clone(),
        &runtime.chunker_fingerprint,
        &snapshot,
        &progress,
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
    chunker_fingerprint: String,
    provider: Box<dyn EmbeddingProvider>,
    model_manifest_hash: String,
    fingerprint_seed: EmbeddingFingerprintSeed,
    output_dimension: usize,
}

#[cfg(feature = "fastembed-provider")]
static EMBEDDING_RUNTIME_CACHE: OnceLock<Mutex<HashMap<String, Arc<EmbeddingRuntime>>>> =
    OnceLock::new();
#[cfg(feature = "fastembed-provider")]
static RERANKER_RUNTIME_CACHE: OnceLock<Mutex<HashMap<String, Arc<QwenReranker>>>> =
    OnceLock::new();

#[cfg(debug_assertions)]
const TEST_EMBEDDING_QUERY_VECTORS_ENV: &str = "QGH_TEST_EMBEDDING_QUERY_VECTORS";
#[cfg(debug_assertions)]
const TEST_EMBEDDING_DOCUMENT_VECTORS_ENV: &str = "QGH_TEST_EMBEDDING_DOCUMENT_VECTORS";
#[cfg(debug_assertions)]
const TEST_RERANK_SCORES_ENV: &str = "QGH_TEST_RERANK_SCORES";

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

#[cfg(any(debug_assertions, test))]
struct TestEmbeddingTokenizer;

#[cfg(any(debug_assertions, test))]
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

#[cfg(all(test, feature = "vector-search"))]
struct ByteEmbeddingTokenizer;

#[cfg(all(test, feature = "vector-search"))]
impl EmbeddingTokenizer for ByteEmbeddingTokenizer {
    fn tokenize(&self, text: &str) -> Result<Vec<TokenSpan>, EmbeddingProviderError> {
        Ok(text
            .char_indices()
            .map(|(start, character)| TokenSpan {
                start,
                end: start + character.len_utf8(),
            })
            .collect())
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
    let configured = configured_embedding_contract_snapshot(embedding);
    Ok(Some(EmbeddingRuntime {
        tokenizer: Box::new(TestEmbeddingTokenizer),
        // Explicit debug-provider fixture identity. Production prepared-model
        // runtimes always derive this from their tokenizer contract.
        chunker_fingerprint: crate::chunking::CHUNKER_FINGERPRINT.to_string(),
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
            pooling: configured.pooling.unwrap_or(PoolingKind::Cls),
            query_prefix: configured
                .query_prefix
                .unwrap_or_else(|| DEFAULT_QUERY_PREFIX.to_string()),
        },
        output_dimension: dimension,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmbeddingRuntimeValidation {
    BatchComparability,
    QueryOnly,
}

#[cfg(feature = "fastembed-provider")]
fn validate_embedding_runtime_if_required(
    provider: &dyn EmbeddingProvider,
    validation: EmbeddingRuntimeValidation,
    smoke_text: &str,
) -> Result<(), QghError> {
    if validation == EmbeddingRuntimeValidation::BatchComparability {
        validate_batch_comparability(provider, smoke_text).map_err(embedding_error)?;
    }
    Ok(())
}

#[cfg(feature = "fastembed-provider")]
fn embedding_runtime_for_acquisition(
    embedding: &EmbeddingConfig,
) -> Result<Arc<EmbeddingRuntime>, QghError> {
    embedding_runtime_with_access(
        embedding,
        PreparedModelAccess::Acquire,
        None,
        EmbeddingRuntimeValidation::BatchComparability,
    )
}

#[cfg(feature = "fastembed-provider")]
fn embedding_runtime_local_only(
    embedding: &EmbeddingConfig,
    cache_profile_id: Option<&str>,
    validation: EmbeddingRuntimeValidation,
) -> Result<Arc<EmbeddingRuntime>, QghError> {
    embedding_runtime_with_access(
        embedding,
        PreparedModelAccess::LoadLocal,
        cache_profile_id,
        validation,
    )
}

#[cfg(feature = "fastembed-provider")]
fn embedding_runtime_with_access(
    embedding: &EmbeddingConfig,
    access: PreparedModelAccess,
    cache_profile_id: Option<&str>,
    validation: EmbeddingRuntimeValidation,
) -> Result<Arc<EmbeddingRuntime>, QghError> {
    if let Some(runtime) = test_embedding_runtime(embedding)? {
        return Ok(Arc::new(runtime));
    }
    if is_qwen_embedding_config(embedding) {
        return qwen_embedding_runtime_local_only(embedding, cache_profile_id, validation);
    }
    match embedding.provider {
        EmbeddingProviderKind::Local => {
            let options = embedding.fastembed_options();
            let prepared_store = default_prepared_model_store().map_err(embedding_error)?;
            match access {
                PreparedModelAccess::Acquire => {
                    let snapshot = prepared_store.acquire(&options).map_err(embedding_error)?;
                    build_embedding_runtime(embedding, &snapshot, validation)
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
                                build_embedding_runtime(embedding, &snapshot, validation)
                            },
                        );
                    }
                    let snapshot = prepared_store.verify(inspection).map_err(embedding_error)?;
                    build_embedding_runtime(embedding, &snapshot, validation)
                }
            }
        }
    }
}

#[cfg(feature = "fastembed-provider")]
fn qwen_embedding_runtime_local_only(
    embedding: &EmbeddingConfig,
    cache_profile_id: Option<&str>,
    validation: EmbeddingRuntimeValidation,
) -> Result<Arc<EmbeddingRuntime>, QghError> {
    let snapshot = installed_qwen_embedding_snapshot()?;
    let runtime_profile = qwen_embedding_runtime_profile_id(embedding.device);
    if let Some(profile_id) = cache_profile_id {
        let cache_key =
            qwen_embedding_runtime_cache_key(profile_id, &snapshot.manifest_hash, runtime_profile);
        let profile_prefix = format!("{profile_id}:");
        return runtime_cache_get_or_try_init(
            EMBEDDING_RUNTIME_CACHE.get_or_init(|| Mutex::new(HashMap::new())),
            &cache_key,
            &profile_prefix,
            || build_qwen_embedding_runtime(embedding, &snapshot, validation),
        );
    }
    build_qwen_embedding_runtime(embedding, &snapshot, validation)
}

#[cfg(feature = "fastembed-provider")]
fn installed_qwen_embedding_snapshot(
) -> Result<crate::local_models::PreparedQwenModelSnapshot, QghError> {
    let spec =
        qwen_model_spec(QWEN_EMBEDDING_PRESET_ID).expect("Qwen embedding preset is registered");
    default_prepared_qwen_model_store()
        .and_then(|store| store.inspect(&spec))
        .map_err(qwen_embedding_snapshot_error)
}

#[cfg(feature = "fastembed-provider")]
fn qwen_embedding_snapshot_error(error: QghError) -> QghError {
    let (code, message, reason) = if error.code == "model.not_installed" {
        (
            "embedding.model_not_installed",
            "The configured local Qwen embedding model is not installed.",
            "embedding_model_not_installed",
        )
    } else {
        (
            "embedding.qwen_snapshot_invalid",
            "The prepared Qwen embedding snapshot failed integrity validation.",
            "embedding_model_invalid",
        )
    };
    let repair_action = CommandAction::new(reason, "qgh model install qwen3-embedding-0.6b");
    QghError::validation(code, message)
        .with_details(json!({ "repair_action": repair_action }))
        .with_hint("Run `qgh model install qwen3-embedding-0.6b` to repair the pinned snapshot.")
}

#[cfg(feature = "fastembed-provider")]
fn qwen_embedding_generation_contract_local_only(
    embedding: &EmbeddingConfig,
) -> Result<EmbeddingGenerationContract, QghError> {
    let spec =
        qwen_model_spec(QWEN_EMBEDDING_PRESET_ID).expect("Qwen embedding preset is registered");
    let manifest_hash = qwen_model_manifest_hash(&spec);
    validate_qwen_embedding_device(embedding.device).map_err(embedding_error)?;
    let runtime_profile = qwen_embedding_runtime_profile_id(embedding.device);
    Ok(EmbeddingGenerationContract {
        model_manifest_hash: manifest_hash.clone(),
        fingerprint_seed: qwen_embedding_fingerprint_seed(
            embedding,
            &manifest_hash,
            runtime_profile,
        ),
        chunker_fingerprint: qwen_embedding_chunker_fingerprint_for_manifest(&manifest_hash),
        output_dimension: crate::qwen::QWEN_EMBEDDING_OUTPUT_DIMENSION,
    })
}

#[cfg(feature = "fastembed-provider")]
fn qwen_embedding_fingerprint_seed(
    embedding: &EmbeddingConfig,
    manifest_hash: &str,
    runtime_profile: &str,
) -> EmbeddingFingerprintSeed {
    EmbeddingFingerprintSeed {
        provider: embedding_provider_name(embedding.provider).to_string(),
        model_id: QWEN_EMBEDDING_MODEL_ID.to_string(),
        model_revision: configured_qwen_runtime_identity(manifest_hash, runtime_profile),
        pooling: PoolingKind::LastToken,
        query_prefix: QWEN_EMBEDDING_QUERY_PREFIX.to_string(),
    }
}

#[cfg(feature = "fastembed-provider")]
fn build_qwen_embedding_runtime(
    embedding: &EmbeddingConfig,
    snapshot: &crate::local_models::PreparedQwenModelSnapshot,
    validation: EmbeddingRuntimeValidation,
) -> Result<Arc<EmbeddingRuntime>, QghError> {
    let parts = load_qwen_embedding(snapshot, embedding.device).map_err(embedding_error)?;
    validate_embedding_runtime_if_required(
        &parts.provider,
        validation,
        "qgh prepared Qwen model smoke",
    )?;
    let runtime_profile = parts.runtime_profile.as_str();
    let chunker_fingerprint = qwen_embedding_chunker_fingerprint(snapshot);
    Ok(Arc::new(EmbeddingRuntime {
        tokenizer: Box::new(parts.tokenizer),
        chunker_fingerprint,
        provider: Box::new(parts.provider),
        model_manifest_hash: snapshot.manifest_hash.clone(),
        fingerprint_seed: qwen_embedding_fingerprint_seed(
            embedding,
            &snapshot.manifest_hash,
            runtime_profile,
        ),
        output_dimension: crate::qwen::QWEN_EMBEDDING_OUTPUT_DIMENSION,
    }))
}

#[cfg(feature = "fastembed-provider")]
fn qwen_embedding_runtime_cache_key(
    profile_id: &str,
    manifest_hash: &str,
    runtime_profile: &str,
) -> String {
    format!(
        "{profile_id}:qwen:{}",
        configured_qwen_runtime_identity(manifest_hash, runtime_profile)
    )
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
    validation: EmbeddingRuntimeValidation,
) -> Result<Arc<EmbeddingRuntime>, QghError> {
    let tokenizer_identity =
        tokenizer_contract_identity_from_manifest(&snapshot.manifest).map_err(embedding_error)?;
    let chunker_fingerprint = chunker_fingerprint_for_tokenizer_identity(&tokenizer_identity);
    let tokenizer = FastembedTokenizer::from_prepared_snapshot(snapshot)
        .map(|tokenizer| Box::new(tokenizer) as Box<dyn EmbeddingTokenizer>)
        .map_err(embedding_error)?;
    let engine = FastembedEngine::from_prepared_snapshot(snapshot).map_err(embedding_error)?;
    let provider =
        LocalEmbeddingProvider::with_contract(engine, snapshot.manifest.runtime_contract())
            .map_err(embedding_error)?;
    validate_embedding_runtime_if_required(&provider, validation, "qgh prepared model smoke")?;
    Ok(Arc::new(EmbeddingRuntime {
        tokenizer,
        chunker_fingerprint,
        provider: Box::new(provider),
        model_manifest_hash: snapshot.manifest.hash(),
        fingerprint_seed: embedding_fingerprint_seed(embedding, snapshot),
        output_dimension: snapshot.manifest.output_dimension,
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
    _validation: EmbeddingRuntimeValidation,
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

fn prepared_manifest_model_id(manifest: &ModelManifestV1) -> String {
    match &manifest.model_source {
        ModelSourceV1::Hf { model_id, .. } => model_id.clone(),
        ModelSourceV1::Local { declared_id } => format!("local:{declared_id}"),
    }
}

#[cfg(all(test, feature = "vector-search"))]
fn refresh_chunk_embeddings(
    store: &mut Store,
    paths: &ProfilePaths,
    provider: &dyn EmbeddingProvider,
    model_manifest_hash: String,
    fingerprint_seed: EmbeddingFingerprintSeed,
    expected_chunker_fingerprint: &str,
    snapshot: &RetrievalBuildSnapshot,
) -> Result<Value, QghError> {
    refresh_chunk_embeddings_with_progress(
        store,
        paths,
        provider,
        model_manifest_hash,
        fingerprint_seed,
        expected_chunker_fingerprint,
        snapshot,
        &StderrSyncProgress::new(false),
    )
}

#[allow(clippy::too_many_arguments)]
fn refresh_chunk_embeddings_with_progress(
    store: &mut Store,
    paths: &ProfilePaths,
    provider: &dyn EmbeddingProvider,
    model_manifest_hash: String,
    fingerprint_seed: EmbeddingFingerprintSeed,
    expected_chunker_fingerprint: &str,
    snapshot: &RetrievalBuildSnapshot,
    progress: &StderrSyncProgress,
) -> Result<Value, QghError> {
    let chunks = snapshot.embedding_chunks();
    if chunks.is_empty() {
        return Err(QghError::validation(
            "embedding.no_chunks",
            "No active chunks are available to embed.",
        )
        .with_hint("Run `qgh sync` with [embedding] configured before `qgh embed --force`."));
    }
    if chunks
        .iter()
        .any(|chunk| chunk.chunk.chunker_fingerprint != expected_chunker_fingerprint)
    {
        return Err(QghError::validation(
            "embedding.generation_invalid_spec",
            "Stored chunks do not match the prepared tokenizer contract.",
        ));
    }

    progress.phase(format_args!("generating vectors total={}", chunks.len()));
    let started = Instant::now();
    let texts = chunks
        .iter()
        .map(|chunk| chunk.prepared_input.as_str())
        .collect::<Vec<_>>();
    // Keep the full corpus in one provider call so Qwen can globally bucket
    // token lengths and choose its adaptive Metal batches. The provider owns
    // inference batching; qgh only batches durable SQLite staging below.
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
    let embeddings = chunks.iter().zip(vectors).collect::<Vec<_>>();
    let elapsed_seconds = started.elapsed().as_secs_f64().max(f64::EPSILON);
    let chunks_per_second = embeddings.len() as f64 / elapsed_seconds;
    progress.phase(format_args!(
        "generated vectors={}/{} elapsed_seconds={:.1} chunks_per_second={:.2}",
        embeddings.len(),
        chunks.len(),
        elapsed_seconds,
        chunks_per_second
    ));
    let fingerprint = fingerprint_seed.with_dimension(dimension);
    let runtime_fingerprint_hash = fingerprint.hash();
    let source_sync_run_id = snapshot.identity().sync_run_id().to_string();
    let context_template_version = crate::context::METADATA_CONTEXT_TEMPLATE_VERSION.to_string();
    let spec = crate::store::EmbeddingGenerationSpec {
        model_manifest_hash: model_manifest_hash.clone(),
        runtime_fingerprint_hash,
        chunker_fingerprint: expected_chunker_fingerprint.to_string(),
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
                        .context_hash(&model_manifest_hash, expected_chunker_fingerprint),
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
            parse_repo(repo).map_err(|_| invalid_repo_input())?;
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
    let policy_plan = RepoPolicyMutationPlan::prepare(&path, &repo, true, args.force, false)?;
    let policy_text = repo_policy_toml(&repo);
    let policy_action = policy_plan.commit(policy_text.as_bytes())?;
    let overwritten = policy_action == "overwritten";

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
            "repo_source": repo_source,
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
    let (repo, repo_source) = match args.repo.as_deref() {
        Some(repo) => {
            parse_repo(repo).map_err(|_| invalid_repo_input())?;
            (repo.to_string(), "cli")
        }
        None => (
            remote
                .map(|remote| remote.repo.clone())
                .ok_or_else(|| missing_init_value("--repo"))?,
            "git_remote",
        ),
    };
    let host_default = args
        .host
        .clone()
        .or_else(|| remote.map(|remote| remote.host.clone()))
        .ok_or_else(|| missing_init_value("--host"))?
        .to_ascii_lowercase();
    let explicit_profile = explicit_profile_for_init(profile_arg);
    let (profile_id, profile_source) = match explicit_profile {
        Some((profile_id, profile_source)) => (profile_id, profile_source),
        None => {
            let profile_default = suggest_init_profile_id(&repo, &host_default)?;
            (prompt_line("profile id", &profile_default)?, "cli")
        }
    };
    let host = prompt_line("host", &host_default)?.to_ascii_lowercase();
    let existing_profile = load_profile_optional(&profile_id)?;
    let existing_profile = existing_profile
        .as_ref()
        .filter(|profile| profile.host.eq_ignore_ascii_case(&host));
    let matching_remote = remote.filter(|remote| remote.host.eq_ignore_ascii_case(&host));
    let api_default = args
        .api_base_url
        .clone()
        .or_else(|| existing_profile.map(|profile| profile.api_base_url.clone()))
        .or_else(|| matching_remote.map(|remote| remote.api_base_url.clone()))
        .unwrap_or_else(|| default_api_base_url(&host));
    let api_base_url = prompt_line("api base url", &api_default)?;
    let api_base_url_explicit =
        args.api_base_url.is_some() || !same_init_endpoint(&api_base_url, &api_default);
    let web_default = args
        .web_base_url
        .clone()
        .or_else(|| existing_profile.map(|profile| profile.web_base_url.clone()))
        .or_else(|| matching_remote.map(|remote| remote.web_base_url.clone()))
        .unwrap_or_else(|| default_web_base_url(&host));
    let web_base_url = prompt_line("web base url", &web_default)?;
    let web_base_url_explicit =
        args.web_base_url.is_some() || !same_init_endpoint(&web_base_url, &web_default);
    let (token_source, token_source_explicit) =
        init_token_source_for_profile(args, existing_profile, true)?;
    let write_repo_policy = prompt_bool("create .qgh.toml", true)?;
    finish_profile_init(
        root,
        ProfileInitPlan {
            profile_target: ProfileBootstrapTarget::Exact(profile_id),
            profile_source,
            repo,
            repo_source,
            host,
            api_base_url,
            web_base_url,
            api_base_url_explicit,
            web_base_url_explicit,
            token_source_explicit,
            token_source,
            write_repo_policy,
            force_repo_policy: args.force,
        },
    )
}

struct InitPreset {
    profile_target: ProfileBootstrapTarget,
    profile_source: &'static str,
    repo: String,
    repo_source: &'static str,
    host: String,
    api_base_url: String,
    web_base_url: String,
    api_base_url_explicit: bool,
    web_base_url_explicit: bool,
    token_source_explicit: bool,
    token_source: TokenSource,
    write_repo_policy: bool,
    force_repo_policy: bool,
    repo_policy_path: PathBuf,
}

fn init_preset(
    profile_arg: Option<&str>,
    args: &InitArgs,
    root: &std::path::Path,
    remote: Option<&GitRemote>,
) -> Result<InitPreset, QghError> {
    let (repo, repo_source) = match args.repo.as_deref() {
        Some(repo) => {
            parse_repo(repo).map_err(|_| invalid_repo_input())?;
            (repo.to_string(), "cli")
        }
        None => (
            remote
                .map(|remote| remote.repo.clone())
                .ok_or_else(|| missing_init_value("--repo"))?,
            "git_remote",
        ),
    };
    let host = args
        .host
        .clone()
        .or_else(|| remote.map(|remote| remote.host.clone()))
        .ok_or_else(|| missing_init_value("--host"))?
        .to_ascii_lowercase();
    let (profile_target, profile_source) = match explicit_profile_for_init(profile_arg) {
        Some((profile_id, profile_source)) => {
            (ProfileBootstrapTarget::Exact(profile_id), profile_source)
        }
        None if args.yes => (ProfileBootstrapTarget::Auto, "cli"),
        None => (
            ProfileBootstrapTarget::Exact(suggest_init_profile_id(&repo, &host)?),
            "cli",
        ),
    };
    let matching_remote = remote.filter(|remote| remote.host.eq_ignore_ascii_case(&host));
    let api_base_url = args
        .api_base_url
        .clone()
        .or_else(|| matching_remote.map(|remote| remote.api_base_url.clone()))
        .unwrap_or_else(|| default_api_base_url(&host));
    let web_base_url = args
        .web_base_url
        .clone()
        .or_else(|| matching_remote.map(|remote| remote.web_base_url.clone()))
        .unwrap_or_else(|| default_web_base_url(&host));
    let existing_profile = match &profile_target {
        ProfileBootstrapTarget::Exact(profile_id) => load_profile_optional(profile_id)?,
        ProfileBootstrapTarget::Auto => None,
    };
    let (token_source, token_source_explicit) =
        init_token_source_for_profile(args, existing_profile.as_ref(), false)?;
    Ok(InitPreset {
        profile_target,
        profile_source,
        repo,
        repo_source,
        host,
        api_base_url,
        web_base_url,
        api_base_url_explicit: args.api_base_url.is_some(),
        web_base_url_explicit: args.web_base_url.is_some(),
        token_source_explicit,
        token_source,
        write_repo_policy: true,
        force_repo_policy: args.force,
        repo_policy_path: root.join(".qgh.toml"),
    })
}

fn finish_init_preset(
    root: &std::path::Path,
    preset: InitPreset,
) -> Result<InitCommandOutcome, QghError> {
    finish_profile_init(
        root,
        ProfileInitPlan {
            profile_target: preset.profile_target,
            profile_source: preset.profile_source,
            repo: preset.repo,
            repo_source: preset.repo_source,
            host: preset.host,
            api_base_url: preset.api_base_url,
            web_base_url: preset.web_base_url,
            api_base_url_explicit: preset.api_base_url_explicit,
            web_base_url_explicit: preset.web_base_url_explicit,
            token_source_explicit: preset.token_source_explicit,
            token_source: preset.token_source,
            write_repo_policy: preset.write_repo_policy,
            force_repo_policy: preset.force_repo_policy,
        },
    )
}

fn write_init_preset_preview(preset: &InitPreset) -> Result<(), QghError> {
    let ProfileBootstrapTarget::Exact(profile_id) = &preset.profile_target else {
        return Err(QghError::config(
            "Automatic profile selection cannot be previewed before the config lock is acquired.",
        ));
    };
    let paths = ProfilePaths::resolve(profile_id)?;
    let mut stderr = io::stderr();
    writeln!(stderr, "Detected qgh init defaults:")?;
    writeln!(stderr, "  repo: {}", preset.repo)?;
    writeln!(stderr, "  host: {}", preset.host)?;
    writeln!(stderr, "  profile id: {profile_id}")?;
    writeln!(
        stderr,
        "  token source: {}",
        token_source_display(&preset.token_source)
    )?;
    writeln!(stderr, "  config path: {}", paths.config_file.display())?;
    writeln!(stderr, "  repo policy: create")?;
    writeln!(
        stderr,
        "  repo policy path: {}",
        preset.repo_policy_path.display()
    )?;
    writeln!(stderr, "  db path: {}", paths.db_path.display())?;
    Ok(())
}

struct ProfileInitPlan {
    profile_target: ProfileBootstrapTarget,
    profile_source: &'static str,
    repo: String,
    repo_source: &'static str,
    host: String,
    api_base_url: String,
    web_base_url: String,
    api_base_url_explicit: bool,
    web_base_url_explicit: bool,
    token_source_explicit: bool,
    token_source: TokenSource,
    write_repo_policy: bool,
    force_repo_policy: bool,
}

fn finish_profile_init(
    root: &std::path::Path,
    plan: ProfileInitPlan,
) -> Result<InitCommandOutcome, QghError> {
    let policy_path = root.join(".qgh.toml");
    let repo_policy_plan = RepoPolicyMutationPlan::prepare(
        &policy_path,
        &plan.repo,
        plan.write_repo_policy,
        plan.force_repo_policy,
        true,
    )?;

    let bootstrap = bootstrap_profile_repo(ProfileBootstrapInput {
        target: plan.profile_target,
        host: plan.host,
        api_base_url: plan.api_base_url,
        web_base_url: plan.web_base_url,
        api_base_url_explicit: plan.api_base_url_explicit,
        web_base_url_explicit: plan.web_base_url_explicit,
        token_source_explicit: plan.token_source_explicit,
        repo: plan.repo.clone(),
        token_source: plan.token_source,
    })?;

    let policy_text = repo_policy_toml(&plan.repo);
    let repo_policy_action = repo_policy_plan.commit(policy_text.as_bytes())?;

    let profile_id = bootstrap.profile_id.clone();
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
    let mut next_steps = Vec::new();
    if let Some(model) = bootstrap.default_model_install.as_deref() {
        next_steps.push(format!("qgh model install {model}"));
    }
    next_steps.push("qgh sync".to_string());
    next_steps.push("qgh query <terms>".to_string());
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
            "next_steps": next_steps
        }),
        warnings,
        meta: json!({
            "profile_id": profile_id,
            "profile_source": plan.profile_source,
            "repo": repo,
            "repo_source": plan.repo_source,
            "repo_policy_path": Value::Null
        }),
    })
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
    let store = Store::open_for_read(&profile.paths)?;
    let allowed_repository_keys = configured_repository_identity_keys(&profile);
    store.validate_profile_read_allowlist(&allowed_repository_keys)?;
    let mut vector_open_warnings = Vec::new();
    let vector_enabled = if profile.embedding.is_some() {
        match store.enable_vector_for_read() {
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
        if let Some(results) = exact_results(
            &store,
            &args.query,
            &filters,
            &profile.id,
            &profile.web_base_url,
        )? {
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
            let coverage = coverage::evaluate(
                &profile_coverage_snapshot(&store, &profile)?,
                false,
                &profile.id,
            );
            let mut warnings = freshness.warnings;
            warnings.extend(coverage.warnings);
            warnings.append(&mut vector_open_warnings);
            if vector_enabled {
                warnings.extend(embedding_warnings(&profile, &store)?);
            }
            let mut data = json!({
                "profile_id": profile.id,
                "freshness": freshness.block,
                "coverage": coverage.block,
                "result_filtering": {
                    "unresolvable_hits": 0
                },
                "results": results.items
            });
            if args.rerank {
                data["rerank"] = json!({
                    "requested": true,
                    "applied": false,
                    "reason": "exact_bypass"
                });
            }
            return Ok(LocalReadOutcome { data, warnings });
        }
        let (hybrid_vector_hits, mut hybrid_warnings) = if vector_enabled {
            hybrid_vector_hits(
                &profile,
                &store,
                publication.as_ref(),
                &args.query,
                &filters,
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
        let mut candidates = Vec::new();
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
            set_final_order_score(&mut hit.ranking, candidates.len() + 1);
            candidates.push(ResolvedQueryCandidate {
                source,
                hit,
                rerank: None,
            });
        }
        let rerank_report = args
            .rerank
            .then(|| rerank_candidates(&profile, &args.query, &mut candidates));
        let mut results = QueryResults::default();
        for candidate in candidates {
            let evidence = candidate
                .hit
                .vector_evidence
                .as_ref()
                .or(candidate.hit.lexical_evidence.as_ref());
            results.push(
                candidate.source,
                candidate.hit.ranking,
                &profile.id,
                evidence,
                candidate.rerank,
            );
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
        let coverage = coverage::evaluate(
            &profile_coverage_snapshot(&store, &profile)?,
            results.items.is_empty(),
            &profile.id,
        );
        let mut warnings = freshness.warnings;
        warnings.extend(coverage.warnings);
        warnings.append(&mut hybrid_warnings);
        if vector_enabled {
            warnings.extend(embedding_warnings(&profile, &store)?);
        }
        let mut data = json!({
            "profile_id": profile.id,
            "freshness": freshness.block,
            "coverage": coverage.block,
            "result_filtering": {
                "unresolvable_hits": unresolvable_hits
            },
            "results": results.items
        });
        if let Some(rerank_report) = rerank_report {
            data["rerank"] = rerank_report.status;
            if let Some(warning) = rerank_report.warning {
                warnings.push(warning);
            }
        }
        Ok(LocalReadOutcome { data, warnings })
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

fn init_token_source_for_profile(
    args: &InitArgs,
    existing_profile: Option<&Profile>,
    prompt_for_new_profile: bool,
) -> Result<(TokenSource, bool), QghError> {
    let Some(profile) = existing_profile else {
        let token_source = if prompt_for_new_profile {
            prompt_init_token_source(args)?
        } else {
            init_token_source_or_default(args)?
        };
        return Ok((token_source, args.token_source.is_some()));
    };

    let explicit = args.token_source.is_some() || args.token_env.is_some();
    let requested = match args.token_source {
        None if args.token_env.is_some() => return Err(missing_init_value("--token-source")),
        None => profile.token_source.clone(),
        Some(InitTokenSourceArg::GithubCli) => {
            if args.token_env.is_some() {
                return Err(QghError::validation(
                    "validation.invalid_token_source",
                    "--token-env can only be used with --token-source env.",
                ));
            }
            TokenSource::GithubCli
        }
        Some(InitTokenSourceArg::Env) => match args.token_env.as_deref() {
            Some(env) => TokenSource::Env {
                env: env.to_string(),
            },
            None => match &profile.token_source {
                TokenSource::Env { env } => TokenSource::Env { env: env.clone() },
                _ => {
                    return Err(QghError::config(format!(
                        "Profile `{}` already exists with a different token source.",
                        profile.id
                    )));
                }
            },
        },
    };
    if requested != profile.token_source {
        return Err(QghError::config(format!(
            "Profile `{}` already exists with a different token source.",
            profile.id
        )));
    }
    Ok((profile.token_source.clone(), explicit))
}

fn prompt_init_token_source(args: &InitArgs) -> Result<TokenSource, QghError> {
    let token_source_name = match args.token_source {
        Some(InitTokenSourceArg::GithubCli) => "github_cli".to_string(),
        Some(InitTokenSourceArg::Env) => "env".to_string(),
        None => prompt_line("token source (github_cli/env)", "github_cli")?,
    };
    match token_source_name.as_str() {
        "github_cli" => {
            if args.token_env.is_some() {
                return Err(QghError::validation(
                    "validation.invalid_token_source",
                    "--token-env can only be used with --token-source env.",
                ));
            }
            Ok(TokenSource::GithubCli)
        }
        "env" => {
            let env = match args.token_env.as_deref() {
                Some(env) => env.to_string(),
                None => prompt_line("token env var", "GITHUB_TOKEN")?,
            };
            Ok(TokenSource::Env { env })
        }
        _ => Err(QghError::validation(
            "validation.invalid_token_source",
            "Token source must be `github_cli` or `env`.",
        )),
    }
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

fn same_init_endpoint(left: &str, right: &str) -> bool {
    left.trim_end_matches('/') == right.trim_end_matches('/')
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

#[derive(Debug)]
struct ResolvedQueryCandidate {
    source: StoredSource,
    hit: QueryHit,
    rerank: Option<RerankMetadata>,
}

#[derive(Debug, Clone, Copy)]
struct RerankMetadata {
    score: f32,
    pre_rerank_rank: usize,
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

    fn into_query_hit(self, rrf_rank_score: f32, final_order_score: f32) -> QueryHit {
        // A candidate only carries genuine hybrid evidence when it actually
        // received a vector contribution. Fusion still runs whenever the
        // hybrid path is eligible (config + coverage), even for a query
        // where the vector search legitimately returns zero hits (e.g. no
        // sqlite-vec table yet) — those candidates are BM25-only in
        // substance and must not report ranking.kind = hybrid, or eval/A-B
        // evidence cannot distinguish real fusion from a BM25 fallback.
        let ranking = if self.vector_rank.is_some() {
            Ranking::Hybrid {
                lexical_score: self.bm25_score,
                vector_distance: self.vector_distance,
                rrf_rank_score,
                final_order_score,
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

fn hybrid_candidate_limit(limit: usize) -> usize {
    LEXICAL_GUARD_V1.candidate_window(limit)
}

fn hybrid_vector_candidate_limit() -> usize {
    LEXICAL_GUARD_V1.dense_candidate_window()
}

fn fuse_hybrid_hits(
    bm25_hits: Vec<index::SearchHit>,
    vector_hits: Vec<crate::model::VectorSearchHit>,
    limit: usize,
) -> Vec<QueryHit> {
    let lexical_ranking = bm25_hits
        .iter()
        .map(|hit| hit.source_id.clone())
        .collect::<Vec<_>>();
    let dense_ranking = vector_hits
        .iter()
        .map(|hit| hit.source_id.clone())
        .collect::<Vec<_>>();
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

    fusion::fuse_ranked(&lexical_ranking, &dense_ranking, limit, LEXICAL_GUARD_V1)
        .into_iter()
        .filter_map(|fused| {
            candidates
                .remove(&fused.key)
                .map(|candidate| candidate.into_query_hit(fused.rrf_score, fused.final_order_score))
        })
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
        || publication.chunker_fingerprint.as_deref() != Some(runtime.chunker_fingerprint.as_str())
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
    let runtime = match embedding_runtime_local_only(
        embedding,
        Some(&profile.id),
        EmbeddingRuntimeValidation::QueryOnly,
    ) {
        Ok(runtime) => runtime,
        Err(error) => {
            return Ok((
                None,
                vec![embedding_runtime_failure_warning(embedding, &error)],
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
        hybrid_vector_candidate_limit(),
    ) {
        Ok(hits) => Ok((Some(hits), Vec::new())),
        Err(error) => Ok((None, vec![vector_search_failure_warning(&error)])),
    }
}

fn vector_search_failure_warning(error: &QghError) -> Value {
    if error.code == "embedding.generation_corrupt" {
        return embedding_warning(
            "embedding.vector_integrity_failed",
            "Stored vector index bytes did not match the authoritative embedding generation. BM25 results are still returned.",
        );
    }
    embedding_warning(
        "embedding.vector_search_failed",
        "Local vector search failed. BM25 results are still returned.",
    )
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
    let configured = configured_embedding_contract_snapshot(embedding);
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

fn embedding_runtime_failure_warning(embedding: &EmbeddingConfig, error: &QghError) -> Value {
    if is_qwen_embedding_config(embedding) {
        let (code, message, action) = match error.code.as_str() {
            "embedding.model_not_installed" => (
                "embedding.model_not_installed",
                "The local Qwen embedding model is not installed. BM25 results are still returned.",
                CommandAction::new(
                    "embedding_model_not_installed",
                    "qgh model install qwen3-embedding-0.6b",
                ),
            ),
            "embedding.qwen_snapshot_invalid" => (
                "embedding.qwen_snapshot_invalid",
                "The local Qwen embedding snapshot failed integrity validation. BM25 results are still returned.",
                CommandAction::new(
                    "embedding_model_invalid",
                    "qgh model install qwen3-embedding-0.6b",
                ),
            ),
            _ => {
                return embedding_warning(
                    "embedding.runtime_unavailable",
                    "Local embedding runtime was unavailable. BM25 results are still returned.",
                );
            }
        };
        return json!({
            "code": code,
            "severity": "warn",
            "message": message,
            "action": action
        });
    }
    embedding_warning(
        "embedding.runtime_unavailable",
        "Local embedding runtime was unavailable. BM25 results are still returned.",
    )
}

fn reranker_warning(code: &'static str, message: &'static str) -> Value {
    json!({
        "code": code,
        "severity": "warn",
        "message": message
    })
}

fn rerank_not_configured_status() -> Value {
    json!({
        "requested": true,
        "applied": false,
        "reason": "not_configured"
    })
}

struct RerankReport {
    status: Value,
    warning: Option<Value>,
}

fn rerank_candidates(
    profile: &Profile,
    query: &str,
    candidates: &mut Vec<ResolvedQueryCandidate>,
) -> RerankReport {
    if candidates.is_empty() {
        return RerankReport {
            status: json!({
                "requested": true,
                "applied": false,
                "reason": "no_candidates"
            }),
            warning: None,
        };
    }
    let Some(reranker) = &profile.reranker else {
        return RerankReport {
            status: rerank_not_configured_status(),
            warning: Some(reranker_warning(
                "reranker.not_configured",
                "Reranking was requested but no local reranker is configured. Original retrieval order is returned.",
            )),
        };
    };
    let candidate_count = candidates.len().min(LOCAL_RERANK_DEPTH);

    #[cfg(debug_assertions)]
    if let Some(scores) = test_rerank_scores(candidates, candidate_count) {
        return match scores {
            Ok(scores) => {
                apply_rerank_scores(candidates, scores);
                rerank_applied_report("cpu_f32", candidate_count, None)
            }
            Err(()) => rerank_failure(
                "inference_failed",
                "reranker.inference_failed",
                "The local reranker did not score the complete candidate set. Original retrieval order is returned.",
            ),
        };
    }

    let spec = qwen_model_spec(&reranker.model).expect("validated reranker preset");
    let snapshot = match default_prepared_qwen_model_store().and_then(|store| store.inspect(&spec))
    {
        Ok(snapshot) => snapshot,
        Err(error) => return rerank_error_report(&error),
    };
    let documents = candidates
        .iter()
        .take(candidate_count)
        .map(rerank_document)
        .collect::<Vec<_>>();
    match production_rerank_scores(reranker, &snapshot, query, &documents) {
        Ok((scores, runtime_profile)) => {
            if scores.len() != candidate_count || scores.iter().any(|score| !score.is_finite()) {
                return rerank_failure(
                    "inference_failed",
                    "reranker.inference_failed",
                    "The local reranker did not score the complete candidate set. Original retrieval order is returned.",
                );
            }
            apply_rerank_scores(candidates, scores);
            let warning = (runtime_profile == "cpu_f32").then(|| {
                reranker_warning(
                    "reranker.cpu_slow_path",
                    "The explicitly selected CPU reranker path is experimental and may be slow.",
                )
            });
            rerank_applied_report(runtime_profile, candidate_count, warning)
        }
        Err(error) => rerank_error_report(&error),
    }
}

fn rerank_applied_report(
    runtime_profile: &str,
    candidate_count: usize,
    warning: Option<Value>,
) -> RerankReport {
    RerankReport {
        status: json!({
            "requested": true,
            "applied": true,
            "model": QWEN_RERANKER_PRESET_ID,
            "runtime_profile": runtime_profile,
            "candidate_count": candidate_count,
            "max_candidates": LOCAL_RERANK_DEPTH,
            "max_tokens": LOCAL_RERANK_MAX_TOKENS
        }),
        warning,
    }
}

fn rerank_failure(reason: &'static str, code: &'static str, message: &'static str) -> RerankReport {
    RerankReport {
        status: json!({
            "requested": true,
            "applied": false,
            "reason": reason
        }),
        warning: Some(reranker_warning(code, message)),
    }
}

fn rerank_error_report(error: &QghError) -> RerankReport {
    match error.code.as_str() {
        "model.not_installed" => with_rerank_repair_action(
            rerank_failure(
                "model_not_installed",
                "reranker.model_not_installed",
                "Reranking was requested but the configured local model is not installed. Original retrieval order is returned.",
            ),
            CommandAction::new(
                "reranker_model_not_installed",
                "qgh model install qwen3-reranker-0.6b",
            ),
        ),
        "model.snapshot_invalid" | "model.artifact_invalid" => with_rerank_repair_action(
            rerank_failure(
                "model_corrupt",
                "reranker.model_corrupt",
                "The configured local reranker failed integrity validation. Original retrieval order is returned.",
            ),
            CommandAction::new(
                "reranker_model_invalid",
                "qgh model install qwen3-reranker-0.6b",
            ),
        ),
        "reranker.device_unavailable" => rerank_failure(
            "device_unavailable",
            "reranker.device_unavailable",
            "The configured local reranker device is unavailable. Original retrieval order is returned.",
        ),
        "reranker.inference_failed" | "reranker.contract_invalid" => rerank_failure(
            "inference_failed",
            "reranker.inference_failed",
            "The local reranker did not score the complete candidate set. Original retrieval order is returned.",
        ),
        _ => rerank_failure(
            "runtime_unavailable",
            "reranker.runtime_unavailable",
            "Reranking was requested but the configured local runtime is unavailable. Original retrieval order is returned.",
        ),
    }
}

fn with_rerank_repair_action(mut report: RerankReport, action: CommandAction) -> RerankReport {
    report.status["repair_action"] = json!(action.clone());
    if let Some(warning) = report.warning.as_mut() {
        warning["action"] = json!(action);
    }
    report
}

fn apply_rerank_scores(candidates: &mut Vec<ResolvedQueryCandidate>, scores: Vec<f32>) {
    let depth = scores.len();
    let drained = candidates.drain(..depth).collect::<Vec<_>>();
    let mut head = drained
        .into_iter()
        .zip(scores)
        .enumerate()
        .map(|(index, (mut candidate, score))| {
            candidate.rerank = Some(RerankMetadata {
                score,
                pre_rerank_rank: index + 1,
            });
            candidate
        })
        .collect::<Vec<_>>();
    head.sort_by(|left, right| {
        let left = left.rerank.expect("rerank metadata assigned");
        let right = right.rerank.expect("rerank metadata assigned");
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.pre_rerank_rank.cmp(&right.pre_rerank_rank))
    });
    head.append(candidates);
    *candidates = head;
}

#[cfg(debug_assertions)]
fn test_rerank_scores(
    candidates: &[ResolvedQueryCandidate],
    candidate_count: usize,
) -> Option<Result<Vec<f32>, ()>> {
    let raw = std::env::var(TEST_RERANK_SCORES_ENV).ok()?;
    let scores: HashMap<String, f32> = match serde_json::from_str(&raw) {
        Ok(scores) => scores,
        Err(_) => return Some(Err(())),
    };
    Some(
        candidates
            .iter()
            .take(candidate_count)
            .map(|candidate| {
                scores
                    .get(source_id(&candidate.source))
                    .copied()
                    .filter(|score| score.is_finite())
                    .ok_or(())
            })
            .collect(),
    )
}

#[cfg(debug_assertions)]
fn source_id(source: &StoredSource) -> &str {
    match source {
        StoredSource::Issue(issue) => &issue.source_id,
        StoredSource::Comment(comment) => &comment.source_id,
    }
}

fn rerank_document(candidate: &ResolvedQueryCandidate) -> String {
    let evidence_body = candidate
        .hit
        .vector_evidence
        .as_ref()
        .or(candidate.hit.lexical_evidence.as_ref())
        .map(|evidence| evidence.chunk.body.as_str());
    match &candidate.source {
        StoredSource::Issue(issue) => prepare_embedding_input(
            EmbeddingSourceContext::Issue {
                repository: &issue.repo,
                issue_number: issue.number,
                title: &issue.title,
            },
            evidence_body.unwrap_or(&issue.body),
        )
        .as_str()
        .to_string(),
        StoredSource::Comment(comment) => prepare_embedding_input(
            EmbeddingSourceContext::Comment {
                repository: &comment.repo,
                parent_issue_number: comment.issue_number,
                parent_issue_title: &comment.parent_issue.title,
            },
            evidence_body.unwrap_or(&comment.body),
        )
        .as_str()
        .to_string(),
    }
}

#[cfg(feature = "fastembed-provider")]
fn production_rerank_scores(
    reranker: &crate::config::RerankerConfig,
    snapshot: &crate::local_models::PreparedQwenModelSnapshot,
    query: &str,
    documents: &[String],
) -> Result<(Vec<f32>, &'static str), QghError> {
    let crate::config::RerankerProviderKind::Local = reranker.provider;
    debug_assert_eq!(LOCAL_RERANK_DEPTH, QWEN_RERANK_DEPTH);
    debug_assert_eq!(LOCAL_RERANK_MAX_TOKENS, QWEN_RERANK_MAX_LENGTH);
    let cache_key = format!(
        "{}:{}:{:?}",
        reranker.model, snapshot.manifest_hash, reranker.device
    );
    let runtime = runtime_cache_get_or_try_init(
        RERANKER_RUNTIME_CACHE.get_or_init(|| Mutex::new(HashMap::new())),
        &cache_key,
        "",
        || load_qwen_reranker(snapshot, reranker.device).map(Arc::new),
    )?;
    let runtime_profile = runtime.runtime_profile.as_str();
    runtime
        .score(query, documents)
        .map(|scores| (scores, runtime_profile))
}

#[cfg(not(feature = "fastembed-provider"))]
fn production_rerank_scores(
    _reranker: &crate::config::RerankerConfig,
    _snapshot: &crate::local_models::PreparedQwenModelSnapshot,
    _query: &str,
    _documents: &[String],
) -> Result<(Vec<f32>, &'static str), QghError> {
    Err(QghError::validation(
        "reranker.runtime_unavailable",
        "This qgh binary was built without the local reranker runtime.",
    ))
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
        rerank: Option<RerankMetadata>,
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
        let mut result = source_result(source, ranking, profile_id, evidence);
        if let Some(rerank) = rerank {
            if let Some(ranking) = result.get_mut("ranking").and_then(Value::as_object_mut) {
                ranking.insert("rerank_score".to_string(), json!(rerank.score));
                ranking.insert("pre_rerank_rank".to_string(), json!(rerank.pre_rerank_rank));
            }
        }
        self.items.push(result);
    }
}

fn embedding_warnings(profile: &Profile, store: &Store) -> Result<Vec<Value>, QghError> {
    let Some(coverage) = embedding_coverage_state(profile, store)? else {
        return Ok(Vec::new());
    };
    // Query-time data readiness must not be overwritten by whether the local
    // runtime snapshot can currently be opened. `hybrid_vector_hits` reports
    // runtime failures separately after stored coverage has been validated.
    let state = coverage.state();
    let mut warnings = embedding_warnings_for_state(state);
    if let (Some(warning), Some(action)) = (
        warnings.first_mut(),
        embedding_repair_action(profile, &coverage, state),
    ) {
        warning["action"] = json!(action);
    }
    Ok(warnings)
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
    let configured = configured_embedding_contract_snapshot(embedding);
    let coverage = embedding_coverage_state_for_config(embedding, store, &configured)?;
    let status_state = coverage.status_state();
    let repair_action = embedding_repair_action(profile, &coverage, status_state);
    let mut warnings = embedding_warnings_for_state(status_state);
    if let (Some(warning), Some(action)) = (warnings.first_mut(), repair_action.as_ref()) {
        warning["action"] = json!(action);
    }

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
            "repair_action": repair_action,
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

fn embedding_repair_action(
    profile: &Profile,
    coverage: &EmbeddingCoverageState,
    state: &str,
) -> Option<CommandAction> {
    if state == "complete" {
        return None;
    }
    let embedding = profile.embedding.as_ref()?;
    if is_qwen_embedding_config(embedding) {
        #[cfg(feature = "fastembed-provider")]
        {
            let spec = qwen_model_spec(QWEN_EMBEDDING_PRESET_ID)
                .expect("Qwen embedding preset is registered");
            let snapshot_state = default_prepared_qwen_model_store()
                .ok()
                .map(|store| store.snapshot_state(&spec))?;
            match snapshot_state {
                ModelSnapshotState::Missing => {
                    return Some(CommandAction::new(
                        "embedding_model_not_installed",
                        "qgh model install qwen3-embedding-0.6b",
                    ));
                }
                ModelSnapshotState::Invalid => {
                    return Some(CommandAction::new(
                        "embedding_model_invalid",
                        "qgh model install qwen3-embedding-0.6b",
                    ));
                }
                ModelSnapshotState::Present => {}
            }
        }
        #[cfg(not(feature = "fastembed-provider"))]
        {
            return None;
        }
    }
    (coverage.total_chunks > 0).then(|| {
        CommandAction::new(
            "embedding_rebuild_required",
            format!("qgh embed --force --profile {}", profile.id),
        )
    })
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
    let mut data = json!({
        "provider": embedding_provider_name(embedding.provider),
        "model": model,
        "model_id": configured.model_id,
        "model_revision": configured.model_revision,
        "model_path": embedding
            .model_path
            .as_ref()
            .or(embedding.manifest_path.as_ref())
            .map(|path| path.to_string_lossy().into_owned())
    });
    if is_qwen_embedding_config(embedding) {
        data["device"] = json!(embedding.device.as_str());
        data["runtime_profile"] = json!(configured_qwen_runtime_profile_id(embedding.device));
    }
    data
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

fn configured_embedding_contract_snapshot(
    embedding: &EmbeddingConfig,
) -> ConfiguredEmbeddingSnapshot {
    if is_qwen_embedding_config(embedding) {
        return configured_qwen_embedding_contract_snapshot(embedding);
    }
    let options = embedding.fastembed_options();
    let mut prepared_runtime = PreparedRuntimeAvailability::Missing;
    if let Ok(store) = default_prepared_model_store() {
        match store.inspect_prepared_alias_contract(&options) {
            Ok(inspection) => {
                return configured_snapshot_from_contract(
                    &inspection,
                    PreparedRuntimeAvailability::Available,
                );
            }
            Err(error) if error.code() == "embedding.prepared_snapshot_missing" => {}
            Err(_) => prepared_runtime = PreparedRuntimeAvailability::Corrupt,
        }
    }
    if let Some(manifest_path) = options.manifest_path.as_deref() {
        match PreparedModelStore::new(PathBuf::new()).inspect_manifest_contract(manifest_path) {
            Ok(inspection) => {
                let availability = if prepared_runtime == PreparedRuntimeAvailability::Corrupt {
                    PreparedRuntimeAvailability::Corrupt
                } else {
                    PreparedRuntimeAvailability::Available
                };
                return configured_snapshot_from_contract(&inspection, availability);
            }
            Err(error) if error.code() == "embedding.prepared_manifest_missing" => {}
            Err(_) => prepared_runtime = PreparedRuntimeAvailability::Corrupt,
        }
    }

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

fn configured_qwen_embedding_contract_snapshot(
    embedding: &EmbeddingConfig,
) -> ConfiguredEmbeddingSnapshot {
    let spec =
        qwen_model_spec(QWEN_EMBEDDING_PRESET_ID).expect("Qwen embedding preset is registered");
    let manifest_hash = qwen_model_manifest_hash(&spec);
    let runtime_profile = configured_qwen_runtime_profile_id(embedding.device);
    let prepared_runtime = {
        #[cfg(feature = "fastembed-provider")]
        {
            match default_prepared_qwen_model_store() {
                Ok(store) => match store.snapshot_state(&spec) {
                    ModelSnapshotState::Missing => PreparedRuntimeAvailability::Missing,
                    ModelSnapshotState::Present => PreparedRuntimeAvailability::Available,
                    ModelSnapshotState::Invalid => PreparedRuntimeAvailability::Corrupt,
                },
                Err(_) => PreparedRuntimeAvailability::Corrupt,
            }
        }
        #[cfg(not(feature = "fastembed-provider"))]
        {
            PreparedRuntimeAvailability::Missing
        }
    };
    ConfiguredEmbeddingSnapshot {
        model_id: Some(QWEN_EMBEDDING_MODEL_ID.to_string()),
        model_revision: Some(configured_qwen_runtime_identity(
            &manifest_hash,
            runtime_profile,
        )),
        pooling: Some(PoolingKind::LastToken),
        query_prefix: Some(QWEN_EMBEDDING_QUERY_PREFIX.to_string()),
        prepared_runtime,
    }
}

fn is_qwen_embedding_config(embedding: &EmbeddingConfig) -> bool {
    embedding.model.as_deref() == Some(QWEN_EMBEDDING_PRESET_ID)
}

fn configured_qwen_runtime_profile_id(device: crate::config::LocalModelDevice) -> &'static str {
    #[cfg(feature = "fastembed-provider")]
    {
        qwen_embedding_runtime_profile_id(device)
    }
    #[cfg(not(feature = "fastembed-provider"))]
    {
        match device {
            crate::config::LocalModelDevice::Cpu => "cpu_f32",
            crate::config::LocalModelDevice::Metal => "metal_f16",
            crate::config::LocalModelDevice::Auto => {
                if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
                    "metal_f16"
                } else {
                    "cpu_f32"
                }
            }
        }
    }
}

fn configured_qwen_runtime_identity(base_revision: &str, runtime_profile: &str) -> String {
    let identity =
        format!("{base_revision}:{runtime_profile}:{QWEN_EMBEDDING_INPUT_ADAPTER_REVISION}");
    if runtime_profile == "metal_f16" {
        format!("{identity}:{QWEN_EMBEDDING_METAL_ADAPTER_REVISION}")
    } else {
        identity
    }
}

fn configured_snapshot_from_contract(
    inspection: &PreparedManifestInspection,
    prepared_runtime: PreparedRuntimeAvailability,
) -> ConfiguredEmbeddingSnapshot {
    configured_snapshot_from_manifest(
        inspection.manifest(),
        inspection.manifest_hash(),
        prepared_runtime,
    )
}

fn configured_snapshot_from_manifest(
    manifest: &ModelManifestV1,
    manifest_hash: &str,
    prepared_runtime: PreparedRuntimeAvailability,
) -> ConfiguredEmbeddingSnapshot {
    ConfiguredEmbeddingSnapshot {
        model_id: Some(prepared_manifest_model_id(manifest)),
        model_revision: Some(manifest_hash.to_string()),
        pooling: Some(manifest.pooling),
        query_prefix: Some(manifest.query_prefix.clone().unwrap_or_default()),
        prepared_runtime,
    }
}

fn configured_embedding_model_revision_without_snapshot(
    embedding: &EmbeddingConfig,
) -> Option<String> {
    if is_qwen_embedding_config(embedding) {
        let spec =
            qwen_model_spec(QWEN_EMBEDDING_PRESET_ID).expect("Qwen embedding preset is registered");
        let manifest_hash = qwen_model_manifest_hash(&spec);
        let runtime_profile = configured_qwen_runtime_profile_id(embedding.device);
        return Some(configured_qwen_runtime_identity(
            &manifest_hash,
            runtime_profile,
        ));
    }
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
    web_base_url: &str,
) -> Result<Option<QueryResults>, QghError> {
    if let Some(kind) = configured_exact_url_kind(query_text, web_base_url) {
        let source = find_exact_url_source(store, query_text, kind)?;
        let mut results = QueryResults::default();
        if let Some(source) = source.filter(|source| filters.matches(source)) {
            results.push(source, Ranking::Exact, profile_id, None, None);
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
            results.push(source, Ranking::Exact, profile_id, None, None);
        }
    }
    Ok(Some(results))
}

enum ExactUrlKind {
    Issue,
    Comment,
}

fn find_exact_url_source(
    store: &Store,
    query_text: &str,
    kind: ExactUrlKind,
) -> Result<Option<StoredSource>, QghError> {
    match kind {
        ExactUrlKind::Issue => store
            .find_issue_by_canonical_url(query_text)
            .map(|issue| issue.map(StoredSource::Issue)),
        ExactUrlKind::Comment => store
            .find_comment_by_canonical_url(query_text)
            .map(|comment| comment.map(StoredSource::Comment)),
    }
}

fn configured_exact_url_kind(query_text: &str, web_base_url: &str) -> Option<ExactUrlKind> {
    let Ok(locator) = reqwest::Url::parse(query_text) else {
        return None;
    };
    let Ok(base) = reqwest::Url::parse(web_base_url) else {
        return None;
    };
    if locator.origin() != base.origin()
        || !locator.username().is_empty()
        || locator.password().is_some()
        || locator.query().is_some()
    {
        return None;
    }
    let segments = locator.path_segments()?;
    let segments = segments.collect::<Vec<_>>();
    if segments.len() != 4
        || segments[0].is_empty()
        || segments[1].is_empty()
        || segments[2] != "issues"
        || !segments[3].parse::<u64>().is_ok_and(|number| number > 0)
    {
        return None;
    }
    match locator.fragment() {
        None => Some(ExactUrlKind::Issue),
        Some(fragment) => fragment
            .strip_prefix("issuecomment-")
            .and_then(|id| id.parse::<u64>().ok())
            .filter(|id| *id > 0)
            .map(|_| ExactUrlKind::Comment),
    }
}

fn parse_issue_number(query_text: &str) -> Option<i64> {
    query_text
        .strip_prefix('#')
        .unwrap_or(query_text)
        .parse::<i64>()
        .ok()
}

fn validate_repo(repo: &str) -> Result<(), QghError> {
    parse_repo(repo)
        .map(|_| ())
        .map_err(|_| invalid_repo_input())
}

fn invalid_repo_input() -> QghError {
    QghError::validation(
        "validation.invalid_repo",
        "Repo must use explicit owner/repo format.",
    )
    .with_hint("Use explicit owner/repo format.")
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
    let mut store = if verify_lifecycle {
        Store::open(&profile.paths)?
    } else {
        Store::open_for_read(&profile.paths)?
    };
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
    let mut store = if verify_lifecycle {
        Store::open(&profile.paths)?
    } else {
        Store::open_for_read(&profile.paths)?
    };
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
                        | github::ClassifiedLifecycleCheck::RequestBudget(_)
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
    let store = Store::open_for_read(&profile.paths)?;
    let status = store.status()?;
    let purge = purge_report(&store)?;
    let coverage = coverage::evaluate(
        &profile_coverage_snapshot(&store, &profile)?,
        false,
        &profile.id,
    );
    let retrieval_warning = match store.resolve_active_tantivy_artifact() {
        Ok(Some(_)) => None,
        Ok(None) if status.last_sync_at.is_some() => Some(json!({
            "code": "publication.tantivy_artifact_not_ready",
            "severity": "warn_strong",
            "message": "The local lexical retrieval artifact is not ready. Run qgh sync to rebuild it."
        })),
        Ok(None) => None,
        Err(error) if is_repairable_retrieval_publication_error(&error.code) => Some(json!({
            "code": error.code,
            "severity": "warn_strong",
            "message": "The local retrieval publication is not ready. Run qgh sync to rebuild it."
        })),
        Err(error) => return Err(error),
    };
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
    if let Some(warning) = retrieval_warning {
        warnings.push(warning);
    }
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
    let rate_budget = stored_rate_budget_block(&store, &profile.host)?;
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
            "schema_version": STORE_SCHEMA_VERSION
        },
        "index": {
            "active_generation": status.active_generation,
            "dirty_task_count": status.dirty_task_count
        },
        "sync": {
            "last_sync_at": status.last_sync_at,
            "cursors": cursors,
            "backoff": status.backoff,
            "rate_budget": rate_budget,
            "scheduler": sync_scheduler_contract(&profile)
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

fn is_repairable_retrieval_publication_error(code: &str) -> bool {
    matches!(
        code,
        "publication.source_snapshot_incomplete"
            | "publication.source_snapshot_changed"
            | "publication.embedding_snapshot_mismatch"
            | "publication.tantivy_artifact_not_ready"
            | "publication.source_inventory_mismatch"
    )
}

pub async fn doctor(profile_id: &str) -> Result<Value, QghError> {
    let profile = load_profile(profile_id)?;
    let store = Store::open_for_read(&profile.paths)?;
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
        if is_qwen_embedding_config(embedding) {
            let spec = qwen_model_spec(QWEN_EMBEDDING_PRESET_ID)
                .expect("Qwen embedding preset is registered");
            let snapshot = default_prepared_qwen_model_store()
                .and_then(|prepared_store| prepared_store.inspect(&spec));
            let artifacts_ok = snapshot.is_ok();
            let runtime_ok = snapshot.as_ref().is_ok_and(|snapshot| {
                build_qwen_embedding_runtime(
                    embedding,
                    snapshot,
                    EmbeddingRuntimeValidation::BatchComparability,
                )
                .is_ok()
            });
            (artifacts_ok, runtime_ok)
        } else {
            let snapshot = default_prepared_model_store().and_then(|prepared_store| {
                prepared_store
                    .inspect(&embedding.fastembed_options())
                    .and_then(|inspection| prepared_store.verify(inspection))
            });
            let artifacts_ok = snapshot.is_ok();
            let runtime_ok = snapshot.as_ref().is_ok_and(|snapshot| {
                build_embedding_runtime(
                    embedding,
                    snapshot,
                    EmbeddingRuntimeValidation::BatchComparability,
                )
                .is_ok()
            });
            (artifacts_ok, runtime_ok)
        }
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
    #[cfg(all(test, feature = "vector-search"))]
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

fn set_final_order_score(ranking: &mut Ranking, result_rank: usize) {
    if let Ranking::Hybrid {
        final_order_score, ..
    } = ranking
    {
        *final_order_score = 1.0 / result_rank.max(1) as f32;
    }
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
        #[cfg(all(test, feature = "vector-search"))]
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
    let Ok(client) = github::github_http_client() else {
        return (false, false, rate_limit_headers_json(None, None));
    };
    let Ok(request) = github::github_get(&client, &url, token, &profile.api_base_url) else {
        return (false, false, rate_limit_headers_json(None, None));
    };
    let response = request.send().await;
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

    #[test]
    fn repo_policy_apply_rejects_a_stale_created_plan() {
        let root = std::env::temp_dir().join(format!(
            "qgh-repo-policy-cas-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join(".qgh.toml");
        let plan =
            RepoPolicyMutationPlan::prepare(&path, "owner/requested", true, false, true).unwrap();
        fs::write(&path, repo_policy_toml("owner/concurrent")).unwrap();

        let candidate = repo_policy_toml("owner/requested");
        let error = plan.commit(candidate.as_bytes()).unwrap_err();

        assert_eq!(error.code, "config.repo_policy_exists");
        assert!(fs::read_to_string(&path)
            .unwrap()
            .contains(r#"github = "owner/concurrent""#));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sync_retry_action_preserves_the_interrupted_operation() {
        assert_eq!(
            sync_retry_command(
                "work",
                None,
                None,
                false,
                None,
                true,
                Some(25),
                Some("90s"),
                true,
                None,
                true,
                false,
            ),
            "qgh sync --all --backfill --max-requests 25 --max-duration 90s --profile work --json"
        );
        assert_eq!(
            sync_retry_command(
                "work",
                Some(ReconcileMode::Recent),
                Some("7d"),
                true,
                Some("30m"),
                false,
                None,
                None,
                false,
                Some("owner/repo"),
                true,
                true,
            ),
            "qgh sync --repo owner/repo --reconcile recent --window 7d --if-stale --max-age 30m --profile work --quiet --json"
        );
    }

    #[test]
    fn transient_lifecycle_interruption_is_retryable() {
        let disposition = interruption_disposition(github::LifecycleInterruption::Transient(
            github::GitHubTransientKind::Server,
        ));
        let InterruptionDisposition::Error(error) = disposition else {
            panic!("transient lifecycle interruption must remain an error");
        };

        assert_eq!(error.code, "github.request_failed");
        assert_eq!(error.exit_code, 3);
        assert!(error.retryable);
        assert_eq!(
            error.hint.as_deref(),
            Some("Retry later; local content was not removed.")
        );
    }

    #[cfg(feature = "fastembed-provider")]
    #[test]
    fn qwen_sync_tokenizer_routes_to_installed_snapshot_not_legacy_hf_acquisition() {
        let embedding = EmbeddingConfig {
            provider: EmbeddingProviderKind::Local,
            manifest_path: None,
            model: Some(QWEN_EMBEDDING_PRESET_ID.to_string()),
            model_path: None,
            file: None,
            pooling: None,
            query_prefix: None,
            quantization: None,
            token_source: None,
            device: crate::config::LocalModelDevice::Auto,
        };

        assert_eq!(
            embedding_tokenizer_route(&embedding),
            EmbeddingTokenizerRoute::PreparedQwenSnapshot
        );
    }

    #[cfg(feature = "fastembed-provider")]
    #[test]
    fn query_runtime_skips_document_batch_comparability_smoke() {
        let provider = RecordingEmbeddingProvider::default();

        validate_embedding_runtime_if_required(
            &provider,
            EmbeddingRuntimeValidation::QueryOnly,
            "public runtime smoke",
        )
        .unwrap();

        assert!(provider.documents.lock().unwrap().is_empty());
        validate_embedding_runtime_if_required(
            &provider,
            EmbeddingRuntimeValidation::BatchComparability,
            "public runtime smoke",
        )
        .unwrap();
        assert_eq!(provider.documents.lock().unwrap().len(), 6);
    }

    #[cfg(feature = "fastembed-provider")]
    #[test]
    fn qwen_adapter_revisions_invalidate_affected_generation_and_cache() {
        assert_eq!(QWEN_EMBEDDING_INPUT_ADAPTER_REVISION, "explicit-window-v2");
        assert_eq!(
            QWEN_EMBEDDING_METAL_ADAPTER_REVISION,
            "metal-sdpa-adaptive-batching-v2"
        );
        let fingerprint = |model_revision: &str| {
            EmbeddingFingerprintSeed {
                provider: "fastembed".to_string(),
                model_id: QWEN_EMBEDDING_MODEL_ID.to_string(),
                model_revision: model_revision.to_string(),
                pooling: PoolingKind::LastToken,
                query_prefix: QWEN_EMBEDDING_QUERY_PREFIX.to_string(),
            }
            .with_dimension(384)
        };
        let expectation = |model_revision: String| EmbeddingFingerprintExpectation {
            provider: "fastembed".to_string(),
            model_id: Some(QWEN_EMBEDDING_MODEL_ID.to_string()),
            model_revision: Some(model_revision),
            pooling: Some(PoolingKind::LastToken),
            query_prefix: Some(QWEN_EMBEDDING_QUERY_PREFIX.to_string()),
        };

        let previous_metal_revision =
            "manifest:metal_f16:explicit-window-v1:metal-sdpa-adaptive-batching-v2";
        let current_metal_revision = configured_qwen_runtime_identity("manifest", "metal_f16");
        assert_eq!(
            current_metal_revision,
            format!(
                "manifest:metal_f16:{QWEN_EMBEDDING_INPUT_ADAPTER_REVISION}:{QWEN_EMBEDDING_METAL_ADAPTER_REVISION}"
            )
        );
        assert!(!fingerprint(previous_metal_revision)
            .matches_expectation(&expectation(current_metal_revision.clone())));
        assert!(fingerprint(&current_metal_revision)
            .matches_expectation(&expectation(current_metal_revision)));
        assert_eq!(
            qwen_embedding_runtime_cache_key("profile", "manifest", "metal_f16"),
            format!(
                "profile:qwen:manifest:metal_f16:{QWEN_EMBEDDING_INPUT_ADAPTER_REVISION}:{QWEN_EMBEDDING_METAL_ADAPTER_REVISION}"
            )
        );

        let cpu_revision = configured_qwen_runtime_identity("manifest", "cpu_f32");
        assert_eq!(
            cpu_revision,
            format!("manifest:cpu_f32:{QWEN_EMBEDDING_INPUT_ADAPTER_REVISION}")
        );
        assert!(!fingerprint("manifest:cpu_f32:explicit-window-v1")
            .matches_expectation(&expectation(cpu_revision.clone())));
        assert!(fingerprint(&cpu_revision).matches_expectation(&expectation(cpu_revision)));
        assert_eq!(
            qwen_embedding_runtime_cache_key("profile", "manifest", "cpu_f32"),
            format!("profile:qwen:manifest:cpu_f32:{QWEN_EMBEDDING_INPUT_ADAPTER_REVISION}")
        );
    }

    #[cfg(feature = "fastembed-provider")]
    #[test]
    #[ignore = "requires explicitly installed pinned Qwen model snapshots"]
    fn installed_qwen_sync_tokenizer_matches_query_runtime_chunk_contract() {
        let root = PathBuf::from(
            std::env::var("QGH_QWEN_PREPARED_MODELS")
                .expect("QGH_QWEN_PREPARED_MODELS must point to the prepared store"),
        );
        let spec = qwen_model_spec(QWEN_EMBEDDING_PRESET_ID).unwrap();
        let snapshot = crate::local_models::PreparedQwenModelStore::new(root)
            .inspect(&spec)
            .unwrap();
        let embedding = EmbeddingConfig {
            provider: EmbeddingProviderKind::Local,
            manifest_path: None,
            model: Some(QWEN_EMBEDDING_PRESET_ID.to_string()),
            model_path: None,
            file: None,
            pooling: None,
            query_prefix: None,
            quantization: None,
            token_source: None,
            device: crate::config::LocalModelDevice::Auto,
        };

        let sync_tokenizer = qwen_embedding_tokenizer_from_snapshot(&snapshot).unwrap();
        let query_runtime = build_qwen_embedding_runtime(
            &embedding,
            &snapshot,
            EmbeddingRuntimeValidation::QueryOnly,
        )
        .unwrap();

        assert_eq!(
            sync_tokenizer.chunker_fingerprint,
            query_runtime.chunker_fingerprint
        );
        assert!(
            sync_tokenizer
                .tokenizer
                .tokenize("public Qwen sync contract")
                .unwrap()
                .len()
                > 1
        );
    }

    #[cfg(feature = "fastembed-provider")]
    #[test]
    fn tokenizer_only_acquisition_reads_no_model_or_external_bytes() {
        use crate::embedding::{
            reset_tokenizer_only_artifact_bytes, tokenizer_only_artifact_bytes, ArtifactRole,
            ModelArtifactV1, ModelProviderKind, NormalizationKind, QuantizationKind, TokenizerKind,
            MODEL_MANIFEST_SCHEMA_VERSION,
        };
        use sha2::{Digest, Sha256};
        use tokenizers::models::wordlevel::WordLevel;

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "qgh-tokenizer-only-acquisition-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let tokenizer = tokenizers::Tokenizer::new(WordLevel::default())
            .to_string(false)
            .unwrap()
            .into_bytes();
        let declarations = [
            (
                ArtifactRole::OnnxModel,
                "model.onnx",
                b"model-must-not-be-read".as_slice(),
                None,
            ),
            (
                ArtifactRole::OnnxExternalData,
                "weights.bin",
                b"external-must-not-be-read".as_slice(),
                Some("weights.bin"),
            ),
            (
                ArtifactRole::Tokenizer,
                "tokenizer.json",
                tokenizer.as_slice(),
                None,
            ),
            (ArtifactRole::Config, "config.json", b"{}".as_slice(), None),
            (
                ArtifactRole::SpecialTokensMap,
                "special_tokens_map.json",
                b"{}".as_slice(),
                None,
            ),
            (
                ArtifactRole::TokenizerConfig,
                "tokenizer_config.json",
                b"{}".as_slice(),
                None,
            ),
        ];
        let artifacts = declarations
            .iter()
            .map(|(role, path, bytes, initializer)| {
                if !matches!(
                    role,
                    ArtifactRole::OnnxModel | ArtifactRole::OnnxExternalData
                ) {
                    std::fs::write(root.join(path), bytes).unwrap();
                }
                ModelArtifactV1 {
                    role: *role,
                    relative_path: path.to_string(),
                    sha256: Sha256::digest(bytes)
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect(),
                    byte_size: bytes.len() as u64,
                    external_initializer_name: initializer.map(ToString::to_string),
                }
            })
            .collect();
        let manifest = ModelManifestV1 {
            schema_version: MODEL_MANIFEST_SCHEMA_VERSION.to_string(),
            preset_id: None,
            provider: ModelProviderKind::Fastembed,
            model_source: ModelSourceV1::Local {
                declared_id: "tokenizer-only-fixture".to_string(),
            },
            artifacts,
            tokenizer: TokenizerKind::HfTokenizerJson,
            query_prefix: Some(String::new()),
            document_prefix: Some(String::new()),
            pooling: crate::embedding::PoolingKind::Cls,
            normalization: NormalizationKind::L2,
            native_dimension: 4,
            output_dimension: 4,
            max_length: 32,
            quantization: QuantizationKind::None,
            context_template_version: crate::context::METADATA_CONTEXT_TEMPLATE_VERSION.to_string(),
        };
        let manifest_path = root.join("manifest.json");
        std::fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        let embedding = EmbeddingConfig {
            provider: EmbeddingProviderKind::Local,
            manifest_path: Some(manifest_path),
            model: None,
            model_path: None,
            file: None,
            pooling: None,
            query_prefix: None,
            quantization: None,
            token_source: None,
            device: crate::config::LocalModelDevice::Auto,
        };

        reset_tokenizer_only_artifact_bytes();
        embedding_tokenizer(&embedding)
            .expect("tokenizer-only acquisition must ignore model files");
        let bytes = tokenizer_only_artifact_bytes();

        assert!(bytes
            .get(&ArtifactRole::Tokenizer)
            .is_some_and(|bytes| *bytes > 0));
        for role in [
            ArtifactRole::Config,
            ArtifactRole::SpecialTokensMap,
            ArtifactRole::TokenizerConfig,
        ] {
            assert_eq!(bytes.get(&role).copied(), Some(2));
        }
        assert_eq!(bytes.get(&ArtifactRole::OnnxModel).copied().unwrap_or(0), 0);
        assert_eq!(
            bytes
                .get(&ArtifactRole::OnnxExternalData)
                .copied()
                .unwrap_or(0),
            0
        );
    }

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
    #[derive(Default)]
    struct FailAfterFirstEmbeddingBatch {
        calls: std::sync::atomic::AtomicUsize,
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
                device: crate::config::LocalModelDevice::Auto,
            }),
            reranker: None,
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

    #[cfg(feature = "vector-search")]
    impl EmbeddingProvider for FailAfterFirstEmbeddingBatch {
        fn embed_documents(
            &self,
            texts: &[&str],
        ) -> Result<Vec<EmbeddingVector>, EmbeddingProviderError> {
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if call > 0 {
                return Err(EmbeddingProviderError::structured(
                    "embedding.test_interrupted",
                    "Synthetic embedding interruption.",
                ));
            }
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
    fn generation_durability_failure_preserves_previous_resolvable_publication() {
        let paths = temp_profile_paths("generation-durability-failure");
        let profile = bm25_test_profile(&paths);
        let mut store = Store::open(&paths).unwrap();
        seed_bm25_snapshot(
            &mut store,
            "sync-generation-durability-initial",
            "I_DURABILITY_FAILURE",
        );
        rebuild_bm25_index(&profile, &mut store, &StderrSyncProgress::new(false)).unwrap();
        let previous = store.active_retrieval_publication().unwrap().unwrap();
        let previous_path = store.resolve_active_tantivy_artifact().unwrap().unwrap();
        crate::index::reset_publication_directory_sync_paths();
        crate::index::fail_publication_directory_sync_after(1);

        let error = match rebuild_bm25_index(&profile, &mut store, &StderrSyncProgress::new(false))
        {
            Ok(_) => panic!("directory durability failure must stop before activation"),
            Err(error) => error,
        };

        assert_eq!(error.code, "publication.tantivy_artifact_not_ready");
        assert_eq!(
            store
                .active_retrieval_publication()
                .unwrap()
                .unwrap()
                .publication_id,
            previous.publication_id
        );
        assert_eq!(
            store.resolve_active_tantivy_artifact().unwrap().unwrap(),
            previous_path
        );

        crate::index::reset_publication_directory_sync_paths();
        drop(store);
        let reopened = Store::open(&paths).unwrap();
        assert_eq!(
            reopened
                .active_retrieval_publication()
                .unwrap()
                .unwrap()
                .publication_id,
            previous.publication_id
        );
        assert_eq!(
            reopened.resolve_active_tantivy_artifact().unwrap().unwrap(),
            previous_path
        );

        drop(reopened);
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
    fn chunker_fingerprint_mismatch_blocks_query_encoding_before_bm25_fallback() {
        let fingerprint_seed = EmbeddingFingerprintSeed {
            provider: "local".to_string(),
            model_id: "fixture/model".to_string(),
            model_revision: "fixture-sha".to_string(),
            pooling: PoolingKind::Cls,
            query_prefix: crate::embedding::DEFAULT_QUERY_PREFIX.to_string(),
        };
        let runtime_fingerprint_hash = fingerprint_seed.clone().with_dimension(3).hash();
        let runtime = EmbeddingRuntime {
            tokenizer: Box::new(TestEmbeddingTokenizer),
            chunker_fingerprint: crate::chunking::CHUNKER_FINGERPRINT.to_string(),
            provider: Box::new(PanicQueryEmbeddingProvider),
            model_manifest_hash: "manifest-query-runtime-check".to_string(),
            fingerprint_seed,
            output_dimension: 3,
        };
        let publication = RetrievalPublicationView {
            publication_id: 1,
            source_snapshot_sync_run_id: "sync-query-runtime-check".to_string(),
            source_snapshot_epoch: 1,
            tantivy_generation: 1,
            embedding_generation_id: Some(1),
            model_manifest_hash: Some("manifest-query-runtime-check".to_string()),
            runtime_fingerprint_hash: Some(runtime_fingerprint_hash),
            chunker_fingerprint: Some("wrong-chunker-fingerprint".to_string()),
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
    fn vector_integrity_failure_uses_content_free_bm25_fallback_warning() {
        let private_marker = "PRIVATE_VECTOR_ROW_MARKER_59c2";
        let error = QghError::validation("embedding.generation_corrupt", private_marker);

        let warning = vector_search_failure_warning(&error);

        assert_eq!(warning["code"], "embedding.vector_integrity_failed");
        assert_eq!(warning["severity"], "warn");
        assert!(warning["message"]
            .as_str()
            .unwrap()
            .contains("BM25 results are still returned"));
        assert!(!warning.to_string().contains(private_marker));
    }

    #[test]
    fn embedding_refresh_failure_warning_preserves_code_without_content() {
        let private_marker = "PRIVATE_EMBEDDING_INPUT_MARKER_90";
        let error = QghError::validation("embedding.input_window_exceeded", private_marker)
            .with_details(json!({ "private_input": private_marker }))
            .with_hint(private_marker);

        let warning = embedding_refresh_failure_warning(&error);

        assert_eq!(warning["code"], "embedding.input_window_exceeded");
        assert_eq!(warning["severity"], "warn");
        assert!(warning["message"]
            .as_str()
            .unwrap()
            .contains("BM25 index refresh remains available"));
        assert_eq!(warning.as_object().unwrap().len(), 3);
        assert!(!warning.to_string().contains(private_marker));
    }

    #[test]
    fn missing_qwen_embedding_warning_has_content_free_install_action() {
        let private_marker = "PRIVATE_MODEL_FAILURE_MARKER_91";
        let error = QghError::validation("embedding.model_not_installed", private_marker)
            .with_hint(private_marker);

        let warning = embedding_refresh_failure_warning(&error);

        assert_eq!(
            warning["action"],
            json!({
                "reason": "embedding_model_not_installed",
                "command": "qgh model install qwen3-embedding-0.6b",
                "json_command": "qgh model install qwen3-embedding-0.6b --json"
            })
        );
        assert!(!warning.to_string().contains(private_marker));
    }

    #[test]
    fn prepared_runtime_failure_overrides_complete_stored_embedding_status() {
        let mut coverage = EmbeddingCoverageState {
            active_fingerprint: None,
            generation_active: true,
            active_matches_config: true,
            artifact_corrupt: false,
            total_chunks: 1,
            completed_chunks: 1,
            missing_chunks: 0,
            mismatched_chunks: 0,
            prepared_runtime: PreparedRuntimeAvailability::Missing,
        };

        assert_eq!(coverage.state(), "complete");
        assert_eq!(coverage.status_state(), "missing");
        coverage.prepared_runtime = PreparedRuntimeAvailability::Corrupt;
        assert_eq!(coverage.status_state(), "corrupt");
    }

    #[cfg(feature = "fastembed-provider")]
    #[test]
    fn qwen_query_runtime_snapshot_failure_has_content_free_install_action() {
        let embedding = EmbeddingConfig {
            provider: EmbeddingProviderKind::Local,
            manifest_path: None,
            model: Some(QWEN_EMBEDDING_PRESET_ID.to_string()),
            model_path: None,
            file: None,
            pooling: None,
            query_prefix: None,
            quantization: None,
            token_source: None,
            device: crate::config::LocalModelDevice::Auto,
        };
        let private_marker = "PRIVATE_QWEN_RUNTIME_FAILURE_91";
        let warning = embedding_runtime_failure_warning(
            &embedding,
            &QghError::validation("embedding.qwen_snapshot_invalid", private_marker),
        );

        assert_eq!(warning["code"], "embedding.qwen_snapshot_invalid");
        assert_eq!(
            warning["action"],
            json!({
                "reason": "embedding_model_invalid",
                "command": "qgh model install qwen3-embedding-0.6b",
                "json_command": "qgh model install qwen3-embedding-0.6b --json"
            })
        );
        assert!(!warning.to_string().contains(private_marker));
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
            chunker_fingerprint: crate::chunking::CHUNKER_FINGERPRINT.to_string(),
            provider: Box::new(RecordingEmbeddingProvider::default()),
            model_manifest_hash: model_manifest_hash.to_string(),
            fingerprint_seed,
            output_dimension: 3,
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
        refresh_incremental_chunk_embeddings_with_provider(
            &mut store,
            &provider,
            "manifest-context-sync".to_string(),
            seed,
            crate::chunking::CHUNKER_FINGERPRINT.to_string(),
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
            crate::chunking::CHUNKER_FINGERPRINT,
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
        let configured = configured_embedding_contract_snapshot(embedding);
        let coverage = embedding_coverage_state_for_config(embedding, &store, &configured).unwrap();
        assert_eq!(coverage.state(), "complete");
        assert!(coverage.hybrid_ready());

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn hybrid_lexical_guard_preserves_bm25_head_and_dedupes_semantic_tail() {
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
                source_id: "source-e".to_string(),
                source_updated_at: Some("2026-01-01T00:00:00Z".to_string()),
                score: 7.0,
            },
            index::SearchHit {
                source_id: "source-f".to_string(),
                source_updated_at: Some("2026-01-01T00:00:00Z".to_string()),
                score: 6.0,
            },
            index::SearchHit {
                source_id: "source-a".to_string(),
                source_updated_at: Some("2026-01-01T00:00:00Z".to_string()),
                score: 1.0,
            },
        ];
        let bm25_snapshot = bm25_hits
            .iter()
            .take(5)
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

        let hits = fuse_hybrid_hits(bm25_hits, vector_hits, 6);
        let hybrid_snapshot = hits
            .iter()
            .map(|hit| hit.source_id.clone())
            .collect::<Vec<_>>();

        assert_eq!(
            bm25_snapshot,
            vec!["source-a", "source-b", "source-d", "source-e", "source-f"]
        );
        assert_eq!(&hybrid_snapshot[..5], bm25_snapshot);
        assert_eq!(hybrid_snapshot[5], "source-c");
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
                let expected = fusion::rrf_component(
                    Some(1),
                    LEXICAL_GUARD_V1.rrf_k(),
                    LEXICAL_GUARD_V1.lexical_weight(),
                ) + fusion::rrf_component(
                    Some(2),
                    LEXICAL_GUARD_V1.rrf_k(),
                    LEXICAL_GUARD_V1.dense_weight(),
                );
                assert_eq!(*rrf_rank_score, expected);
                assert_eq!(*final_order_score, 1.0);
            }
            _ => panic!("hybrid sources must expose fused ranking evidence"),
        }
        match &hits[5].ranking {
            Ranking::Hybrid {
                lexical_score,
                vector_distance,
                rrf_rank_score,
                final_order_score,
            } => {
                assert_eq!(*lexical_score, None);
                assert_eq!(*vector_distance, Some(0.01));
                let expected = fusion::rrf_component(
                    Some(1),
                    LEXICAL_GUARD_V1.rrf_k(),
                    LEXICAL_GUARD_V1.dense_weight(),
                );
                assert_eq!(*rrf_rank_score, expected);
                assert_eq!(*final_order_score, 1.0 / 6.0);
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

    #[test]
    fn hybrid_final_order_score_tracks_the_post_resolution_result_rank() {
        let mut hybrid = Ranking::Hybrid {
            lexical_score: Some(1.0),
            vector_distance: Some(0.1),
            rrf_rank_score: 0.03,
            final_order_score: 1.0,
        };
        set_final_order_score(&mut hybrid, 2);
        match hybrid {
            Ranking::Hybrid {
                final_order_score, ..
            } => assert_eq!(final_order_score, 0.5),
            _ => unreachable!(),
        }

        let mut bm25 = Ranking::Bm25(2.0);
        set_final_order_score(&mut bm25, 2);
        assert!(matches!(bm25, Ranking::Bm25(2.0)));
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
        let generation_path = store
            .rebuild_reserved_index_generation(generation, first_snapshot.sources())
            .unwrap();
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
            crate::chunking::CHUNKER_FINGERPRINT,
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
            body: "public synthetic chunk inventory ".repeat(100),
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
        let arctic_fingerprint =
            chunker_fingerprint_for_tokenizer_identity("arctic-tokenizer-contract");
        let gte_fingerprint = chunker_fingerprint_for_tokenizer_identity("gte-tokenizer-contract");
        refresh_embedding_chunks(
            &mut store,
            &ByteEmbeddingTokenizer,
            &arctic_fingerprint,
            &progress,
        )
        .unwrap();
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

        let refreshed = refresh_embedding_chunks(
            &mut store,
            &ByteEmbeddingTokenizer,
            &arctic_fingerprint,
            &progress,
        )
        .unwrap();

        assert_eq!(refreshed.skipped_sources, 0);
        assert!(refreshed.refreshed_chunks > 0);
        let refreshed_chunks = store.chunks_for_source_version(source_version_id).unwrap();
        assert!(refreshed_chunks
            .iter()
            .all(|chunk| chunk.chunker_fingerprint == arctic_fingerprint));
        assert_eq!(
            store.get_issue(source_id).unwrap().unwrap().body,
            raw_body_before
        );
        let refreshed_ids = refreshed_chunks
            .iter()
            .map(|chunk| chunk.chunk_id)
            .collect::<Vec<_>>();

        let second = refresh_embedding_chunks(
            &mut store,
            &ByteEmbeddingTokenizer,
            &arctic_fingerprint,
            &progress,
        )
        .unwrap();

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
        let expected_chunk_count = refreshed_ids.len();
        assert!(expected_chunk_count > 1);
        rusqlite::Connection::open(&paths.db_path)
            .unwrap()
            .execute(
                "DELETE FROM chunks WHERE id = (
                    SELECT max(id) FROM chunks WHERE source_version_id = ?1
                 )",
                rusqlite::params![source_version_id],
            )
            .unwrap();

        let repaired = refresh_embedding_chunks(
            &mut store,
            &ByteEmbeddingTokenizer,
            &arctic_fingerprint,
            &progress,
        )
        .unwrap();

        assert_eq!(repaired.skipped_sources, 0);
        assert_eq!(
            store
                .chunks_for_source_version(source_version_id)
                .unwrap()
                .len(),
            expected_chunk_count
        );
        let repaired_ids = store
            .chunks_for_source_version(source_version_id)
            .unwrap()
            .into_iter()
            .map(|chunk| chunk.chunk_id)
            .collect::<Vec<_>>();
        let stable_after_repair = refresh_embedding_chunks(
            &mut store,
            &ByteEmbeddingTokenizer,
            &arctic_fingerprint,
            &progress,
        )
        .unwrap();
        assert_eq!(stable_after_repair.skipped_sources, 1);
        assert_eq!(
            store
                .chunks_for_source_version(source_version_id)
                .unwrap()
                .into_iter()
                .map(|chunk| chunk.chunk_id)
                .collect::<Vec<_>>(),
            repaired_ids
        );
        store
            .mark_sync_run_completed("sync-stale-chunk-fingerprint")
            .unwrap();
        assert!(store.capture_retrieval_build_snapshot().unwrap().is_some());

        let epoch_before_switch: i64 = rusqlite::Connection::open(&paths.db_path)
            .unwrap()
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM profile_meta
                 WHERE key = 'source_snapshot_epoch'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let switched = refresh_embedding_chunks(
            &mut store,
            &ByteEmbeddingTokenizer,
            &gte_fingerprint,
            &progress,
        )
        .unwrap();
        assert!(switched.refreshed_chunks > 0);
        let switched_chunks = store.chunks_for_source_version(source_version_id).unwrap();
        assert!(switched_chunks
            .iter()
            .all(|chunk| chunk.chunker_fingerprint == gte_fingerprint));
        assert_ne!(
            switched_chunks
                .iter()
                .map(|chunk| chunk.chunk_id)
                .collect::<Vec<_>>(),
            refreshed_ids
        );
        let epoch_after_switch: i64 = rusqlite::Connection::open(&paths.db_path)
            .unwrap()
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM profile_meta
                 WHERE key = 'source_snapshot_epoch'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(epoch_after_switch > epoch_before_switch);
        assert_eq!(
            store.capture_retrieval_build_snapshot().unwrap_err().code,
            "publication.source_snapshot_incomplete"
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
        let mixed = refresh_embedding_chunks(
            &mut store,
            &ByteEmbeddingTokenizer,
            &gte_fingerprint,
            &progress,
        )
        .unwrap();
        assert!(mixed.refreshed_chunks > 0);
        assert!(store
            .chunks_for_source_version(source_version_id)
            .unwrap()
            .iter()
            .all(|chunk| chunk.chunker_fingerprint == gte_fingerprint));

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

        let null_fingerprint = refresh_embedding_chunks(
            &mut store,
            &ByteEmbeddingTokenizer,
            &gte_fingerprint,
            &progress,
        )
        .unwrap();
        assert!(null_fingerprint.refreshed_chunks > 0);
        assert!(store
            .chunks_for_source_version(source_version_id)
            .unwrap()
            .iter()
            .all(|chunk| chunk.chunker_fingerprint == gte_fingerprint));
        assert_eq!(
            store.get_issue(source_id).unwrap().unwrap().body,
            raw_body_before
        );

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(all(feature = "vector-search", feature = "fastembed-provider"))]
    #[test]
    fn qwen_nochange_plan_uses_pinned_contract_without_model_snapshot_access() {
        let paths = temp_profile_paths("qwen-nochange-pinned-contract");
        let mut store = Store::open(&paths).unwrap();
        store.enable_vector().unwrap();
        let source_id = "qgh://github.com/issue/I_QWEN_NOCHANGE_CONTRACT";
        let issue = IssueRecord {
            source_id: source_id.to_string(),
            host: "github.com".to_string(),
            repo: "owner/repo".to_string(),
            node_id: "I_QWEN_NOCHANGE_CONTRACT".to_string(),
            github_id: 907,
            number: 907,
            title: "Qwen no-change contract".to_string(),
            body: "public no-change body".to_string(),
            state: "open".to_string(),
            labels: Vec::new(),
            milestone: None,
            assignees: Vec::new(),
            author: Some("alice".to_string()),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-02T00:00:00Z".to_string(),
            closed_at: None,
            canonical_url: "https://github.com/owner/repo/issues/907".to_string(),
            body_hash: "body-hash-qwen-nochange-contract".to_string(),
            indexed_at: "2026-01-02T00:00:01Z".to_string(),
        };
        store
            .upsert_sources_for_run("sync-qwen-nochange-contract", &[issue], &[], 0, &[])
            .unwrap();
        let source_version_id = store.latest_source_version_id(source_id).unwrap().unwrap();
        let expected_chunker_fingerprint = configured_qwen_chunker_fingerprint();
        store
            .replace_chunks_for_source_version(
                source_id,
                source_version_id,
                &[MarkdownChunk {
                    chunk_index: 0,
                    byte_start: 0,
                    byte_end: 21,
                    token_start: 0,
                    token_end: 3,
                    token_count: 3,
                    body: "public no-change body".to_string(),
                    chunker_version: crate::chunking::CHUNKER_VERSION.to_string(),
                    chunker_fingerprint: expected_chunker_fingerprint.clone(),
                    heading_path: Vec::new(),
                }],
            )
            .unwrap();
        let embedding = EmbeddingConfig {
            provider: EmbeddingProviderKind::Local,
            manifest_path: None,
            model: Some(QWEN_EMBEDDING_PRESET_ID.to_string()),
            model_path: None,
            file: None,
            pooling: None,
            query_prefix: None,
            quantization: None,
            token_source: None,
            device: crate::config::LocalModelDevice::Auto,
        };

        let contract = qwen_embedding_generation_contract_local_only(&embedding).unwrap();
        let spec = qwen_model_spec(QWEN_EMBEDDING_PRESET_ID).unwrap();
        assert_eq!(
            contract.model_manifest_hash,
            qwen_model_manifest_hash(&spec)
        );
        assert_eq!(contract.chunker_fingerprint, expected_chunker_fingerprint);
        let stats = verified_embedding_chunk_nochange(
            &store,
            &contract.chunker_fingerprint,
            &StderrSyncProgress::new(false),
        )
        .unwrap()
        .unwrap();
        assert_eq!(stats.refreshed_chunks, 0);
        assert_eq!(stats.skipped_sources, 1);

        rusqlite::Connection::open(&paths.db_path)
            .unwrap()
            .execute(
                "UPDATE chunks SET body = 'public changed body' WHERE source_version_id = ?1",
                rusqlite::params![source_version_id],
            )
            .unwrap();
        assert!(verified_embedding_chunk_nochange(
            &store,
            &contract.chunker_fingerprint,
            &StderrSyncProgress::new(false),
        )
        .unwrap()
        .is_none());

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
        let error = refresh_incremental_chunk_embeddings_with_provider(
            &mut store,
            &MockEmbeddingProvider,
            "manifest-sync-embed".to_string(),
            seed.clone(),
            chunker_fingerprint_for_tokenizer_identity("different-tokenizer-contract"),
        )
        .unwrap_err();
        assert_eq!(error.code, "embedding.generation_invalid_spec");

        let provider = RecordingEmbeddingProvider::default();
        let embedded = refresh_incremental_chunk_embeddings_with_provider(
            &mut store,
            &provider,
            "manifest-sync-embed".to_string(),
            seed.clone(),
            crate::chunking::CHUNKER_FINGERPRINT.to_string(),
        )
        .unwrap();
        assert_eq!(embedded, 2);
        assert_eq!(provider.documents.lock().unwrap().len(), 2);
        assert_eq!(
            store
                .latest_embedding_generation_state()
                .unwrap()
                .as_deref(),
            Some("ready")
        );

        provider.documents.lock().unwrap().clear();
        let skipped = refresh_incremental_chunk_embeddings_with_provider(
            &mut store,
            &provider,
            "manifest-sync-embed".to_string(),
            seed.clone(),
            crate::chunking::CHUNKER_FINGERPRINT.to_string(),
        )
        .unwrap();
        assert_eq!(skipped, 0);
        assert!(provider.documents.lock().unwrap().is_empty());

        let snapshot = store.capture_retrieval_build_snapshot().unwrap().unwrap();
        let contract = EmbeddingGenerationContract {
            model_manifest_hash: "manifest-sync-embed".to_string(),
            fingerprint_seed: seed,
            chunker_fingerprint: crate::chunking::CHUNKER_FINGERPRINT.to_string(),
            output_dimension: 3,
        };
        let runtime_loads = std::cell::Cell::new(0usize);
        let lazy_skipped = refresh_incremental_chunk_embeddings_with_runtime_loader(
            &mut store,
            &contract,
            &snapshot,
            &StderrSyncProgress::new(false),
            || {
                runtime_loads.set(runtime_loads.get() + 1);
                Err(QghError::validation(
                    "embedding.test_runtime_loaded",
                    "No-change sync must not load the embedding runtime.",
                ))
            },
        )
        .unwrap();
        assert_eq!(lazy_skipped.0, 0);
        assert_eq!(runtime_loads.get(), 0);

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn interrupted_embedding_batches_resume_after_a_later_noop_sync() {
        let paths = temp_profile_paths("sync-incremental-resume-across-run");
        let mut store = Store::open(&paths).unwrap();
        store.enable_vector().unwrap();
        let source_id = "qgh://github.com/issue/I_SYNC_EMBED_RESUME";
        let issue = IssueRecord {
            source_id: source_id.to_string(),
            host: "github.com".to_string(),
            repo: "owner/repo".to_string(),
            node_id: "I_SYNC_EMBED_RESUME".to_string(),
            github_id: 405,
            number: 11,
            title: "Sync embed resume".to_string(),
            body: "public synthetic embedding resume corpus".to_string(),
            state: "open".to_string(),
            labels: Vec::new(),
            milestone: None,
            assignees: Vec::new(),
            author: Some("alice".to_string()),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-02T00:00:00Z".to_string(),
            closed_at: None,
            canonical_url: "https://github.com/owner/repo/issues/11".to_string(),
            body_hash: "body-hash-sync-embed-resume".to_string(),
            indexed_at: "2026-01-02T00:00:01Z".to_string(),
        };
        store
            .upsert_sources_for_run("sync-embed-resume-first", &[issue], &[], 0, &[])
            .unwrap();
        let source_version_id = store.latest_source_version_id(source_id).unwrap().unwrap();
        let chunks = (0..33)
            .map(|index| MarkdownChunk {
                chunk_index: index,
                byte_start: index,
                byte_end: index + 1,
                token_start: index,
                token_end: index + 1,
                token_count: 1,
                body: format!("public synthetic chunk {index}"),
                chunker_version: crate::chunking::CHUNKER_VERSION.to_string(),
                chunker_fingerprint: crate::chunking::CHUNKER_FINGERPRINT.to_string(),
                heading_path: Vec::new(),
            })
            .collect::<Vec<_>>();
        store
            .replace_chunks_for_source_version(source_id, source_version_id, &chunks)
            .unwrap();
        store
            .mark_sync_run_completed("sync-embed-resume-first")
            .unwrap();
        let first_snapshot = store.capture_retrieval_build_snapshot().unwrap().unwrap();
        let contract = EmbeddingGenerationContract {
            model_manifest_hash: "manifest-sync-embed-resume".to_string(),
            fingerprint_seed: EmbeddingFingerprintSeed {
                provider: "local".to_string(),
                model_id: "fixture/resume-model".to_string(),
                model_revision: "fixture-resume-sha".to_string(),
                pooling: PoolingKind::Cls,
                query_prefix: crate::embedding::DEFAULT_QUERY_PREFIX.to_string(),
            },
            chunker_fingerprint: crate::chunking::CHUNKER_FINGERPRINT.to_string(),
            output_dimension: 3,
        };
        let interrupted = FailAfterFirstEmbeddingBatch::default();

        let error = refresh_incremental_chunk_embeddings_with_provider_and_contract(
            &mut store,
            &interrupted,
            &contract,
            &first_snapshot,
            &StderrSyncProgress::new(false),
        )
        .unwrap_err();

        assert_eq!(error.code, "embedding.test_interrupted");
        assert_eq!(
            interrupted.calls.load(std::sync::atomic::Ordering::SeqCst),
            2
        );
        let partial_generation: (i64, i64) = rusqlite::Connection::open(&paths.db_path)
            .unwrap()
            .query_row(
                "SELECT id, completed_chunks FROM embedding_generations
                 WHERE state = 'building' ORDER BY id DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(partial_generation.1, 32);

        store
            .upsert_sources_for_run("sync-embed-resume-noop", &[], &[], 0, &[])
            .unwrap();
        store
            .mark_sync_run_completed("sync-embed-resume-noop")
            .unwrap();
        let second_snapshot = store.capture_retrieval_build_snapshot().unwrap().unwrap();
        let provider = RecordingEmbeddingProvider::default();
        let resumed = refresh_incremental_chunk_embeddings_with_provider_and_contract(
            &mut store,
            &provider,
            &contract,
            &second_snapshot,
            &StderrSyncProgress::new(false),
        )
        .unwrap();

        assert_eq!(resumed.0, 1);
        assert_eq!(provider.documents.lock().unwrap().len(), 1);
        assert_ne!(resumed.1, Some(partial_generation.0));
        assert_eq!(
            store
                .embedding_generation_state(resumed.1.unwrap())
                .unwrap(),
            "ready"
        );

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
                let mut chunks = vec![
                    test_chunk("far allowed chunk".to_string()),
                    test_chunk("best allowed chunk".to_string()),
                ];
                chunks[1].chunk_index = 1;
                chunks[1].token_start = 1;
                chunks[1].token_end = 2;
                chunks[1].byte_start = chunks[0].byte_end;
                chunks[1].byte_end = chunks[1].byte_start + chunks[1].body.len();
                chunks
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
            reranker: None,
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
