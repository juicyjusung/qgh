use crate::freshness;
use crate::model::{CommandAction, CoverageSnapshot};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone)]
pub struct CoverageOutcome {
    pub block: Value,
    pub warnings: Vec<Value>,
}

/// Build the `coverage` envelope block and any coverage warnings.
///
/// `coverage.mode` is derived from the completion flags, never set directly:
/// `complete` iff both open and historical backfill are complete. Recent
/// lookback is bootstrap acceleration, not a corpus boundary, so a partial
/// corpus that returns no results gets a strong warning instead of silently
/// implying the corpus is exhaustive.
pub fn repository_scope_fingerprint<'a>(repos: impl IntoIterator<Item = &'a str>) -> String {
    let mut repos = repos
        .into_iter()
        .map(|repo| repo.trim().to_ascii_lowercase())
        .collect::<Vec<_>>();
    repos.sort();
    repos.dedup();
    let mut hasher = Sha256::new();
    hasher.update(b"qgh.coverage.repository-scope.v1\0");
    for repo in repos {
        hasher.update((repo.len() as u64).to_be_bytes());
        hasher.update(repo.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

pub fn next_action(snapshot: &CoverageSnapshot, profile_id: &str) -> Option<CommandAction> {
    if !snapshot.open_backfill_complete {
        Some(CommandAction::new(
            "open_coverage_incomplete",
            format!("qgh sync --all --profile {profile_id}"),
        ))
    } else if !snapshot.historical_backfill_complete {
        Some(CommandAction::new(
            "historical_coverage_incomplete",
            format!("qgh sync --backfill --all --profile {profile_id}"),
        ))
    } else {
        None
    }
}

pub fn evaluate(snapshot: &CoverageSnapshot, no_result: bool, profile_id: &str) -> CoverageOutcome {
    let complete = snapshot.open_backfill_complete && snapshot.historical_backfill_complete;
    let mode = if complete { "complete" } else { "partial" };
    let next_action = next_action(snapshot, profile_id);
    let block = json!({
        "mode": mode,
        "open_cursor": snapshot.open_cursor,
        "history_cursor": snapshot.history_cursor,
        "open_backfill_complete": snapshot.open_backfill_complete,
        "historical_backfill_complete": snapshot.historical_backfill_complete,
        "next_action": next_action,
        "oldest_synced_updated_at": snapshot.oldest_synced_updated_at,
        "recent_bootstrap_floor": snapshot.recent_bootstrap_floor,
        "next_backfill_window_hint": snapshot.next_backfill_window_hint
    });

    let mut warnings = Vec::new();
    if !complete && no_result {
        warnings.push(freshness::warning(
            "coverage.partial_no_result",
            "warn_strong",
            "No results were found while local corpus coverage is still partial. \
             Recent lookback is bootstrap acceleration, not a corpus boundary, so \
             open or older closed issues may not be indexed yet. Follow \
             `coverage.next_action` before treating this as a true no-result.",
        ));
    }

    CoverageOutcome { block, warnings }
}
