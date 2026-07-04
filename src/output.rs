use crate::error::QghError;
use serde_json::{json, Value};
use std::fmt::{self, Write as _};

#[derive(Debug, Clone, Copy)]
pub enum SuccessOutputKind {
    Init,
    Sync,
    Embed,
    Query,
    Get,
    Status,
    Doctor,
}

pub fn success_envelope_with_meta_and_warnings(
    data: Value,
    meta: Value,
    warnings: Vec<Value>,
) -> Value {
    json!({
        "schema_version": "qgh.v1",
        "ok": true,
        "data": data,
        "warnings": warnings,
        "meta": meta
    })
}

pub fn error_envelope(error: &QghError) -> Value {
    json!({
        "schema_version": "qgh.v1",
        "ok": false,
        "error": error,
        "warnings": [],
        "meta": {}
    })
}

pub fn print_success(data: Value, warnings: Vec<Value>, meta: Value) {
    let envelope = success_envelope_with_meta_and_warnings(data, meta, warnings);
    println!("{}", serde_json::to_string_pretty(&envelope).unwrap());
}

pub fn print_human_success(
    kind: SuccessOutputKind,
    data: &Value,
    warnings: &[Value],
    meta: &Value,
) {
    let rendered = match kind {
        SuccessOutputKind::Init => render_init(data, warnings),
        SuccessOutputKind::Sync => render_sync(data, meta),
        SuccessOutputKind::Embed => render_embed(data),
        SuccessOutputKind::Query => render_query(data),
        SuccessOutputKind::Get => render_get(data),
        SuccessOutputKind::Status => render_status(data),
        SuccessOutputKind::Doctor => render_doctor(data),
    };
    print!("{rendered}");
}

pub fn print_human_warnings(warnings: &[Value]) {
    for warning in warnings {
        eprintln!(
            "{}: {}",
            display_at(warning, &["code"]),
            display_at(warning, &["message"])
        );
    }
}

pub fn print_error(error: &QghError, json_mode: bool) {
    if json_mode {
        let envelope = error_envelope(error);
        println!("{}", serde_json::to_string_pretty(&envelope).unwrap());
    } else {
        eprintln!("{}: {}", error.code, error.message);
        if let Some(hint) = &error.hint {
            eprintln!("hint: {hint}");
        }
    }
}

fn render_embed(data: &Value) -> String {
    let mut out = String::new();
    line(&mut out, format_args!("qgh embed complete"));
    line(
        &mut out,
        format_args!("profile: {}", display_at(data, &["profile_id"])),
    );
    line(
        &mut out,
        format_args!("state: {}", display_at(data, &["embedding_state"])),
    );
    line(
        &mut out,
        format_args!(
            "chunks: refreshed {}, embedded {}",
            display_at(data, &["chunks", "refreshed"]),
            display_at(data, &["chunks", "embedded"])
        ),
    );
    out
}

fn render_init(data: &Value, warnings: &[Value]) -> String {
    let mut out = String::new();
    if string_at(data, &["profile_config_path"]).is_some() {
        line(&mut out, format_args!("qgh init complete"));
        line(
            &mut out,
            format_args!(
                "profile: {} ({})",
                display_at(data, &["profile_id"]),
                display_at(data, &["profile_action"])
            ),
        );
        line(
            &mut out,
            format_args!(
                "repo: {} (allowlist {})",
                display_at(data, &["repo"]),
                display_at(data, &["repo_allowlist_action"])
            ),
        );
        line(
            &mut out,
            format_args!(
                "token source: {}",
                display_at(data, &["token_source", "kind"])
            ),
        );
        line(
            &mut out,
            format_args!("config: {}", display_at(data, &["profile_config_path"])),
        );
        line(
            &mut out,
            format_args!(
                "repo policy: {} at {}",
                display_at(data, &["repo_policy_action"]),
                display_at(data, &["repo_policy_path"])
            ),
        );
        append_next_steps(&mut out, data);
    } else {
        line(&mut out, format_args!("qgh init repo complete"));
        line(
            &mut out,
            format_args!(
                "repo: {} ({})",
                display_at(data, &["repo"]),
                display_at(data, &["repo_source"])
            ),
        );
        line(
            &mut out,
            format_args!("repo policy: {}", display_at(data, &["path"])),
        );
        line(
            &mut out,
            format_args!("overwritten: {}", display_at(data, &["overwritten"])),
        );
        line(
            &mut out,
            format_args!(
                "profile check: {}",
                display_at(data, &["profile_validation", "status"])
            ),
        );
    }
    append_warnings(&mut out, warnings);
    out
}

