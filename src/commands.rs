use crate::config::{load_profile, resolve_token};
use crate::error::QghError;
use crate::github;
use crate::index;
use crate::model::{StoredComment, StoredIssue, StoredSource};
use crate::store::Store;
use serde_json::{json, Value};

pub async fn sync(profile_id: &str) -> Result<Value, QghError> {
    let profile = load_profile(profile_id)?;
    let token = resolve_token(&profile)?;
    let fetched = github::fetch_issues(&profile, &token).await?;
    let mut store = Store::open(&profile.paths)?;
    let summary = store.upsert_sources(
        &fetched.issues,
        &fetched.comments,
        fetched.skipped_pull_requests,
    )?;
    let sources = store.active_index_sources()?;
    let generation = store.next_index_generation()?;
    index::rebuild(
        &profile.paths.index_root,
        &profile.paths.index_active,
        generation,
        &sources,
    )?;
    store.mark_index_published(
        generation,
        &profile.paths.index_active.to_string_lossy(),
        sources.len(),
    )?;
    let status = store.status()?;
    Ok(json!({
        "profile_id": profile.id,
        "sync_run_id": summary.sync_run_id,
        "issues": {
            "fetched": summary.fetched_issues,
            "upserted": summary.upserted_issues,
            "skipped_pull_requests": summary.skipped_pull_requests
        },
        "comments": {
            "fetched": summary.fetched_comments,
            "upserted": summary.upserted_comments
        },
        "index": {
            "active_generation": generation,
            "dirty_task_count": status.dirty_task_count
        }
    }))
}

pub fn query(profile_id: &str, query_text: &str, limit: usize) -> Result<Value, QghError> {
    let profile = load_profile(profile_id)?;
    let store = Store::open(&profile.paths)?;
    let hits = index::search(&profile.paths.index_active, query_text, limit)?;
    let mut results = Vec::new();
    for hit in hits {
        let Some(source) = store.get_source(&hit.source_id)? else {
            continue;
        };
        results.push(source_result(source, hit.score));
    }
    Ok(json!({
        "profile_id": profile.id,
        "results": results
    }))
}

pub fn get(profile_id: &str, source_id: &str) -> Result<Value, QghError> {
    let profile = load_profile(profile_id)?;
    let store = Store::open(&profile.paths)?;
    let Some(source) = store.get_source(source_id)? else {
        return Err(QghError::source_not_found(source_id));
    };
    let source_json = match source {
        StoredSource::Issue(issue) => issue_source(issue),
        StoredSource::Comment(comment) => comment_source(comment),
    };
    Ok(json!({
        "profile_id": profile.id,
        "source": source_json
    }))
}

pub fn status(profile_id: &str) -> Result<Value, QghError> {
    let profile = load_profile(profile_id)?;
    let store = Store::open(&profile.paths)?;
    let status = store.status()?;
    Ok(json!({
        "profile_id": profile.id,
        "github": {
            "host": profile.host,
            "api_base_url": profile.api_base_url,
            "web_base_url": profile.web_base_url
        },
        "paths": {
            "config": profile.paths.config_file,
            "profile_data": profile.paths.profile_dir,
            "database": profile.paths.db_path,
            "tantivy_index": profile.paths.index_active,
            "cache": profile.paths.cache_dir
        },
        "sources": {
            "issue_count": status.issue_count,
            "comment_count": status.comment_count,
            "tombstone_count": status.tombstone_count
        },
        "database": {
            "schema_version": "qgh.db.v1"
        },
        "index": {
            "active_generation": status.active_generation,
            "dirty_task_count": status.dirty_task_count
        },
        "sync": {
            "last_sync_at": status.last_sync_at
        }
    }))
}

pub fn doctor(profile_id: &str) -> Result<Value, QghError> {
    let profile = load_profile(profile_id)?;
    Ok(json!({
        "profile_id": profile.id,
        "checks": [
            {
                "name": "config",
                "ok": true
            }
        ]
    }))
}

fn source_result(source: StoredSource, score: f32) -> Value {
    match source {
        StoredSource::Issue(issue) => {
            let mut value = issue_source(issue);
            value["snippet"] = json!(snippet(value["body"].as_str().unwrap_or_default()));
            value["get_args"] = json!({ "source_id": value["source_id"] });
            value["parent_issue"] = Value::Null;
            value["ranking"] = json!({ "lexical_score": score });
            value
        }
        StoredSource::Comment(comment) => {
            let mut value = comment_source(comment);
            value["snippet"] = json!(snippet(value["body"].as_str().unwrap_or_default()));
            value["get_args"] = json!({ "source_id": value["source_id"] });
            value["ranking"] = json!({ "lexical_score": score });
            value
        }
    }
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
