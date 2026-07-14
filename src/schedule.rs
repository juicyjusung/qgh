use crate::cli::{ScheduleArgs, ScheduleCommand};
use crate::commands::{self, LocalReadOutcome, DEFAULT_SYNC_MAX_AGE_SECONDS};
use crate::config::{load_profile, Profile, TokenSource};
use crate::error::QghError;
use crate::freshness;
use crate::lease::{FileLease, LeaseAvailability};
use crate::model::{BackoffView, CommandAction, RateBudgetObservation};
use crate::paths::{ensure_private_dir, schedule_hosts_dir, set_private_file};
use crate::rate_budget;
use crate::schedule_lifecycle;
use crate::store::Store;
use crate::time::now_run_id_suffix;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

const MAX_REMOTE_ATTEMPTS_PER_PASS: usize = 8;
const RESERVE_PERCENT: i64 = 20;
const HOST_BUDGET_GUARD_MAX_SECONDS: i64 = 24 * 60 * 60;
const HOST_BUDGET_GUARD_SCHEMA_VERSION: &str = "qgh.schedule-budget-guard.v1";

pub(crate) async fn execute(args: &ScheduleArgs) -> Result<LocalReadOutcome, QghError> {
    match &args.command {
        ScheduleCommand::Run(args) => run_foreground(&args.profile_ids, args.manager_invoked).await,
        ScheduleCommand::Start(args) => {
            schedule_lifecycle::start(&args.profile_ids, &args.interval)
        }
        ScheduleCommand::Status(_) => schedule_lifecycle::status(),
        ScheduleCommand::Stop(_) => schedule_lifecycle::stop(),
    }
}

#[derive(Debug)]
struct PlannedProfile {
    profile: Profile,
    state: PlannedState,
    active_backoff: bool,
    rate_budget: Vec<RateBudgetObservation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlannedState {
    BootstrapRequired,
    Fresh,
    Cooldown,
    SyncBusy,
    Eligible,
}

#[derive(Debug, Clone)]
struct HostBudget {
    admission: Option<RateBudgetObservation>,
    evidence: Vec<RateBudgetObservation>,
}

impl HostBudget {
    fn state(&self) -> &'static str {
        if self.admission.is_some() {
            "fresh"
        } else {
            "unknown"
        }
    }

    fn snapshot(&self) -> Value {
        if self.evidence.is_empty() {
            Value::Null
        } else {
            rate_budget::block(&self.evidence)
        }
    }

    fn is_unknown(&self) -> bool {
        self.admission.is_none()
    }