fn render_sync(data: &Value, meta: &Value) -> String {
    let mut out = String::new();
    let state = string_at(data, &["sync_state"]).unwrap_or("ok");
    let title = if state == "backoff" {
        "qgh sync paused by backoff"
    } else {
        "qgh sync complete"
    };
    line(&mut out, format_args!("{title}"));
    line(
        &mut out,
        format_args!("profile: {}", display_at(data, &["profile_id"])),
    );
    line(
        &mut out,
        format_args!(
            "synced repo scope: {}",
            repo_scope_summary(meta.get("repo"), meta.get("repo_source"))
        ),
    );
    line(&mut out, format_args!("state: {state}"));
    if state == "backoff" {
        line(
            &mut out,
            format_args!(
                "sources: issues {}, comments {}, tombstones {}",
                display_at(data, &["sources", "issue_count"]),
                display_at(data, &["sources", "comment_count"]),
                display_at(data, &["sources", "tombstone_count"])
            ),
        );
    } else {
        line(
            &mut out,
            format_args!(
                "issues: fetched {}, upserted {}, skipped PRs {}",
                display_at(data, &["issues", "fetched"]),
                display_at(data, &["issues", "upserted"]),
                display_at(data, &["issues", "skipped_pull_requests"])
            ),
        );
        line(
            &mut out,
            format_args!(
                "comments: fetched {}, upserted {}",
                display_at(data, &["comments", "fetched"]),
                display_at(data, &["comments", "upserted"])
            ),
        );
        if data
            .get("comments")
            .and_then(|comments| comments.get("added"))
            .is_some()
        {
            line(
                &mut out,
                format_args!(
                    "comment changes: added {}, updated {}, deleted {}",
                    display_at(data, &["comments", "added"]),
                    display_at(data, &["comments", "updated"]),
                    display_at(data, &["comments", "deleted"])
                ),
            );
        }
    }
    line(
        &mut out,
        format_args!("backoff: {}", backoff_summary(data.get("backoff"))),
    );
    line(
        &mut out,
        format_args!(
            "active index generation: {}",
            display_at(data, &["index", "active_generation"])
        ),
    );
    line(
        &mut out,
        format_args!(
            "next: qgh query <terms> --profile {}",
            display_at(data, &["profile_id"])
        ),
    );
    out
}

fn render_query(data: &Value) -> String {
    let mut out = String::new();
    let results = data
        .get("results")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    line(&mut out, format_args!("qgh query results"));
    line(
        &mut out,
        format_args!("profile: {}", display_at(data, &["profile_id"])),
    );
    line(&mut out, format_args!("results: {}", results.len()));
    line(
        &mut out,
        format_args!(
            "freshness: {} (age {}, max-age {}, remote_checked {})",
            display_at(data, &["freshness", "decision"]),
            display_at(data, &["freshness", "snapshot_age_seconds"]),
            display_at(data, &["freshness", "max_age_seconds"]),
            display_at(data, &["freshness", "remote_checked"])
        ),
    );
    line(
        &mut out,
        format_args!(
            "These are source candidates, not answers. Snippets are previews, not citation evidence."
        ),
    );
    line(
        &mut out,
        format_args!(
            "Before citing, run get and use the full source body, canonical URL, and source version."
        ),
    );
    for (index, result) in results.iter().enumerate() {
        let number = index + 1;
        line(
            &mut out,
            format_args!(
                "{number}. [{}] {}#{} {}",
                display_at(result, &["entity_type"]),
                display_at(result, &["repo"]),
                display_at(result, &["issue_number"]),
                display_title(result)
            ),
        );
        line(
            &mut out,
            format_args!("   source: {}", display_at(result, &["source_id"])),
        );
        line(
            &mut out,
            format_args!("   url: {}", display_at(result, &["canonical_url"])),
        );
        line(
            &mut out,
            format_args!("   snippet: {}", compact(&display_at(result, &["snippet"]))),
        );
        line(
            &mut out,
            format_args!(
                "   get: qgh get {} --profile-id {}",
                display_at(result, &["get_args", "source_id"]),
                display_at(result, &["get_args", "profile_id"])
            ),
        );
    }
    if results.is_empty() {
        line(
            &mut out,
            format_args!("next: adjust filters or run qgh sync before searching again."),
        );
    }
    out
}

