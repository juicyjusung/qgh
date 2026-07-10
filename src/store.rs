use crate::chunking::MarkdownChunk;
use crate::embedding::{EmbeddingFingerprint, EmbeddingVector};
use crate::error::QghError;
use crate::model::{
    BackoffView, CommentRecord, CoverageSnapshot, CursorUpdate, CursorView, IndexSource,
    IssueRecord, ParentIssueView, ReconciliationCandidate, ReconciliationRunView,
    SourceVersionView, StatusSnapshot, StoredChunk, StoredComment, StoredCursor, StoredIssue,
    StoredSource, SyncSummary, TargetedSyncSummary, TombstoneView, VectorSearchFilters,
    VectorSearchHit,
};
use crate::paths::ProfilePaths;
use crate::paths::{ensure_private_dir, set_private_file};
use crate::time::{now_rfc3339, now_run_id_suffix};
use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
#[cfg(feature = "vector-search")]
use std::os::raw::{c_char, c_int};
use std::path::{Path, PathBuf};

const CHUNK_EMBEDDING_VECTORS_TABLE: &str = "chunk_embedding_vectors";
const CHUNK_EMBEDDING_VECTOR_ROWIDS_TABLE: &str = "chunk_embedding_vectors_rowids";
const CHUNK_EMBEDDING_VECTOR_CHUNKS_TABLE: &str = "chunk_embedding_vectors_vector_chunks00";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingGenerationSpec {
    pub model_manifest_hash: String,
    pub chunker_fingerprint: String,
    pub context_template_version: String,
    pub output_dimension: usize,
    pub source_sync_run_id: String,
    pub source_snapshot_hash: String,
    pub total_chunks: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EmbeddingGenerationChunk {
    pub chunk_id: i64,
    pub source_version_id: i64,
    pub source_version_hash: String,
    pub context_hash: String,
    pub vector: EmbeddingVector,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingGenerationChunkBlob {
    pub bytes: Vec<u8>,
    pub dimension: usize,
    pub checksum: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetrievalPublicationView {
    pub publication_id: i64,
    pub source_snapshot_sync_run_id: String,
    pub tantivy_generation: i64,
    pub embedding_generation_id: Option<i64>,
    pub model_manifest_hash: Option<String>,
    pub chunker_fingerprint: Option<String>,
    pub context_template_version: Option<String>,
    pub output_dimension: Option<usize>,
}

pub fn embedding_context_hash(
    model_manifest_hash: &str,
    chunker_fingerprint: &str,
    context_template_version: &str,
    embedding_input: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(model_manifest_hash.as_bytes());
    hasher.update([0]);
    hasher.update(chunker_fingerprint.as_bytes());
    hasher.update([0]);
    hasher.update(context_template_version.as_bytes());
    hasher.update([0]);
    hasher.update(embedding_input.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn new_sync_run_id() -> String {
        format!("sync-{}", now_run_id_suffix())
    }

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

    #[cfg(feature = "vector-search")]
    pub fn enable_vector(&mut self) -> Result<(), QghError> {
        register_sqlite_vec_extension(&self.conn)?;
        self.migrate_vector_schema()
    }

    #[cfg(not(feature = "vector-search"))]
    pub fn enable_vector(&mut self) -> Result<(), QghError> {
        Err(QghError::validation(
            "embedding.vector_capability_unavailable",
            "This qgh binary was built without the vector-search feature.",
        ))
    }

    pub fn upsert_sources(
        &mut self,
        issues: &[IssueRecord],
        comments: &[CommentRecord],
        skipped_pull_requests: usize,
        cursor_updates: &[CursorUpdate],
    ) -> Result<SyncSummary, QghError> {
        let sync_run_id = Self::new_sync_run_id();
        let summary = self.upsert_sources_for_run(
            &sync_run_id,
            issues,
            comments,
            skipped_pull_requests,
            cursor_updates,
        )?;
        self.mark_sync_run_completed(&sync_run_id)?;
        Ok(summary)
    }

    pub fn upsert_sources_for_run(
        &mut self,
        sync_run_id: &str,
        issues: &[IssueRecord],
        comments: &[CommentRecord],
        skipped_pull_requests: usize,
        cursor_updates: &[CursorUpdate],
    ) -> Result<SyncSummary, QghError> {
        let now = now_rfc3339();
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO sync_runs
                (id, started_at, completed_at, completed_successfully, fetched_issue_count, upserted_issue_count, fetched_comment_count, upserted_comment_count, skipped_pull_request_count)
             VALUES (?1, ?2, ?2, 0, ?3, ?3, ?4, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET
                completed_at = excluded.completed_at,
                completed_successfully = 0,
                fetched_issue_count = sync_runs.fetched_issue_count + excluded.fetched_issue_count,
                upserted_issue_count = sync_runs.upserted_issue_count + excluded.upserted_issue_count,
                fetched_comment_count = sync_runs.fetched_comment_count + excluded.fetched_comment_count,
                upserted_comment_count = sync_runs.upserted_comment_count + excluded.upserted_comment_count,
                skipped_pull_request_count = sync_runs.skipped_pull_request_count + excluded.skipped_pull_request_count",
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
                sync_run_id,
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
                sync_run_id,
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
            sync_run_id: sync_run_id.to_string(),
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

    pub fn mark_sync_run_completed(&self, sync_run_id: &str) -> Result<(), QghError> {
        let changed = self.conn.execute(
            "UPDATE sync_runs
             SET completed_at = ?1, completed_successfully = 1
             WHERE id = ?2",
            params![now_rfc3339(), sync_run_id],
        )?;
        if changed == 0 {
            return Err(QghError::storage(format!(
                "Cannot mark missing sync run `{sync_run_id}` completed."
            )));
        }
        Ok(())
    }

    pub fn upsert_target_issue_refresh(
        &mut self,
        issue: &IssueRecord,
        comments: &[CommentRecord],
    ) -> Result<TargetedSyncSummary, QghError> {
        let existing_comments =
            self.active_comment_versions_for_issue(&issue.repo, issue.number)?;
        let incoming_source_ids = comments
            .iter()
            .map(|comment| comment.source_id.clone())
            .collect::<BTreeSet<_>>();
        let added_comments = comments
            .iter()
            .filter(|comment| !existing_comments.contains_key(&comment.source_id))
            .count();
        let updated_comments = comments
            .iter()
            .filter(|comment| {
                existing_comments
                    .get(&comment.source_id)
                    .is_some_and(|body_hash| body_hash != &comment.body_hash)
            })
            .count();
        let deleted_source_ids = existing_comments
            .keys()
            .filter(|source_id| !incoming_source_ids.contains(*source_id))
            .cloned()
            .collect::<Vec<_>>();

        let summary = self.upsert_sources(std::slice::from_ref(issue), comments, 0, &[])?;
        let mut deleted_comments = 0;
        for source_id in deleted_source_ids {
            self.tombstone_source(&source_id, "deleted")?;
            deleted_comments += 1;
        }

        Ok(TargetedSyncSummary {
            sync_run_id: summary.sync_run_id,
            fetched_issues: 1,
            upserted_issues: 1,
            fetched_comments: comments.len(),
            upserted_comments: comments.len(),
            added_comments,
            updated_comments,
            deleted_comments,
            tombstoned_issues: 0,
            tombstoned_comments: deleted_comments,
        })
    }

    pub fn tombstone_target_issue_refresh(
        &mut self,
        repo: &str,
        issue_number: i64,
        reason: &str,
    ) -> Result<TargetedSyncSummary, QghError> {
        let (tombstoned_issues, tombstoned_comments) =
            self.tombstone_target_issue_sources(repo, issue_number, reason)?;
        let sync_run_id = self.record_empty_sync_run()?;
        Ok(TargetedSyncSummary {
            sync_run_id,
            fetched_issues: 0,
            upserted_issues: 0,
            fetched_comments: 0,
            upserted_comments: 0,
            added_comments: 0,
            updated_comments: 0,
            deleted_comments: tombstoned_comments,
            tombstoned_issues,
            tombstoned_comments,
        })
    }

    pub fn tombstone_target_issue_sources(
        &mut self,
        repo: &str,
        issue_number: i64,
        reason: &str,
    ) -> Result<(usize, usize), QghError> {
        let mut tombstoned_issues = 0;
        if let Some(issue) = self.find_issue_by_repo_number(repo, issue_number)? {
            self.tombstone_source(&issue.source_id, reason)?;
            tombstoned_issues += 1;
        }
        let comment_source_ids = self.active_comment_source_ids_for_issue(repo, issue_number)?;
        let mut tombstoned_comments = 0;
        for source_id in comment_source_ids {
            self.tombstone_source(&source_id, reason)?;
            tombstoned_comments += 1;
        }
        Ok((tombstoned_issues, tombstoned_comments))
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

    pub fn latest_source_version_id(&self, source_id: &str) -> Result<Option<i64>, QghError> {
        self.conn
            .query_row(
                "SELECT coalesce(im.latest_version_id, cm.latest_version_id)
                 FROM source_entities se
                 LEFT JOIN issue_metadata im ON im.source_id = se.source_id
                 LEFT JOIN comment_metadata cm ON cm.source_id = se.source_id
                 WHERE se.source_id = ?1 AND se.lifecycle_state = 'active'",
                params![source_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(QghError::from)
    }

    pub fn replace_chunks_for_source_version(
        &mut self,
        source_id: &str,
        source_version_id: i64,
        chunks: &[MarkdownChunk],
    ) -> Result<Vec<StoredChunk>, QghError> {
        let version_exists = self
            .conn
            .query_row(
                "SELECT 1 FROM source_versions WHERE id = ?1 AND source_id = ?2",
                params![source_version_id, source_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !version_exists {
            return Err(QghError::storage(format!(
                "Cannot store chunks for missing source version `{source_version_id}`."
            )));
        }

        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM chunks WHERE source_version_id = ?1",
            params![source_version_id],
        )?;
        for chunk in chunks {
            let heading_path_json =
                serde_json::to_string(&chunk.heading_path).map_err(|error| {
                    QghError::storage(format!("Failed to serialize chunk heading path: {error}"))
                })?;
            tx.execute(
                "INSERT INTO chunks
                    (source_id, source_version_id, body, chunk_index, token_start,
                     token_end, byte_start, byte_end, chunker_version,
                     chunker_fingerprint, heading_path_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    source_id,
                    source_version_id,
                    chunk.body,
                    chunk.chunk_index as i64,
                    chunk.token_start as i64,
                    chunk.token_end as i64,
                    chunk.byte_start as i64,
                    chunk.byte_end as i64,
                    chunk.chunker_version,
                    chunk.chunker_fingerprint,
                    heading_path_json,
                ],
            )?;
        }
        tx.commit()?;
        self.chunks_for_source_version(source_version_id)
    }

    pub fn chunks_for_source_version(
        &self,
        source_version_id: i64,
    ) -> Result<Vec<StoredChunk>, QghError> {
        if !embedding_schema_exists(&self.conn)? {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(
            "SELECT id, source_id, source_version_id, body, chunk_index, token_start,
                     token_end, byte_start, byte_end, chunker_version,
                     chunker_fingerprint, heading_path_json
             FROM chunks
             WHERE source_version_id = ?1
             ORDER BY id",
        )?;
        let rows = stmt.query_map(params![source_version_id], stored_chunk_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(QghError::from)
    }

    pub fn source_version_has_chunks(&self, source_version_id: i64) -> Result<bool, QghError> {
        if !embedding_schema_exists(&self.conn)? {
            return Ok(false);
        }
        let count: i64 = self.conn.query_row(
            "SELECT count(*) FROM chunks WHERE source_version_id = ?1",
            params![source_version_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn cleanup_inactive_embedding_artifacts(&mut self) -> Result<usize, QghError> {
        if !embedding_schema_exists(&self.conn)? {
            return Ok(0);
        }
        const STALE_CHUNK_FILTER: &str = "SELECT c.id
             FROM chunks c
             LEFT JOIN source_entities se ON se.source_id = c.source_id
             LEFT JOIN issue_metadata im ON im.source_id = c.source_id
             LEFT JOIN comment_metadata cm ON cm.source_id = c.source_id
             WHERE se.lifecycle_state IS NULL
                OR se.lifecycle_state != 'active'
                OR c.source_version_id != coalesce(im.latest_version_id, cm.latest_version_id, -1)";

        let vector_table_exists = vector_table_dimension(&self.conn)?.is_some();
        let tx = self.conn.transaction()?;
        if vector_table_exists {
            tx.execute(
                &format!(
                    "DELETE FROM {CHUNK_EMBEDDING_VECTORS_TABLE}
                     WHERE rowid NOT IN (SELECT id FROM chunks)
                        OR rowid IN ({STALE_CHUNK_FILTER})"
                ),
                [],
            )?;
        }
        tx.execute(
            &format!(
                "DELETE FROM chunk_embeddings
                 WHERE chunk_id IN ({STALE_CHUNK_FILTER})"
            ),
            [],
        )?;
        let deleted_chunks = tx.execute(
            &format!("DELETE FROM chunks WHERE id IN ({STALE_CHUNK_FILTER})"),
            [],
        )?;
        tx.commit()?;
        Ok(deleted_chunks)
    }

    pub fn active_embedding_chunks(&self) -> Result<Vec<StoredChunk>, QghError> {
        if !embedding_schema_exists(&self.conn)? {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(
            "SELECT c.id, c.source_id, c.source_version_id, c.body, c.chunk_index,
                     c.token_start, c.token_end, c.byte_start, c.byte_end,
                     c.chunker_version, c.chunker_fingerprint, c.heading_path_json
             FROM chunks c
             JOIN source_entities se ON se.source_id = c.source_id
             LEFT JOIN issue_metadata im ON im.source_id = c.source_id
             LEFT JOIN comment_metadata cm ON cm.source_id = c.source_id
             WHERE se.lifecycle_state = 'active'
               AND c.source_version_id = coalesce(im.latest_version_id, cm.latest_version_id)
             ORDER BY c.id",
        )?;
        let rows = stmt.query_map([], stored_chunk_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(QghError::from)
    }

    pub fn active_embedding_chunk_count(&self) -> Result<i64, QghError> {
        if !embedding_schema_exists(&self.conn)? {
            return Ok(0);
        }
        self.conn
            .query_row(
                "SELECT count(*)
                 FROM chunks c
                 JOIN source_entities se ON se.source_id = c.source_id
                 LEFT JOIN issue_metadata im ON im.source_id = c.source_id
                 LEFT JOIN comment_metadata cm ON cm.source_id = c.source_id
                 WHERE se.lifecycle_state = 'active'
                   AND c.source_version_id = coalesce(im.latest_version_id, cm.latest_version_id)",
                [],
                |row| row.get(0),
            )
            .map_err(QghError::from)
    }

    pub fn active_chunks_missing_embedding_for_fingerprint(
        &self,
        fingerprint: &EmbeddingFingerprint,
    ) -> Result<Vec<StoredChunk>, QghError> {
        if !embedding_schema_exists(&self.conn)? {
            return Ok(Vec::new());
        }
        let fingerprint_hash = fingerprint.hash();
        let mut stmt = self.conn.prepare(
            "SELECT c.id, c.source_id, c.source_version_id, c.body, c.chunk_index,
                     c.token_start, c.token_end, c.byte_start, c.byte_end,
                     c.chunker_version, c.chunker_fingerprint, c.heading_path_json
             FROM chunks c
             JOIN source_entities se ON se.source_id = c.source_id
             LEFT JOIN issue_metadata im ON im.source_id = c.source_id
             LEFT JOIN comment_metadata cm ON cm.source_id = c.source_id
             LEFT JOIN embedding_fingerprints ef ON ef.fingerprint_hash = ?1
             LEFT JOIN chunk_embeddings ce
                ON ce.chunk_id = c.id
               AND ce.fingerprint_id = ef.id
             WHERE se.lifecycle_state = 'active'
               AND c.source_version_id = coalesce(im.latest_version_id, cm.latest_version_id)
               AND ce.chunk_id IS NULL
             ORDER BY c.id",
        )?;
        let rows = stmt.query_map(params![fingerprint_hash], stored_chunk_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(QghError::from)
    }

    pub fn active_embedding_fingerprint(&self) -> Result<Option<EmbeddingFingerprint>, QghError> {
        if !embedding_schema_exists(&self.conn)? {
            return Ok(None);
        }
        let fingerprint_json = self
            .conn
            .query_row(
                "SELECT fingerprint_json
                 FROM embedding_fingerprints
                 WHERE active = 1
                 ORDER BY id DESC
                 LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        fingerprint_json
            .map(|value| {
                serde_json::from_str(&value).map_err(|error| {
                    QghError::storage(format!("Stored embedding fingerprint is invalid: {error}"))
                })
            })
            .transpose()
    }

    pub fn replace_all_chunk_embeddings(
        &mut self,
        fingerprint: &EmbeddingFingerprint,
        embeddings: &[(i64, EmbeddingVector)],
    ) -> Result<usize, QghError> {
        self.ensure_vector_storage(fingerprint.dimension)?;
        let fingerprint_hash = fingerprint.hash();
        let fingerprint_json = serde_json::to_string(fingerprint).map_err(|error| {
            QghError::storage(format!(
                "Failed to serialize embedding fingerprint: {error}"
            ))
        })?;
        let now = now_rfc3339();
        let tx = self.conn.transaction()?;
        tx.execute("UPDATE embedding_fingerprints SET active = 0", [])?;
        tx.execute(
            "INSERT INTO embedding_fingerprints
                (fingerprint_hash, fingerprint_json, provider, model_id, model_revision,
                 dimension, pooling, query_prefix, chunker_version, source_schema_version,
                 created_at, active)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 1)
             ON CONFLICT(fingerprint_hash) DO UPDATE SET
                fingerprint_json = excluded.fingerprint_json,
                provider = excluded.provider,
                model_id = excluded.model_id,
                model_revision = excluded.model_revision,
                dimension = excluded.dimension,
                pooling = excluded.pooling,
                query_prefix = excluded.query_prefix,
                chunker_version = excluded.chunker_version,
                source_schema_version = excluded.source_schema_version,
                active = 1",
            params![
                &fingerprint_hash,
                &fingerprint_json,
                &fingerprint.provider,
                &fingerprint.model_id,
                &fingerprint.model_revision,
                fingerprint.dimension as i64,
                fingerprint.pooling.as_str(),
                &fingerprint.query_prefix,
                &fingerprint.chunker_version,
                &fingerprint.source_schema_version,
                &now
            ],
        )?;
        let fingerprint_id: i64 = tx.query_row(
            "SELECT id FROM embedding_fingerprints WHERE fingerprint_hash = ?1",
            params![&fingerprint_hash],
            |row| row.get(0),
        )?;
        tx.execute("DELETE FROM chunk_embeddings", [])?;
        tx.execute(&format!("DELETE FROM {CHUNK_EMBEDDING_VECTORS_TABLE}"), [])?;
        for (chunk_id, vector) in embeddings {
            if vector.len() != fingerprint.dimension {
                return Err(QghError::storage(format!(
                    "Embedding vector dimension {} does not match fingerprint dimension {}.",
                    vector.len(),
                    fingerprint.dimension
                )));
            }
            let vector_json = serde_json::to_string(vector).map_err(|error| {
                QghError::storage(format!("Failed to serialize embedding vector: {error}"))
            })?;
            tx.execute(
                "INSERT INTO chunk_embeddings
                    (chunk_id, fingerprint_id, vector_json, embedded_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![chunk_id, fingerprint_id, vector_json, &now],
            )?;
            upsert_vector_row(&tx, *chunk_id, vector)?;
        }
        tx.commit()?;
        Ok(embeddings.len())
    }

    pub fn upsert_chunk_embeddings(
        &mut self,
        fingerprint: &EmbeddingFingerprint,
        embeddings: &[(i64, EmbeddingVector)],
    ) -> Result<usize, QghError> {
        self.ensure_vector_storage(fingerprint.dimension)?;
        let fingerprint_hash = fingerprint.hash();
        let fingerprint_json = serde_json::to_string(fingerprint).map_err(|error| {
            QghError::storage(format!(
                "Failed to serialize embedding fingerprint: {error}"
            ))
        })?;
        let now = now_rfc3339();
        let tx = self.conn.transaction()?;
        let previous_active_fingerprint_hash = tx
            .query_row(
                "SELECT fingerprint_hash
                 FROM embedding_fingerprints
                 WHERE active = 1
                 ORDER BY id DESC
                 LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        tx.execute("UPDATE embedding_fingerprints SET active = 0", [])?;
        tx.execute(
            "INSERT INTO embedding_fingerprints
                (fingerprint_hash, fingerprint_json, provider, model_id, model_revision,
                 dimension, pooling, query_prefix, chunker_version, source_schema_version,
                 created_at, active)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 1)
             ON CONFLICT(fingerprint_hash) DO UPDATE SET
                fingerprint_json = excluded.fingerprint_json,
                provider = excluded.provider,
                model_id = excluded.model_id,
                model_revision = excluded.model_revision,
                dimension = excluded.dimension,
                pooling = excluded.pooling,
                query_prefix = excluded.query_prefix,
                chunker_version = excluded.chunker_version,
                source_schema_version = excluded.source_schema_version,
                active = 1",
            params![
                &fingerprint_hash,
                &fingerprint_json,
                &fingerprint.provider,
                &fingerprint.model_id,
                &fingerprint.model_revision,
                fingerprint.dimension as i64,
                fingerprint.pooling.as_str(),
                &fingerprint.query_prefix,
                &fingerprint.chunker_version,
                &fingerprint.source_schema_version,
                &now
            ],
        )?;
        let fingerprint_id: i64 = tx.query_row(
            "SELECT id FROM embedding_fingerprints WHERE fingerprint_hash = ?1",
            params![&fingerprint_hash],
            |row| row.get(0),
        )?;
        if previous_active_fingerprint_hash.as_deref() != Some(fingerprint_hash.as_str()) {
            tx.execute(&format!("DELETE FROM {CHUNK_EMBEDDING_VECTORS_TABLE}"), [])?;
        }
        for (chunk_id, vector) in embeddings {
            if vector.len() != fingerprint.dimension {
                return Err(QghError::storage(format!(
                    "Embedding vector dimension {} does not match fingerprint dimension {}.",
                    vector.len(),
                    fingerprint.dimension
                )));
            }
            let vector_json = serde_json::to_string(vector).map_err(|error| {
                QghError::storage(format!("Failed to serialize embedding vector: {error}"))
            })?;
            tx.execute(
                "INSERT INTO chunk_embeddings
                    (chunk_id, fingerprint_id, vector_json, embedded_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(chunk_id, fingerprint_id) DO UPDATE SET
                    vector_json = excluded.vector_json,
                    embedded_at = excluded.embedded_at",
                params![chunk_id, fingerprint_id, vector_json, &now],
            )?;
            upsert_vector_row(&tx, *chunk_id, vector)?;
        }
        tx.commit()?;
        Ok(embeddings.len())
    }

    pub fn current_chunk_embedding_count_for_fingerprint(
        &self,
        fingerprint: &EmbeddingFingerprint,
    ) -> Result<i64, QghError> {
        if !embedding_schema_exists(&self.conn)? {
            return Ok(0);
        }
        let fingerprint_hash = fingerprint.hash();
        self.conn
            .query_row(
                "SELECT count(*)
                 FROM chunk_embeddings ce
                 JOIN embedding_fingerprints ef ON ef.id = ce.fingerprint_id
                 JOIN chunks c ON c.id = ce.chunk_id
                 JOIN source_entities se ON se.source_id = c.source_id
                 LEFT JOIN issue_metadata im ON im.source_id = c.source_id
                 LEFT JOIN comment_metadata cm ON cm.source_id = c.source_id
                 WHERE ef.fingerprint_hash = ?1
                   AND se.lifecycle_state = 'active'
                   AND c.source_version_id = coalesce(im.latest_version_id, cm.latest_version_id)",
                params![fingerprint_hash],
                |row| row.get(0),
            )
            .map_err(QghError::from)
    }

    pub fn vector_index_ready_for_fingerprint(
        &self,
        fingerprint: &EmbeddingFingerprint,
        expected_rows: i64,
    ) -> Result<bool, QghError> {
        if expected_rows == 0 {
            return Ok(true);
        }
        if vector_table_dimension(&self.conn)? != Some(fingerprint.dimension) {
            return Ok(false);
        }
        if !table_exists(&self.conn, CHUNK_EMBEDDING_VECTOR_ROWIDS_TABLE)?
            || !table_exists(&self.conn, CHUNK_EMBEDDING_VECTOR_CHUNKS_TABLE)?
        {
            return Ok(false);
        }

        let fingerprint_hash = fingerprint.hash();
        let indexed_rows = self.conn.query_row(
            &format!(
                "SELECT count(*)
                 FROM chunk_embeddings ce
                 JOIN embedding_fingerprints ef ON ef.id = ce.fingerprint_id
                 JOIN chunks c ON c.id = ce.chunk_id
                 JOIN source_entities se ON se.source_id = c.source_id
                 LEFT JOIN issue_metadata im ON im.source_id = c.source_id
                 LEFT JOIN comment_metadata cm ON cm.source_id = c.source_id
                 JOIN {CHUNK_EMBEDDING_VECTOR_ROWIDS_TABLE} vr ON vr.rowid = ce.chunk_id
                 WHERE ef.fingerprint_hash = ?1
                   AND se.lifecycle_state = 'active'
                   AND c.source_version_id = coalesce(im.latest_version_id, cm.latest_version_id)"
            ),
            params![fingerprint_hash],
            |row| row.get::<_, i64>(0),
        )?;
        if indexed_rows != expected_rows {
            return Ok(false);
        }

        let vector_chunks = self.conn.query_row(
            &format!("SELECT count(*) FROM {CHUNK_EMBEDDING_VECTOR_CHUNKS_TABLE}"),
            [],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(vector_chunks > 0)
    }

    pub fn ensure_vector_storage_for_fingerprint(
        &mut self,
        fingerprint: &EmbeddingFingerprint,
    ) -> Result<usize, QghError> {
        self.ensure_vector_storage(fingerprint.dimension)?;
        let fingerprint_hash = fingerprint.hash();
        let rows = {
            let mut stmt = self.conn.prepare(
                "SELECT ce.chunk_id, ce.vector_json
                 FROM chunk_embeddings ce
                 JOIN embedding_fingerprints ef ON ef.id = ce.fingerprint_id
                 JOIN chunks c ON c.id = ce.chunk_id
                 JOIN source_entities se ON se.source_id = c.source_id
                 LEFT JOIN issue_metadata im ON im.source_id = c.source_id
                 LEFT JOIN comment_metadata cm ON cm.source_id = c.source_id
                 WHERE ef.fingerprint_hash = ?1
                   AND se.lifecycle_state = 'active'
                   AND c.source_version_id = coalesce(im.latest_version_id, cm.latest_version_id)
                 ORDER BY ce.chunk_id",
            )?;
            let rows = stmt.query_map(params![fingerprint_hash], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };

        let tx = self.conn.transaction()?;
        for (chunk_id, vector_json) in &rows {
            let vector: EmbeddingVector = serde_json::from_str(vector_json).map_err(|error| {
                QghError::storage(format!("Stored embedding vector is invalid: {error}"))
            })?;
            if vector.len() != fingerprint.dimension {
                return Err(QghError::storage(format!(
                    "Stored embedding vector dimension {} does not match fingerprint dimension {}.",
                    vector.len(),
                    fingerprint.dimension
                )));
            }
            upsert_vector_row(&tx, *chunk_id, &vector)?;
        }
        tx.commit()?;
        Ok(rows.len())
    }

    pub fn vector_only_search(
        &self,
        query_vector: &[f32],
        filters: &VectorSearchFilters,
        limit: usize,
    ) -> Result<Vec<VectorSearchHit>, QghError> {
        if limit == 0 || filters.source_types.is_empty() {
            return Ok(Vec::new());
        }
        if query_vector.is_empty() {
            return Err(QghError::validation(
                "embedding.empty_vector",
                "Vector-only search requires a non-empty query vector.",
            ));
        }
        let Some(dimension) = vector_table_dimension(&self.conn)? else {
            return Ok(Vec::new());
        };
        if dimension != query_vector.len() {
            return Err(QghError::validation(
                "embedding.dimension_mismatch",
                "Query vector dimension does not match the active sqlite-vec table.",
            )
            .with_details(serde_json::json!({
                "query_dimension": query_vector.len(),
                "vector_table_dimension": dimension
            })));
        }

        let candidate_limit = limit.saturating_mul(4).max(limit).max(1);
        let mut params = vec![
            Value::Blob(embedding_vector_blob(query_vector)),
            Value::Integer(candidate_limit as i64),
        ];
        let mut prefilter_sql = String::from(
            "SELECT c2.id
             FROM chunks c2
             JOIN source_entities se ON se.source_id = c2.source_id
             LEFT JOIN issue_metadata im ON im.source_id = c2.source_id
             LEFT JOIN comment_metadata cm ON cm.source_id = c2.source_id
             JOIN embedding_fingerprints ef ON ef.active = 1
             JOIN chunk_embeddings ce
                ON ce.chunk_id = c2.id
               AND ce.fingerprint_id = ef.id
             WHERE se.lifecycle_state = 'active'
               AND c2.source_version_id = coalesce(im.latest_version_id, cm.latest_version_id)",
        );
        push_vector_filter_sql(filters, &mut prefilter_sql, &mut params);
        params.push(Value::Integer(limit as i64));

        let sql = format!(
            "WITH vector_candidates AS (
                SELECT v.rowid AS chunk_id, v.distance AS vector_distance
                FROM {CHUNK_EMBEDDING_VECTORS_TABLE} v
                WHERE v.embedding MATCH ? AND v.k = ?
                  AND v.rowid IN ({prefilter_sql})
                ORDER BY v.distance
             )
             SELECT source_id, chunk_id, source_version_hash, vector_distance,
                    body, source_version_id, chunk_index, token_start, token_end,
                    byte_start, byte_end, chunker_version, chunker_fingerprint,
                    heading_path_json
             FROM (
                 SELECT c.source_id, c.id AS chunk_id, sv.body_hash AS source_version_hash,
                        vector_candidates.vector_distance, c.body, c.source_version_id,
                        c.chunk_index, c.token_start, c.token_end, c.byte_start,
                        c.byte_end, c.chunker_version, c.chunker_fingerprint,
                        c.heading_path_json,
                        row_number() OVER (
                            PARTITION BY c.source_id
                            ORDER BY vector_candidates.vector_distance ASC, c.id ASC
                        ) AS source_rank
                 FROM vector_candidates
                 JOIN chunks c ON c.id = vector_candidates.chunk_id
                 JOIN source_versions sv ON sv.id = c.source_version_id
             )
             WHERE source_rank = 1
             ORDER BY vector_distance ASC, source_id ASC
             LIMIT ?"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(params.iter()), |row| {
            let heading_path_json: String = row.get(13)?;
            Ok(VectorSearchHit {
                source_id: row.get(0)?,
                chunk: StoredChunk {
                    chunk_id: row.get(1)?,
                    source_id: row.get(0)?,
                    source_version_id: row.get(5)?,
                    body: row.get(4)?,
                    chunk_index: row.get::<_, i64>(6)? as usize,
                    token_start: row.get::<_, i64>(7)? as usize,
                    token_end: row.get::<_, i64>(8)? as usize,
                    byte_start: row.get::<_, i64>(9)? as usize,
                    byte_end: row.get::<_, i64>(10)? as usize,
                    chunker_version: row.get(11)?,
                    chunker_fingerprint: row.get(12)?,
                    heading_path: serde_json::from_str(&heading_path_json).unwrap_or_default(),
                },
                source_version_hash: row.get(2)?,
                vector_distance: row.get(3)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(QghError::from)
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
        self.reconciliation_candidates(None)
    }

    /// Active sources updated at or after `updated_since`, for window-bounded
    /// `--reconcile recent`.
    pub fn recent_reconciliation_candidates(
        &self,
        updated_since: &str,
    ) -> Result<Vec<ReconciliationCandidate>, QghError> {
        self.reconciliation_candidates(Some(updated_since))
    }

    fn reconciliation_candidates(
        &self,
        updated_since: Option<&str>,
    ) -> Result<Vec<ReconciliationCandidate>, QghError> {
        let mut sql = String::from(
            "SELECT se.source_id, se.entity_type, se.repo,
                    coalesce(im.issue_number, cm.issue_number) AS issue_number,
                    se.github_id
             FROM source_entities se
             LEFT JOIN issue_metadata im ON im.source_id = se.source_id
             LEFT JOIN comment_metadata cm ON cm.source_id = se.source_id
             WHERE se.lifecycle_state = 'active'",
        );
        if updated_since.is_some() {
            sql.push_str(" AND se.updated_at >= ?1");
        }
        sql.push_str(" ORDER BY se.repo, issue_number, se.entity_type, se.source_id");
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = match updated_since {
            Some(since) => stmt.query_map(params![since], reconciliation_candidate_from_row)?,
            None => stmt.query_map([], reconciliation_candidate_from_row)?,
        };
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

    pub fn active_comment_source_ids_for_issue(
        &self,
        repo: &str,
        issue_number: i64,
    ) -> Result<Vec<String>, QghError> {
        Ok(self
            .active_comment_versions_for_issue(repo, issue_number)?
            .into_keys()
            .collect())
    }

    fn active_comment_versions_for_issue(
        &self,
        repo: &str,
        issue_number: i64,
    ) -> Result<BTreeMap<String, String>, QghError> {
        let mut stmt = self.conn.prepare(
            "SELECT cm.source_id, sv.body_hash
             FROM comment_metadata cm
             JOIN source_entities se ON se.source_id = cm.source_id
             JOIN source_versions sv ON sv.id = cm.latest_version_id
             WHERE cm.repo = ?1 AND cm.issue_number = ?2 AND se.lifecycle_state = 'active'
             ORDER BY cm.source_id",
        )?;
        let rows = stmt.query_map(params![repo, issue_number], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut comments = BTreeMap::new();
        for row in rows {
            let (source_id, body_hash) = row?;
            comments.insert(source_id, body_hash);
        }
        Ok(comments)
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
                "SELECT completed_at
                 FROM sync_runs
                 WHERE completed_successfully = 1
                 ORDER BY completed_at DESC
                 LIMIT 1",
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

    fn record_empty_sync_run(&self) -> Result<String, QghError> {
        let sync_run_id = format!("sync-{}", now_run_id_suffix());
        let now = now_rfc3339();
        self.conn.execute(
            "INSERT INTO sync_runs
                (id, started_at, completed_at, fetched_issue_count, upserted_issue_count, fetched_comment_count, upserted_comment_count, skipped_pull_request_count)
             VALUES (?1, ?2, ?2, 0, 0, 0, 0, 0)",
            params![sync_run_id, now],
        )?;
        Ok(sync_run_id)
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
                "SELECT completed_at
                 FROM sync_runs
                 WHERE completed_successfully = 1
                 ORDER BY completed_at DESC
                 LIMIT 1",
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

    pub fn latest_successful_sync_run_id(&self) -> Result<Option<String>, QghError> {
        self.conn
            .query_row(
                "SELECT id FROM sync_runs
                 WHERE completed_successfully = 1
                 ORDER BY completed_at DESC, id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(QghError::from)
    }

    pub fn source_version_hash(&self, source_version_id: i64) -> Result<Option<String>, QghError> {
        self.conn
            .query_row(
                "SELECT body_hash FROM source_versions WHERE id = ?1",
                params![source_version_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(QghError::from)
    }

    pub fn coverage_snapshot(&self) -> Result<CoverageSnapshot, QghError> {
        let snapshot = self
            .conn
            .query_row(
                "SELECT open_cursor, history_cursor, open_backfill_complete,
                        historical_backfill_complete, oldest_synced_updated_at,
                        recent_bootstrap_floor, next_backfill_window_hint
                 FROM coverage_state
                 WHERE id = 1",
                [],
                |row| {
                    Ok(CoverageSnapshot {
                        open_cursor: row.get(0)?,
                        history_cursor: row.get(1)?,
                        open_backfill_complete: row.get::<_, i64>(2)? != 0,
                        historical_backfill_complete: row.get::<_, i64>(3)? != 0,
                        oldest_synced_updated_at: row.get(4)?,
                        recent_bootstrap_floor: row.get(5)?,
                        next_backfill_window_hint: row.get(6)?,
                    })
                },
            )
            .optional()?;
        Ok(snapshot.unwrap_or_default())
    }

    /// Persist the full coverage row (singleton). Callers read the current
    /// snapshot, mutate the fields they own, and write it back so phases that
    /// own different fields don't clobber each other.
    pub fn update_coverage(&self, coverage: &CoverageSnapshot) -> Result<(), QghError> {
        self.conn.execute(
            "INSERT INTO coverage_state
                (id, open_cursor, history_cursor, open_backfill_complete,
                 historical_backfill_complete, oldest_synced_updated_at,
                 recent_bootstrap_floor, next_backfill_window_hint)
             VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET
                open_cursor = excluded.open_cursor,
                history_cursor = excluded.history_cursor,
                open_backfill_complete = excluded.open_backfill_complete,
                historical_backfill_complete = excluded.historical_backfill_complete,
                oldest_synced_updated_at = excluded.oldest_synced_updated_at,
                recent_bootstrap_floor = excluded.recent_bootstrap_floor,
                next_backfill_window_hint = excluded.next_backfill_window_hint",
            params![
                coverage.open_cursor,
                coverage.history_cursor,
                coverage.open_backfill_complete as i64,
                coverage.historical_backfill_complete as i64,
                coverage.oldest_synced_updated_at,
                coverage.recent_bootstrap_floor,
                coverage.next_backfill_window_hint,
            ],
        )?;
        Ok(())
    }

    pub fn begin_embedding_generation(
        &mut self,
        spec: &EmbeddingGenerationSpec,
    ) -> Result<i64, QghError> {
        if spec.output_dimension == 0 || spec.total_chunks < 0 {
            return Err(QghError::validation(
                "embedding.generation_invalid_spec",
                "Embedding generation dimension and total chunk count must be positive.",
            ));
        }
        if let Some(id) = self
            .conn
            .query_row(
                "SELECT id FROM embedding_generations
                 WHERE state = 'building'
                   AND model_manifest_hash = ?1
                   AND chunker_fingerprint = ?2
                   AND context_template_version = ?3
                   AND output_dimension = ?4
                   AND source_sync_run_id = ?5
                   AND source_snapshot_hash = ?6
                 ORDER BY id DESC LIMIT 1",
                params![
                    spec.model_manifest_hash,
                    spec.chunker_fingerprint,
                    spec.context_template_version,
                    spec.output_dimension as i64,
                    spec.source_sync_run_id,
                    spec.source_snapshot_hash
                ],
                |row| row.get(0),
            )
            .optional()?
        {
            return Ok(id);
        }
        let now = now_rfc3339();
        self.conn.execute(
            "INSERT INTO embedding_generations
                (state, model_manifest_hash, chunker_fingerprint,
                 context_template_version, output_dimension, source_sync_run_id,
                 source_snapshot_hash, total_chunks, created_at, updated_at)
             VALUES ('building', ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
            params![
                spec.model_manifest_hash,
                spec.chunker_fingerprint,
                spec.context_template_version,
                spec.output_dimension as i64,
                spec.source_sync_run_id,
                spec.source_snapshot_hash,
                spec.total_chunks,
                now
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn stage_embedding_generation_batch(
        &mut self,
        generation_id: i64,
        chunks: &[EmbeddingGenerationChunk],
    ) -> Result<usize, QghError> {
        let (dimension, state): (usize, String) = self.conn.query_row(
            "SELECT output_dimension, state FROM embedding_generations WHERE id = ?1",
            params![generation_id],
            |row| Ok((row.get::<_, i64>(0)? as usize, row.get(1)?)),
        )?;
        if state != "building" {
            return Err(QghError::validation(
                "embedding.generation_not_building",
                "Only a building embedding generation can accept staged batches.",
            ));
        }
        if chunks.iter().any(|chunk| chunk.vector.len() != dimension) {
            return Err(QghError::validation(
                "embedding.generation_dimension_mismatch",
                "A staged vector dimension does not match the generation dimension.",
            ));
        }
        let vector_table = generation_vector_table_name(dimension);
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| {
            self.conn.execute(
                &format!(
                    "CREATE VIRTUAL TABLE IF NOT EXISTS {vector_table}
                     USING vec0(embedding float[{dimension}])"
                ),
                [],
            )?;
            for chunk in chunks {
                let bytes = encode_embedding_blob(&chunk.vector);
                let checksum = embedding_blob_checksum(&bytes);
                if let Some((old_table, old_rowid)) = self
                    .conn
                    .query_row(
                        "SELECT vector_table, vector_rowid
                         FROM embedding_generation_vector_rows
                         WHERE generation_id = ?1 AND chunk_id = ?2",
                        params![generation_id, chunk.chunk_id],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                    )
                    .optional()?
                {
                    self.conn.execute(
                        &format!("DELETE FROM {old_table} WHERE rowid = ?1"),
                        params![old_rowid],
                    )?;
                }
                self.conn.execute(
                    "DELETE FROM embedding_generation_vector_rows
                     WHERE generation_id = ?1 AND chunk_id = ?2",
                    params![generation_id, chunk.chunk_id],
                )?;
                self.conn.execute(
                    "INSERT INTO embedding_generation_chunks
                        (generation_id, chunk_id, source_version_id, source_version_hash,
                         context_hash, vector_blob, vector_checksum, vector_dimension, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                     ON CONFLICT(generation_id, chunk_id) DO UPDATE SET
                        source_version_id = excluded.source_version_id,
                        source_version_hash = excluded.source_version_hash,
                        context_hash = excluded.context_hash,
                        vector_blob = excluded.vector_blob,
                        vector_checksum = excluded.vector_checksum,
                        vector_dimension = excluded.vector_dimension,
                        created_at = excluded.created_at",
                    params![
                        generation_id,
                        chunk.chunk_id,
                        chunk.source_version_id,
                        chunk.source_version_hash,
                        chunk.context_hash,
                        bytes,
                        checksum,
                        dimension as i64,
                        now_rfc3339()
                    ],
                )?;
                let mapping_id = {
                    self.conn.execute(
                        "INSERT INTO embedding_generation_vector_rows
                            (generation_id, chunk_id, dimension, vector_table, vector_rowid)
                         VALUES (?1, ?2, ?3, ?4, 0)",
                        params![
                            generation_id,
                            chunk.chunk_id,
                            dimension as i64,
                            vector_table
                        ],
                    )?;
                    self.conn.last_insert_rowid()
                };
                self.conn.execute(
                    &format!("INSERT INTO {vector_table}(rowid, embedding) VALUES (?1, ?2)"),
                    params![mapping_id, encode_embedding_blob(&chunk.vector)],
                )?;
                self.conn.execute(
                    "UPDATE embedding_generation_vector_rows
                     SET vector_rowid = ?1 WHERE id = ?1",
                    params![mapping_id],
                )?;
            }
            let now = now_rfc3339();
            self.conn.execute(
                "UPDATE embedding_generations
                 SET completed_chunks = (SELECT count(*) FROM embedding_generation_chunks WHERE generation_id = ?1),
                     checkpoint_chunk_id = (SELECT max(chunk_id) FROM embedding_generation_chunks WHERE generation_id = ?1),
                     updated_at = ?2
                 WHERE id = ?1",
                params![generation_id, now],
            )?;
            Ok::<usize, rusqlite::Error>(chunks.len())
        })();
        match result {
            Ok(count) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(count)
            }
            Err(error) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(QghError::from(error))
            }
        }
    }

    pub fn embedding_generation_chunk_blob(
        &self,
        generation_id: i64,
        chunk_id: i64,
    ) -> Result<EmbeddingGenerationChunkBlob, QghError> {
        self.conn
            .query_row(
                "SELECT vector_blob, vector_checksum, vector_dimension
                 FROM embedding_generation_chunks
                 WHERE generation_id = ?1 AND chunk_id = ?2",
                params![generation_id, chunk_id],
                |row| {
                    Ok(EmbeddingGenerationChunkBlob {
                        bytes: row.get(0)?,
                        checksum: row.get(1)?,
                        dimension: row.get::<_, i64>(2)? as usize,
                    })
                },
            )
            .map_err(QghError::from)
    }

    pub fn validate_embedding_generation(&mut self, generation_id: i64) -> Result<(), QghError> {
        let (
            state,
            dimension,
            total_chunks,
            completed_chunks,
            model_manifest_hash,
            chunker_fingerprint,
            context_template_version,
        ): (String, usize, i64, i64, String, String, String) = self.conn.query_row(
            "SELECT state, output_dimension, total_chunks, completed_chunks,
                        model_manifest_hash, chunker_fingerprint, context_template_version
                 FROM embedding_generations WHERE id = ?1",
            params![generation_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get::<_, i64>(1)? as usize,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )?;
        if state != "building" || completed_chunks != total_chunks {
            return self.fail_embedding_generation(
                generation_id,
                "embedding.generation_incomplete",
                "Embedding generation is incomplete and cannot be activated.",
            );
        }
        let mut stmt = self.conn.prepare(
            "SELECT gc.vector_blob, gc.vector_checksum, gc.vector_dimension,
                    gc.source_version_id, gc.source_version_hash, gc.context_hash,
                    sv.body_hash, coalesce(im.latest_version_id, cm.latest_version_id), c.body
             FROM embedding_generation_chunks gc
             JOIN source_versions sv ON sv.id = gc.source_version_id
             JOIN chunks c ON c.id = gc.chunk_id
             LEFT JOIN issue_metadata im ON im.source_id = sv.source_id
             LEFT JOIN comment_metadata cm ON cm.source_id = sv.source_id
             WHERE gc.generation_id = ?1",
        )?;
        let rows = stmt.query_map(params![generation_id], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)? as usize,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, Option<i64>>(7)?,
                row.get::<_, String>(8)?,
            ))
        })?;
        let mut invalid = false;
        let mut validated_rows = 0i64;
        for row in rows {
            validated_rows += 1;
            let (
                bytes,
                checksum,
                stored_dimension,
                source_version_id,
                source_hash,
                context_hash,
                body_hash,
                latest_version_id,
                embedding_input,
            ) = row?;
            if stored_dimension != dimension
                || decode_embedding_blob(&bytes, dimension).is_err()
                || embedding_blob_checksum(&bytes) != checksum
                || source_hash != body_hash
                || latest_version_id != Some(source_version_id)
                || context_hash
                    != embedding_context_hash(
                        &model_manifest_hash,
                        &chunker_fingerprint,
                        &context_template_version,
                        &embedding_input,
                    )
            {
                invalid = true;
                break;
            }
        }
        drop(stmt);
        if invalid || validated_rows != total_chunks {
            return self.fail_embedding_generation(
                generation_id,
                "embedding.generation_validation_failed",
                "Embedding generation validation failed.",
            );
        }
        self.conn.execute(
            "UPDATE embedding_generations SET state = 'ready', updated_at = ?2, failure_code = NULL WHERE id = ?1",
            params![generation_id, now_rfc3339()],
        )?;
        Ok(())
    }

    pub fn embedding_generation_state(&self, generation_id: i64) -> Result<String, QghError> {
        self.conn
            .query_row(
                "SELECT state FROM embedding_generations WHERE id = ?1",
                params![generation_id],
                |row| row.get(0),
            )
            .map_err(QghError::from)
    }

    pub fn latest_embedding_generation_state(&self) -> Result<Option<String>, QghError> {
        self.conn
            .query_row(
                "SELECT state FROM embedding_generations ORDER BY id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(QghError::from)
    }

    pub fn activate_retrieval_publication(
        &mut self,
        source_snapshot_sync_run_id: &str,
        tantivy_generation: i64,
        embedding_generation_id: Option<i64>,
        expected_publication_id: Option<i64>,
    ) -> Result<i64, QghError> {
        let embedding_metadata = if let Some(generation_id) = embedding_generation_id {
            Some(self.conn.query_row(
                "SELECT state, model_manifest_hash, chunker_fingerprint,
                        context_template_version, output_dimension, total_chunks,
                        completed_chunks
                 FROM embedding_generations WHERE id = ?1",
                params![generation_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)? as usize,
                        row.get::<_, i64>(5)?,
                        row.get::<_, i64>(6)?,
                    ))
                },
            )?)
        } else {
            None
        };
        if let Some((state, _, _, _, _, total_chunks, completed_chunks)) = &embedding_metadata {
            if state != "ready" || total_chunks != completed_chunks {
                return Err(QghError::validation(
                    "publication.embedding_not_ready",
                    "Only a complete ready embedding generation can be published.",
                ));
            }
        }

        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| {
            let current = self
                .conn
                .query_row(
                    "SELECT publication_id FROM retrieval_publication_pointer WHERE id = 1",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?;
            if expected_publication_id.is_some() && current != expected_publication_id {
                return Err(rusqlite::Error::InvalidParameterName(
                    "publication.cas_conflict".to_string(),
                ));
            }
            let now = now_rfc3339();
            let (manifest, chunker, context, dimension) = embedding_metadata
                .as_ref()
                .map(|(_, manifest, chunker, context, dimension, _, _)| {
                    (
                        Some(manifest.as_str()),
                        Some(chunker.as_str()),
                        Some(context.as_str()),
                        Some(*dimension as i64),
                    )
                })
                .unwrap_or((None, None, None, None));
            self.conn.execute(
                "INSERT INTO retrieval_publications
                    (source_snapshot_sync_run_id, tantivy_generation,
                     embedding_generation_id, model_manifest_hash,
                     chunker_fingerprint, context_template_version,
                     output_dimension, active, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1, ?8)",
                params![
                    source_snapshot_sync_run_id,
                    tantivy_generation,
                    embedding_generation_id,
                    manifest,
                    chunker,
                    context,
                    dimension,
                    now
                ],
            )?;
            let publication_id = self.conn.last_insert_rowid();
            self.conn.execute(
                "UPDATE retrieval_publications SET active = 0 WHERE publication_id != ?1",
                params![publication_id],
            )?;
            self.conn.execute(
                "INSERT INTO retrieval_publication_pointer(id, publication_id)
                 VALUES (1, ?1)
                 ON CONFLICT(id) DO UPDATE SET publication_id = excluded.publication_id",
                params![publication_id],
            )?;
            if let Some(generation_id) = embedding_generation_id {
                self.conn.execute(
                    "UPDATE embedding_generations SET state = 'active', updated_at = ?2 WHERE id = ?1",
                    params![generation_id, now_rfc3339()],
                )?;
            }
            if let Some(previous) = current {
                if let Some(previous_generation) = self
                    .conn
                    .query_row(
                        "SELECT embedding_generation_id FROM retrieval_publications WHERE publication_id = ?1",
                        params![previous],
                        |row| row.get::<_, Option<i64>>(0),
                    )?
                {
                    self.conn.execute(
                        "UPDATE embedding_generations SET state = 'ready', updated_at = ?2
                         WHERE id = ?1 AND state = 'active'",
                        params![previous_generation, now_rfc3339()],
                    )?;
                }
            }
            Ok::<i64, rusqlite::Error>(publication_id)
        })();
        match result {
            Ok(publication_id) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(publication_id)
            }
            Err(rusqlite::Error::InvalidParameterName(code))
                if code == "publication.cas_conflict" =>
            {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(QghError::validation(
                    "publication.cas_conflict",
                    "Retrieval publication changed before activation.",
                ))
            }
            Err(error) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(QghError::from(error))
            }
        }
    }

    pub fn active_retrieval_publication(
        &self,
    ) -> Result<Option<RetrievalPublicationView>, QghError> {
        self.conn
            .query_row(
                "SELECT rp.publication_id, rp.source_snapshot_sync_run_id,
                        rp.tantivy_generation, rp.embedding_generation_id,
                        rp.model_manifest_hash, rp.chunker_fingerprint,
                        rp.context_template_version, rp.output_dimension
                 FROM retrieval_publication_pointer p
                 JOIN retrieval_publications rp ON rp.publication_id = p.publication_id
                 WHERE p.id = 1",
                [],
                |row| {
                    Ok(RetrievalPublicationView {
                        publication_id: row.get(0)?,
                        source_snapshot_sync_run_id: row.get(1)?,
                        tantivy_generation: row.get(2)?,
                        embedding_generation_id: row.get(3)?,
                        model_manifest_hash: row.get(4)?,
                        chunker_fingerprint: row.get(5)?,
                        context_template_version: row.get(6)?,
                        output_dimension: row.get::<_, Option<i64>>(7)?.map(|value| value as usize),
                    })
                },
            )
            .optional()
            .map_err(QghError::from)
    }

    pub fn cleanup_embedding_generations(
        &mut self,
        stale_building_before: &str,
        previous_ready_before: &str,
    ) -> Result<usize, QghError> {
        let active_generation = self
            .conn
            .query_row(
                "SELECT embedding_generation_id FROM retrieval_publications WHERE active = 1",
                [],
                |row| row.get::<_, Option<i64>>(0),
            )
            .optional()?
            .flatten();
        let previous_generation = self
            .conn
            .query_row(
                "SELECT embedding_generation_id FROM retrieval_publications
                 WHERE active = 0 AND embedding_generation_id IS NOT NULL
                 ORDER BY created_at DESC LIMIT 1",
                [],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        let mut keep = BTreeSet::new();
        if let Some(id) = active_generation {
            keep.insert(id);
        }
        if let Some(id) = previous_generation {
            let created_at = self.conn.query_row(
                "SELECT created_at FROM embedding_generations WHERE id = ?1",
                params![id],
                |row| row.get::<_, String>(0),
            )?;
            let recent = created_at.as_str() >= previous_ready_before;
            if recent {
                keep.insert(id);
            }
        }
        let mut candidates = Vec::new();
        let mut stmt = self.conn.prepare(
            "SELECT id FROM embedding_generations
             WHERE (state = 'building' AND updated_at < ?1)
                OR (state IN ('failed', 'ready') AND id NOT IN (SELECT value FROM json_each(?2)))",
        )?;
        let keep_json = serde_json::to_string(&keep.iter().copied().collect::<Vec<_>>()).unwrap();
        let rows = stmt.query_map(params![stale_building_before, keep_json], |row| row.get(0))?;
        for row in rows {
            candidates.push(row?);
        }
        drop(stmt);
        let mut removed = 0;
        for generation_id in candidates {
            if keep.contains(&generation_id) {
                continue;
            }
            let mappings = self
                .conn
                .prepare(
                    "SELECT vector_table, vector_rowid FROM embedding_generation_vector_rows
                     WHERE generation_id = ?1",
                )?
                .query_map(params![generation_id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            for (table, rowid) in mappings {
                self.conn.execute(
                    &format!("DELETE FROM {table} WHERE rowid = ?1"),
                    params![rowid],
                )?;
            }
            self.conn.execute(
                "DELETE FROM embedding_generation_vector_rows WHERE generation_id = ?1",
                params![generation_id],
            )?;
            self.conn.execute(
                "DELETE FROM embedding_generation_chunks WHERE generation_id = ?1",
                params![generation_id],
            )?;
            removed += self.conn.execute(
                "DELETE FROM embedding_generations WHERE id = ?1",
                params![generation_id],
            )?;
        }
        Ok(removed)
    }

    fn fail_embedding_generation(
        &mut self,
        generation_id: i64,
        code: &str,
        message: &str,
    ) -> Result<(), QghError> {
        self.conn.execute(
            "UPDATE embedding_generations
             SET state = 'failed', failure_code = ?2, updated_at = ?3 WHERE id = ?1",
            params![generation_id, code, now_rfc3339()],
        )?;
        Err(QghError::validation(code, message))
    }

    fn ensure_vector_storage(&mut self, dimension: usize) -> Result<(), QghError> {
        if dimension == 0 {
            return Err(QghError::storage(
                "Cannot create sqlite-vec storage for zero-dimensional embeddings.",
            ));
        }
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = self.ensure_vector_storage_inner(dimension);
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

    fn ensure_vector_storage_inner(&self, dimension: usize) -> Result<(), QghError> {
        if let Some(existing_dimension) = vector_table_dimension(&self.conn)? {
            if existing_dimension == dimension {
                return Ok(());
            }
            self.conn
                .execute(&format!("DROP TABLE {CHUNK_EMBEDDING_VECTORS_TABLE}"), [])?;
        }
        self.conn.execute(
            &format!(
                "CREATE VIRTUAL TABLE {CHUNK_EMBEDDING_VECTORS_TABLE}
                 USING vec0(embedding float[{dimension}])"
            ),
            [],
        )?;
        Ok(())
    }

    /// Oldest `updated_at` across active issues. NULL (empty corpus) maps to None.
    pub fn oldest_active_issue_updated_at(&self) -> Result<Option<String>, QghError> {
        let oldest: Option<String> = self.conn.query_row(
            "SELECT min(updated_at) FROM source_entities
             WHERE entity_type = 'issue' AND lifecycle_state = 'active'",
            [],
            |row| row.get(0),
        )?;
        Ok(oldest)
    }

    #[cfg(feature = "vector-search")]
    fn migrate_vector_schema(&mut self) -> Result<(), QghError> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = self.migrate_vector_schema_inner();
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

    #[cfg(feature = "vector-search")]
    fn migrate_vector_schema_inner(&self) -> Result<(), QghError> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS chunks (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                source_id TEXT NOT NULL,
                source_version_id INTEGER NOT NULL,
                body TEXT NOT NULL,
                chunk_index INTEGER NOT NULL DEFAULT 0,
                token_start INTEGER NOT NULL DEFAULT 0,
                token_end INTEGER NOT NULL DEFAULT 0,
                byte_start INTEGER NOT NULL DEFAULT 0,
                byte_end INTEGER NOT NULL DEFAULT 0,
                chunker_version TEXT NOT NULL DEFAULT 'markdown-token-v1',
                chunker_fingerprint TEXT NOT NULL DEFAULT 'legacy',
                heading_path_json TEXT NOT NULL DEFAULT '[]'
            );

            CREATE TABLE IF NOT EXISTS embedding_fingerprints (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                fingerprint_hash TEXT NOT NULL UNIQUE,
                fingerprint_json TEXT NOT NULL,
                provider TEXT NOT NULL,
                model_id TEXT NOT NULL,
                model_revision TEXT NOT NULL,
                dimension INTEGER NOT NULL,
                pooling TEXT NOT NULL,
                query_prefix TEXT NOT NULL,
                chunker_version TEXT NOT NULL,
                source_schema_version TEXT NOT NULL,
                created_at TEXT NOT NULL,
                active INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS chunk_embeddings (
                chunk_id INTEGER NOT NULL,
                fingerprint_id INTEGER NOT NULL,
                vector_json TEXT NOT NULL,
                embedded_at TEXT NOT NULL,
                PRIMARY KEY (chunk_id, fingerprint_id)
            );

            CREATE TABLE IF NOT EXISTS embedding_generations (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                state TEXT NOT NULL,
                model_manifest_hash TEXT NOT NULL,
                chunker_fingerprint TEXT NOT NULL,
                context_template_version TEXT NOT NULL,
                output_dimension INTEGER NOT NULL,
                source_sync_run_id TEXT NOT NULL,
                source_snapshot_hash TEXT NOT NULL,
                total_chunks INTEGER NOT NULL,
                completed_chunks INTEGER NOT NULL DEFAULT 0,
                checkpoint_chunk_id INTEGER,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                failure_code TEXT
            );

            CREATE TABLE IF NOT EXISTS embedding_generation_chunks (
                generation_id INTEGER NOT NULL,
                chunk_id INTEGER NOT NULL,
                source_version_id INTEGER NOT NULL,
                source_version_hash TEXT NOT NULL,
                context_hash TEXT NOT NULL,
                vector_blob BLOB NOT NULL,
                vector_checksum TEXT NOT NULL,
                vector_dimension INTEGER NOT NULL,
                created_at TEXT NOT NULL,
                PRIMARY KEY (generation_id, chunk_id),
                FOREIGN KEY (generation_id) REFERENCES embedding_generations(id)
            );

            CREATE TABLE IF NOT EXISTS embedding_generation_vector_rows (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                generation_id INTEGER NOT NULL,
                chunk_id INTEGER NOT NULL,
                dimension INTEGER NOT NULL,
                vector_table TEXT NOT NULL,
                vector_rowid INTEGER NOT NULL,
                UNIQUE (generation_id, chunk_id),
                FOREIGN KEY (generation_id) REFERENCES embedding_generations(id)
            );

            CREATE TABLE IF NOT EXISTS retrieval_publications (
                publication_id INTEGER PRIMARY KEY AUTOINCREMENT,
                source_snapshot_sync_run_id TEXT NOT NULL,
                tantivy_generation INTEGER NOT NULL,
                embedding_generation_id INTEGER,
                model_manifest_hash TEXT,
                chunker_fingerprint TEXT,
                context_template_version TEXT,
                output_dimension INTEGER,
                active INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS retrieval_publication_pointer (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                publication_id INTEGER NOT NULL
            );
            "#,
        )?;
        self.conn.execute(
            "INSERT INTO schema_migrations (version, applied_at)
             VALUES (?1, ?2)
             ON CONFLICT(version) DO NOTHING",
            params!["qgh.vector.v1", now_rfc3339()],
        )?;
        Ok(())
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

            CREATE TABLE IF NOT EXISTS sync_runs (
                id TEXT PRIMARY KEY,
                started_at TEXT NOT NULL,
                completed_at TEXT NOT NULL,
                completed_successfully INTEGER NOT NULL DEFAULT 1,
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

            CREATE TABLE IF NOT EXISTS coverage_state (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                open_cursor TEXT,
                history_cursor TEXT,
                open_backfill_complete INTEGER NOT NULL DEFAULT 0,
                historical_backfill_complete INTEGER NOT NULL DEFAULT 0,
                oldest_synced_updated_at TEXT,
                recent_bootstrap_floor TEXT,
                next_backfill_window_hint TEXT
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

            CREATE TABLE IF NOT EXISTS retrieval_publications (
                publication_id INTEGER PRIMARY KEY AUTOINCREMENT,
                source_snapshot_sync_run_id TEXT NOT NULL,
                tantivy_generation INTEGER NOT NULL,
                embedding_generation_id INTEGER,
                model_manifest_hash TEXT,
                chunker_fingerprint TEXT,
                context_template_version TEXT,
                output_dimension INTEGER,
                active INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS retrieval_publication_pointer (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                publication_id INTEGER NOT NULL
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
        if table_exists(&self.conn, "chunks")? {
            ensure_column(
                &self.conn,
                "chunks",
                "chunk_index",
                "INTEGER NOT NULL DEFAULT 0",
            )?;
            ensure_column(
                &self.conn,
                "chunks",
                "token_start",
                "INTEGER NOT NULL DEFAULT 0",
            )?;
            ensure_column(
                &self.conn,
                "chunks",
                "token_end",
                "INTEGER NOT NULL DEFAULT 0",
            )?;
            ensure_column(
                &self.conn,
                "chunks",
                "byte_start",
                "INTEGER NOT NULL DEFAULT 0",
            )?;
            ensure_column(
                &self.conn,
                "chunks",
                "byte_end",
                "INTEGER NOT NULL DEFAULT 0",
            )?;
            ensure_column(
                &self.conn,
                "chunks",
                "chunker_version",
                "TEXT NOT NULL DEFAULT 'markdown-token-v1'",
            )?;
            ensure_column(
                &self.conn,
                "chunks",
                "chunker_fingerprint",
                "TEXT NOT NULL DEFAULT 'legacy'",
            )?;
            ensure_column(
                &self.conn,
                "chunks",
                "heading_path_json",
                "TEXT NOT NULL DEFAULT '[]'",
            )?;
        }
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
        ensure_column(
            &self.conn,
            "sync_runs",
            "completed_successfully",
            "INTEGER NOT NULL DEFAULT 1",
        )?;
        self.conn.execute(
            "INSERT OR IGNORE INTO repository_sync_state (repo, last_successful_sync_at)
             SELECT DISTINCT repo, (SELECT max(completed_at) FROM sync_runs)
             FROM source_entities
             WHERE (SELECT max(completed_at) FROM sync_runs) IS NOT NULL",
            [],
        )?;
        // Remap legacy lifecycle/reconcile tombstone reasons to the unified
        // vocabulary so pre-existing tombstones match the documented contract.
        self.conn.execute(
            "UPDATE tombstones SET reason = CASE reason
                WHEN 'not_found' THEN 'deleted'
                WHEN 'gone' THEN 'deleted'
                WHEN 'moved' THEN 'transferred'
                WHEN 'permission_denied' THEN 'permission_loss'
                ELSE reason END
             WHERE reason IN ('not_found', 'gone', 'moved', 'permission_denied')",
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

#[cfg(feature = "vector-search")]
fn register_sqlite_vec_extension(conn: &Connection) -> Result<(), QghError> {
    #[cfg(debug_assertions)]
    if std::env::var_os("QGH_TEST_VECTOR_INIT_FAILURE").is_some() {
        return Err(QghError::validation(
            "embedding.vector_init_failed",
            "Local vector storage initialization failed.",
        ));
    }

    type SqliteVecEntryPoint = unsafe extern "C" fn(
        db: *mut rusqlite::ffi::sqlite3,
        pz_err_msg: *mut *mut c_char,
        p_api: *const rusqlite::ffi::sqlite3_api_routines,
    ) -> c_int;

    // sqlite-vec is compiled with SQLITE_CORE, so its init entry point can be
    // applied directly to this connection without installing a process-wide
    // auto-extension that would leak into later BM25-only Store::open calls.
    let entry_point = unsafe {
        std::mem::transmute::<unsafe extern "C" fn(), SqliteVecEntryPoint>(
            sqlite_vec::sqlite3_vec_init,
        )
    };
    let rc = unsafe { entry_point(conn.handle(), std::ptr::null_mut(), std::ptr::null()) };
    if rc != rusqlite::ffi::SQLITE_OK {
        return Err(QghError::storage(format!(
            "Failed to register sqlite-vec extension: sqlite rc {rc}."
        )));
    }
    Ok(())
}

fn embedding_schema_exists(conn: &Connection) -> Result<bool, QghError> {
    for table in ["chunks", "embedding_fingerprints", "chunk_embeddings"] {
        if !table_exists(conn, table)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool, QghError> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?1",
            params![table],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn generation_vector_table_name(dimension: usize) -> String {
    format!("embedding_generation_vectors_d{dimension}")
}

fn encode_embedding_blob(vector: &[f32]) -> Vec<u8> {
    vector
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

fn decode_embedding_blob(bytes: &[u8], dimension: usize) -> Result<Vec<f32>, QghError> {
    if bytes.len() != dimension.saturating_mul(std::mem::size_of::<f32>()) {
        return Err(QghError::validation(
            "embedding.generation_blob_dimension_mismatch",
            "Embedding generation BLOB length does not match its dimension.",
        ));
    }
    bytes
        .chunks_exact(4)
        .map(|chunk| {
            let bytes: [u8; 4] = chunk.try_into().expect("chunks_exact guarantees width");
            Ok(f32::from_le_bytes(bytes))
        })
        .collect()
}

fn embedding_blob_checksum(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn vector_table_dimension(conn: &Connection) -> Result<Option<usize>, QghError> {
    let Some(sql) = conn
        .query_row(
            "SELECT sql
             FROM sqlite_schema
             WHERE type = 'table' AND name = ?1",
            params![CHUNK_EMBEDDING_VECTORS_TABLE],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    else {
        return Ok(None);
    };
    parse_vector_table_dimension(&sql).map(Some)
}

fn parse_vector_table_dimension(sql: &str) -> Result<usize, QghError> {
    const FLOAT_PREFIX: &str = "float[";
    let Some(start) = sql.find(FLOAT_PREFIX) else {
        return Err(QghError::storage(format!(
            "Stored sqlite-vec table schema is missing a vector dimension for {CHUNK_EMBEDDING_VECTORS_TABLE}."
        )));
    };
    let dimension_start = start + FLOAT_PREFIX.len();
    let Some(end) = sql[dimension_start..].find(']') else {
        return Err(QghError::storage(format!(
            "Stored sqlite-vec table schema has an unterminated vector dimension for {CHUNK_EMBEDDING_VECTORS_TABLE}."
        )));
    };
    sql[dimension_start..dimension_start + end]
        .parse::<usize>()
        .map_err(|error| {
            QghError::storage(format!(
                "Stored sqlite-vec table schema has an invalid vector dimension for {CHUNK_EMBEDDING_VECTORS_TABLE}: {error}."
            ))
        })
}

fn embedding_vector_blob(vector: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(std::mem::size_of_val(vector));
    for value in vector {
        bytes.extend_from_slice(&value.to_ne_bytes());
    }
    bytes
}

fn upsert_vector_row(
    tx: &rusqlite::Transaction<'_>,
    chunk_id: i64,
    vector: &[f32],
) -> Result<(), rusqlite::Error> {
    tx.execute(
        &format!("DELETE FROM {CHUNK_EMBEDDING_VECTORS_TABLE} WHERE rowid = ?1"),
        params![chunk_id],
    )?;
    tx.execute(
        &format!(
            "INSERT INTO {CHUNK_EMBEDDING_VECTORS_TABLE}(rowid, embedding)
             VALUES (?1, ?2)"
        ),
        params![chunk_id, embedding_vector_blob(vector)],
    )?;
    Ok(())
}

fn push_vector_filter_sql(
    filters: &VectorSearchFilters,
    sql: &mut String,
    params: &mut Vec<Value>,
) {
    sql.push_str(" AND se.entity_type IN (");
    for (index, source_type) in filters.source_types.iter().enumerate() {
        if index > 0 {
            sql.push_str(", ");
        }
        sql.push('?');
        params.push(Value::Text(source_type.clone()));
    }
    sql.push(')');
    if let Some(repo) = &filters.repo {
        sql.push_str(" AND se.repo = ?");
        params.push(Value::Text(repo.clone()));
    }
    if let Some(issue) = filters.issue {
        sql.push_str(" AND coalesce(im.issue_number, cm.issue_number) = ?");
        params.push(Value::Integer(issue));
    }
    if let Some(author) = &filters.author {
        sql.push_str(" AND coalesce(im.author, cm.author) = ?");
        params.push(Value::Text(author.clone()));
    }
    if let Some(state) = &filters.state {
        sql.push_str(" AND se.entity_type = 'issue' AND im.state = ?");
        params.push(Value::Text(state.clone()));
    }
    if !filters.labels.is_empty() {
        sql.push_str(" AND se.entity_type = 'issue'");
        for label in &filters.labels {
            sql.push_str(
                " AND EXISTS (
                    SELECT 1
                    FROM json_each(im.labels_json)
                    WHERE json_each.value = ?
                )",
            );
            params.push(Value::Text(label.clone()));
        }
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
    if let Some(version_id) = tx
        .query_row(
            "SELECT id FROM source_versions
             WHERE source_id = ?1 AND body_hash = ?2
             ORDER BY id DESC
             LIMIT 1",
            params![source_id, body_hash],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
    {
        tx.execute(
            "UPDATE source_versions
             SET github_updated_at = ?1,
                 indexed_at = ?2,
                 sync_run_id = ?3,
                 lifecycle_state = 'active'
             WHERE id = ?4",
            params![github_updated_at, indexed_at, sync_run_id, version_id],
        )?;
        return Ok(version_id);
    }

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

fn stored_chunk_from_row(row: &rusqlite::Row<'_>) -> Result<StoredChunk, rusqlite::Error> {
    let heading_path_json: String = row.get(11)?;
    let heading_path = serde_json::from_str(&heading_path_json).unwrap_or_default();
    Ok(StoredChunk {
        chunk_id: row.get(0)?,
        source_id: row.get(1)?,
        source_version_id: row.get(2)?,
        body: row.get(3)?,
        chunk_index: row.get::<_, i64>(4)? as usize,
        token_start: row.get::<_, i64>(5)? as usize,
        token_end: row.get::<_, i64>(6)? as usize,
        byte_start: row.get::<_, i64>(7)? as usize,
        byte_end: row.get::<_, i64>(8)? as usize,
        chunker_version: row.get(9)?,
        chunker_fingerprint: row.get(10)?,
        heading_path,
    })
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

    #[test]
    fn base_store_open_does_not_initialize_vector_capability() {
        let paths = temp_profile_paths("base-store-open");
        let store = Store::open(&paths).unwrap();

        let vec_version = store
            .conn
            .query_row("SELECT vec_version()", [], |row| row.get::<_, String>(0));
        assert!(vec_version.is_err(), "base store loaded sqlite-vec");
        assert!(!vector_table_exists(&store.conn));
        let tables = table_names(&store.conn);
        for excluded in ["chunks", "embedding_fingerprints", "chunk_embeddings"] {
            assert!(
                !tables.iter().any(|table| table == excluded),
                "base store unexpectedly created vector table `{excluded}`: {tables:?}"
            );
        }

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn vector_storage_migration_is_dimension_driven_and_idempotent() {
        let paths = temp_profile_paths("vector-storage-idempotent");
        let mut store = Store::open(&paths).unwrap();
        store.enable_vector().unwrap();
        assert!(!vector_table_exists(&store.conn));

        store.ensure_vector_storage(3).unwrap();
        let first_sql = vector_table_sql(&store.conn).unwrap();
        assert!(first_sql.contains("vec0"));
        assert!(first_sql.contains("float[3]"));

        store.ensure_vector_storage(3).unwrap();
        assert_eq!(vector_table_sql(&store.conn).unwrap(), first_sql);

        store
            .conn
            .execute(
                &format!(
                    "INSERT INTO {CHUNK_EMBEDDING_VECTORS_TABLE}(rowid, embedding)
                     VALUES (?1, ?2)"
                ),
                params![1_i64, embedding_vector_blob(&[0.1, 0.2, 0.3])],
            )
            .unwrap();
        let stored: String = store
            .conn
            .query_row(
                &format!("SELECT vec_to_json(embedding) FROM {CHUNK_EMBEDDING_VECTORS_TABLE}"),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored, "[0.100000,0.200000,0.300000]");

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn vector_storage_rebuilds_when_fingerprint_dimension_changes() {
        let paths = temp_profile_paths("vector-storage-dimension-change");
        let mut store = Store::open(&paths).unwrap();
        store.enable_vector().unwrap();
        let chunk_id = insert_test_issue_chunk(
            &mut store,
            "qgh://github.com/issue/I_VECTOR_DIMENSION_CHANGE",
            "sync-vector-dimension-change",
        );
        let first = embedding_fingerprint_with_dimension("Example/first-model", 3);
        let second = embedding_fingerprint_with_dimension("Example/second-model", 4);

        store
            .replace_all_chunk_embeddings(&first, &[(chunk_id, vec![0.1, 0.2, 0.3])])
            .unwrap();
        assert!(vector_table_sql(&store.conn).unwrap().contains("float[3]"));
        assert_eq!(vector_row_count(&store.conn), 1);

        store
            .replace_all_chunk_embeddings(&second, &[(chunk_id, vec![0.4, 0.5, 0.6, 0.7])])
            .unwrap();

        assert!(vector_table_sql(&store.conn).unwrap().contains("float[4]"));
        assert_eq!(vector_row_count(&store.conn), 1);
        let stored: String = store
            .conn
            .query_row(
                &format!("SELECT vec_to_json(embedding) FROM {CHUNK_EMBEDDING_VECTORS_TABLE}"),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored, "[0.400000,0.500000,0.600000,0.700000]");

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn chunks_round_trip_through_source_version_mapping() {
        let paths = temp_profile_paths("chunks-source-version-round-trip");
        let mut store = Store::open(&paths).unwrap();
        store.enable_vector().unwrap();
        let source_id = "qgh://github.com/issue/I_CHUNK_ROUNDTRIP";
        let issue = IssueRecord {
            source_id: source_id.to_string(),
            host: "github.com".to_string(),
            repo: "owner/repo".to_string(),
            node_id: "I_CHUNK_ROUNDTRIP".to_string(),
            github_id: 101,
            number: 7,
            title: "Chunk round trip".to_string(),
            body: "alpha beta gamma delta".to_string(),
            state: "open".to_string(),
            labels: Vec::new(),
            milestone: None,
            assignees: Vec::new(),
            author: Some("alice".to_string()),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-02T00:00:00Z".to_string(),
            closed_at: None,
            canonical_url: "https://github.com/owner/repo/issues/7".to_string(),
            body_hash: "body-hash-1".to_string(),
            indexed_at: "2026-01-02T00:00:01Z".to_string(),
        };
        store
            .upsert_sources_for_run("sync-chunk-test", &[issue], &[], 0, &[])
            .unwrap();
        let source_version_id = store.latest_source_version_id(source_id).unwrap().unwrap();
        let chunks = vec![
            MarkdownChunk {
                chunk_index: 0,
                byte_start: 0,
                byte_end: 10,
                token_start: 0,
                token_end: 2,
                token_count: 2,
                body: "alpha beta".to_string(),
                chunker_version: crate::chunking::CHUNKER_VERSION.to_string(),
                chunker_fingerprint: crate::chunking::CHUNKER_FINGERPRINT.to_string(),
                heading_path: Vec::new(),
            },
            MarkdownChunk {
                chunk_index: 1,
                byte_start: 6,
                byte_end: 22,
                token_start: 1,
                token_end: 4,
                token_count: 3,
                body: "beta gamma delta".to_string(),
                chunker_version: crate::chunking::CHUNKER_VERSION.to_string(),
                chunker_fingerprint: crate::chunking::CHUNKER_FINGERPRINT.to_string(),
                heading_path: Vec::new(),
            },
        ];

        let stored = store
            .replace_chunks_for_source_version(source_id, source_version_id, &chunks)
            .unwrap();
        let loaded = store.chunks_for_source_version(source_version_id).unwrap();

        assert_eq!(stored, loaded);
        assert_eq!(loaded.len(), 2);
        assert!(loaded.iter().all(|chunk| chunk.chunk_id > 0));
        assert!(loaded.iter().all(|chunk| chunk.source_id == source_id));
        assert!(loaded
            .iter()
            .all(|chunk| chunk.source_version_id == source_version_id));
        assert_eq!(loaded[0].body, "alpha beta");
        assert_eq!(loaded[1].body, "beta gamma delta");

        let stored_again = store
            .replace_chunks_for_source_version(source_id, source_version_id, &chunks)
            .unwrap();
        assert_eq!(stored_again.len(), 2);
        assert_eq!(
            store
                .chunks_for_source_version(source_version_id)
                .unwrap()
                .len(),
            2
        );

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn chunk_embeddings_are_usable_only_for_matching_fingerprint() {
        let paths = temp_profile_paths("embedding-fingerprint-gate");
        let mut store = Store::open(&paths).unwrap();
        store.enable_vector().unwrap();
        let source_id = "qgh://github.com/issue/I_EMBED_GATE";
        let issue = IssueRecord {
            source_id: source_id.to_string(),
            host: "github.com".to_string(),
            repo: "owner/repo".to_string(),
            node_id: "I_EMBED_GATE".to_string(),
            github_id: 202,
            number: 8,
            title: "Embedding gate".to_string(),
            body: "alpha beta gamma delta".to_string(),
            state: "open".to_string(),
            labels: Vec::new(),
            milestone: None,
            assignees: Vec::new(),
            author: Some("alice".to_string()),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-02T00:00:00Z".to_string(),
            closed_at: None,
            canonical_url: "https://github.com/owner/repo/issues/8".to_string(),
            body_hash: "body-hash-embed".to_string(),
            indexed_at: "2026-01-02T00:00:01Z".to_string(),
        };
        store
            .upsert_sources_for_run("sync-embed-test", &[issue], &[], 0, &[])
            .unwrap();
        let source_version_id = store.latest_source_version_id(source_id).unwrap().unwrap();
        let chunks = vec![MarkdownChunk {
            chunk_index: 0,
            byte_start: 0,
            byte_end: 10,
            token_start: 0,
            token_end: 2,
            token_count: 2,
            body: "alpha beta".to_string(),
            chunker_version: crate::chunking::CHUNKER_VERSION.to_string(),
            chunker_fingerprint: crate::chunking::CHUNKER_FINGERPRINT.to_string(),
            heading_path: Vec::new(),
        }];
        let stored_chunks = store
            .replace_chunks_for_source_version(source_id, source_version_id, &chunks)
            .unwrap();
        let matching = embedding_fingerprint("Snowflake/snowflake-arctic-embed-l-v2.0");
        let mismatched = embedding_fingerprint("Example/other-model");

        store
            .replace_all_chunk_embeddings(
                &matching,
                &[(stored_chunks[0].chunk_id, vec![0.1, 0.2, 0.3])],
            )
            .unwrap();

        assert_eq!(
            store
                .current_chunk_embedding_count_for_fingerprint(&matching)
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .current_chunk_embedding_count_for_fingerprint(&mismatched)
                .unwrap(),
            0
        );

        store
            .conn
            .execute(&format!("DROP TABLE {CHUNK_EMBEDDING_VECTORS_TABLE}"), [])
            .unwrap();
        assert!(!vector_table_exists(&store.conn));
        assert_eq!(
            store
                .ensure_vector_storage_for_fingerprint(&matching)
                .unwrap(),
            1
        );
        let stored_vectors: i64 = store
            .conn
            .query_row(
                &format!("SELECT count(*) FROM {CHUNK_EMBEDDING_VECTORS_TABLE}"),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored_vectors, 1);

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn cleanup_inactive_embedding_artifacts_deletes_vector_rows() {
        let paths = temp_profile_paths("embedding-vector-cleanup");
        let mut store = Store::open(&paths).unwrap();
        store.enable_vector().unwrap();
        let source_id = "qgh://github.com/issue/I_VECTOR_CLEANUP";
        let chunk_id = insert_test_issue_chunk(&mut store, source_id, "sync-vector-cleanup");
        let fingerprint = embedding_fingerprint("Snowflake/snowflake-arctic-embed-l-v2.0");

        store
            .replace_all_chunk_embeddings(&fingerprint, &[(chunk_id, vec![0.1, 0.2, 0.3])])
            .unwrap();
        assert_eq!(vector_row_count(&store.conn), 1);
        assert_eq!(chunk_embedding_row_count(&store.conn), 1);

        store.tombstone_source(source_id, "deleted").unwrap();
        assert_eq!(store.cleanup_inactive_embedding_artifacts().unwrap(), 1);

        assert_eq!(vector_row_count(&store.conn), 0);
        assert_eq!(chunk_embedding_row_count(&store.conn), 0);

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(feature = "vector-search")]
    fn embedding_fingerprint(model_id: &str) -> EmbeddingFingerprint {
        embedding_fingerprint_with_dimension(model_id, 3)
    }

    #[cfg(feature = "vector-search")]
    fn embedding_fingerprint_with_dimension(
        model_id: &str,
        dimension: usize,
    ) -> EmbeddingFingerprint {
        crate::embedding::EmbeddingFingerprintSeed {
            provider: "local".to_string(),
            model_id: model_id.to_string(),
            model_revision: "fixture-sha".to_string(),
            pooling: crate::embedding::PoolingKind::Cls,
            query_prefix: crate::embedding::DEFAULT_QUERY_PREFIX.to_string(),
        }
        .with_dimension(dimension)
    }

    fn vector_table_exists(conn: &Connection) -> bool {
        vector_table_sql(conn).is_some()
    }

    fn vector_table_sql(conn: &Connection) -> Option<String> {
        conn.query_row(
            "SELECT sql
             FROM sqlite_schema
             WHERE type = 'table' AND name = ?1",
            params![CHUNK_EMBEDDING_VECTORS_TABLE],
            |row| row.get(0),
        )
        .optional()
        .unwrap()
    }

    #[cfg(feature = "vector-search")]
    fn vector_row_count(conn: &Connection) -> i64 {
        conn.query_row(
            &format!("SELECT count(*) FROM {CHUNK_EMBEDDING_VECTORS_TABLE}"),
            [],
            |row| row.get(0),
        )
        .unwrap()
    }

    #[cfg(feature = "vector-search")]
    fn chunk_embedding_row_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT count(*) FROM chunk_embeddings", [], |row| {
            row.get(0)
        })
        .unwrap()
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn embedding_generation_stages_resumes_and_validates_little_endian_vectors() {
        let paths = temp_profile_paths("generation-stage");
        let mut store = Store::open(&paths).unwrap();
        store.enable_vector().unwrap();
        store.ensure_vector_storage(2).unwrap();
        let chunk_id = insert_test_issue_chunk(
            &mut store,
            "qgh://github.com/issue/I_GENERATION",
            "generation-sync",
        );
        let source_version_id = store
            .latest_source_version_id("qgh://github.com/issue/I_GENERATION")
            .unwrap()
            .unwrap();
        let spec = EmbeddingGenerationSpec {
            model_manifest_hash: "manifest-a".to_string(),
            chunker_fingerprint: "chunker-a".to_string(),
            context_template_version: "context-v1".to_string(),
            output_dimension: 2,
            source_sync_run_id: "generation-sync".to_string(),
            source_snapshot_hash: "snapshot-a".to_string(),
            total_chunks: 1,
        };
        let generation_id = store.begin_embedding_generation(&spec).unwrap();
        store
            .stage_embedding_generation_batch(
                generation_id,
                &[EmbeddingGenerationChunk {
                    chunk_id,
                    source_version_id,
                    source_version_hash: "body-hash-generation-sync".to_string(),
                    context_hash: embedding_context_hash(
                        "manifest-a",
                        "chunker-a",
                        "context-v1",
                        "alpha beta",
                    ),
                    vector: vec![1.0, 2.0],
                }],
            )
            .unwrap();
        assert_eq!(
            store.begin_embedding_generation(&spec).unwrap(),
            generation_id
        );
        let staged = store
            .embedding_generation_chunk_blob(generation_id, chunk_id)
            .unwrap();
        assert_eq!(staged.dimension, 2);
        assert_eq!(staged.bytes, vec![0, 0, 128, 63, 0, 0, 0, 64]);
        store.validate_embedding_generation(generation_id).unwrap();
        assert_eq!(
            store.embedding_generation_state(generation_id).unwrap(),
            "ready"
        );
        store
            .conn
            .execute(
                "UPDATE embedding_generation_chunks SET context_hash = 'wrong' WHERE generation_id = ?1",
                params![generation_id],
            )
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE embedding_generations SET state = 'building' WHERE id = ?1",
                params![generation_id],
            )
            .unwrap();
        assert_eq!(
            store
                .validate_embedding_generation(generation_id)
                .unwrap_err()
                .code,
            "embedding.generation_validation_failed"
        );
        assert_eq!(
            store.embedding_generation_state(generation_id).unwrap(),
            "failed"
        );
        assert!(table_names(&store.conn)
            .iter()
            .any(|name| name == "chunk_embedding_vectors"));
        let second_spec = EmbeddingGenerationSpec {
            output_dimension: 3,
            model_manifest_hash: "manifest-b".to_string(),
            ..spec.clone()
        };
        let second_generation = store.begin_embedding_generation(&second_spec).unwrap();
        store
            .stage_embedding_generation_batch(
                second_generation,
                &[EmbeddingGenerationChunk {
                    chunk_id,
                    source_version_id,
                    source_version_hash: "body-hash-generation-sync".to_string(),
                    context_hash: embedding_context_hash(
                        "manifest-b",
                        "chunker-a",
                        "context-v1",
                        "alpha beta",
                    ),
                    vector: vec![1.0, 2.0, 3.0],
                }],
            )
            .unwrap();
        assert!(table_names(&store.conn)
            .iter()
            .any(|name| name == "embedding_generation_vectors_d2"));
        assert!(table_names(&store.conn)
            .iter()
            .any(|name| name == "embedding_generation_vectors_d3"));
        store
            .validate_embedding_generation(second_generation)
            .unwrap();
        let publication = store
            .activate_retrieval_publication("generation-sync", 1, Some(second_generation), None)
            .unwrap();
        let publication_view = store.active_retrieval_publication().unwrap().unwrap();
        assert_eq!(publication_view.publication_id, publication);
        assert_eq!(
            publication_view.embedding_generation_id,
            Some(second_generation)
        );
        assert_eq!(publication_view.output_dimension, Some(3));
        let missing_generation = store
            .begin_embedding_generation(&EmbeddingGenerationSpec {
                model_manifest_hash: "manifest-missing".to_string(),
                ..second_spec.clone()
            })
            .unwrap();
        store
            .stage_embedding_generation_batch(
                missing_generation,
                &[EmbeddingGenerationChunk {
                    chunk_id,
                    source_version_id,
                    source_version_hash: "body-hash-generation-sync".to_string(),
                    context_hash: embedding_context_hash(
                        "manifest-missing",
                        "chunker-a",
                        "context-v1",
                        "alpha beta",
                    ),
                    vector: vec![1.0, 2.0, 3.0],
                }],
            )
            .unwrap();
        store
            .conn
            .execute("DELETE FROM chunks WHERE id = ?1", params![chunk_id])
            .unwrap();
        assert_eq!(
            store
                .validate_embedding_generation(missing_generation)
                .unwrap_err()
                .code,
            "embedding.generation_validation_failed"
        );
        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn retrieval_publication_cas_keeps_bm25_embedding_null_and_rolls_back_conflicts() {
        let paths = temp_profile_paths("publication-cas");
        let mut store = Store::open(&paths).unwrap();
        let first = store
            .activate_retrieval_publication("sync-one", 1, None, None)
            .unwrap();
        let active = store.active_retrieval_publication().unwrap().unwrap();
        assert_eq!(active.publication_id, first);
        assert_eq!(active.embedding_generation_id, None);
        assert_eq!(active.tantivy_generation, 1);
        let second = store
            .activate_retrieval_publication("sync-two", 2, None, Some(first))
            .unwrap();
        assert_ne!(first, second);
        let conflict = store.activate_retrieval_publication("sync-three", 3, None, Some(first));
        assert_eq!(conflict.unwrap_err().code, "publication.cas_conflict");
        assert_eq!(
            store
                .active_retrieval_publication()
                .unwrap()
                .unwrap()
                .publication_id,
            second
        );
        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn invalid_generation_checksum_fails_without_touching_legacy_storage() {
        let paths = temp_profile_paths("generation-invalid-checksum");
        let mut store = Store::open(&paths).unwrap();
        store.enable_vector().unwrap();
        store.ensure_vector_storage(2).unwrap();
        let chunk_id = insert_test_issue_chunk(
            &mut store,
            "qgh://github.com/issue/I_GENERATION_BAD",
            "generation-bad-sync",
        );
        let source_version_id = store
            .latest_source_version_id("qgh://github.com/issue/I_GENERATION_BAD")
            .unwrap()
            .unwrap();
        let generation_id = store
            .begin_embedding_generation(&EmbeddingGenerationSpec {
                model_manifest_hash: "manifest-bad".to_string(),
                chunker_fingerprint: "chunker-bad".to_string(),
                context_template_version: "context-v1".to_string(),
                output_dimension: 2,
                source_sync_run_id: "generation-bad-sync".to_string(),
                source_snapshot_hash: "snapshot-bad".to_string(),
                total_chunks: 1,
            })
            .unwrap();
        store
            .stage_embedding_generation_batch(
                generation_id,
                &[EmbeddingGenerationChunk {
                    chunk_id,
                    source_version_id,
                    source_version_hash: "body-hash-generation-bad".to_string(),
                    context_hash: embedding_context_hash(
                        "manifest-bad",
                        "chunker-bad",
                        "context-v1",
                        "alpha beta",
                    ),
                    vector: vec![1.0, 2.0],
                }],
            )
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE embedding_generation_chunks SET vector_checksum = 'bad' WHERE generation_id = ?1",
                params![generation_id],
            )
            .unwrap();
        assert_eq!(
            store
                .validate_embedding_generation(generation_id)
                .unwrap_err()
                .code,
            "embedding.generation_validation_failed"
        );
        assert_eq!(
            store.embedding_generation_state(generation_id).unwrap(),
            "failed"
        );
        assert!(table_names(&store.conn)
            .iter()
            .any(|name| name == "chunk_embedding_vectors"));
        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn stale_building_generation_is_removed_only_by_explicit_cleanup() {
        let paths = temp_profile_paths("generation-retention");
        let mut store = Store::open(&paths).unwrap();
        store.enable_vector().unwrap();
        let generation_id = store
            .begin_embedding_generation(&EmbeddingGenerationSpec {
                model_manifest_hash: "manifest-retention".to_string(),
                chunker_fingerprint: "chunker-retention".to_string(),
                context_template_version: "context-v1".to_string(),
                output_dimension: 2,
                source_sync_run_id: "sync-retention".to_string(),
                source_snapshot_hash: "snapshot-retention".to_string(),
                total_chunks: 0,
            })
            .unwrap();
        assert_eq!(
            store.embedding_generation_state(generation_id).unwrap(),
            "building"
        );
        assert_eq!(
            store
                .cleanup_embedding_generations("9999-01-01T00:00:00Z", "9999-01-01T00:00:00Z")
                .unwrap(),
            1
        );
        assert!(store.embedding_generation_state(generation_id).is_err());
        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(feature = "vector-search")]
    fn insert_test_issue_chunk(store: &mut Store, source_id: &str, sync_run_id: &str) -> i64 {
        let issue = IssueRecord {
            source_id: source_id.to_string(),
            host: "github.com".to_string(),
            repo: "owner/repo".to_string(),
            node_id: source_id.rsplit('/').next().unwrap().to_string(),
            github_id: 303,
            number: 9,
            title: "Vector storage regression".to_string(),
            body: "alpha beta gamma delta".to_string(),
            state: "open".to_string(),
            labels: Vec::new(),
            milestone: None,
            assignees: Vec::new(),
            author: Some("alice".to_string()),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-02T00:00:00Z".to_string(),
            closed_at: None,
            canonical_url: "https://github.com/owner/repo/issues/9".to_string(),
            body_hash: format!("body-hash-{sync_run_id}"),
            indexed_at: "2026-01-02T00:00:01Z".to_string(),
        };
        store
            .upsert_sources_for_run(sync_run_id, &[issue], &[], 0, &[])
            .unwrap();
        let source_version_id = store.latest_source_version_id(source_id).unwrap().unwrap();
        let chunks = vec![MarkdownChunk {
            chunk_index: 0,
            byte_start: 0,
            byte_end: 10,
            token_start: 0,
            token_end: 2,
            token_count: 2,
            body: "alpha beta".to_string(),
            chunker_version: crate::chunking::CHUNKER_VERSION.to_string(),
            chunker_fingerprint: crate::chunking::CHUNKER_FINGERPRINT.to_string(),
            heading_path: Vec::new(),
        }];
        store
            .replace_chunks_for_source_version(source_id, source_version_id, &chunks)
            .unwrap()[0]
            .chunk_id
    }

    fn table_names(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name")
            .unwrap();
        stmt.query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
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
