use crate::config::{FreshnessSettings, StaleBehavior};
use crate::error::QghError;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};

pub const DEFAULT_QUERY_MAX_AGE_SECONDS: i64 = 7 * 24 * 60 * 60;

#[derive(Debug, Clone, Copy)]
pub struct FreshnessOverrides {
    pub max_age_seconds: Option<i64>,
    pub require_fresh: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct FreshnessContext<'a> {
    pub last_successful_sync_at: Option<&'a str>,
    pub includes_open_issue: bool,
    pub overrides: FreshnessOverrides,
}

#[derive(Debug, Clone)]
pub struct FreshnessOutcome {
    pub block: Value,
    pub warnings: Vec<Value>,
    pub fails: bool,
}

pub fn evaluate(
    settings: FreshnessSettings,
    context: FreshnessContext<'_>,
) -> Result<FreshnessOutcome, QghError> {
    let behavior = if context.overrides.require_fresh {
        StaleBehavior::Fail
    } else {
        settings.query_stale_behavior
    };
    let query_max_age_seconds = context
        .overrides
        .max_age_seconds
        .unwrap_or(settings.query_max_age_seconds);
    let max_age_seconds = if context.includes_open_issue {
        settings
            .active_issue_max_age_seconds
            .map(|active_max_age| active_max_age.min(query_max_age_seconds))
            .unwrap_or(query_max_age_seconds)
    } else {
        query_max_age_seconds
    };

    let snapshot_age_seconds = context
        .last_successful_sync_at
        .map(snapshot_age_seconds)
        .transpose()?;

    let mut warnings = Vec::new();
    if context.last_successful_sync_at.is_none() {
        warnings.push(warning(
            "freshness.never_synced",
            stale_severity(behavior, "warn_strong"),
            "Local snapshot has never completed a successful sync.",
        ));
    } else if snapshot_age_seconds.is_some_and(|age| age > query_max_age_seconds) {
        warnings.push(warning(
            "freshness.query_snapshot_stale",
            stale_severity(behavior, "warn"),
            "Local snapshot is older than the query max-age policy.",
        ));
    }

    if context.includes_open_issue {
        if let Some(active_issue_max_age_seconds) = settings.active_issue_max_age_seconds {
            if snapshot_age_seconds.is_some_and(|age| age > active_issue_max_age_seconds) {
                warnings.push(warning(
                    "freshness.active_issue_snapshot_stale",
                    stale_severity(behavior, "warn_strong"),
                    "Open issue results are older than the active issue max-age policy.",
                ));
            }
        }
    }

    let fails = warnings
        .iter()
        .any(|warning| warning.get("severity").and_then(Value::as_str) == Some("fail"));
    let decision = if context.last_successful_sync_at.is_none() {
        "never_synced"
    } else if fails {
        "stale_fail"
    } else if warnings.is_empty() {
        "fresh"
    } else {
        "stale_warn"
    };
    let block = json!({
        "decision": decision,
        "remote_checked": false,
        "snapshot_age_seconds": snapshot_age_seconds,
        "max_age_seconds": max_age_seconds
    });

    Ok(FreshnessOutcome {
        block,
        warnings,
        fails,
    })
}

pub fn parse_duration_seconds(field: &str, value: &str) -> Result<i64, QghError> {
    let (number, unit) = split_duration(value).ok_or_else(|| {
        QghError::config(format!(
            "Config field `{field}` must be a duration string like `90s`, `30m`, `7d`, or `12mo`."
        ))
    })?;
    if number <= 0 {
        return Err(QghError::config(format!(
            "Config field `{field}` must be greater than zero."
        )));
    }
    let multiplier = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 24 * 60 * 60,
        "mo" => 30 * 24 * 60 * 60,
        _ => {
            return Err(QghError::config(format!(
                "Config field `{field}` has unsupported duration unit `{unit}`."
            )));
        }
    };
    number
        .checked_mul(multiplier)
        .ok_or_else(|| QghError::config(format!("Config field `{field}` duration is too large.")))
}

fn split_duration(value: &str) -> Option<(i64, &str)> {
    let value = value.trim();
    let digit_count = value
        .bytes()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if digit_count == 0 || digit_count == value.len() {
        return None;
    }
    let number = value[..digit_count].parse::<i64>().ok()?;
    Some((number, &value[digit_count..]))
}

fn snapshot_age_seconds(value: &str) -> Result<i64, QghError> {
    let synced_at = DateTime::parse_from_rfc3339(value).map_err(|error| {
        QghError::storage(format!("Invalid stored sync timestamp `{value}`: {error}"))
    })?;
    let age = Utc::now()
        .signed_duration_since(synced_at.with_timezone(&Utc))
        .num_seconds();
    Ok(age.max(0))
}

fn stale_severity(behavior: StaleBehavior, warning_severity: &'static str) -> &'static str {
    match behavior {
        StaleBehavior::Warn => warning_severity,
        StaleBehavior::Fail => "fail",
    }
}

pub(crate) fn warning(code: &'static str, severity: &'static str, message: &'static str) -> Value {
    json!({
        "code": code,
        "severity": severity,
        "message": message
    })
}