fn render_get(data: &Value) -> String {
    if data.get("items").is_some() {
        return render_get_batch(data);
    }
    let mut out = String::new();
    let source = data.get("source").unwrap_or(&Value::Null);
    line(&mut out, format_args!("qgh source"));
    line(
        &mut out,
        format_args!("profile: {}", display_at(data, &["profile_id"])),
    );
    line(
        &mut out,
        format_args!("source: {}", display_at(source, &["source_id"])),
    );
    line(
        &mut out,
        format_args!("type: {}", display_at(source, &["entity_type"])),
    );
    line(
        &mut out,
        format_args!(
            "repo issue: {}#{}",
            display_at(source, &["repo"]),
            display_at(source, &["issue_number"])
        ),
    );
    line(
        &mut out,
        format_args!("canonical URL: {}", display_at(source, &["canonical_url"])),
    );
    line(
        &mut out,
        format_args!(
            "source version: body_hash={}, github_updated_at={}, indexed_at={}, sync_run_id={}, lifecycle_state={}",
            display_at(source, &["source_version", "body_hash"]),
            display_at(source, &["source_version", "github_updated_at"]),
            display_at(source, &["source_version", "indexed_at"]),
            display_at(source, &["source_version", "sync_run_id"]),
            display_at(source, &["source_version", "lifecycle_state"])
        ),
    );
    line(
        &mut out,
        format_args!(
            "staleness metadata: github_updated_at={}, indexed_at={}",
            display_at(source, &["source_version", "github_updated_at"]),
            display_at(source, &["source_version", "indexed_at"])
        ),
    );
    line(
        &mut out,
        format_args!(
            "lifecycle check: {}",
            lifecycle_summary(source.get("lifecycle_check"))
        ),
    );
    line(&mut out, format_args!("body:"));
    line(&mut out, format_args!("{}", display_at(source, &["body"])));
    out
}

fn render_get_batch(data: &Value) -> String {
    let mut out = String::new();
    let items = data
        .get("items")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    line(&mut out, format_args!("qgh get batch"));
    line(
        &mut out,
        format_args!("profile: {}", display_at(data, &["profile_id"])),
    );
    line(
        &mut out,
        format_args!(
            "summary: requested {}, returned {}, failed {}",
            display_at(data, &["summary", "requested"]),
            display_at(data, &["summary", "returned"]),
            display_at(data, &["summary", "failed"])
        ),
    );
    line(
        &mut out,
        format_args!(
            "lifecycle checks: {} max_in_flight={}",
            display_at(data, &["lifecycle_check_policy", "mode"]),
            display_at(data, &["lifecycle_check_policy", "max_in_flight_requests"])
        ),
    );
    for item in items {
        if item.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            let source = item.get("source").unwrap_or(&Value::Null);
            line(
                &mut out,
                format_args!(
                    "{}. OK {} {}",
                    display_at(item, &["input_index"]),
                    display_at(item, &["source_id"]),
                    display_at(source, &["canonical_url"])
                ),
            );
        } else {
            line(
                &mut out,
                format_args!(
                    "{}. FAIL {} {}",
                    display_at(item, &["input_index"]),
                    display_at(item, &["source_id"]),
                    display_at(item, &["error", "code"])
                ),
            );
        }
    }
    out
}

