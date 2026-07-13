use crate::error::QghError;
use crate::terminal::{SummaryTone, TerminalUi};
use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};
use std::fmt::{self, Write as _};

#[derive(Debug, Clone, Copy)]
pub enum SuccessOutputKind {
    Init,
    Sync,
    Embed,
    Model,
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
    decorate: bool,
) {
    let rendered = match kind {
        SuccessOutputKind::Init => render_init(data),
        SuccessOutputKind::Sync => render_sync(data, warnings, meta),
        SuccessOutputKind::Embed => render_embed(data),
        SuccessOutputKind::Model => render_model(data),
        SuccessOutputKind::Query => render_query(data, warnings),
        SuccessOutputKind::Get => render_get(data),
        SuccessOutputKind::Status => render_status(data, warnings),
        SuccessOutputKind::Doctor => render_doctor(data),
    };
    let tone = success_tone(kind, data, warnings);
    let terminal = if decorate {
        TerminalUi::stdout()
    } else {
        TerminalUi::plain()
    };
    print!(
        "{}",
        terminal.render_summary(&rendered, tone, !matches!(kind, SuccessOutputKind::Get),)
    );
}

fn success_tone(kind: SuccessOutputKind, data: &Value, warnings: &[Value]) -> SummaryTone {
    if !warnings.is_empty() {
        return SummaryTone::Warning;
    }
    let needs_attention = match kind {
        SuccessOutputKind::Status => status_readiness(data, warnings) != "ready",
        SuccessOutputKind::Doctor => {
            data.get("checks")
                .and_then(Value::as_array)
                .is_some_and(|checks| {
                    checks
                        .iter()
                        .any(|check| !check.get("ok").and_then(Value::as_bool).unwrap_or(false))
                })
        }
        SuccessOutputKind::Sync => {
            string_at(data, &["coverage", "mode"]) == Some("partial")
                || (data.get("backfill").is_some()
                    && !(data
                        .pointer("/backfill/open_backfill_complete")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                        && data
                            .pointer("/backfill/historical_backfill_complete")
                            .and_then(Value::as_bool)
                            .unwrap_or(false)))
        }
        SuccessOutputKind::Get => data
            .pointer("/summary/failed")
            .and_then(Value::as_u64)
            .is_some_and(|failed| failed > 0),
        SuccessOutputKind::Init
        | SuccessOutputKind::Embed
        | SuccessOutputKind::Model
        | SuccessOutputKind::Query => false,
    };
    if needs_attention {
        SummaryTone::Warning
    } else {
        SummaryTone::Success
    }
}

pub fn print_human_warnings(warnings: &[Value], decorate: bool) {
    let terminal = if decorate {
        TerminalUi::stderr()
    } else {
        TerminalUi::plain()
    };
    for warning in warnings {
        let severity = match string_at(warning, &["severity"]) {
            Some("fail") => "error",
            Some("warn_strong") => "strong warning",
            Some("warn") | None => "warning",
            Some(other) => other,
        };
        eprintln!(
            "{}",
            terminal.warning(
                severity,
                &display_at(warning, &["code"]),
                &display_at(warning, &["message"]),
            )
        );
        if let Some(command) = string_at(warning, &["action", "command"]) {
            eprintln!("{}", terminal.hint(command));
        }
    }
}

pub fn print_error(error: &QghError, json_mode: bool, decorate: bool) {
    if json_mode {
        let envelope = error_envelope(error);
        println!("{}", serde_json::to_string_pretty(&envelope).unwrap());
    } else {
        let terminal = if decorate {
            TerminalUi::stderr()
        } else {
            TerminalUi::plain()
        };
        let rendered = if error.retryable {
            terminal.retryable_error(&error.code, &error.message)
        } else {
            terminal.error(&error.code, &error.message)
        };
        eprintln!("{rendered}");
        if let Some(hint) = &error.hint {
            eprintln!("{}", terminal.hint(hint));
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
            "text chunks rebuilt: {}",
            display_at(data, &["chunks", "refreshed"]),
        ),
    );
    line(
        &mut out,
        format_args!(
            "vectors generated: {}",
            display_at(data, &["chunks", "embedded"])
        ),
    );
    out
}

