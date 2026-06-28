use crate::cli::{QueryArgs, ReconcileMode};
use crate::config::{load_profile, resolve_token};
use crate::error::QghError;
use crate::github;
use crate::index;
use crate::model::{StoredComment, StoredIssue, StoredSource};
use crate::store::Store;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};

pub async fn sync(profile_id: &str, reconcile: Option<ReconcileMode>) -> Result<Value, QghError> {
    let profile = load_profile(profile_id)?;
    let token = resolve_token(&profile)?;
    let mut store = Store::open(&profile.paths)?;
    let cursors = store.sync_cursors()?;
    let fetched = github::fetch_issues(&profile, &token, &cursors).await?;
    let summary = store.upsert_sources(
        &fetched.issues,
        &fetched.comments,
        fetched.skipped_pull_requests,
        &fetched.cursor_updates,
    )?;
    let reconciliation = if reconcile == Some(ReconcileMode::Full) {
        let candidates = store.active_reconciliation_candidates()?;
        let estimated_api_cost_class = estimate_api_cost_class(candidates.len());
        let result = github::reconcile_sources(&profile, &token, &candidates).await?;
        let mut tombstoned_sources = 0;
        for unavailable in result.unavailable_sources {
            store.tombstone_source(&unavailable.source_id, &unavailable.reason)?;
            tombstoned_sources += 1;
        }
        store.record_reconciliation_run(
            "full",
            result.checked_sources,
            tombstoned_sources,
            estimated_api_cost_class,
        )?;
        json!({
            "mode": "full",
            "checked_sources": result.checked_sources,
            "tombstoned_sources": tombstoned_sources,
            "estimated_api_cost_class": estimated_api_cost_class
        })
    } else {
        json!({
            "mode": "none"
        })
    };
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
    let watermarks = summary
        .cursor_updates
        .iter()
        .map(|cursor| (cursor.endpoint.clone(), json!(cursor.watermark)))
        .collect::<serde_json::Map<_, _>>();
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
        "cursors": {
            "updated": summary.cursor_updates.len(),
            "not_modified_endpoints": summary.not_modified_endpoints,
            "watermarks": watermarks
        },
        "index": {
            "active_generation": generation,
            "dirty_task_count": status.dirty_task_count
        },
        "reconciliation": reconciliation
    }))
}

pub fn query(profile_id: &str, args: QueryArgs) -> Result<Value, QghError> {
    let filters = QueryFilters::from_args(&args)?;
    let profile = load_profile(profile_id)?;
    let store = Store::open(&profile.paths)?;
    if let Some(results) = exact_results(&store, &args.query, &filters)? {
        return Ok(json!({
            "profile_id": profile.id,
            "result_filtering": {
                "unresolvable_hits": 0
            },
            "results": results
        }));
    }
    let hits = index::search(&profile.paths.index_active, &args.query, args.limit)?;
    let mut results = Vec::new();
    let mut unresolvable_hits = 0;
    for hit in hits {
        let Some(source) = store.get_source(&hit.source_id)? else {
            unresolvable_hits += 1;
            continue;
        };
        if !filters.matches(&source) {
            continue;
        }
        results.push(source_result(source, Ranking::Bm25(hit.score)));
    }
    Ok(json!({
        "profile_id": profile.id,
        "result_filtering": {
            "unresolvable_hits": unresolvable_hits
        },
        "results": results
    }))
}

#[derive(Debug)]
struct QueryFilters {
    repo: Option<String>,
    labels: Vec<String>,
    state: Option<String>,
    author: Option<String>,
    issue: Option<i64>,
}

impl QueryFilters {
    fn from_args(args: &QueryArgs) -> Result<Self, QghError> {
        if args.wiki.is_some() {
            return Err(QghError::validation(
                "validation.unsupported_filter",
                "Wiki filters are post-MVP and unsupported.",
            ));
        }
        if let Some(repo) = &args.repo {
            validate_repo(repo)?;
        }
        if let Some(state) = &args.state {
            if !matches!(state.as_str(), "open" | "closed") {
                return Err(QghError::validation(
                    "validation.invalid_state",
                    "State filter must be `open` or `closed`.",
                ));
            }
        }
        Ok(Self {
            repo: args.repo.clone(),
            labels: args.label.clone(),
            state: args.state.clone(),
            author: args.author.clone(),
            issue: args.issue,
        })
    }