fn render_status(data: &Value) -> String {
    let mut out = String::new();
    let resolution = data.get("resolution").unwrap_or(&Value::Null);
    line(&mut out, format_args!("qgh status"));
    line(
        &mut out,
        format_args!(
            "selected profile: {} ({})",
            display_at(data, &["profile_id"]),
            display_at(resolution, &["profile_source"])
        ),
    );
    line(
        &mut out,
        format_args!(
            "effective repo scope: {}",
            repo_scope_summary(
                resolution.get("effective_repo_scope"),
                resolution.get("repo_source")
            )
        ),
    );
    line(
        &mut out,
        format_args!(
            "repo policy: {}",
            display_at(resolution, &["repo_policy_path"])
        ),
    );
    line(
        &mut out,
        format_args!("DB path: {}", display_at(data, &["paths", "database"])),
    );
    line(
        &mut out,
        format_args!(
            "Tantivy index path: {}",
            display_at(data, &["paths", "tantivy_index"])
        ),
    );
    line(
        &mut out,
        format_args!(
            "sources: issues {}, comments {}, tombstones {}",
            display_at(data, &["sources", "issue_count"]),
            display_at(data, &["sources", "comment_count"]),
            display_at(data, &["sources", "tombstone_count"])
        ),
    );
    line(
        &mut out,
        format_args!(
            "freshness: {} (age {}, max-age {}, remote_checked {})",
            display_at(data, &["freshness", "decision"]),
            display_at(data, &["freshness", "snapshot_age_seconds"]),
            display_at(data, &["freshness", "max_age_seconds"]),
            display_at(data, &["freshness", "remote_checked"])
        ),
    );
    if data.get("embedding").is_some() {
        line(
            &mut out,
            format_args!("embedding: {}", embedding_status_summary(data)),
        );
    }
    line(
        &mut out,
        format_args!(
            "active index generation: {}",
            display_at(data, &["index", "active_generation"])
        ),
    );
    line(
        &mut out,
        format_args!(
            "backoff: {}",
            backoff_summary(data.pointer("/sync/backoff"))
        ),
    );
    line(
        &mut out,
        format_args!(
            "default sync scope: {}",
            default_sync_scope(resolution.get("effective_repo_scope"))
        ),
    );
    line(
        &mut out,
        format_args!("next: qgh sync --all syncs every repo in the selected profile."),
    );
    out
}

fn render_doctor(data: &Value) -> String {
    let mut out = String::new();
    let checks = data
        .get("checks")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let failed = checks
        .iter()
        .filter(|check| !check.get("ok").and_then(Value::as_bool).unwrap_or(false))
        .collect::<Vec<_>>();
    line(&mut out, format_args!("qgh doctor"));
    line(
        &mut out,
        format_args!("profile: {}", display_at(data, &["profile_id"])),
    );
    line(&mut out, format_args!("failed checks: {}", failed.len()));
    for check in &failed {
        let name = display_at(check, &["name"]);
        line(
            &mut out,
            format_args!("FAIL {name}: {}", doctor_hint(&name)),
        );
    }
    if failed.is_empty() {
        line(&mut out, format_args!("all checks passed"));
    }
    line(&mut out, format_args!("checks:"));
    for check in checks {
        let status = if check.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            "OK"
        } else {
            "FAIL"
        };
        line(
            &mut out,
            format_args!("  {status} {}", display_at(check, &["name"])),
        );
    }
    line(
        &mut out,
        format_args!("MCP tools: {}", join_array(data.pointer("/mcp/tools"))),
    );
    line(
        &mut out,
        format_args!(
            "doctor exposed to MCP: {}",
            display_at(data, &["mcp", "doctor_exposed"])
        ),
    );
    out
}

fn append_next_steps(out: &mut String, data: &Value) {
    if let Some(steps) = data.get("next_steps").and_then(Value::as_array) {
        for step in steps {
            if let Some(step) = step.as_str() {
                line(out, format_args!("next: {step}"));
            }
        }
    }
}