fn render_model(data: &Value) -> String {
    let mut out = String::new();
    line(&mut out, format_args!("qgh model install complete"));
    line(
        &mut out,
        format_args!("model: {}", display_at(data, &["model"])),
    );
    line(
        &mut out,
        format_args!("action: {}", display_at(data, &["action"])),
    );
    line(
        &mut out,
        format_args!("revision: {}", display_at(data, &["resolved_revision"])),
    );
    line(
        &mut out,
        format_args!("verified bytes: {}", display_at(data, &["verified_bytes"])),
    );
    out
}

fn render_init(data: &Value) -> String {
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
    out
}

fn render_sync(data: &Value, warnings: &[Value], meta: &Value) -> String {
    if data.get("backfill").is_some() {
        return render_backfill_sync(data, warnings, meta);
    }
    let mut out = String::new();
    let state = string_at(data, &["sync_state"]).unwrap_or("ok");
    if state == "skipped_fresh" {
        return render_skipped_fresh_sync(data, meta);
    }
    let title = if !warnings.is_empty() {
        "qgh sync complete — search ready with limitations"
    } else if string_at(data, &["coverage", "mode"]) == Some("partial") {
        "qgh sync complete — search ready; coverage partial"
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
    if warnings.iter().any(is_semantic_unavailable_warning) {
        line(
            &mut out,
            format_args!("search: BM25 ready; semantic unavailable"),
        );
    } else {
        line(&mut out, format_args!("search: local index ready"));
    }
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
    line(
        &mut out,
        format_args!(
            "active index generation: {}",
            display_at(data, &["index", "active_generation"])
        ),
    );
    if data.get("coverage").is_some() {
        line(
            &mut out,
            format_args!("coverage: {}", status_coverage_summary(data)),
        );
    }
    let next_action = string_at(data, &["coverage", "next_action", "command"])
        .map(ToString::to_string)
        .unwrap_or_else(|| {
            format!(
                "qgh query <terms> --profile {}",
                display_at(data, &["profile_id"])
            )
        });
    line(&mut out, format_args!("next: {next_action}"));
    out
}

fn render_skipped_fresh_sync(data: &Value, meta: &Value) -> String {
    let mut out = String::new();
    line(
        &mut out,
        format_args!("qgh sync skipped — local snapshot is fresh"),
    );
    line(
        &mut out,
        format_args!("profile: {}", display_at(data, &["profile_id"])),
    );
    line(
        &mut out,
        format_args!(
            "repo scope: {}",
            repo_scope_summary(meta.get("repo"), meta.get("repo_source"))
        ),
    );
    line(&mut out, format_args!("state: skipped_fresh"));
    line(
        &mut out,
        format_args!("network: skipped; no GitHub request was needed"),
    );
    line(
        &mut out,
        format_args!(
            "last successful sync: {}",
            display_at(data, &["sync", "last_successful_sync"])
        ),
    );
    line(
        &mut out,
        format_args!(
            "snapshot age: {} seconds",
            display_at(data, &["sync", "snapshot_age_seconds"])
        ),
    );
    line(
        &mut out,
        format_args!(
            "max age: {} seconds",
            display_at(data, &["sync", "max_age_seconds"])
        ),
    );
    if data.get("coverage").is_some() {
        line(
            &mut out,
            format_args!("coverage: {}", status_coverage_summary(data)),
        );
    }
    let next_action = string_at(data, &["coverage", "next_action", "command"])
        .map(ToString::to_string)
        .unwrap_or_else(|| {
            format!(
                "qgh query <terms> --profile {}",
                display_at(data, &["profile_id"])
            )
        });
    line(&mut out, format_args!("next: {next_action}"));
    out
}

fn is_semantic_unavailable_warning(warning: &Value) -> bool {
    string_at(warning, &["code"]).is_some_and(|code| {
        code == "publication.embedding_snapshot_mismatch"
            || (code.starts_with("embedding.")
                && !matches!(
                    code,
                    "embedding.generation_cleanup_failed" | "embedding.tombstone_cleanup_failed"
                ))
    })
}

fn render_backfill_sync(data: &Value, warnings: &[Value], meta: &Value) -> String {
    let mut out = String::new();
    let open_complete = data
        .pointer("/backfill/open_backfill_complete")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let historical_complete = data
        .pointer("/backfill/historical_backfill_complete")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let complete = open_complete && historical_complete;
    let title = if warnings.is_empty() {
        "qgh historical backfill pass complete"
    } else {
        "qgh historical backfill pass complete — search ready with limitations"
    };
    line(&mut out, format_args!("{title}"));
    line(
        &mut out,
        format_args!("profile: {}", display_at(data, &["profile_id"])),
    );
    if warnings.iter().any(is_semantic_unavailable_warning) {
        line(
            &mut out,
            format_args!("search: BM25 ready; semantic unavailable"),
        );
    } else {
        line(&mut out, format_args!("search: local index ready"));
    }
    line(
        &mut out,
        format_args!(
            "repo scope: {}",
            repo_scope_summary(meta.get("repo"), meta.get("repo_source"))
        ),
    );
    line(
        &mut out,
        format_args!(
            "fetched: issues {}, comments {}, skipped PRs {}",
            display_at(data, &["backfill", "issues"]),
            display_at(data, &["backfill", "comments"]),
            display_at(data, &["backfill", "skipped_pull_requests"])
        ),
    );
    line(
        &mut out,
        format_args!(
            "coverage: {}",
            if complete { "complete" } else { "partial" }
        ),
    );
    line(
        &mut out,
        format_args!("open coverage complete: {open_complete}"),
    );
    line(
        &mut out,
        format_args!("historical coverage complete: {historical_complete}"),
    );
    line(
        &mut out,
        format_args!(
            "history cursor: {}",
            display_at(data, &["backfill", "history_cursor"])
        ),
    );
    line(
        &mut out,
        format_args!(
            "repo scope history end reached: {}",
            display_at(data, &["backfill", "reached_end"])
        ),
    );
    if complete {
        line(
            &mut out,
            format_args!(
                "next: qgh query <terms> --profile {}",
                display_at(data, &["profile_id"])
            ),
        );
    } else if let Some(command) = string_at(data, &["backfill", "next_action", "command"]) {
        line(&mut out, format_args!("next: {command}"));
    } else {
        line(&mut out, format_args!("next: qgh status"));
    }
    out
}

fn render_query(data: &Value, warnings: &[Value]) -> String {
    let mut out = String::new();
    let results = data
        .get("results")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    line(
        &mut out,
        format_args!("qgh query — {} source candidates", results.len()),
    );
    line(
        &mut out,
        format_args!("profile: {}", display_at(data, &["profile_id"])),
    );
    line(&mut out, format_args!("results: {}", results.len()));
    line(
        &mut out,
        format_args!("search: {}", query_search_summary(results)),
    );
    line(
        &mut out,
        format_args!("coverage: {}", status_coverage_summary(data)),
    );
    if let Some(rerank) = data.get("rerank") {
        if rerank
            .get("applied")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            line(
                &mut out,
                format_args!(
                    "rerank: applied {} to {} candidates ({})",
                    display_at(rerank, &["model"]),
                    display_at(rerank, &["candidate_count"]),
                    display_at(rerank, &["runtime_profile"])
                ),
            );
        } else {
            line(
                &mut out,
                format_args!("rerank: not applied ({})", display_at(rerank, &["reason"])),
            );
        }
    }
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
            format_args!("   ranking: {}", display_at(result, &["ranking", "kind"])),
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
        if warnings
            .iter()
            .any(|warning| string_at(warning, &["code"]) == Some("coverage.partial_no_result"))
        {
            line(
                &mut out,
                format_args!("No matches in the current partial corpus."),
            );
            line(
                &mut out,
                format_args!(
                    "next: {}",
                    string_at(data, &["coverage", "next_action", "command"])
                        .unwrap_or("qgh status")
                ),
            );
        } else {
            line(
                &mut out,
                format_args!("No matches in the current local corpus."),
            );
            line(
                &mut out,
                format_args!("next: adjust the query or filters and search again."),
            );
        }
    }
    out
}

