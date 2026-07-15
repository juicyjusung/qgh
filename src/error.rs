use serde::Serialize;
use serde_json::{json, Value};

#[derive(Debug, Clone, Serialize)]
pub struct QghError {
    pub code: String,
    pub message: String,
    pub details: Value,
    pub hint: Option<String>,
    pub retryable: bool,
    pub exit_code: i32,
}

impl QghError {
    pub fn new(code: impl Into<String>, message: impl Into<String>, exit_code: i32) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            details: json!({}),
            hint: None,
            retryable: false,
            exit_code,
        }
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = details;
        self
    }

    pub fn with_retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }

    pub fn no_matching_profile(repo: Option<&str>) -> Self {
        let details = repo
            .map(|repo| json!({ "repo": repo }))
            .unwrap_or_else(|| json!({}));
        Self::new(
            "config.no_matching_profile",
            "No configured profile matches the effective repo scope.",
            2,
        )
        .with_details(details)
        .with_hint("Run qgh with --profile <profile-id> or configure a matching repo allowlist.")
    }

    pub fn ambiguous_profile(repo: &str, matching_profile_ids: Vec<String>) -> Self {
        Self::new(
            "config.ambiguous_profile",
            "Multiple configured profiles match the effective repo scope.",
            2,
        )
        .with_details(json!({
            "repo": repo,
            "matching_profile_ids": matching_profile_ids
        }))
        .with_hint("Run qgh with --profile <profile-id>.")
    }

    pub fn config(message: impl Into<String>) -> Self {
        Self::new("config.invalid", message, 2)
    }

    pub fn invalid_repo_policy(message: impl Into<String>) -> Self {
        Self::new("config.invalid_repo_policy", message, 2)
    }

    pub fn validation(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(code, message, 2)
    }

    pub fn auth(message: impl Into<String>) -> Self {
        Self::new("auth.token_unavailable", message, 3)
    }

    pub fn github(message: impl Into<String>) -> Self {
        Self::new("github.request_failed", message, 3)
    }

    pub fn source_not_found(source_id: &str) -> Self {
        Self::new("source.not_found", "Source not found.", 4)
            .with_details(json!({ "source_id": source_id }))
    }

    pub fn source_tombstoned(source_id: &str, reason: &str, observed_at: &str) -> Self {
        Self::new("source.tombstoned", "Source is tombstoned.", 4).with_details(json!({
            "source_id": source_id,
            "reason": reason,
            "observed_at": observed_at,
            "lifecycle_state": "tombstoned"
        }))
    }

    pub fn source_outside_effective_scope(
        source_id: &str,
        source_repo: &str,
        effective_repo_scope: &str,
    ) -> Self {
        Self::new(
            "source.outside_effective_scope",
            "Source is outside the effective repo scope.",
            4,
        )
        .with_details(json!({
            "source_id": source_id,
            "source_repo": source_repo,
            "effective_repo_scope": effective_repo_scope
        }))
        .with_hint("Use get --profile-id from query get_args to round-trip an explicit result.")
    }

    pub fn storage(message: impl Into<String>) -> Self {
        Self::new("storage.failure", message, 6)
    }

    pub fn unsupported_store_schema(expected: &str, actual: Option<&str>) -> Self {
        let actual = match actual {
            Some(value) if reportable_store_schema_version(value) => json!(value),
            Some(_) => json!("unrecognized"),
            None => serde_json::Value::Null,
        };
        Self::new(
            "storage.failure",
            "The local store schema is not supported by this qgh version.",
            6,
        )
        .with_details(json!({
            "reason": "unsupported_schema",
            "expected_schema_version": expected,
            "actual_schema_version": actual
        }))
        .with_hint(
            "Upgrade qgh or restore a compatible store backup; no schema migration or content repair was attempted.",
        )
    }

    pub fn index(message: impl Into<String>) -> Self {
        Self::new("index.failure", message, 6)
    }
}

fn reportable_store_schema_version(value: &str) -> bool {
    let Some(version) = value.strip_prefix("qgh.db.v") else {
        return false;
    };
    !version.is_empty() && version.len() <= 10 && version.bytes().all(|byte| byte.is_ascii_digit())
}

impl From<rusqlite::Error> for QghError {
    fn from(value: rusqlite::Error) -> Self {
        QghError::storage(value.to_string())
    }
}

impl From<std::io::Error> for QghError {
    fn from(value: std::io::Error) -> Self {
        QghError::storage(value.to_string())
    }
}