fn append_warnings(out: &mut String, warnings: &[Value]) {
    if warnings.is_empty() {
        return;
    }
    line(out, format_args!("warnings:"));
    for warning in warnings {
        line(
            out,
            format_args!(
                "  {}: {}",
                display_at(warning, &["code"]),
                display_at(warning, &["message"])
            ),
        );
    }
}

fn display_title(result: &Value) -> String {
    string_at(result, &["title"])
        .or_else(|| string_at(result, &["parent_issue", "title"]))
        .map(ToString::to_string)
        .unwrap_or_default()
}

fn default_sync_scope(scope: Option<&Value>) -> String {
    match scope.and_then(Value::as_str) {
        Some(repo) => format!("{repo} (run qgh sync --all to include every profile repo)"),
        None => "all repos in the selected profile".to_string(),
    }
}

fn repo_scope_summary(repo: Option<&Value>, source: Option<&Value>) -> String {
    match (repo.and_then(Value::as_str), source.and_then(Value::as_str)) {
        (Some(repo), Some(source)) => format!("{repo} ({source})"),
        (Some(repo), None) => repo.to_string(),
        _ => "all profile repos".to_string(),
    }
}

fn backoff_summary(backoff: Option<&Value>) -> String {
    match backoff {
        Some(Value::Object(_)) => format!(
            "reason={}, scope={}, retry_after_seconds={}",
            display_at(backoff.unwrap(), &["reason"]),
            display_at(backoff.unwrap(), &["scope"]),
            display_at(backoff.unwrap(), &["retry_after_seconds"])
        ),
        _ => "none".to_string(),
    }
}

fn embedding_status_summary(data: &Value) -> String {
    format!(
        "{} chunks completed {}/{} missing {} mismatched {} fingerprint {} model {}",
        display_at(data, &["embedding", "state"]),
        display_at(data, &["embedding", "coverage", "completed_chunks"]),
        display_at(data, &["embedding", "coverage", "total_chunks"]),
        display_at(data, &["embedding", "coverage", "missing_chunks"]),
        display_at(data, &["embedding", "coverage", "mismatched_chunks"]),
        display_at(data, &["embedding", "fingerprint", "hash"]),
        display_at(data, &["embedding", "configured_model", "model_id"])
    )
}

fn lifecycle_summary(check: Option<&Value>) -> String {
    match check {
        Some(Value::Object(_)) => {
            let status = display_at(check.unwrap(), &["status"]);
            let reason = string_at(check.unwrap(), &["reason"])
                .or_else(|| string_at(check.unwrap(), &["error_code"]));
            match reason {
                Some(reason) => format!("{status} ({reason})"),
                None => status.to_string(),
            }
        }
        _ => "not_checked".to_string(),
    }
}

fn doctor_hint(name: &str) -> &'static str {
    match name {
        "file_permissions" => {
            "restrict qgh profile data, cache, logs, and DB paths to the current user"
        }
        "sqlite" => "run qgh sync and check the configured DB path",
        "tantivy" => "run qgh sync to rebuild the active Tantivy generation",
        "github_auth_reachability" => "check token source and GitHub host reachability",
        "rate_limit_headers" => "verify GitHub API responses include rate-limit headers",
        "repo_policy" => "update .qgh.toml or the selected profile repo allowlist",
        "profile_resolution" => "pass --profile or adjust profile allowlists",
        _ => "inspect the corresponding JSON check details with --json",
    }
}

fn join_array(value: Option<&Value>) -> String {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(display_value)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_else(|| "n/a".to_string())
}

fn display_at(value: &Value, path: &[&str]) -> String {
    let mut current = value;
    for key in path {
        let Some(next) = current.get(*key) else {
            return "n/a".to_string();
        };
        current = next;
    }
    display_value(current)
}

fn string_at<'a>(value: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    match current {
        Value::String(value) => Some(value),
        Value::Number(_) | Value::Bool(_) => None,
        _ => current.as_str(),
    }
}

fn display_value(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Number(value) => value.to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Null => "n/a".to_string(),
        other => other.to_string(),
    }
}

fn compact(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn line(out: &mut String, args: fmt::Arguments<'_>) {
    let _ = writeln!(out, "{args}");
}