fn query_search_summary(results: &[Value]) -> &'static str {
    if results.iter().any(|result| {
        matches!(
            string_at(result, &["ranking", "kind"]),
            Some("hybrid") | Some("reranked")
        )
    }) {
        "hybrid"
    } else if results
        .iter()
        .any(|result| string_at(result, &["ranking", "kind"]) == Some("vector"))
    {
        "vector"
    } else if results
        .iter()
        .any(|result| string_at(result, &["ranking", "kind"]) == Some("exact"))
    {
        "exact lookup"
    } else if results.is_empty() {
        "local snapshot"
    } else {
        "BM25"
    }
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

fn render_status(data: &Value, warnings: &[Value]) -> String {
    let mut out = String::new();
    let resolution = data.get("resolution").unwrap_or(&Value::Null);
    let readiness = status_readiness(data, warnings);
    line(&mut out, format_args!("qgh status — search {readiness}"));
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
            "search: {}",
            status_search_summary(data, warnings, readiness)
        ),
    );
    line(
        &mut out,
        format_args!("coverage: {}", status_coverage_summary(data)),
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
            "purge: pending {}, successor repair required {}, retrieval blocked {}, stages {}",
            display_at(data, &["purge", "pending_count"]),
            display_at(data, &["purge", "successor_repair_required"]),
            display_at(data, &["purge", "retrieval_blocked"]),
            join_array(data.pointer("/purge/current_stages"))
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
        format_args!(
            "next: {}",
            status_next_action(data, display_at(data, &["profile_id"]), readiness)
        ),
    );
    out
}