    fn matches(&self, source: &StoredSource) -> bool {
        match source {
            StoredSource::Issue(issue) => {
                self.repo_matches(&issue.repo)
                    && self.issue_matches(issue.number)
                    && self.author_matches(issue.author.as_deref())
                    && self.state_matches(Some(&issue.state))
                    && self.labels.iter().all(|label| issue.labels.contains(label))
            }
            StoredSource::Comment(comment) => {
                self.repo_matches(&comment.repo)
                    && self.issue_matches(comment.issue_number)
                    && self.author_matches(comment.author.as_deref())
                    && self.state.is_none()
                    && self.labels.is_empty()
            }
        }
    }

    fn repo_matches(&self, repo: &str) -> bool {
        self.repo.as_deref().is_none_or(|expected| expected == repo)
    }

    fn issue_matches(&self, issue_number: i64) -> bool {
        self.issue.is_none_or(|expected| expected == issue_number)
    }

    fn author_matches(&self, author: Option<&str>) -> bool {
        self.author
            .as_deref()
            .is_none_or(|expected| author == Some(expected))
    }

    fn state_matches(&self, state: Option<&String>) -> bool {
        self.state
            .as_ref()
            .is_none_or(|expected| state == Some(expected))
    }
}

fn exact_results(
    store: &Store,
    query_text: &str,
    filters: &QueryFilters,
) -> Result<Option<Vec<Value>>, QghError> {
    if let Some(source) = exact_url_result(store, query_text)? {
        return Ok(Some(if filters.matches(&source) {
            vec![source_result(source, Ranking::Exact)]
        } else {
            Vec::new()
        }));
    }
    let issue_number = filters.issue.or_else(|| parse_issue_number(query_text));
    let Some(issue_number) = issue_number else {
        return Ok(None);
    };
    let matches = if let Some(repo) = &filters.repo {
        store
            .find_issue_by_repo_number(repo, issue_number)?
            .into_iter()
            .collect::<Vec<_>>()
    } else {
        store.find_issues_by_number(issue_number)?
    };
    if matches.len() > 1 {
        return Err(QghError::validation(
            "validation.ambiguous_locator",
            "Issue number matches multiple repos; add --repo.",
        ));
    }
    Ok(Some(
        matches
            .into_iter()
            .map(StoredSource::Issue)
            .filter(|source| filters.matches(source))
            .map(|source| source_result(source, Ranking::Exact))
            .collect(),
    ))
}

fn exact_url_result(store: &Store, query_text: &str) -> Result<Option<StoredSource>, QghError> {
    if !query_text.starts_with("https://github.com/") {
        return Ok(None);
    }
    if query_text.contains("#issuecomment-") {
        return store
            .find_comment_by_canonical_url(query_text)
            .map(|comment| comment.map(StoredSource::Comment));
    }
    store
        .find_issue_by_canonical_url(query_text)
        .map(|issue| issue.map(StoredSource::Issue))
}

fn parse_issue_number(query_text: &str) -> Option<i64> {
    query_text
        .strip_prefix('#')
        .unwrap_or(query_text)
        .parse::<i64>()
        .ok()
}

fn validate_repo(repo: &str) -> Result<(), QghError> {
    let Some((owner, name)) = repo.split_once('/') else {
        return Err(QghError::validation(
            "validation.invalid_repo",
            "Repo filter must use owner/repo format.",
        ));
    };
    if owner.is_empty() || name.is_empty() || name.contains('/') || repo.contains('*') {
        return Err(QghError::validation(
            "validation.invalid_repo",
            "Repo filter must use explicit owner/repo format.",
        ));
    }
    Ok(())
}

