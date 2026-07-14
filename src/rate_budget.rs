use crate::model::RateBudgetObservation;
use crate::time::now_rfc3339;
use chrono::{DateTime, SecondsFormat, Utc};
use reqwest::header::HeaderMap;
use serde_json::{json, Value};

pub(crate) const STALE_AFTER_SECONDS: i64 = 300;

pub(crate) fn observe(host: &str, headers: &HeaderMap) -> RateBudgetObservation {
    RateBudgetObservation {
        host: host.to_ascii_lowercase(),
        resource: safe_resource(header(headers, "x-ratelimit-resource")),
        limit: nonnegative_header(headers, "x-ratelimit-limit"),
        remaining: nonnegative_header(headers, "x-ratelimit-remaining"),
        reset_at: epoch_header(headers, "x-ratelimit-reset"),
        observed_at: now_rfc3339(),
        best_effort: true,
    }
}

pub(crate) fn block(observations: &[RateBudgetObservation]) -> Value {
    json!({
        "best_effort": true,
        "stale_after_seconds": STALE_AFTER_SECONDS,
        "observations": observations.iter().map(view).collect::<Vec<_>>()
    })
}

pub(crate) fn state(observation: &RateBudgetObservation) -> &'static str {
    let now = Utc::now();
    let observed_at = DateTime::parse_from_rfc3339(&observation.observed_at)
        .ok()
        .map(|value| value.with_timezone(&Utc));
    let reset_at = observation
        .reset_at
        .as_deref()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc));
    if observed_at
        .is_none_or(|value| value > now || (now - value).num_seconds() > STALE_AFTER_SECONDS)
        || (observation.reset_at.is_some() && reset_at.is_none())
        || reset_at.is_some_and(|value| value <= now)
    {
        "stale"
    } else if observation.limit.is_none()
        || observation.remaining.is_none()
        || observation.reset_at.is_none()
    {
        "partial"
    } else {
        "fresh"
    }
}

pub(crate) fn is_fresh(observation: &RateBudgetObservation) -> bool {
    state(observation) == "fresh"
}

fn view(observation: &RateBudgetObservation) -> Value {
    let state = state(observation);
    json!({
        "host": observation.host,
        "resource": observation.resource,
        "limit": observation.limit,
        "remaining": observation.remaining,
        "reset_at": observation.reset_at,
        "observed_at": observation.observed_at,
        "best_effort": observation.best_effort,
        "state": state,
        "stale": state == "stale"
    })
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()
}

fn nonnegative_header(headers: &HeaderMap, name: &str) -> Option<i64> {
    header(headers, name)?
        .parse::<i64>()
        .ok()
        .filter(|value| *value >= 0)
}

fn epoch_header(headers: &HeaderMap, name: &str) -> Option<String> {
    let epoch = nonnegative_header(headers, name)?;
    DateTime::from_timestamp(epoch, 0).map(|value| value.to_rfc3339_opts(SecondsFormat::Secs, true))
}

fn safe_resource(value: Option<&str>) -> Option<String> {
    let value = value?;
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return None;
    }
    Some(value.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderName, HeaderValue};

    #[test]
    fn unsafe_resource_and_invalid_numbers_become_partial_content_free_observation() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("x-ratelimit-resource"),
            HeaderValue::from_static("private/path"),
        );
        headers.insert(
            HeaderName::from_static("x-ratelimit-limit"),
            HeaderValue::from_static("-1"),
        );
        let observation = observe("GHE.EXAMPLE", &headers);
        assert_eq!(observation.host, "ghe.example");
        assert_eq!(observation.resource, None);
        assert_eq!(observation.limit, None);
        assert_eq!(state(&observation), "partial");
    }

    #[test]
    fn future_observation_is_stale_instead_of_bypassing_the_ttl() {
        let observation = RateBudgetObservation {
            host: "github.com".to_string(),
            resource: Some("core".to_string()),
            limit: Some(5_000),
            remaining: Some(4_000),
            reset_at: Some(
                (Utc::now() + chrono::Duration::hours(1))
                    .to_rfc3339_opts(SecondsFormat::Secs, true),
            ),
            observed_at: (Utc::now() + chrono::Duration::minutes(1))
                .to_rfc3339_opts(SecondsFormat::Secs, true),
            best_effort: true,
        };

        assert_eq!(state(&observation), "stale");
    }

    #[test]
    fn malformed_reset_timestamp_is_not_a_usable_fresh_budget() {
        let observation = RateBudgetObservation {
            host: "github.com".to_string(),
            resource: Some("core".to_string()),
            limit: Some(5_000),
            remaining: Some(4_000),
            reset_at: Some("not-rfc3339".to_string()),
            observed_at: now_rfc3339(),
            best_effort: true,
        };

        assert_eq!(state(&observation), "stale");
        assert!(!is_fresh(&observation));
    }
}
