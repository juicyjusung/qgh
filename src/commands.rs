use crate::config::{load_profile, resolve_token};
use crate::error::QghError;
use crate::github;
use crate::index;
use crate::model::StoredIssue;
use crate::store::Store;
use serde_json::{json, Value};

pub async fn sync(profile_id: &str) -> Result<Value, QghError> {
    let profile = load_profile(profile_id)?;
    let token = resolve_token(&profile)?;
    let fetched = github::fetch_issues(&profile, &token).await?;
    let mut store = Store::open(&profile.paths)?;
    let summary = store.upsert_issues(&fetched.issues, fetched.skipped_pull_requests)?;
    let issues = store.active_issues()?;
    let generation = store.next_index_generation()?;
    index::rebuild(
        &profile.paths.index_root,
        &profile.paths.index_active,
        generation,
        &issues,
    )?;
    store.mark_index_published(
        generation,
        &profile.paths.index_active.to_string_lossy(),
        issues.len(),
    )?;
    let status = store.status()?;
    Ok(json!({
        "profile_id": profile.id,
        "sync_run_id": summary.sync_run_id,
        "issues": {
            "fetched": summary.fetched,
            "upserted": summary.upserted,
            "skipped_pull_requests": summary.skipped_pull_requests
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
        let Some(issue) = store.get_issue(&hit.source_id)? else {
            continue;
        };
        results.push(issue_result(issue, hit.score));
    }
    Ok(json!({
        "profile_id": profile.id,
        "results": results
    }))
}

pub fn get(profile_id: &str, source_id: &str) -> Result<Value, QghError> {
    let profile = load_profile(profile_id)?;
    let store = Store::open(&profile.paths)?;
    let Some(issue) = store.get_issue(source_id)? else {
        return Err(QghError::source_not_found(source_id));
    };
    Ok(json!({
        "profile_id": profile.id,
        "source": {
            "source_id": issue.source_id,
            "entity_type": "issue",
            "repo": issue.repo,
            "issue_number": issue.number,
            "title": issue.title,
            "body": issue.body,
            "canonical_url": issue.canonical_url,
            "source_version": issue.source_version
        }
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

fn issue_result(issue: StoredIssue, score: f32) -> Value {
    json!({
        "source_id": issue.source_id,
        "entity_type": "issue",
        "snippet": snippet(&issue.body),
        "canonical_url": issue.canonical_url,
        "get_args": {
            "source_id": issue.source_id
        },
        "repo": issue.repo,
        "issue_number": issue.number,
        "title": issue.title,
        "parent": null,
        "source_version": issue.source_version,
        "ranking": {
            "lexical_score": score
        }
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