pub async fn get(profile_id: &str, source_id: &str) -> Result<Value, QghError> {
    let profile = load_profile(profile_id)?;
    let mut store = Store::open(&profile.paths)?;
    if let Some(tombstone) = store.get_tombstone(source_id)? {
        return Err(QghError::source_tombstoned(
            &tombstone.source_id,
            &tombstone.reason,
            &tombstone.observed_at,
        ));
    }
    let Some(source) = store.get_source(source_id)? else {
        return Err(QghError::source_not_found(source_id));
    };
    let mut source_json = match source {
        StoredSource::Issue(issue) => issue_source(issue),
        StoredSource::Comment(comment) => comment_source(comment),
    };
    source_json["lifecycle_check"] = match resolve_token(&profile) {
        Ok(token) => {
            if let Some(candidate) = store.get_reconciliation_candidate(source_id)? {
                match github::check_source_lifecycle(&profile, &token, &candidate).await {
                    Ok(github::LifecycleCheck::Active) => json!({ "status": "active" }),
                    Ok(github::LifecycleCheck::Unavailable { reason }) => {
                        let tombstone = store.tombstone_source(source_id, &reason)?;
                        return Err(QghError::source_tombstoned(
                            &tombstone.source_id,
                            &tombstone.reason,
                            &tombstone.observed_at,
                        ));
                    }
                    Err(error) => json!({
                        "status": "not_checked",
                        "error_code": error.code
                    }),
                }
            } else {
                json!({ "status": "not_checked", "reason": "missing_candidate" })
            }
        }
        Err(error) => json!({
            "status": "not_checked",
            "error_code": error.code
        }),
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
    let source_count = (status.issue_count + status.comment_count) as usize;
    let age_days = status
        .last_reconciliation
        .as_ref()
        .and_then(|run| age_days(&run.completed_at));
    let stale = profile
        .reconcile_after_days
        .is_some_and(|days| age_days.is_none_or(|age| age > days));
    let stale_warning = if stale {
        json!("reconciliation.stale")
    } else {
        Value::Null
    };
    let last_reconciliation = status.last_reconciliation.as_ref();
    let cursors = status
        .cursors
        .iter()
        .map(|cursor| {
            (
                cursor.endpoint.clone(),
                json!({
                    "watermark": cursor.watermark,
                    "has_etag": cursor.has_etag
                }),
            )
        })
        .collect::<serde_json::Map<_, _>>();
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
            "last_sync_at": status.last_sync_at,
            "cursors": cursors
        },
        "reconciliation": {
            "last_full_at": last_reconciliation.map(|run| run.completed_at.clone()),
            "age_days": age_days,
            "stale": stale,
            "stale_warning": stale_warning,
            "estimated_api_cost_class": estimate_api_cost_class(source_count),
            "last_checked_source_count": last_reconciliation.map(|run| run.checked_source_count),
            "last_tombstoned_count": last_reconciliation.map(|run| run.tombstoned_count),
            "last_estimated_api_cost_class": last_reconciliation.map(|run| run.estimated_api_cost_class.clone())
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

enum Ranking {
    Bm25(f32),
    Exact,
}

fn source_result(source: StoredSource, ranking: Ranking) -> Value {
    match source {
        StoredSource::Issue(issue) => issue_result(issue, ranking),
        StoredSource::Comment(comment) => comment_result(comment, ranking),
    }
}

fn issue_result(issue: StoredIssue, ranking: Ranking) -> Value {
    let source_id = issue.source_id;
    json!({
        "source_id": source_id,
        "entity_type": "issue",
        "repo": issue.repo,
        "issue_number": issue.number,
        "title": issue.title,
        "canonical_url": issue.canonical_url,
        "snippet": snippet(&issue.body),
        "get_args": {
            "source_id": source_id
        },
        "parent_issue": Value::Null,
        "source_version": issue.source_version,
        "ranking": ranking_json(ranking)
    })
}

fn comment_result(comment: StoredComment, ranking: Ranking) -> Value {
    let source_id = comment.source_id;
    json!({
        "source_id": source_id,
        "entity_type": "issue_comment",
        "repo": comment.repo,
        "issue_number": comment.issue_number,
        "author": comment.author,
        "canonical_url": comment.canonical_url,
        "parent_issue": comment.parent_issue,
        "snippet": snippet(&comment.body),
        "get_args": {
            "source_id": source_id
        },
        "source_version": comment.source_version,
        "ranking": ranking_json(ranking)
    })
}

fn ranking_json(ranking: Ranking) -> Value {
    match ranking {
        Ranking::Bm25(score) => json!({
            "kind": "bm25",
            "lexical_score": score
        }),
        Ranking::Exact => json!({
            "kind": "exact",
            "lexical_score": Value::Null
        }),
    }
}

fn estimate_api_cost_class(source_count: usize) -> &'static str {
    match source_count {
        0 => "none",
        1..=100 => "low",
        101..=1000 => "medium",
        _ => "high",
    }
}

fn age_days(timestamp: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(timestamp).ok().map(|parsed| {
        Utc::now()
            .signed_duration_since(parsed.with_timezone(&Utc))
            .num_days()
            .max(0)
    })
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