fn status_readiness(data: &Value, warnings: &[Value]) -> &'static str {
    if data
        .pointer("/purge/retrieval_blocked")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || warnings.iter().any(is_retrieval_blocking_warning)
    {
        "blocked"
    } else if string_at(data, &["freshness", "decision"]) == Some("never_synced") {
        "not ready"
    } else {
        "ready"
    }
}

fn is_retrieval_blocking_warning(warning: &Value) -> bool {
    matches!(
        string_at(warning, &["code"]),
        Some(
            "publication.source_snapshot_incomplete"
                | "publication.source_snapshot_changed"
                | "publication.source_inventory_mismatch"
                | "publication.embedding_snapshot_mismatch"
                | "publication.tantivy_artifact_not_ready"
        )
    )
}

fn status_search_summary(data: &Value, warnings: &[Value], readiness: &str) -> String {
    if readiness == "blocked" {
        if data
            .pointer("/purge/retrieval_blocked")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return "blocked until pending purge repair completes".to_string();
        }
        if warnings.iter().any(is_retrieval_blocking_warning) {
            return "blocked until the local index is rebuilt".to_string();
        }
    }
    if readiness == "not ready" {
        return "not ready; no successful local snapshot".to_string();
    }
    match string_at(data, &["embedding", "state"]) {
        None => "BM25 ready; semantic not configured".to_string(),
        Some("complete") => "hybrid ready; BM25 + semantic".to_string(),
        Some(state) => format!("BM25 ready; semantic {state}"),
    }
}

fn status_coverage_summary(data: &Value) -> String {
    match string_at(data, &["coverage", "mode"]) {
        Some("complete") => "complete".to_string(),
        Some("partial") => match (
            data.pointer("/coverage/open_backfill_complete")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            data.pointer("/coverage/historical_backfill_complete")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        ) {
            (false, false) => "partial; open and historical coverage incomplete".to_string(),
            (false, true) => "partial; open coverage incomplete".to_string(),
            (true, false) => "partial; historical coverage incomplete".to_string(),
            (true, true) => "partial; coverage state inconsistent".to_string(),
        },
        Some(mode) => mode.to_string(),
        None => "unknown".to_string(),
    }
}

