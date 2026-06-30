use crate::freshness;
use crate::model::CoverageSnapshot;
use serde_json::{json, Value};

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
pub fn evaluate(snapshot: &CoverageSnapshot, no_result: bool) -> CoverageOutcome {
    let complete = snapshot.open_backfill_complete && snapshot.historical_backfill_complete;
    let mode = if complete { "complete" } else { "partial" };
    let block = json!({
        "mode": mode,
        "open_cursor": snapshot.open_cursor,
        "history_cursor": snapshot.history_cursor,
        "open_backfill_complete": snapshot.open_backfill_complete,
        "historical_backfill_complete": snapshot.historical_backfill_complete,
        "oldest_synced_updated_at": snapshot.oldest_synced_updated_at,
        "recent_bootstrap_floor": snapshot.recent_bootstrap_floor,
        "next_backfill_window_hint": snapshot.next_backfill_window_hint
    });

    let mut warnings = Vec::new();
    if !complete && no_result {
        warnings.push(freshness::warning(
            "coverage.partial_no_result",
            "warn_strong",
            "No results were found while historical coverage is still partial. \
             Recent lookback is bootstrap acceleration, not a corpus boundary, so \
             older closed issues may not be indexed yet. Run `qgh sync --backfill` \
             to extend coverage before treating this as a true no-result.",
        ));
    }

    CoverageOutcome { block, warnings }
}
