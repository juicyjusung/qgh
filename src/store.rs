use crate::error::QghError;
use crate::model::{
    BackoffView, CommentRecord, CursorUpdate, CursorView, IndexSource, IssueRecord,
    ParentIssueView, ReconciliationCandidate, ReconciliationRunView, SourceVersionView,
    StatusSnapshot, StoredComment, StoredCursor, StoredIssue, StoredSource, SyncSummary,
    TombstoneView,
};
use crate::paths::ProfilePaths;
use crate::paths::{ensure_private_dir, set_private_file};
use crate::time::{now_rfc3339, now_run_id_suffix};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(paths: &ProfilePaths) -> Result<Self, QghError> {
        ensure_private_dir(&paths.profile_dir)?;
        ensure_private_dir(&paths.cache_dir)?;
        ensure_private_dir(&paths.log_dir)?;
        let conn = Connection::open(&paths.db_path)?;
        set_private_file(&paths.db_path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        let mut store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    pub fn upsert_sources(
        &mut self,
        issues: &[IssueRecord],
        comments: &[CommentRecord],
        skipped_pull_requests: usize,
        cursor_updates: &[CursorUpdate],
    ) -> Result<SyncSummary, QghError> {
        let sync_run_id = format!("sync-{}", now_run_id_suffix());
        let now = now_rfc3339();
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO sync_runs
                (id, started_at, completed_at, fetched_issue_count, upserted_issue_count, fetched_comment_count, upserted_comment_count, skipped_pull_request_count)
             VALUES (?1, ?2, ?2, ?3, ?3, ?4, ?4, ?5)",
            params![
                sync_run_id,
                now,
                issues.len() as i64,
                comments.len() as i64,
                skipped_pull_requests as i64
            ],
        )?;

        for issue in issues {
            tx.execute(
                "INSERT INTO source_entities
                    (source_id, entity_type, host, repo, node_id, github_id, lifecycle_state, created_at, updated_at, last_seen_at)
                 VALUES (?1, 'issue', ?2, ?3, ?4, ?5, 'active', ?6, ?7, ?8)
                 ON CONFLICT(source_id) DO UPDATE SET
                    repo = excluded.repo,
                    lifecycle_state = 'active',
                    updated_at = excluded.updated_at,
                    last_seen_at = excluded.last_seen_at",
                params![
                    issue.source_id,
                    issue.host,
                    issue.repo,
                    issue.node_id,
                    issue.github_id,
                    issue.created_at,
                    issue.updated_at,
                    now
                ],
            )?;
            tx.execute(
                "DELETE FROM tombstones WHERE source_id = ?1",
                params![issue.source_id],
            )?;
            tx.execute(
                "INSERT INTO repositories (repo, host, owner, name)
                 VALUES (?1, ?2, substr(?1, 1, instr(?1, '/') - 1), substr(?1, instr(?1, '/') + 1))
                 ON CONFLICT(repo) DO UPDATE SET host = excluded.host",
                params![issue.repo, issue.host],
            )?;
            let version_id = upsert_source_version(
                &tx,
                &issue.source_id,
                &issue.body_hash,
                &issue.updated_at,
                &issue.indexed_at,
                &sync_run_id,
            )?;
            tx.execute(
                "INSERT INTO issue_metadata
                    (source_id, repo, issue_number, title, body, state, labels_json, milestone, assignees_json, author, created_at, updated_at, closed_at, canonical_url, latest_version_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
                 ON CONFLICT(source_id) DO UPDATE SET
                    repo = excluded.repo,
                    issue_number = excluded.issue_number,
                    title = excluded.title,
                    body = excluded.body,
                    state = excluded.state,
                    labels_json = excluded.labels_json,
                    milestone = excluded.milestone,
                    assignees_json = excluded.assignees_json,
                    author = excluded.author,
                    created_at = excluded.created_at,
                    updated_at = excluded.updated_at,
                    closed_at = excluded.closed_at,
                    canonical_url = excluded.canonical_url,
                    latest_version_id = excluded.latest_version_id",
                params![
                    issue.source_id,
                    issue.repo,
                    issue.number,
                    issue.title,
                    issue.body,
                    issue.state,
                    serde_json::to_string(&issue.labels).unwrap(),
                    issue.milestone,
                    serde_json::to_string(&issue.assignees).unwrap(),
                    issue.author,
                    issue.created_at,
                    issue.updated_at,
                    issue.closed_at,
                    issue.canonical_url,
                    version_id
                ],
            )?;
            upsert_alias(&tx, &issue.source_id, "canonical_url", &issue.canonical_url)?;
            upsert_alias(
                &tx,
                &issue.source_id,
                "issue_number",
                &issue.number.to_string(),
            )?;
            upsert_alias(&tx, &issue.source_id, "title", &issue.title)?;
            tx.execute(
                "INSERT INTO index_tasks (source_id, task_type, created_at, completed_at)
                 VALUES (?1, 'upsert', ?2, NULL)",
                params![issue.source_id, now],
            )?;
        }

        for comment in comments {
            tx.execute(
                "INSERT INTO source_entities
                    (source_id, entity_type, host, repo, node_id, github_id, lifecycle_state, created_at, updated_at, last_seen_at)
                 VALUES (?1, 'issue_comment', ?2, ?3, ?4, ?5, 'active', ?6, ?7, ?8)
                 ON CONFLICT(source_id) DO UPDATE SET
                    repo = excluded.repo,
                    lifecycle_state = 'active',
                    updated_at = excluded.updated_at,
                    last_seen_at = excluded.last_seen_at",
                params![
                    comment.source_id,
                    comment.host,
                    comment.repo,
                    comment.node_id,
                    comment.github_id,
                    comment.created_at,
                    comment.updated_at,
                    now
                ],
            )?;
            tx.execute(
                "DELETE FROM tombstones WHERE source_id = ?1",
                params![comment.source_id],
            )?;
            let version_id = upsert_source_version(
                &tx,
                &comment.source_id,
                &comment.body_hash,
                &comment.updated_at,
                &comment.indexed_at,
                &sync_run_id,
            )?;
            tx.execute(
                "INSERT INTO comment_metadata
                    (source_id, repo, issue_number, body, author, created_at, updated_at, canonical_url, parent_issue_source_id, parent_issue_title, parent_issue_canonical_url, latest_version_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                 ON CONFLICT(source_id) DO UPDATE SET
                    repo = excluded.repo,
                    issue_number = excluded.issue_number,
                    body = excluded.body,
                    author = excluded.author,
                    created_at = excluded.created_at,
                    updated_at = excluded.updated_at,
                    canonical_url = excluded.canonical_url,
                    parent_issue_source_id = excluded.parent_issue_source_id,
                    parent_issue_title = excluded.parent_issue_title,
                    parent_issue_canonical_url = excluded.parent_issue_canonical_url,
                    latest_version_id = excluded.latest_version_id",
                params![
                    comment.source_id,
                    comment.repo,
                    comment.parent_issue_number,
                    comment.body,
                    comment.author,
                    comment.created_at,
                    comment.updated_at,
                    comment.canonical_url,
                    comment.parent_issue_source_id,
                    comment.parent_issue_title,
                    comment.parent_issue_canonical_url,
                    version_id
                ],
            )?;
            upsert_alias(
                &tx,
                &comment.source_id,
                "canonical_url",
                &comment.canonical_url,
            )?;
            upsert_alias(
                &tx,
                &comment.source_id,
                "rest_id",
                &comment.github_id.to_string(),
            )?;
            tx.execute(
                "INSERT INTO index_tasks (source_id, task_type, created_at, completed_at)
                 VALUES (?1, 'upsert', ?2, NULL)",
                params![comment.source_id, now],
            )?;
        }

        for cursor in cursor_updates {
            tx.execute(
                "INSERT INTO sync_cursors (endpoint, cursor, etag)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(endpoint) DO UPDATE SET
                    cursor = coalesce(excluded.cursor, sync_cursors.cursor),
                    etag = coalesce(excluded.etag, sync_cursors.etag)",
                params![cursor.endpoint, cursor.cursor, cursor.etag],
            )?;
            if let Some(repo) = issue_repo_from_cursor_endpoint(&cursor.endpoint) {
                tx.execute(
                    "INSERT INTO repository_sync_state (repo, last_successful_sync_at)
                     VALUES (?1, ?2)
                     ON CONFLICT(repo) DO UPDATE SET
                        last_successful_sync_at = excluded.last_successful_sync_at",
                    params![repo, now],
                )?;
            }
        }

        tx.commit()?;
        let cursor_views = cursor_updates
            .iter()
            .map(|cursor| CursorView {
                endpoint: cursor.endpoint.clone(),
                watermark: cursor.cursor.clone(),
                has_etag: cursor.etag.is_some(),
            })
            .collect::<Vec<_>>();
        Ok(SyncSummary {
            sync_run_id,
            fetched_issues: issues.len(),
            upserted_issues: issues.len(),
            fetched_comments: comments.len(),
            upserted_comments: comments.len(),
            skipped_pull_requests,
            cursor_updates: cursor_views,
            not_modified_endpoints: cursor_updates
                .iter()
                .filter(|cursor| cursor.not_modified)
                .count(),
        })
    }

    pub fn active_issues(&self) -> Result<Vec<StoredIssue>, QghError> {
        let mut stmt = self.conn.prepare(
            "SELECT im.source_id, im.repo, im.issue_number, im.title, im.body, im.state,
                    im.labels_json, im.author, im.canonical_url,
                    sv.body_hash, sv.github_updated_at, sv.indexed_at, sv.sync_run_id, sv.lifecycle_state
             FROM issue_metadata im
             JOIN source_entities se ON se.source_id = im.source_id
             JOIN source_versions sv ON sv.id = im.latest_version_id
             WHERE se.lifecycle_state = 'active'
             ORDER BY im.repo, im.issue_number",
        )?;
        let rows = stmt.query_map([], stored_issue_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(QghError::from)
    }

    pub fn active_index_sources(&self) -> Result<Vec<IndexSource>, QghError> {
        let mut sources = Vec::new();
        for issue in self.active_issues()? {
            sources.push(IndexSource {
                source_id: issue.source_id,
                entity_type: "issue".to_string(),
                repo: issue.repo,
                issue_number: issue.number,
                state: issue.state,
                labels: issue.labels,
                author: issue.author,
                title: issue.title,
                body: issue.body,
                parent_issue_title: String::new(),
                github_updated_at: issue.source_version.github_updated_at,
                indexed_at: issue.source_version.indexed_at,
            });
        }
        let mut stmt = self.conn.prepare(
            "SELECT cm.source_id, cm.repo, cm.issue_number, cm.body, cm.author,
                    cm.parent_issue_title, sv.github_updated_at, sv.indexed_at
             FROM comment_metadata cm
             JOIN source_entities se ON se.source_id = cm.source_id
             JOIN source_versions sv ON sv.id = cm.latest_version_id
             WHERE se.lifecycle_state = 'active'
             ORDER BY cm.repo, cm.issue_number, cm.source_id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(IndexSource {
                source_id: row.get(0)?,
                entity_type: "issue_comment".to_string(),
                repo: row.get(1)?,
                issue_number: row.get(2)?,
                state: String::new(),
                labels: Vec::new(),
                author: row.get(4)?,
                title: String::new(),
                body: row.get(3)?,
                parent_issue_title: row.get(5)?,
                github_updated_at: row.get(6)?,
                indexed_at: row.get(7)?,
            })
        })?;
        sources.extend(rows.collect::<Result<Vec<_>, _>>()?);
        Ok(sources)
    }

    pub fn get_issue(&self, source_id: &str) -> Result<Option<StoredIssue>, QghError> {
        self.conn
            .query_row(
                "SELECT im.source_id, im.repo, im.issue_number, im.title, im.body, im.state,
                        im.labels_json, im.author, im.canonical_url,
                        sv.body_hash, sv.github_updated_at, sv.indexed_at, sv.sync_run_id, sv.lifecycle_state
                 FROM issue_metadata im
                 JOIN source_entities se ON se.source_id = im.source_id
                 JOIN source_versions sv ON sv.id = im.latest_version_id
                 WHERE im.source_id = ?1 AND se.lifecycle_state = 'active'",
                params![source_id],
                stored_issue_from_row,
            )
            .optional()
            .map_err(QghError::from)
    }

    pub fn get_comment(&self, source_id: &str) -> Result<Option<StoredComment>, QghError> {
        self.conn
            .query_row(
                "SELECT cm.source_id, cm.repo, cm.issue_number, cm.body, cm.author,
                        cm.canonical_url, cm.parent_issue_source_id, cm.parent_issue_title,
                        cm.parent_issue_canonical_url, sv.body_hash, sv.github_updated_at, sv.indexed_at,
                        sv.sync_run_id, sv.lifecycle_state
                 FROM comment_metadata cm
                 JOIN source_entities se ON se.source_id = cm.source_id
                 JOIN source_versions sv ON sv.id = cm.latest_version_id
                 WHERE cm.source_id = ?1 AND se.lifecycle_state = 'active'",
                params![source_id],
                stored_comment_from_row,
            )
            .optional()
            .map_err(QghError::from)
    }

    pub fn get_source(&self, source_id: &str) -> Result<Option<StoredSource>, QghError> {
        if let Some(issue) = self.get_issue(source_id)? {
            return Ok(Some(StoredSource::Issue(issue)));
        }
        if let Some(comment) = self.get_comment(source_id)? {
            return Ok(Some(StoredSource::Comment(comment)));
        }
        Ok(None)
    }

    pub fn get_tombstone(&self, source_id: &str) -> Result<Option<TombstoneView>, QghError> {
        self.conn
            .query_row(
                "SELECT source_id, reason, observed_at FROM tombstones WHERE source_id = ?1",
                params![source_id],
                |row| {
                    Ok(TombstoneView {
                        source_id: row.get(0)?,
                        reason: row.get(1)?,
                        observed_at: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(QghError::from)
    }

    pub fn get_reconciliation_candidate(
        &self,
        source_id: &str,
    ) -> Result<Option<ReconciliationCandidate>, QghError> {
        self.conn
            .query_row(
                "SELECT se.source_id, se.entity_type, se.repo,
                        coalesce(im.issue_number, cm.issue_number) AS issue_number,
                        se.github_id
                 FROM source_entities se
                 LEFT JOIN issue_metadata im ON im.source_id = se.source_id
                 LEFT JOIN comment_metadata cm ON cm.source_id = se.source_id
                 WHERE se.source_id = ?1 AND se.lifecycle_state = 'active'",
                params![source_id],
                reconciliation_candidate_from_row,
            )
            .optional()
            .map_err(QghError::from)
    }

    pub fn active_reconciliation_candidates(
        &self,
    ) -> Result<Vec<ReconciliationCandidate>, QghError> {
        let mut stmt = self.conn.prepare(
            "SELECT se.source_id, se.entity_type, se.repo,
                    coalesce(im.issue_number, cm.issue_number) AS issue_number,
                    se.github_id
             FROM source_entities se
             LEFT JOIN issue_metadata im ON im.source_id = se.source_id
             LEFT JOIN comment_metadata cm ON cm.source_id = se.source_id
             WHERE se.lifecycle_state = 'active'
             ORDER BY se.repo, issue_number, se.entity_type, se.source_id",
        )?;
        let rows = stmt.query_map([], reconciliation_candidate_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(QghError::from)
    }

    pub fn tombstone_source(
        &mut self,
        source_id: &str,
        reason: &str,
    ) -> Result<TombstoneView, QghError> {
        let observed_at = now_rfc3339();
        let tx = self.conn.transaction()?;
        let changed = tx.execute(
            "UPDATE source_entities
             SET lifecycle_state = 'tombstoned'
             WHERE source_id = ?1 AND lifecycle_state = 'active'",
            params![source_id],
        )?;
        tx.execute(
            "UPDATE source_versions
             SET lifecycle_state = 'tombstoned'
             WHERE source_id = ?1",
            params![source_id],
        )?;
        tx.execute(
            "UPDATE source_aliases
             SET is_current = 0
             WHERE source_id = ?1",
            params![source_id],
        )?;
        tx.execute(
            "INSERT INTO tombstones (source_id, reason, observed_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(source_id) DO UPDATE SET
                reason = excluded.reason,
                observed_at = excluded.observed_at",
            params![source_id, reason, observed_at],
        )?;
        if changed > 0 {
            tx.execute(
                "INSERT INTO index_tasks (source_id, task_type, created_at, completed_at)
                 VALUES (?1, 'delete', ?2, NULL)",
                params![source_id, observed_at],
            )?;
        }
        tx.commit()?;
        Ok(TombstoneView {
            source_id: source_id.to_string(),
            reason: reason.to_string(),
            observed_at,
        })
    }

    pub fn find_issue_by_canonical_url(
        &self,
        canonical_url: &str,
    ) -> Result<Option<StoredIssue>, QghError> {
        self.conn
            .query_row(
                "SELECT im.source_id, im.repo, im.issue_number, im.title, im.body, im.state,
                        im.labels_json, im.author, im.canonical_url,
                        sv.body_hash, sv.github_updated_at, sv.indexed_at, sv.sync_run_id, sv.lifecycle_state
                 FROM issue_metadata im
                 JOIN source_entities se ON se.source_id = im.source_id
                 JOIN source_versions sv ON sv.id = im.latest_version_id
                 WHERE im.canonical_url = ?1 AND se.lifecycle_state = 'active'",
                params![canonical_url],
                stored_issue_from_row,
            )
            .optional()
            .map_err(QghError::from)
    }

    pub fn find_comment_by_canonical_url(
        &self,
        canonical_url: &str,
    ) -> Result<Option<StoredComment>, QghError> {
        self.conn
            .query_row(
                "SELECT cm.source_id, cm.repo, cm.issue_number, cm.body, cm.author,
                        cm.canonical_url, cm.parent_issue_source_id, cm.parent_issue_title,
                        cm.parent_issue_canonical_url, sv.body_hash, sv.github_updated_at, sv.indexed_at,
                        sv.sync_run_id, sv.lifecycle_state
                 FROM comment_metadata cm
                 JOIN source_entities se ON se.source_id = cm.source_id
                 JOIN source_versions sv ON sv.id = cm.latest_version_id
                 WHERE cm.canonical_url = ?1 AND se.lifecycle_state = 'active'",
                params![canonical_url],
                stored_comment_from_row,
            )
            .optional()
            .map_err(QghError::from)
    }

    pub fn find_issue_by_repo_number(
        &self,
        repo: &str,
        issue_number: i64,
    ) -> Result<Option<StoredIssue>, QghError> {
        self.conn
            .query_row(
                "SELECT im.source_id, im.repo, im.issue_number, im.title, im.body, im.state,
                        im.labels_json, im.author, im.canonical_url,
                        sv.body_hash, sv.github_updated_at, sv.indexed_at, sv.sync_run_id, sv.lifecycle_state
                 FROM issue_metadata im
                 JOIN source_entities se ON se.source_id = im.source_id
                 JOIN source_versions sv ON sv.id = im.latest_version_id
                 WHERE im.repo = ?1 AND im.issue_number = ?2 AND se.lifecycle_state = 'active'",
                params![repo, issue_number],
                stored_issue_from_row,
            )
            .optional()
            .map_err(QghError::from)
    }

    pub fn find_issues_by_number(&self, issue_number: i64) -> Result<Vec<StoredIssue>, QghError> {
        let mut stmt = self.conn.prepare(
            "SELECT im.source_id, im.repo, im.issue_number, im.title, im.body, im.state,
                    im.labels_json, im.author, im.canonical_url,
                    sv.body_hash, sv.github_updated_at, sv.indexed_at, sv.sync_run_id, sv.lifecycle_state
             FROM issue_metadata im
             JOIN source_entities se ON se.source_id = im.source_id
             JOIN source_versions sv ON sv.id = im.latest_version_id
             WHERE im.issue_number = ?1 AND se.lifecycle_state = 'active'
             ORDER BY im.repo",
        )?;
        let rows = stmt.query_map(params![issue_number], stored_issue_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(QghError::from)
    }

    pub fn sync_cursors(&self) -> Result<Vec<StoredCursor>, QghError> {
        let mut stmt = self
            .conn
            .prepare("SELECT endpoint, cursor, etag FROM sync_cursors ORDER BY endpoint")?;
        let rows = stmt.query_map([], |row| {
            Ok(StoredCursor {
                endpoint: row.get(0)?,
                cursor: row.get(1)?,
                etag: row.get(2)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(QghError::from)
    }

    pub fn cursor_views(&self) -> Result<Vec<CursorView>, QghError> {
        Ok(self
            .sync_cursors()?
            .into_iter()
            .map(|cursor| CursorView {
                endpoint: cursor.endpoint,
                watermark: cursor.cursor,
                has_etag: cursor.etag.is_some(),
            })
            .collect())
    }

    pub fn oldest_successful_sync_at_for_repos(
        &self,
        repos: &[String],
    ) -> Result<Option<String>, QghError> {
        let mut oldest: Option<String> = None;
        for repo in repos {
            let repo_sync_at: Option<String> = self
                .conn
                .query_row(
                    "SELECT last_successful_sync_at
                     FROM repository_sync_state
                     WHERE repo = ?1",
                    params![repo],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(repo_sync_at) = repo_sync_at else {
                return Ok(None);
            };
            if oldest
                .as_ref()
                .is_none_or(|current| repo_sync_at < *current)
            {
                oldest = Some(repo_sync_at);
            }
        }
        Ok(oldest)
    }

    pub fn mark_index_published(
        &mut self,
        generation: i64,
        path: &str,
        source_count: usize,
    ) -> Result<(), QghError> {
        let now = now_rfc3339();
        let tx = self.conn.transaction()?;
        tx.execute("UPDATE index_generations SET active = 0", [])?;
        tx.execute(
            "INSERT INTO index_generations (generation, path, source_count, created_at, active)
             VALUES (?1, ?2, ?3, ?4, 1)
             ON CONFLICT(generation) DO UPDATE SET
                path = excluded.path,
                source_count = excluded.source_count,
                created_at = excluded.created_at,
                active = 1",
            params![generation, path, source_count as i64, now],
        )?;
        tx.execute(
            "UPDATE index_tasks SET completed_at = ?1 WHERE completed_at IS NULL",
            params![now],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn active_index_path(&self) -> Result<Option<String>, QghError> {
        self.conn
            .query_row(
                "SELECT path FROM index_generations WHERE active = 1 ORDER BY generation DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(QghError::from)
    }

    pub fn record_reconciliation_run(
        &self,
        mode: &str,
        checked_source_count: usize,
        tombstoned_count: usize,
        estimated_api_cost_class: &str,
    ) -> Result<(), QghError> {
        let now = now_rfc3339();
        self.conn.execute(
            "INSERT INTO reconciliation_runs
                (id, mode, started_at, completed_at, checked_source_count, tombstoned_count, estimated_api_cost_class)
             VALUES (?1, ?2, ?3, ?3, ?4, ?5, ?6)",
            params![
                format!("reconcile-{}", now_run_id_suffix()),
                mode,
                now,
                checked_source_count as i64,
                tombstoned_count as i64,
                estimated_api_cost_class
            ],
        )?;
        Ok(())
    }

    pub fn record_backoff_state(
        &self,
        reason: &str,
        scope: &str,
        retry_after_seconds: i64,
        reset_at: Option<&str>,
    ) -> Result<BackoffView, QghError> {
        let observed_at = now_rfc3339();
        let last_successful_sync: Option<String> = self
            .conn
            .query_row(
                "SELECT completed_at FROM sync_runs ORDER BY completed_at DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        self.conn.execute(
            "INSERT INTO sync_backoff_state
                (id, reason, scope, retry_after_seconds, reset_at, observed_at, last_successful_sync)
             VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET
                reason = excluded.reason,
                scope = excluded.scope,
                retry_after_seconds = excluded.retry_after_seconds,
                reset_at = excluded.reset_at,
                observed_at = excluded.observed_at,
                last_successful_sync = excluded.last_successful_sync",
            params![
                reason,
                scope,
                retry_after_seconds,
                reset_at,
                observed_at,
                last_successful_sync
            ],
        )?;
        Ok(BackoffView {
            reason: reason.to_string(),
            scope: scope.to_string(),
            retry_after_seconds,
            reset_at: reset_at.map(ToString::to_string),
            observed_at,
            last_successful_sync,
        })
    }

    pub fn clear_backoff_state(&self) -> Result<(), QghError> {
        self.conn.execute("DELETE FROM sync_backoff_state", [])?;
        Ok(())
    }

    pub fn reserve_index_generation(
        &mut self,
        index_root: &Path,
        source_count: usize,
    ) -> Result<(i64, PathBuf), QghError> {
        let now = now_rfc3339();
        let tx = self.conn.transaction()?;
        let current: Option<i64> = tx
            .query_row("SELECT max(generation) FROM index_generations", [], |row| {
                row.get(0)
            })
            .optional()?
            .flatten();
        let generation = current.unwrap_or(0) + 1;
        let generation_path = index_root.join(format!("generation-{generation}"));
        tx.execute(
            "INSERT INTO index_generations (generation, path, source_count, created_at, active)
             VALUES (?1, ?2, ?3, ?4, 0)",
            params![
                generation,
                generation_path.to_string_lossy(),
                source_count as i64,
                now
            ],
        )?;
        tx.commit()?;
        Ok((generation, generation_path))
    }

    pub fn status(&self) -> Result<StatusSnapshot, QghError> {
        let issue_count: i64 = self.conn.query_row(
            "SELECT count(*) FROM source_entities WHERE entity_type = 'issue' AND lifecycle_state = 'active'",
            [],
            |row| row.get(0),
        )?;
        let comment_count: i64 = self.conn.query_row(
            "SELECT count(*) FROM source_entities WHERE entity_type = 'issue_comment' AND lifecycle_state = 'active'",
            [],
            |row| row.get(0),
        )?;
        let tombstone_count: i64 =
            self.conn
                .query_row("SELECT count(*) FROM tombstones", [], |row| row.get(0))?;
        let active_generation: Option<i64> = self
            .conn
            .query_row(
                "SELECT generation FROM index_generations WHERE active = 1 ORDER BY generation DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let dirty_task_count: i64 = self.conn.query_row(
            "SELECT count(*) FROM index_tasks WHERE completed_at IS NULL",
            [],
            |row| row.get(0),
        )?;
        let last_sync_at: Option<String> = self
            .conn
            .query_row(
                "SELECT completed_at FROM sync_runs ORDER BY completed_at DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let last_reconciliation = self
            .conn
            .query_row(
                "SELECT completed_at, checked_source_count, tombstoned_count, estimated_api_cost_class
                 FROM reconciliation_runs
                 WHERE mode = 'full'
                 ORDER BY completed_at DESC
                 LIMIT 1",
                [],
                |row| {
                    Ok(ReconciliationRunView {
                        completed_at: row.get(0)?,
                        checked_source_count: row.get(1)?,
                        tombstoned_count: row.get(2)?,
                        estimated_api_cost_class: row.get(3)?,
                    })
                },
            )
            .optional()?;
        let backoff = self
            .conn
            .query_row(
                "SELECT reason, scope, retry_after_seconds, reset_at, observed_at, last_successful_sync
                 FROM sync_backoff_state
                 WHERE id = 1",
                [],
                |row| {
                    Ok(BackoffView {
                        reason: row.get(0)?,
                        scope: row.get(1)?,
                        retry_after_seconds: row.get(2)?,
                        reset_at: row.get(3)?,
                        observed_at: row.get(4)?,
                        last_successful_sync: row.get(5)?,
                    })
                },
            )
            .optional()?;
        Ok(StatusSnapshot {
            issue_count,
            comment_count,
            tombstone_count,
            active_generation: active_generation.unwrap_or(0),
            dirty_task_count,
            last_sync_at,
            last_reconciliation,
            backoff,
            cursors: self.cursor_views()?,
        })
    }

    fn migrate(&mut self) -> Result<(), QghError> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = self.migrate_inner();
        match result {
            Ok(()) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(())
            }
            Err(error) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    fn migrate_inner(&self) -> Result<(), QghError> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS profile_meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            INSERT INTO profile_meta (key, value)
                VALUES ('schema_version', 'qgh.db.v1')
                ON CONFLICT(key) DO UPDATE SET value = excluded.value;

            CREATE TABLE IF NOT EXISTS repositories (
                repo TEXT PRIMARY KEY,
                host TEXT NOT NULL,
                owner TEXT NOT NULL,
                name TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS source_entities (
                source_id TEXT PRIMARY KEY,
                entity_type TEXT NOT NULL,
                host TEXT NOT NULL,
                repo TEXT NOT NULL,
                node_id TEXT NOT NULL,
                github_id INTEGER NOT NULL,
                lifecycle_state TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                last_seen_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS source_versions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                source_id TEXT NOT NULL,
                body_hash TEXT NOT NULL,
                github_updated_at TEXT NOT NULL,
                indexed_at TEXT NOT NULL,
                sync_run_id TEXT NOT NULL,
                lifecycle_state TEXT NOT NULL,
                UNIQUE(source_id, body_hash, github_updated_at)
            );

            CREATE TABLE IF NOT EXISTS source_aliases (
                source_id TEXT NOT NULL,
                alias_type TEXT NOT NULL,
                alias_value TEXT NOT NULL,
                is_current INTEGER NOT NULL,
                UNIQUE(source_id, alias_type, alias_value)
            );

            CREATE TABLE IF NOT EXISTS issue_metadata (
                source_id TEXT PRIMARY KEY,
                repo TEXT NOT NULL,
                issue_number INTEGER NOT NULL,
                title TEXT NOT NULL,
                body TEXT NOT NULL,
                state TEXT NOT NULL,
                labels_json TEXT NOT NULL,
                milestone TEXT,
                assignees_json TEXT NOT NULL,
                author TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                closed_at TEXT,
                canonical_url TEXT NOT NULL,
                latest_version_id INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS comment_metadata (
                source_id TEXT PRIMARY KEY,
                repo TEXT NOT NULL,
                issue_number INTEGER NOT NULL,
                body TEXT NOT NULL,
                author TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                canonical_url TEXT NOT NULL,
                parent_issue_source_id TEXT NOT NULL,
                parent_issue_title TEXT NOT NULL,
                parent_issue_canonical_url TEXT NOT NULL,
                latest_version_id INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS chunks (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                source_id TEXT NOT NULL,
                source_version_id INTEGER NOT NULL,
                body TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS sync_runs (
                id TEXT PRIMARY KEY,
                started_at TEXT NOT NULL,
                completed_at TEXT NOT NULL,
                fetched_issue_count INTEGER NOT NULL,
                upserted_issue_count INTEGER NOT NULL,
                fetched_comment_count INTEGER NOT NULL DEFAULT 0,
                upserted_comment_count INTEGER NOT NULL DEFAULT 0,
                skipped_pull_request_count INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS sync_cursors (
                endpoint TEXT PRIMARY KEY,
                cursor TEXT,
                etag TEXT
            );

            CREATE TABLE IF NOT EXISTS repository_sync_state (
                repo TEXT PRIMARY KEY,
                last_successful_sync_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS sync_backoff_state (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                reason TEXT NOT NULL,
                scope TEXT NOT NULL,
                retry_after_seconds INTEGER NOT NULL,
                reset_at TEXT,
                observed_at TEXT NOT NULL,
                last_successful_sync TEXT
            );

            CREATE TABLE IF NOT EXISTS tombstones (
                source_id TEXT PRIMARY KEY,
                reason TEXT NOT NULL,
                observed_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS reconciliation_runs (
                id TEXT PRIMARY KEY,
                mode TEXT NOT NULL,
                started_at TEXT NOT NULL,
                completed_at TEXT NOT NULL,
                checked_source_count INTEGER NOT NULL,
                tombstoned_count INTEGER NOT NULL,
                estimated_api_cost_class TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS index_generations (
                generation INTEGER PRIMARY KEY,
                path TEXT NOT NULL,
                source_count INTEGER NOT NULL,
                created_at TEXT NOT NULL,
                active INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS index_tasks (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                source_id TEXT NOT NULL,
                task_type TEXT NOT NULL,
                created_at TEXT NOT NULL,
                completed_at TEXT
            );

            CREATE TABLE IF NOT EXISTS schema_migrations (
                version TEXT PRIMARY KEY,
                applied_at TEXT NOT NULL
            );
            "#,
        )?;
        ensure_column(
            &self.conn,
            "comment_metadata",
            "repo",
            "TEXT NOT NULL DEFAULT ''",
        )?;
        ensure_column(
            &self.conn,
            "comment_metadata",
            "issue_number",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        ensure_column(
            &self.conn,
            "comment_metadata",
            "body",
            "TEXT NOT NULL DEFAULT ''",
        )?;
        ensure_column(&self.conn, "comment_metadata", "author", "TEXT")?;
        ensure_column(
            &self.conn,
            "comment_metadata",
            "created_at",
            "TEXT NOT NULL DEFAULT ''",
        )?;
        ensure_column(
            &self.conn,
            "comment_metadata",
            "updated_at",
            "TEXT NOT NULL DEFAULT ''",
        )?;
        ensure_column(
            &self.conn,
            "comment_metadata",
            "canonical_url",
            "TEXT NOT NULL DEFAULT ''",
        )?;
        ensure_column(
            &self.conn,
            "comment_metadata",
            "parent_issue_title",
            "TEXT NOT NULL DEFAULT ''",
        )?;
        ensure_column(
            &self.conn,
            "comment_metadata",
            "parent_issue_canonical_url",
            "TEXT NOT NULL DEFAULT ''",
        )?;
        ensure_column(
            &self.conn,
            "comment_metadata",
            "latest_version_id",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        ensure_column(
            &self.conn,
            "sync_runs",
            "fetched_comment_count",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        ensure_column(
            &self.conn,
            "sync_runs",
            "upserted_comment_count",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        self.conn.execute(
            "INSERT OR IGNORE INTO repository_sync_state (repo, last_successful_sync_at)
             SELECT DISTINCT repo, (SELECT max(completed_at) FROM sync_runs)
             FROM source_entities
             WHERE (SELECT max(completed_at) FROM sync_runs) IS NOT NULL",
            [],
        )?;
        self.conn.execute(
            "INSERT INTO schema_migrations (version, applied_at)
             VALUES (?1, ?2)
             ON CONFLICT(version) DO NOTHING",
            params!["qgh.db.v1", now_rfc3339()],
        )?;
        Ok(())
    }
}

fn upsert_alias(
    tx: &rusqlite::Transaction<'_>,
    source_id: &str,
    alias_type: &str,
    alias_value: &str,
) -> Result<(), rusqlite::Error> {
    tx.execute(
        "INSERT INTO source_aliases (source_id, alias_type, alias_value, is_current)
         VALUES (?1, ?2, ?3, 1)
         ON CONFLICT(source_id, alias_type, alias_value) DO UPDATE SET is_current = 1",
        params![source_id, alias_type, alias_value],
    )?;
    Ok(())
}

fn upsert_source_version(
    tx: &rusqlite::Transaction<'_>,
    source_id: &str,
    body_hash: &str,
    github_updated_at: &str,
    indexed_at: &str,
    sync_run_id: &str,
) -> Result<i64, rusqlite::Error> {
    tx.execute(
        "INSERT INTO source_versions
            (source_id, body_hash, github_updated_at, indexed_at, sync_run_id, lifecycle_state)
         VALUES (?1, ?2, ?3, ?4, ?5, 'active')
         ON CONFLICT(source_id, body_hash, github_updated_at) DO UPDATE SET
            indexed_at = excluded.indexed_at,
            sync_run_id = excluded.sync_run_id,
            lifecycle_state = 'active'",
        params![
            source_id,
            body_hash,
            github_updated_at,
            indexed_at,
            sync_run_id
        ],
    )?;
    tx.query_row(
        "SELECT id FROM source_versions
         WHERE source_id = ?1 AND body_hash = ?2 AND github_updated_at = ?3",
        params![source_id, body_hash, github_updated_at],
        |row| row.get(0),
    )
}

fn issue_repo_from_cursor_endpoint(endpoint: &str) -> Option<&str> {
    endpoint.strip_prefix("issues:")
}

fn stored_issue_from_row(row: &rusqlite::Row<'_>) -> Result<StoredIssue, rusqlite::Error> {
    let labels_json: String = row.get(6)?;
    let labels = serde_json::from_str(&labels_json).unwrap_or_default();
    Ok(StoredIssue {
        source_id: row.get(0)?,
        repo: row.get(1)?,
        number: row.get(2)?,
        title: row.get(3)?,
        body: row.get(4)?,
        state: row.get(5)?,
        labels,
        author: row.get(7)?,
        canonical_url: row.get(8)?,
        source_version: SourceVersionView {
            body_hash: row.get(9)?,
            github_updated_at: row.get(10)?,
            indexed_at: row.get(11)?,
            sync_run_id: row.get(12)?,
            lifecycle_state: row.get(13)?,
        },
    })
}

fn stored_comment_from_row(row: &rusqlite::Row<'_>) -> Result<StoredComment, rusqlite::Error> {
    let repo: String = row.get(1)?;
    let issue_number: i64 = row.get(2)?;
    Ok(StoredComment {
        source_id: row.get(0)?,
        repo: repo.clone(),
        issue_number,
        body: row.get(3)?,
        author: row.get(4)?,
        canonical_url: row.get(5)?,
        parent_issue: ParentIssueView {
            source_id: row.get(6)?,
            repo,
            number: issue_number,
            title: row.get(7)?,
            canonical_url: row.get(8)?,
        },
        source_version: SourceVersionView {
            body_hash: row.get(9)?,
            github_updated_at: row.get(10)?,
            indexed_at: row.get(11)?,
            sync_run_id: row.get(12)?,
            lifecycle_state: row.get(13)?,
        },
    })
}

fn reconciliation_candidate_from_row(
    row: &rusqlite::Row<'_>,
) -> Result<ReconciliationCandidate, rusqlite::Error> {
    Ok(ReconciliationCandidate {
        source_id: row.get(0)?,
        entity_type: row.get(1)?,
        repo: row.get(2)?,
        issue_number: row.get(3)?,
        github_id: row.get(4)?,
    })
}

fn ensure_column(
    conn: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<(), QghError> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for existing in columns {
        if existing? == column {
            return Ok(());
        }
    }
    conn.execute(
        &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
        [],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn reserve_index_generation_allocates_distinct_inactive_rows() {
        let paths = temp_profile_paths("index-generation-reservation");
        let mut store = Store::open(&paths).unwrap();

        let (first_generation, first_path) = store
            .reserve_index_generation(&paths.index_root, 2)
            .unwrap();
        let (second_generation, second_path) = store
            .reserve_index_generation(&paths.index_root, 2)
            .unwrap();

        assert_eq!(first_generation, 1);
        assert_eq!(second_generation, 2);
        assert_ne!(first_path, second_path);
        assert_eq!(store.status().unwrap().active_generation, 0);

        store
            .mark_index_published(first_generation, &first_path.to_string_lossy(), 2)
            .unwrap();
        store
            .mark_index_published(second_generation, &second_path.to_string_lossy(), 2)
            .unwrap();

        assert_eq!(store.status().unwrap().active_generation, 2);
        let active_count: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM index_generations WHERE active = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(active_count, 1);

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    fn temp_profile_paths(name: &str) -> ProfilePaths {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let profile_dir = std::env::temp_dir().join(format!("qgh-store-{name}-{nanos}"));
        let cache_dir = profile_dir.join("cache");
        let log_dir = cache_dir.join("logs");
        let index_root = profile_dir.join("tantivy");
        ProfilePaths {
            config_file: profile_dir.join("config.toml"),
            db_path: profile_dir.join("qgh.sqlite3"),
            index_active: index_root.join("active"),
            index_root,
            log_dir,
            cache_dir,
            profile_dir,
        }
    }
}