fn status_next_action(data: &Value, profile_id: String, readiness: &str) -> String {
    if let Some(backoff) = data
        .pointer("/sync/backoff")
        .filter(|value| value.is_object())
    {
        let retry_command = string_at(backoff, &["retry_action", "command"])
            .or_else(|| string_at(backoff, &["retry_command"]));
        if let Some(retry_at) = backoff_retry_at(backoff) {
            if retry_at <= Utc::now() {
                return retry_command.map_or_else(
                    || "retry the interrupted sync command now".to_string(),
                    |command| format!("retry now: {command}"),
                );
            }
            return retry_command.map_or_else(
                || {
                    format!(
                        "wait until {}, then retry the interrupted sync command",
                        retry_at.to_rfc3339()
                    )
                },
                |command| format!("wait until {}, then {command}", retry_at.to_rfc3339()),
            );
        }
        return retry_command.map_or_else(
            || {
                "wait for GitHub backoff to clear, then retry the interrupted sync command"
                    .to_string()
            },
            |command| format!("wait for GitHub backoff to clear, then {command}"),
        );
    }
    if readiness == "blocked" {
        return status_sync_command(data, &profile_id);
    }
    if readiness == "not ready" {
        return status_sync_command(data, &profile_id);
    }
    if let Some(action) = string_at(data, &["coverage", "next_action", "command"]) {
        return action.to_string();
    }
    format!("qgh query <terms> --profile {profile_id}")
}

fn status_sync_command(data: &Value, profile_id: &str) -> String {
    if let Some(repo) = string_at(data, &["resolution", "effective_repo_scope"]) {
        format!("qgh sync --repo {repo} --profile {profile_id}")
    } else {
        format!("qgh sync --all --profile {profile_id}")
    }
}

fn backoff_retry_at(backoff: &Value) -> Option<DateTime<Utc>> {
    if let Some(reset_at) = string_at(backoff, &["reset_at"]) {
        if let Some(reset_at) = DateTime::parse_from_rfc3339(reset_at)
            .ok()
            .map(|value| value.with_timezone(&Utc))
        {
            return Some(reset_at);
        }
    }
    let observed_at = string_at(backoff, &["observed_at"])
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())?
        .with_timezone(&Utc);
    let retry_after_seconds = backoff
        .get("retry_after_seconds")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0);
    Duration::try_seconds(retry_after_seconds)
        .and_then(|duration| observed_at.checked_add_signed(duration))
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
    line(
        &mut out,
        format_args!(
            "purge successor repair required: {}",
            display_at(data, &["purge", "successor_repair_required"])
        ),
    );
    line(
        &mut out,
        format_args!(
            "user-created filesystem backups/snapshots: {}",
            display_at(data, &["purge", "unmanaged_filesystem_backups"])
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
            "reason={}, scope={}, retry_after_seconds={}, reset_at={}",
            display_at(backoff.unwrap(), &["reason"]),
            display_at(backoff.unwrap(), &["scope"]),
            display_at(backoff.unwrap(), &["retry_after_seconds"]),
            display_at(backoff.unwrap(), &["reset_at"])
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
        "embedding_artifacts" => "reacquire or replace the configured prepared model snapshot",
        "embedding_runtime" => "verify the local model runtime and its tokenizer contract",
        "embedding_generation" => "run qgh embed --force to publish a valid vector generation",
        "github_auth_reachability" => "check token source and GitHub host reachability",
        "rate_limit_headers" => "verify GitHub API responses include rate-limit headers",
        "repo_policy" => "update .qgh.toml or the selected profile repo allowlist",
        "profile_resolution" => "pass --profile or adjust profile allowlists",
        "purge" => "run qgh sync to retry pending qgh-managed cleanup",
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cleanup_warnings_do_not_claim_semantic_search_is_unavailable() {
        for code in [
            "embedding.generation_cleanup_failed",
            "embedding.tombstone_cleanup_failed",
        ] {
            assert!(!is_semantic_unavailable_warning(&json!({ "code": code })));
        }
    }

    #[test]
    fn runtime_and_snapshot_warnings_report_semantic_search_unavailable() {
        for code in [
            "embedding.sync_tokenizer_failed",
            "publication.embedding_snapshot_mismatch",
        ] {
            assert!(is_semantic_unavailable_warning(&json!({ "code": code })));
        }
    }

    #[test]
    fn legacy_backoff_status_does_not_invent_a_different_retry_command() {
        let data = json!({
            "sync": {
                "backoff": {
                    "retry_after_seconds": 0,
                    "observed_at": "2026-01-01T00:00:00Z"
                }
            }
        });
        let action = status_next_action(&data, "work".to_string(), "ready");
        assert_eq!(action, "retry the interrupted sync command now");
        assert!(!action.contains("qgh sync --profile"));
    }
}