    fn revalidated(mut self) -> Self {
        if self
            .admission
            .as_ref()
            .is_some_and(|observation| !rate_budget::is_fresh_core(observation))
        {
            self.admission = None;
        }
        self
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct HostCursorState {
    schema_version: String,
    cursor_profile_id: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct HostOrderCursorState {
    schema_version: String,
    cursor_host_key: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct HostBudgetGuardState {
    schema_version: String,
    guarded_until: String,
}

async fn run_foreground(
    profile_ids: &[String],
    manager_invoked: bool,
) -> Result<LocalReadOutcome, QghError> {
    validate_profile_ids(profile_ids)?;
    let mut plans = Vec::with_capacity(profile_ids.len());
    for profile_id in profile_ids {
        let profile = load_profile(profile_id)?;
        if manager_invoked && !matches!(profile.token_source, TokenSource::GithubCli) {
            return Err(managed_credentials_unsupported(profile_id));
        }
        plans.push(plan_profile(profile)?);
    }

    let mut groups = BTreeMap::<String, Vec<usize>>::new();
    for (index, plan) in plans.iter().enumerate() {
        groups
            .entry(plan.profile.host.to_ascii_lowercase())
            .or_default()
            .push(index);
    }

    let mut results = HashMap::<String, Value>::new();
    for plan in &plans {
        match plan.state {
            PlannedState::BootstrapRequired => {
                results.insert(
                    plan.profile.id.clone(),
                    profile_result(
                        &plan.profile,
                        false,
                        "skipped",
                        "bootstrap_required",
                        None,
                        Some(CommandAction::new(
                            "bootstrap_required",
                            format!("qgh sync --all --profile {}", plan.profile.id),
                        )),
                        Value::Null,
                    ),
                );
            }
            PlannedState::Fresh => {
                results.insert(
                    plan.profile.id.clone(),
                    profile_result(
                        &plan.profile,
                        false,
                        "skipped",
                        "fresh",
                        None,
                        None,
                        rate_budget::block(&plan.rate_budget),
                    ),
                );
            }
            PlannedState::Cooldown => {
                results.insert(
                    plan.profile.id.clone(),
                    profile_result(
                        &plan.profile,
                        false,
                        "deferred",
                        "host_cooldown",
                        None,
                        None,
                        rate_budget::block(&plan.rate_budget),
                    ),
                );
            }
            PlannedState::SyncBusy | PlannedState::Eligible => {}
        }
    }

    let host_order_state_path = host_order_state_path()?;
    let host_order_cursor_before = read_host_order_cursor(&host_order_state_path)?;
    let hosts = groups.keys().cloned().collect::<Vec<_>>();
    let ordered_hosts = rotate_hosts_after_cursor(&hosts, host_order_cursor_before.as_deref());
    let mut host_reports = Vec::with_capacity(groups.len());
    let mut remote_attempts = 0usize;
    for host in ordered_hosts {
        let indexes = groups
            .remove(&host)
            .expect("ordered host must come from grouped plans");
        let state_path = host_state_path(&host)?;
        let budget_guard_path = host_budget_guard_path(&host)?;
        let has_eligible_profiles = indexes.iter().any(|index| {
            matches!(
                plans[*index].state,
                PlannedState::Eligible | PlannedState::SyncBusy
            )
        });
        let has_selected_backoff = indexes.iter().any(|index| plans[*index].active_backoff);
        let host_profiles = indexes
            .iter()
            .map(|index| plans[*index].profile.clone())
            .collect::<Vec<_>>();
        let needs_host_lease = has_eligible_profiles || has_selected_backoff;
        let host_lock_path = state_path.with_extension("lock");
        let host_lease = if needs_host_lease {
            FileLease::try_acquire_schedule_host(&host_lock_path)?
        } else {
            None
        };
        let active_budget_guard = if host_lease.is_some() {
            read_active_host_budget_guard(&budget_guard_path, Utc::now())?
        } else {
            None
        };
        let selected_backoff_deadline = if host_lease.is_some() {
            max_active_host_backoff_deadline(&host_profiles, Utc::now())?
        } else {
            None
        };
        // A process that pauses between planning and locking must rotate from
        // the cursor published by the previous lock owner, not a stale read.
        let cursor_before = read_cursor(&state_path)?;
        let mut cursor_after = cursor_before.clone();
        let ordered = rotate_after_cursor(&indexes, &plans, cursor_before.as_deref());
        let eligible_indexes = ordered
            .iter()
            .copied()
            .filter(|index| {
                matches!(
                    plans[*index].state,
                    PlannedState::Eligible | PlannedState::SyncBusy
                )
            })
            .collect::<Vec<_>>();
        let mut budget = derive_host_budget(&plans, &indexes);
        let initial_budget = budget.clone();
        let initial_budget_unknown = initial_budget.is_unknown();
        let mut host_budget_unknown = initial_budget_unknown;
        let mut host_attempts = 0usize;
        let mut host_consumed_remote = false;
        let mut host_cooldown = active_budget_guard.is_some()
            || has_selected_backoff
            || selected_backoff_deadline.is_some();
        let mut host_guard_reset = later_host_guard_reset(
            max_fresh_core_reset(&budget.evidence, Utc::now()),
            later_host_guard_reset(active_budget_guard, selected_backoff_deadline),
        );
        if host_lease.is_some() && selected_backoff_deadline.is_some() {
            write_host_budget_guard(
                &budget_guard_path,
                host_budget_guard_deadline(Utc::now(), host_guard_reset.as_ref()),
            )?;
        }

        if has_eligible_profiles && host_lease.is_none() {
            for index in eligible_indexes {
                let plan = &plans[index];
                results.insert(
                    plan.profile.id.clone(),
                    profile_result(
                        &plan.profile,
                        false,
                        "deferred",
                        "host_busy",
                        None,
                        None,
                        budget.snapshot(),
                    ),
                );
            }
        } else {
            for index in eligible_indexes {
                let plan = &plans[index];
                budget = budget.revalidated();
                if results.contains_key(&plan.profile.id) {
                    continue;
                }
                if host_cooldown {
                    results.insert(
                        plan.profile.id.clone(),
                        profile_result(
                            &plan.profile,
                            false,
                            "deferred",
                            "host_cooldown",
                            None,
                            None,
                            budget.snapshot(),
                        ),
                    );
                    continue;
                }
                if initial_budget_unknown && host_attempts >= 1 {
                    results.insert(
                        plan.profile.id.clone(),
                        profile_result(
                            &plan.profile,
                            false,
                            "deferred",
                            "unknown_budget_limit",
                            None,
                            None,
                            budget.snapshot(),
                        ),
                    );
                    continue;
                }
                if remote_attempts >= MAX_REMOTE_ATTEMPTS_PER_PASS {
                    results.insert(
                        plan.profile.id.clone(),
                        profile_result(
                            &plan.profile,
                            false,
                            "deferred",
                            "pass_limit",
                            None,
                            None,
                            budget.snapshot(),
                        ),
                    );
                    continue;
                }
                if plan.state == PlannedState::SyncBusy {
                    cursor_after = Some(plan.profile.id.clone());
                    write_cursor(&state_path, &plan.profile.id)?;
                    results.insert(
                        plan.profile.id.clone(),
                        profile_result(
                            &plan.profile,
                            false,
                            "deferred",
                            "sync_busy",
                            Some("sync.busy"),
                            None,
                            budget.snapshot(),
                        ),
                    );
                    continue;
                }

                write_host_budget_guard(
                    &budget_guard_path,
                    host_budget_guard_deadline(Utc::now(), host_guard_reset.as_ref()),
                )?;
                let execution = commands::sync_scheduled(
                    plan.profile.clone(),
                    &host_profiles,
                    host_attempts,
                    host_budget_unknown,
                    manager_invoked,
                )
                .await;
                if execution.remote_started {
                    host_attempts += 1;
                    remote_attempts += 1;
                    host_consumed_remote = true;
                }
                let advance_profile_cursor = execution.remote_started || execution.result.is_err();
                if advance_profile_cursor {
                    cursor_after = Some(plan.profile.id.clone());
                    write_cursor(&state_path, &plan.profile.id)?;
                }
                let latest_budget = load_rate_budget(&plan.profile)?;
                let active_backoff_deadline =
                    max_active_host_backoff_deadline(&host_profiles, Utc::now())?;
                host_guard_reset = later_host_guard_reset(
                    later_host_guard_reset(
                        host_guard_reset,
                        max_fresh_core_reset(&latest_budget, Utc::now()),
                    ),
                    active_backoff_deadline,
                );
                budget = update_budget_after_attempt(budget, &latest_budget);
                host_budget_unknown = budget.is_unknown();
                if execution.budget_uncertain {
                    host_budget_unknown = true;
                    budget.admission = None;
                }
                let completed_with_confirmed_headroom = matches!(
                    &execution.result,
                    Ok(commands::ScheduledSyncResult::Completed(_))
                ) && !execution.budget_uncertain
                    && fresh_core_has_headroom(&latest_budget);
                let may_clear_guard = active_backoff_deadline.is_none()
                    && (!execution.remote_started || completed_with_confirmed_headroom);
                if may_clear_guard {
                    remove_host_budget_guard(&budget_guard_path)?;
                } else {
                    write_host_budget_guard(
                        &budget_guard_path,
                        host_budget_guard_deadline(Utc::now(), host_guard_reset.as_ref()),
                    )?;
                    host_cooldown = true;
                }
                match execution.result {
                    Ok(commands::ScheduledSyncResult::Deferred {
                        reason,
                        rate_budget: latest_evidence,
                    }) => {
                        budget = host_budget_from_observations(latest_evidence);
                        if reason == commands::ScheduleSyncDeferral::UnknownBudgetLimit {
                            host_budget_unknown = true;
                            budget.admission = None;
                        }
                        let reason = reason.reason();
                        if reason == "host_cooldown" {
                            host_cooldown = true;
                        }
                        results.insert(
                            plan.profile.id.clone(),
                            profile_result(
                                &plan.profile,
                                execution.remote_started,
                                "deferred",
                                reason,
                                None,
                                None,
                                budget.snapshot(),
                            ),
                        );
                    }
                    Ok(commands::ScheduledSyncResult::Completed(outcome)) => {
                        let sync_state = outcome
                            .data
                            .get("sync_state")
                            .and_then(Value::as_str)
                            .unwrap_or("ok");
                        let (profile_outcome, reason) = if sync_state == "skipped_fresh" {
                            ("skipped", "fresh")
                        } else {
                            ("completed", "synced")
                        };
                        results.insert(
                            plan.profile.id.clone(),
                            profile_result(
                                &plan.profile,
                                execution.remote_started,
                                profile_outcome,
                                reason,
                                None,
                                None,
                                budget.snapshot(),
                            ),
                        );
                    }
                    Err(error) if error.code == "sync.busy" => {
                        results.insert(
                            plan.profile.id.clone(),
                            profile_result(
                                &plan.profile,
                                false,
                                "deferred",
                                "sync_busy",
                                Some("sync.busy"),
                                None,
                                budget.snapshot(),
                            ),
                        );
                    }
                    Err(error) if error.code == "sync.backoff" => {
                        host_cooldown = true;
                        results.insert(
                            plan.profile.id.clone(),
                            profile_result(
                                &plan.profile,
                                execution.remote_started,
                                "deferred",
                                "host_cooldown",
                                Some("sync.backoff"),
                                None,
                                budget.snapshot(),
                            ),
                        );
                    }
                    Err(error) if error.code == "auth.token_unavailable" => {
                        results.insert(
                            plan.profile.id.clone(),
                            profile_result(
                                &plan.profile,
                                execution.remote_started,
                                "failed",
                                "sync_failed",
                                Some("auth.token_unavailable"),
                                None,
                                budget.snapshot(),
                            ),
                        );
                    }
                    Err(error) => {
                        results.insert(
                            plan.profile.id.clone(),
                            profile_result(
                                &plan.profile,
                                execution.remote_started,
                                "failed",
                                "sync_failed",
                                Some(&error.code),
                                None,
                                budget.snapshot(),
                            ),
                        );
                    }
                }
            }
        }
        if host_consumed_remote {
            let key = host_key(&host);
            write_host_order_cursor(&host_order_state_path, &key)?;
        }
        drop(host_lease);
        host_reports.push(json!({
            "host": host,
            "profile_ids": indexes.iter().map(|index| plans[*index].profile.id.clone()).collect::<Vec<_>>(),
            "cursor_before": cursor_before,
            "cursor_after": cursor_after,
            "budget_state": initial_budget.state(),
            "budget_snapshot": initial_budget.snapshot()
        }));
    }

    let profile_results = profile_ids
        .iter()
        .map(|profile_id| {
            results.remove(profile_id).ok_or_else(|| {
                QghError::new(
                    "internal.failure",
                    "Schedule did not produce a result for an explicit profile.",
                    6,
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let summary = summarize(&profile_results);
    let failed = summary["failed"].as_u64().unwrap_or(0);
    let warnings = if failed > 0 {
        vec![json!({
            "code": "schedule.partial_failure",
            "severity": "warn",
            "message": "One or more explicit profiles failed; remaining profiles continued."
        })]
    } else {
        Vec::new()
    };
    Ok(LocalReadOutcome {
        data: json!({
            "operation": "run",
            "pass_state": if failed > 0 { "completed_with_failures" } else { "completed" },
            "policy": {
                "explicit_profiles": true,
                "host_max_in_flight": 1,
                "unknown_budget_max_attempts": 1,
                "reserve_percent": RESERVE_PERCENT,
                "max_attempts_per_profile": 1,
                "max_remote_attempts_per_pass": MAX_REMOTE_ATTEMPTS_PER_PASS,
                "rate_observation_fresh_seconds": rate_budget::STALE_AFTER_SECONDS
            },
            "summary": summary,
            "hosts": host_reports,
            "profiles": profile_results
        }),
        warnings,
    })
}

fn plan_profile(profile: Profile) -> Result<PlannedProfile, QghError> {
    if !profile.paths.db_path.exists() {
        return Ok(PlannedProfile {
            profile,
            state: PlannedState::BootstrapRequired,
            active_backoff: false,
            rate_budget: Vec::new(),
        });
    }
    let store = Store::open_for_read(&profile.paths)?;
    let status = store.status()?;
    let rate_budget = store.rate_budget_observations(&profile.host)?;
    let active_backoff = status.backoff.as_ref().is_some_and(backoff_is_active);
    let state = match status.last_sync_at.as_deref() {
        None => PlannedState::BootstrapRequired,
        Some(last_sync_at) => {
            let max_age = profile
                .sync_max_age_seconds
                .unwrap_or(DEFAULT_SYNC_MAX_AGE_SECONDS);
            let fresh = freshness::snapshot_age_seconds(last_sync_at)? <= max_age
                && matches!(store.resolve_active_tantivy_artifact(), Ok(Some(_)));
            if fresh {
                PlannedState::Fresh
            } else if active_backoff {
                PlannedState::Cooldown
            } else if FileLease::probe_profile_sync(&profile.paths)? == LeaseAvailability::Busy {
                PlannedState::SyncBusy
            } else {
                PlannedState::Eligible
            }
        }
    };
    Ok(PlannedProfile {
        profile,
        state,
        active_backoff,
        rate_budget,
    })
}

fn parsed_backoff_deadline(backoff: &BackoffView) -> Option<DateTime<Utc>> {
    let reset_at = backoff
        .reset_at
        .as_deref()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc));
    let retry_after_seconds = backoff
        .retry_after_seconds
        .clamp(0, HOST_BUDGET_GUARD_MAX_SECONDS);
    let retry_at = DateTime::parse_from_rfc3339(&backoff.observed_at)
        .ok()
        .map(|value| value.with_timezone(&Utc))
        .and_then(|observed_at| {
            Duration::try_seconds(retry_after_seconds)
                .and_then(|duration| observed_at.checked_add_signed(duration))
        });
    reset_at.into_iter().chain(retry_at).max()
}

fn backoff_is_active(backoff: &BackoffView) -> bool {
    parsed_backoff_deadline(backoff).is_none_or(|value| value > Utc::now())
}

fn active_backoff_guard_deadline(
    backoff: &BackoffView,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    match parsed_backoff_deadline(backoff) {
        Some(deadline) if deadline > now => Some(deadline),
        Some(_) => None,
        None => Some(now + Duration::seconds(HOST_BUDGET_GUARD_MAX_SECONDS)),
    }
}

fn derive_host_budget(plans: &[PlannedProfile], indexes: &[usize]) -> HostBudget {
    let mut candidate = None::<RateBudgetObservation>;
    let mut evidence = Vec::new();
    let mut complete = true;
    for index in indexes {
        let observations = &plans[*index].rate_budget;
        let profile_core = fresh_core_observation(observations);
        if profile_core.is_none() {
            complete = false;
        }
        evidence.extend(observations.iter().cloned());
        if let Some(observation) = profile_core {
            candidate = Some(match candidate {
                None => observation,
                Some(current) => conservative_observation(current, observation),
            });
        }
    }
    HostBudget {
        admission: complete.then_some(candidate).flatten(),
        evidence,
    }
}

fn update_budget_after_attempt(
    current: HostBudget,
    observations: &[RateBudgetObservation],
) -> HostBudget {
    let latest = fresh_core_observation(observations);
    if latest.is_none() {
        return HostBudget {
            admission: None,
            evidence: observations.to_vec(),
        };
    }
    let admission = match (current.revalidated().admission, latest) {
        (_, None) => None,
        (None, Some(observation)) => Some(observation),
        (Some(current), Some(latest)) => Some(conservative_observation(current, latest)),
    };
    HostBudget {
        admission,
        evidence: observations.to_vec(),
    }
}

fn fresh_core_observation(observations: &[RateBudgetObservation]) -> Option<RateBudgetObservation> {
    observations
        .iter()
        .filter(|observation| rate_budget::is_fresh_core(observation))
        .cloned()
        .reduce(conservative_observation)
}

fn host_budget_from_observations(observations: Vec<RateBudgetObservation>) -> HostBudget {
    let admission = fresh_core_observation(&observations);
    HostBudget {
        admission,
        evidence: observations,
    }
}

fn conservative_observation(
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

fn load_rate_budget(profile: &Profile) -> Result<Vec<RateBudgetObservation>, QghError> {
    let store = Store::open_for_read(&profile.paths)?;
    store.rate_budget_observations(&profile.host)
}

fn max_active_host_backoff_deadline(
    profiles: &[Profile],
    now: DateTime<Utc>,
) -> Result<Option<DateTime<Utc>>, QghError> {
    let mut deadline = None;
    for profile in profiles {
        if !profile.paths.db_path.exists() {
            continue;
        }
        let store = Store::open_for_read(&profile.paths)?;
        let status = store.status()?;
        let observed = status
            .backoff
            .as_ref()
            .and_then(|backoff| active_backoff_guard_deadline(backoff, now));
        deadline = later_host_guard_reset(deadline, observed);
    }
    Ok(deadline)
}

fn rotate_after_cursor(
    indexes: &[usize],
    plans: &[PlannedProfile],
    cursor: Option<&str>,
) -> Vec<usize> {
    let Some(position) = cursor.and_then(|cursor| {
        indexes
            .iter()
            .position(|index| plans[*index].profile.id == cursor)
    }) else {
        return indexes.to_vec();
    };
    indexes[position + 1..]
        .iter()
        .chain(indexes[..=position].iter())
        .copied()
        .collect()
}

fn profile_result(
    profile: &Profile,
    started: bool,
    outcome: &str,
    reason: &str,
    error_code: Option<&str>,
    next_action: Option<CommandAction>,
    budget_snapshot: Value,
) -> Value {
    json!({
        "profile_id": profile.id,
        "host": profile.host.to_ascii_lowercase(),
        "planned": true,
        "started": started,
        "outcome": outcome,
        "reason": reason,
        "error_code": error_code,
        "next_action": next_action,
        "budget_snapshot": budget_snapshot
    })
}

fn summarize(results: &[Value]) -> Value {
    let count = |outcome: &str| {
        results
            .iter()
            .filter(|result| result["outcome"] == outcome)
            .count()
    };
    json!({
        "requested": results.len(),
        "planned": results.len(),
        "started": results.iter().filter(|result| result["started"] == true).count(),
        "skipped": count("skipped"),
        "deferred": count("deferred"),
        "completed": count("completed"),
        "failed": count("failed")
    })
}

fn validate_profile_ids(profile_ids: &[String]) -> Result<(), QghError> {
    let mut seen = BTreeSet::new();
    for profile_id in profile_ids {
        if !seen.insert(profile_id) {
            return Err(QghError::validation(
                "validation.duplicate_profile",
                "Schedule profile ids must be unique.",
            )
            .with_details(json!({ "profile_id": profile_id })));
        }
    }
    Ok(())
}

fn managed_credentials_unsupported(profile_id: &str) -> QghError {
    QghError::new(
        "schedule.credentials_unsupported",
        "Scheduled profiles must use the github_cli token source.",
        2,
    )
    .with_details(json!({
        "profile_id": profile_id,
        "supported_token_source": "github_cli"
    }))
    .with_hint("Update the profile to use GitHub CLI credentials available to the user manager.")
}

fn host_key(host: &str) -> String {
    Sha256::digest(host.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn host_state_path(host: &str) -> Result<PathBuf, QghError> {
    Ok(schedule_hosts_dir()?.join(format!("{}.json", host_key(host))))
}

fn host_budget_guard_path(host: &str) -> Result<PathBuf, QghError> {
    Ok(schedule_hosts_dir()?.join(format!("{}.guard.json", host_key(host))))
}

fn max_fresh_core_reset(
    observations: &[RateBudgetObservation],
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    observations
        .iter()
        .filter(|observation| rate_budget::is_fresh_core(observation))
        .filter_map(|observation| observation.reset_at.as_deref())
        .filter_map(|reset_at| DateTime::parse_from_rfc3339(reset_at).ok())
        .map(|reset_at| reset_at.with_timezone(&Utc))
        .filter(|reset_at| *reset_at > now)
        .max()
}

fn later_host_guard_reset(
    current: Option<DateTime<Utc>>,
    observed: Option<DateTime<Utc>>,
) -> Option<DateTime<Utc>> {
    current.into_iter().chain(observed).max()
}

fn host_budget_guard_deadline(
    now: DateTime<Utc>,
    known_reset: Option<&DateTime<Utc>>,
) -> DateTime<Utc> {
    let fallback = now + Duration::seconds(rate_budget::STALE_AFTER_SECONDS);
    let cap = now + Duration::seconds(HOST_BUDGET_GUARD_MAX_SECONDS);
    known_reset
        .filter(|reset_at| **reset_at > now)
        .cloned()
        .unwrap_or(fallback)
        .min(cap)
}

fn fresh_core_has_headroom(observations: &[RateBudgetObservation]) -> bool {
    fresh_core_observation(observations).is_some_and(|observation| {
        rate_budget::scheduled_additional_requests(&observation).is_some_and(|value| value > 0)
    })
}

fn read_active_host_budget_guard(
    path: &Path,
    now: DateTime<Utc>,
) -> Result<Option<DateTime<Utc>>, QghError> {
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(path)
        .map_err(|_| QghError::storage("Could not read schedule host budget guard."))?;
    let state: HostBudgetGuardState = serde_json::from_slice(&bytes)
        .map_err(|_| QghError::storage("Schedule host budget guard is invalid."))?;
    if state.schema_version != HOST_BUDGET_GUARD_SCHEMA_VERSION {
        return Err(QghError::storage(
            "Schedule host budget guard schema is unsupported.",
        ));
    }
    let guarded_until = DateTime::parse_from_rfc3339(&state.guarded_until)
        .map_err(|_| QghError::storage("Schedule host budget guard timestamp is invalid."))?
        .with_timezone(&Utc);
    if guarded_until > now {
        return Ok(Some(guarded_until));
    }
    remove_host_budget_guard(path)?;
    Ok(None)
}

fn write_host_budget_guard(path: &Path, guarded_until: DateTime<Utc>) -> Result<(), QghError> {
    let Some(parent) = path.parent() else {
        return Err(QghError::storage(
            "Schedule host budget guard path is invalid.",
        ));
    };
    ensure_private_dir(parent)?;
    let temporary = parent.join(format!(
        ".schedule-budget-guard-{}.tmp",
        now_run_id_suffix()
    ));
    let state = HostBudgetGuardState {
        schema_version: HOST_BUDGET_GUARD_SCHEMA_VERSION.to_string(),
        guarded_until: guarded_until.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    };
    let bytes = serde_json::to_vec(&state)
        .map_err(|_| QghError::storage("Could not serialize schedule host budget guard."))?;
    let write_result = (|| -> Result<(), QghError> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .map_err(|_| QghError::storage("Could not create schedule host budget guard."))?;
        set_private_file(&temporary)?;
        file.write_all(&bytes)
            .map_err(|_| QghError::storage("Could not write schedule host budget guard."))?;
        file.sync_all()
            .map_err(|_| QghError::storage("Could not sync schedule host budget guard."))?;
        fs::rename(&temporary, path)
            .map_err(|_| QghError::storage("Could not publish schedule host budget guard."))?;
        set_private_file(path)?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| {
                QghError::storage("Could not sync schedule host budget guard directory.")
            })?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    write_result
}

fn remove_host_budget_guard(path: &Path) -> Result<(), QghError> {
    if !path.exists() {
        return Ok(());
    }
    fs::remove_file(path)
        .map_err(|_| QghError::storage("Could not remove schedule host budget guard."))?;
    let Some(parent) = path.parent() else {
        return Err(QghError::storage(
            "Schedule host budget guard path is invalid.",
        ));
    };
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| QghError::storage("Could not sync schedule host budget guard directory."))?;
    Ok(())
}

fn host_order_state_path() -> Result<PathBuf, QghError> {
    let hosts = schedule_hosts_dir()?;
    let Some(schedule_dir) = hosts.parent() else {
        return Err(QghError::storage("Schedule host order path is invalid."));
    };
    Ok(schedule_dir.join("host-order.json"))
}

fn rotate_hosts_after_cursor(hosts: &[String], cursor_key: Option<&str>) -> Vec<String> {
    let Some(position) =
        cursor_key.and_then(|cursor| hosts.iter().position(|host| host_key(host) == cursor))
    else {
        return hosts.to_vec();
    };
    hosts[position + 1..]
        .iter()
        .chain(hosts[..=position].iter())
        .cloned()
        .collect()
}

fn read_host_order_cursor(path: &Path) -> Result<Option<String>, QghError> {
    if !path.exists() {
        return Ok(None);
    }
    let bytes =
        fs::read(path).map_err(|_| QghError::storage("Could not read schedule host order."))?;
    let state: HostOrderCursorState = serde_json::from_slice(&bytes)
        .map_err(|_| QghError::storage("Schedule host order is invalid."))?;
    if state.schema_version != "qgh.schedule-host-order.v1" {
        return Err(QghError::storage(
            "Schedule host order schema is unsupported.",
        ));
    }
    Ok(Some(state.cursor_host_key))
}

fn write_host_order_cursor(path: &Path, cursor_host_key: &str) -> Result<(), QghError> {
    let Some(parent) = path.parent() else {
        return Err(QghError::storage("Schedule host order path is invalid."));
    };
    ensure_private_dir(parent)?;
    let temporary = parent.join(format!(".host-order-{}.tmp", now_run_id_suffix()));
    let state = HostOrderCursorState {
        schema_version: "qgh.schedule-host-order.v1".to_string(),
        cursor_host_key: cursor_host_key.to_string(),
    };
    let bytes = serde_json::to_vec(&state)
        .map_err(|_| QghError::storage("Could not serialize schedule host order."))?;
    let write_result = (|| -> Result<(), QghError> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .map_err(|_| QghError::storage("Could not create schedule host order."))?;
        set_private_file(&temporary)?;
        file.write_all(&bytes)
            .map_err(|_| QghError::storage("Could not write schedule host order."))?;
        file.sync_all()
            .map_err(|_| QghError::storage("Could not sync schedule host order."))?;
        fs::rename(&temporary, path)
            .map_err(|_| QghError::storage("Could not publish schedule host order."))?;
        set_private_file(path)?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| QghError::storage("Could not sync schedule host order directory."))?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    write_result
}

fn read_cursor(path: &Path) -> Result<Option<String>, QghError> {
    if !path.exists() {
        return Ok(None);
    }
    let bytes =
        fs::read(path).map_err(|_| QghError::storage("Could not read schedule host state."))?;
    let state: HostCursorState = serde_json::from_slice(&bytes)
        .map_err(|_| QghError::storage("Schedule host state is invalid."))?;
    if state.schema_version != "qgh.schedule-state.v1" {
        return Err(QghError::storage(
            "Schedule host state schema is unsupported.",
        ));
    }
    Ok(Some(state.cursor_profile_id))
}

fn write_cursor(path: &Path, profile_id: &str) -> Result<(), QghError> {
    let Some(parent) = path.parent() else {
        return Err(QghError::storage("Schedule host state path is invalid."));
    };
    ensure_private_dir(parent)?;
    let temporary = parent.join(format!(".schedule-state-{}.tmp", now_run_id_suffix()));
    let state = HostCursorState {
        schema_version: "qgh.schedule-state.v1".to_string(),
        cursor_profile_id: profile_id.to_string(),
    };
    let bytes = serde_json::to_vec(&state)
        .map_err(|_| QghError::storage("Could not serialize schedule host state."))?;
    let write_result = (|| -> Result<(), QghError> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .map_err(|_| QghError::storage("Could not create schedule host state."))?;
        set_private_file(&temporary)?;
        file.write_all(&bytes)
            .map_err(|_| QghError::storage("Could not write schedule host state."))?;
        file.sync_all()
            .map_err(|_| QghError::storage("Could not sync schedule host state."))?;
        fs::rename(&temporary, path)
            .map_err(|_| QghError::storage("Could not publish schedule host state."))?;
        set_private_file(path)?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| QghError::storage("Could not sync schedule host state directory."))?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    write_result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(limit: i64, remaining: i64, reset_at: DateTime<Utc>) -> RateBudgetObservation {
        RateBudgetObservation {
            host: "github.com".to_string(),
            resource: Some("core".to_string()),
            limit: Some(limit),
            remaining: Some(remaining),
            reset_at: Some(reset_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
            observed_at: crate::time::now_rfc3339(),
            best_effort: true,
        }
    }

    #[test]
    fn conservative_budget_uses_lowest_scheduled_allowance() {
        let reset_at = Utc::now() + Duration::hours(1);
        let selected = conservative_observation(
            observation(10, 3, reset_at),
            observation(5_000, 1_200, reset_at),
        );
        assert_eq!(selected.limit, Some(10));
        assert_eq!(selected.remaining, Some(3));
    }

    #[test]
    fn host_budget_guard_uses_latest_fresh_core_reset_and_caps_it_at_twenty_four_hours() {
        let now = DateTime::parse_from_rfc3339(
            &Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        )
        .unwrap()
        .with_timezone(&Utc);
        let observations = vec![
            observation(10, 3, now + Duration::hours(1)),
            observation(5_000, 4_000, now + Duration::hours(2)),
        ];

        let reset = max_fresh_core_reset(&observations, now).unwrap();
        assert_eq!(reset, now + Duration::hours(2));
        assert_eq!(host_budget_guard_deadline(now, Some(&reset)), reset);

        let far_reset = now + Duration::hours(48);
        assert_eq!(
            host_budget_guard_deadline(now, Some(&far_reset)),
            now + Duration::hours(24)
        );
    }

    #[test]
    fn host_budget_guard_uses_active_backoff_and_clamps_extreme_retry_after() {
        let now = DateTime::parse_from_rfc3339(
            &Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        )
        .unwrap()
        .with_timezone(&Utc);
        let backoff = BackoffView {
            reason: "secondary_rate_limit".to_string(),
            scope: "host".to_string(),
            retry_command: None,
            retry_action: None,
            retry_after_seconds: i64::MAX,
            reset_at: Some(
                (now + Duration::hours(2)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            ),
            observed_at: now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            last_successful_sync: None,
        };

        let deadline = active_backoff_guard_deadline(&backoff, now).unwrap();
        assert_eq!(deadline, now + Duration::hours(24));
        assert_eq!(
            host_budget_guard_deadline(now, Some(&deadline)),
            now + Duration::hours(24)
        );
    }

    #[test]
    fn expired_host_budget_guard_is_removed_and_active_state_is_private_and_content_free() {
        let directory =
            std::env::temp_dir().join(format!("qgh-schedule-guard-test-{}", now_run_id_suffix()));
        let path = directory.join("host.guard.json");
        let now = Utc::now();

        write_host_budget_guard(&path, now + Duration::minutes(5)).unwrap();
        assert!(read_active_host_budget_guard(&path, now).unwrap().is_some());
        let state = fs::read_to_string(&path).unwrap();
        assert!(state.contains(HOST_BUDGET_GUARD_SCHEMA_VERSION));
        assert!(!state.contains("github.com"));
        assert!(!state.contains("owner/repo"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o077, 0);
        }

        write_host_budget_guard(&path, now - Duration::seconds(1)).unwrap();
        assert!(read_active_host_budget_guard(&path, now).unwrap().is_none());
        assert!(!path.exists());
        fs::remove_dir_all(directory).unwrap();
    }
}
