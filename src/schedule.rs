use crate::cli::{ScheduleArgs, ScheduleCommand};
use crate::commands::{self, LocalReadOutcome, DEFAULT_SYNC_MAX_AGE_SECONDS};
use crate::config::{load_profile, Profile};
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

pub(crate) async fn execute(args: &ScheduleArgs) -> Result<LocalReadOutcome, QghError> {
    match &args.command {
        ScheduleCommand::Run(args) => run_foreground(&args.profile_ids).await,
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
enum HostBudget {
    Unknown,
    Fresh(RateBudgetObservation),
}

impl HostBudget {
    fn state(&self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Fresh(_) => "fresh",
        }
    }

    fn snapshot(&self) -> Value {
        match self {
            Self::Unknown => Value::Null,
            Self::Fresh(observation) => rate_budget::block(std::slice::from_ref(observation)),
        }
    }

    fn reserve_exhausted(&self) -> bool {
        let Self::Fresh(observation) = self else {
            return false;
        };
        let (Some(limit), Some(remaining)) = (observation.limit, observation.remaining) else {
            return true;
        };
        let reserve = (limit.saturating_add(4)) / 5;
        remaining <= reserve
    }

    fn revalidated(self) -> Self {
        match self {
            Self::Fresh(observation) if rate_budget::is_fresh(&observation) => {
                Self::Fresh(observation)
            }
            Self::Fresh(_) | Self::Unknown => Self::Unknown,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct HostCursorState {
    schema_version: String,
    cursor_profile_id: String,
}

async fn run_foreground(profile_ids: &[String]) -> Result<LocalReadOutcome, QghError> {
    validate_profile_ids(profile_ids)?;
    let mut plans = Vec::with_capacity(profile_ids.len());
    for profile_id in profile_ids {
        plans.push(plan_profile(load_profile(profile_id)?)?);
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

    let mut host_reports = Vec::with_capacity(groups.len());
    let mut remote_attempts = 0usize;
    for (host, indexes) in groups {
        let state_path = host_state_path(&host)?;
        let has_eligible_profiles = indexes.iter().any(|index| {
            matches!(
                plans[*index].state,
                PlannedState::Eligible | PlannedState::SyncBusy
            )
        });
        let host_lock_path = state_path.with_extension("lock");
        let host_lease = if has_eligible_profiles {
            FileLease::try_acquire_schedule_host(&host_lock_path)?
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
        let initial_budget_unknown = matches!(initial_budget, HostBudget::Unknown);
        let mut host_attempts = 0usize;
        let mut host_cooldown = indexes.iter().any(|index| plans[*index].active_backoff);

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
                if (initial_budget_unknown || matches!(budget, HostBudget::Unknown))
                    && host_attempts >= 1
                {
                    results.insert(
                        plan.profile.id.clone(),
                        profile_result(
                            &plan.profile,
                            false,
                            "deferred",
                            "unknown_budget_limit",
                            None,
                            None,
                            Value::Null,
                        ),
                    );
                    continue;
                }
                if budget.reserve_exhausted() {
                    results.insert(
                        plan.profile.id.clone(),
                        profile_result(
                            &plan.profile,
                            false,
                            "deferred",
                            "rate_budget_reserve",
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

                let outcome = commands::sync(
                    &plan.profile.id,
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
                )
                .await;
                let skipped_fresh = matches!(
                    &outcome,
                    Ok(outcome)
                        if outcome.data.get("sync_state").and_then(Value::as_str)
                            == Some("skipped_fresh")
                );
                let consumed_remote_attempt = !skipped_fresh
                    && !matches!(
                        &outcome,
                        Err(error)
                            if error.code == "sync.busy"
                                || error.code == "auth.token_unavailable"
                    );
                if consumed_remote_attempt {
                    host_attempts += 1;
                    remote_attempts += 1;
                }
                cursor_after = Some(plan.profile.id.clone());
                write_cursor(&state_path, &plan.profile.id)?;
                let latest_budget = load_rate_budget(&plan.profile)?;
                budget = update_budget_after_attempt(budget, &latest_budget);
                match outcome {
                    Ok(outcome) => {
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
                                !skipped_fresh,
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
                                true,
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
                                false,
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
                                true,
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

fn backoff_is_active(backoff: &BackoffView) -> bool {
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

fn derive_host_budget(plans: &[PlannedProfile], indexes: &[usize]) -> HostBudget {
    let mut candidate = None::<RateBudgetObservation>;
    for index in indexes {
        let observations = &plans[*index].rate_budget;
        if observations.is_empty() || observations.iter().any(|item| !rate_budget::is_fresh(item)) {
            return HostBudget::Unknown;
        }
        for observation in observations {
            candidate = Some(match candidate {
                None => observation.clone(),
                Some(current) => conservative_observation(current, observation.clone()),
            });
        }
    }
    candidate.map_or(HostBudget::Unknown, HostBudget::Fresh)
}

fn update_budget_after_attempt(
    current: HostBudget,
    observations: &[RateBudgetObservation],
) -> HostBudget {
    if observations.is_empty() || observations.iter().any(|item| !rate_budget::is_fresh(item)) {
        return HostBudget::Unknown;
    }
    let latest = observations
        .iter()
        .cloned()
        .reduce(conservative_observation);
    match (current.revalidated(), latest) {
        (_, None) => HostBudget::Unknown,
        (HostBudget::Unknown, Some(observation)) => HostBudget::Fresh(observation),
        (HostBudget::Fresh(current), Some(latest)) => {
            HostBudget::Fresh(conservative_observation(current, latest))
        }
    }
}

fn conservative_observation(
    left: RateBudgetObservation,
    right: RateBudgetObservation,
) -> RateBudgetObservation {
    let left_limit = left.limit.unwrap_or(1).max(1) as i128;
    let right_limit = right.limit.unwrap_or(1).max(1) as i128;
    let left_remaining = left.remaining.unwrap_or(0) as i128;
    let right_remaining = right.remaining.unwrap_or(0) as i128;
    if left_remaining.saturating_mul(right_limit) <= right_remaining.saturating_mul(left_limit) {
        left
    } else {
        right
    }
}

fn load_rate_budget(profile: &Profile) -> Result<Vec<RateBudgetObservation>, QghError> {
    let store = Store::open_for_read(&profile.paths)?;
    store.rate_budget_observations(&profile.host)
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

fn host_state_path(host: &str) -> Result<PathBuf, QghError> {
    let digest = Sha256::digest(host.as_bytes());
    let key = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(schedule_hosts_dir()?.join(format!("{key}.json")))
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

    #[test]
    fn conservative_budget_uses_lowest_remaining_ratio() {
        let observation = |limit, remaining| RateBudgetObservation {
            host: "github.com".to_string(),
            resource: Some("core".to_string()),
            limit: Some(limit),
            remaining: Some(remaining),
            reset_at: Some("2100-01-01T00:00:00Z".to_string()),
            observed_at: crate::time::now_rfc3339(),
            best_effort: true,
        };
        let selected = conservative_observation(observation(5_000, 1_000), observation(100, 10));
        assert_eq!(selected.limit, Some(100));
        assert_eq!(selected.remaining, Some(10));
    }
}
