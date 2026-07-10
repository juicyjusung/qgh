mod purge_fs;

use self::purge_fs::{
    filesystem_identity, filesystem_identity_from_file, open_anchored_directory,
    remove_anchored_directory_contents, sync_directory, FilesystemIdentity,
};
use crate::chunking::MarkdownChunk;
use crate::context::{prepare_embedding_input, EmbeddingSourceContext, PreparedEmbeddingInput};
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
use rusqlite::{
    params, params_from_iter, Connection, OpenFlags, OptionalExtension, Transaction,
    TransactionBehavior,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
#[cfg(feature = "vector-search")]
use std::os::raw::{c_char, c_int};
use std::path::{Path, PathBuf};

const CHUNK_EMBEDDING_VECTORS_TABLE: &str = "chunk_embedding_vectors";
#[cfg(not(feature = "vector-search"))]
const CHUNK_EMBEDDING_VECTOR_CHUNKS_META_TABLE: &str = "chunk_embedding_vectors_chunks";
const CHUNK_EMBEDDING_VECTOR_ROWIDS_TABLE: &str = "chunk_embedding_vectors_rowids";
const CHUNK_EMBEDDING_VECTOR_CHUNKS_TABLE: &str = "chunk_embedding_vectors_vector_chunks00";
const TANTIVY_COMMIT_INVENTORY_MIGRATION: &str = "qgh.tantivy.commit_inventory.v1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingGenerationSpec {
    pub model_manifest_hash: String,
    pub runtime_fingerprint_hash: String,
    pub chunker_fingerprint: String,
    pub context_template_version: String,
    pub output_dimension: usize,
}

type EmbeddingGenerationValidationRow = (
    String,
    i64,
    i64,
    i64,
    String,
    String,
    String,
    String,
    i64,
    String,
    String,
    i64,
    Option<String>,
);

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
    pub source_snapshot_epoch: i64,
    pub tantivy_generation: i64,
    pub embedding_generation_id: Option<i64>,
    pub model_manifest_hash: Option<String>,
    pub runtime_fingerprint_hash: Option<String>,
    pub chunker_fingerprint: Option<String>,
    pub context_template_version: Option<String>,
    pub output_dimension: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSnapshotIdentity {
    sync_run_id: String,
    epoch: i64,
}

impl SourceSnapshotIdentity {
    pub(crate) fn sync_run_id(&self) -> &str {
        &self.sync_run_id
    }
}

/// A qgh-managed lifecycle target. Source purges affect exactly one stable
/// source identity; issue purges affect the issue and every known comment in
/// that thread; repository purges affect every source in that explicit
/// `owner/repo` scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PurgeTarget {
    Source { source_id: String },
    Issue { repo: String, issue_number: i64 },
    Repository { repo: String },
}

impl PurgeTarget {
    /// Stable content-free target-kind label for status/doctor output.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Source { .. } => "source",
            Self::Issue { .. } => "issue",
            Self::Repository { .. } => "repository",
        }
    }

    fn kind_and_value(&self) -> (&'static str, String) {
        match self {
            Self::Source { source_id } => (self.kind(), source_id.clone()),
            Self::Issue { repo, issue_number } => (self.kind(), format!("{repo}#{issue_number}")),
            Self::Repository { repo } => (self.kind(), repo.clone()),
        }
    }

    fn from_stored(kind: &str, value: String) -> Result<Self, QghError> {
        match kind {
            "source" => Ok(Self::Source { source_id: value }),
            "issue" => {
                let (repo, issue_number) = value.rsplit_once('#').ok_or_else(purge_error)?;
                let issue_number = issue_number.parse::<i64>().map_err(|_| purge_error())?;
                Ok(Self::Issue {
                    repo: repo.to_string(),
                    issue_number,
                })
            }
            "repository" => Ok(Self::Repository { repo: value }),
            _ => Err(purge_error()),
        }
    }
}

/// Stable, content-free reasons that are allowed to trigger destructive
/// lifecycle cleanup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PurgeTrigger {
    ConfirmedDelete,
    ConfirmedTombstone,
    PermissionLoss,
    AllowlistRemoval,
}

impl PurgeTrigger {
    /// Stable content-free label for status/doctor output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ConfirmedDelete => "confirmed_delete",
            Self::ConfirmedTombstone => "confirmed_tombstone",
            Self::PermissionLoss => "permission_loss",
            Self::AllowlistRemoval => "allowlist_removal",
        }
    }

    fn from_stored(value: &str) -> Result<Self, QghError> {
        match value {
            "confirmed_delete" => Ok(Self::ConfirmedDelete),
            "confirmed_tombstone" => Ok(Self::ConfirmedTombstone),
            "permission_loss" => Ok(Self::PermissionLoss),
            "allowlist_removal" => Ok(Self::AllowlistRemoval),
            _ => Err(purge_error()),
        }
    }

    fn tombstone_reason(self) -> &'static str {
        match self {
            Self::ConfirmedDelete => "deleted",
            Self::ConfirmedTombstone => "transferred",
            Self::PermissionLoss => "permission_loss",
            Self::AllowlistRemoval => "allowlist_removal",
        }
    }
}

/// Coarse, content-free retry location persisted after a partial purge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PurgeFailureStage {
    SecureDelete,
    Tantivy,
    Storage,
    WalCheckpoint,
    Finalize,
}

impl PurgeFailureStage {
    /// Stable content-free label for status/doctor output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SecureDelete => "secure_delete",
            Self::Tantivy => "tantivy",
            Self::Storage => "storage",
            Self::WalCheckpoint => "wal_checkpoint",
            Self::Finalize => "finalize",
        }
    }

    fn from_stored(value: &str) -> Result<Self, QghError> {
        match value {
            "secure_delete" => Ok(Self::SecureDelete),
            "tantivy" => Ok(Self::Tantivy),
            "storage" => Ok(Self::Storage),
            "wal_checkpoint" => Ok(Self::WalCheckpoint),
            "finalize" => Ok(Self::Finalize),
            _ => Err(purge_error()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPurgeView {
    pub target: PurgeTarget,
    pub trigger: PurgeTrigger,
    pub current_stage: PurgeFailureStage,
    pub failure_stage: Option<PurgeFailureStage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PurgeOutcome {
    pub target: PurgeTarget,
    pub purged_sources: usize,
    pub purged_issues: usize,
    pub purged_comments: usize,
    pub discarded_embedding_generations: usize,
    pub discarded_tantivy_generations: usize,
    /// True once WAL frames that could contain purged content were checkpointed
    /// and truncated. Content-free completion frames may be written afterward.
    pub sensitive_wal_truncated: bool,
}

/// Opaque epoch captured by a Store read transaction. Callers must end the
/// snapshot and revalidate this fence before releasing loaded content.
#[derive(Debug, PartialEq, Eq)]
#[must_use]
pub struct ReadSnapshotFence {
    content_write_epoch: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextualEmbeddingChunk {
    pub chunk: StoredChunk,
    pub prepared_input: PreparedEmbeddingInput,
}

#[derive(Debug, Clone)]
pub struct RetrievalBuildSnapshot {
    identity: SourceSnapshotIdentity,
    expected_publication_id: Option<i64>,
    sources: Vec<IndexSource>,
    embedding_chunks: Vec<ContextualEmbeddingChunk>,
    source_inventory_hash: String,
    embedding_inventory_hash: String,
}

impl RetrievalBuildSnapshot {
    pub(crate) fn identity(&self) -> &SourceSnapshotIdentity {
        &self.identity
    }

    pub(crate) fn expected_publication_id(&self) -> Option<i64> {
        self.expected_publication_id
    }

    pub(crate) fn sources(&self) -> &[IndexSource] {
        &self.sources
    }

    pub(crate) fn embedding_chunks(&self) -> &[ContextualEmbeddingChunk] {
        &self.embedding_chunks
    }
}

pub struct Store {
    conn: Connection,
    profile_dir: PathBuf,
    index_root: PathBuf,
    content_write_epoch: i64,
    index_build_tokens: BTreeMap<i64, String>,
    #[cfg(test)]
    purge_failure_stage: Option<PurgeFailureStage>,
    #[cfg(test)]
    purge_queue_failure_after_first: bool,
    #[cfg(test)]
    cleanup_promote_generation_after_scan: Option<i64>,
    #[cfg(test)]
    cleanup_fail_after_first_generation_delete: bool,
    #[cfg(test)]
    purge_swap_generation_after_validation: Option<i64>,
    #[cfg(test)]
    purge_swap_quarantine_after_open: Option<i64>,
    #[cfg(test)]
    activation_failure: Option<QghError>,
}

impl Store {
    pub fn new_sync_run_id() -> String {
        format!("sync-{}", now_run_id_suffix())
    }

    pub fn begin_read_snapshot(&self) -> Result<ReadSnapshotFence, QghError> {
        self.begin_read_snapshot_with_repository_allowlist(None)
    }

    pub fn validate_profile_read_allowlist(
        &self,
        allowed_repository_keys: &BTreeSet<String>,
    ) -> Result<(), QghError> {
        let _fence =
            self.begin_read_snapshot_with_repository_allowlist(Some(allowed_repository_keys))?;
        self.rollback_read_snapshot()
    }

    pub fn begin_profile_read_snapshot(
        &self,
        allowed_repository_keys: &BTreeSet<String>,
    ) -> Result<ReadSnapshotFence, QghError> {
        self.begin_read_snapshot_with_repository_allowlist(Some(allowed_repository_keys))
    }

    fn begin_read_snapshot_with_repository_allowlist(
        &self,
        allowed_repository_keys: Option<&BTreeSet<String>>,
    ) -> Result<ReadSnapshotFence, QghError> {
        self.conn.execute_batch("BEGIN")?;
        let state = self.conn.query_row(
            "SELECT CAST(value AS INTEGER),
                    EXISTS(SELECT 1 FROM purge_requests WHERE purge_pending = 1)
             FROM profile_meta WHERE key = 'content_write_epoch'",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, bool>(1)?)),
        );
        let (content_write_epoch, purge_pending) = match state {
            Ok(state) => state,
            Err(error) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                return Err(error.into());
            }
        };
        if purge_pending {
            let _ = self.conn.execute_batch("ROLLBACK");
            return Err(read_fence_error());
        }
        if let Some(allowed_repository_keys) = allowed_repository_keys {
            let repositories = match self.known_repositories() {
                Ok(repositories) => repositories,
                Err(error) => {
                    let _ = self.conn.execute_batch("ROLLBACK");
                    return Err(error);
                }
            };
            if repositories
                .iter()
                .map(|repo| repo.to_ascii_lowercase())
                .any(|repo| !allowed_repository_keys.contains(&repo))
            {
                let _ = self.conn.execute_batch("ROLLBACK");
                return Err(allowlist_reconciliation_required_error());
            }
        }
        Ok(ReadSnapshotFence {
            content_write_epoch,
        })
    }

    /// Ends a read snapshot without releasing any loaded content. Use this on
    /// error paths where no output will be emitted.
    pub fn rollback_read_snapshot(&self) -> Result<(), QghError> {
        self.conn.execute_batch("ROLLBACK")?;
        Ok(())
    }

    /// Ends the old snapshot, then verifies the latest durable purge state.
    /// Loaded content is safe to release only when this returns `Ok(())`.
    pub fn end_read_snapshot_and_validate(&self, fence: ReadSnapshotFence) -> Result<(), QghError> {
        self.rollback_read_snapshot()?;
        let (current_epoch, purge_pending) = self.conn.query_row(
            "SELECT CAST(value AS INTEGER),
                    EXISTS(SELECT 1 FROM purge_requests WHERE purge_pending = 1)
             FROM profile_meta WHERE key = 'content_write_epoch'",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, bool>(1)?)),
        )?;
        if current_epoch != fence.content_write_epoch || purge_pending {
            return Err(read_fence_error());
        }
        Ok(())
    }

    pub fn open(paths: &ProfilePaths) -> Result<Self, QghError> {
        ensure_private_dir(&paths.profile_dir)?;
        ensure_private_dir(&paths.cache_dir)?;
        ensure_private_dir(&paths.log_dir)?;
        let conn = Connection::open(&paths.db_path)?;
        set_private_file(&paths.db_path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "secure_delete", "ON")?;
        let mut store = Self::from_connection(conn, paths);
        store.migrate()?;
        store.detach_unbound_tantivy_publication()?;
        store.detach_legacy_embedding_publication_identity()?;
        store.enforce_pending_purge_guards()?;
        store.content_write_epoch = read_content_write_epoch(&store.conn)?;
        Ok(store)
    }

    /// Opens an initialized store without applying migrations or operational
    /// repairs. Existing stores are opened with SQLite read-only flags so
    /// query/get/status/doctor cannot detach publications, advance purge
    /// guards, or otherwise turn observation into recovery work. A missing
    /// database is represented by an in-memory empty schema so read commands
    /// do not create profile, cache, log, or database files.
    pub fn open_for_read(paths: &ProfilePaths) -> Result<Self, QghError> {
        if !paths.db_path.exists() {
            let conn = Connection::open_in_memory()?;
            conn.busy_timeout(std::time::Duration::from_secs(5))?;
            conn.pragma_update(None, "secure_delete", "ON")?;
            let mut store = Self::from_connection(conn, paths);
            store.migrate()?;
            store.content_write_epoch = read_content_write_epoch(&store.conn)?;
            return Ok(store);
        }
        let conn = Connection::open_with_flags(
            &paths.db_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let mut store = Self::from_connection(conn, paths);
        store.validate_read_schema()?;
        store.content_write_epoch = read_content_write_epoch(&store.conn)?;
        Ok(store)
    }

    fn from_connection(conn: Connection, paths: &ProfilePaths) -> Self {
        Self {
            conn,
            profile_dir: paths.profile_dir.clone(),
            index_root: paths.index_root.clone(),
            content_write_epoch: 0,
            index_build_tokens: BTreeMap::new(),
            #[cfg(test)]
            purge_failure_stage: None,
            #[cfg(test)]
            purge_queue_failure_after_first: false,
            #[cfg(test)]
            cleanup_promote_generation_after_scan: None,
            #[cfg(test)]
            cleanup_fail_after_first_generation_delete: false,
            #[cfg(test)]
            purge_swap_generation_after_validation: None,
            #[cfg(test)]
            purge_swap_quarantine_after_open: None,
            #[cfg(test)]
            activation_failure: None,
        }
    }

    fn validate_read_schema(&self) -> Result<(), QghError> {
        let schema_version = self
            .conn
            .query_row(
                "SELECT value FROM profile_meta WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if schema_version.as_deref() != Some("qgh.db.v1") {
            return Err(QghError::storage(
                "The local store schema is not initialized for read-only retrieval. Run `qgh sync` to migrate it.",
            ));
        }
        Ok(())
    }

    /// Detaches an active publication whose Tantivy artifact does not carry the
    /// reserved canonical source inventory digest. The artifact and embedding
    /// payload stay intact for rollback/repair, but query cannot trust them.
    fn detach_unbound_tantivy_publication(&mut self) -> Result<(), QghError> {
        type ActiveArtifact = (i64, i64, String, i64, Option<String>, Option<i64>);
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let migration_complete = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM schema_migrations WHERE version = ?1)",
            params![TANTIVY_COMMIT_INVENTORY_MIGRATION],
            |row| row.get::<_, bool>(0),
        )?;
        if migration_complete {
            tx.commit()?;
            return Ok(());
        }
        let active_artifact = tx
            .query_row(
                "SELECT rp.publication_id, ig.generation, ig.path, ig.source_count,
                        ig.source_inventory_hash, rp.embedding_generation_id
                 FROM retrieval_publication_pointer p
                 JOIN retrieval_publications rp ON rp.publication_id = p.publication_id
                 JOIN index_generations ig ON ig.generation = rp.tantivy_generation
                 WHERE p.id = 1",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            publication_id,
            generation,
            stored_path,
            source_count,
            source_inventory_hash,
            embedding_generation_id,
        )): Option<ActiveArtifact> = active_artifact
        else {
            return finish_schema_migration(tx, TANTIVY_COMMIT_INVENTORY_MIGRATION);
        };
        let expected_path = self.index_root.join(format!("generation-{generation}"));
        if Path::new(&stored_path) != expected_path {
            return finish_schema_migration(tx, TANTIVY_COMMIT_INVENTORY_MIGRATION);
        }
        let Some(source_count) = usize::try_from(source_count).ok() else {
            return finish_schema_migration(tx, TANTIVY_COMMIT_INVENTORY_MIGRATION);
        };
        let Some(source_inventory_hash) = source_inventory_hash.as_deref() else {
            return finish_schema_migration(tx, TANTIVY_COMMIT_INVENTORY_MIGRATION);
        };
        match validate_tantivy_generation_artifact(
            &expected_path,
            source_count,
            source_inventory_hash,
        ) {
            Ok(()) => return finish_schema_migration(tx, TANTIVY_COMMIT_INVENTORY_MIGRATION),
            // Legacy generations that predate the builder commit payload (or
            // carry a different payload) are detached once during open-time
            // migration. Operational filesystem loss/corruption is only
            // reported by the read-only resolver below.
            Err(error) if error.code == "publication.source_inventory_mismatch" => {}
            Err(_) => {
                return finish_schema_migration(tx, TANTIVY_COMMIT_INVENTORY_MIGRATION);
            }
        }

        let embedding_table_exists = table_exists(&tx, "embedding_generations")?;
        let detached = tx.execute(
            "DELETE FROM retrieval_publication_pointer
             WHERE id = 1 AND publication_id = ?1",
            params![publication_id],
        )?;
        if detached == 0 {
            return finish_schema_migration(tx, TANTIVY_COMMIT_INVENTORY_MIGRATION);
        }
        tx.execute(
            "UPDATE retrieval_publications SET active = 0 WHERE publication_id = ?1",
            params![publication_id],
        )?;
        tx.execute(
            "UPDATE index_generations SET active = 0 WHERE generation = ?1",
            params![generation],
        )?;
        if embedding_table_exists {
            if let Some(embedding_generation_id) = embedding_generation_id {
                tx.execute(
                    "UPDATE embedding_generations SET state = 'ready'
                     WHERE id = ?1 AND state = 'active'",
                    params![embedding_generation_id],
                )?;
            }
        }
        finish_schema_migration(tx, TANTIVY_COMMIT_INVENTORY_MIGRATION)
    }

    /// Replaces an active legacy vector publication whose embedding identity
    /// cannot prove both the prepared manifest and runtime fingerprint. The
    /// trusted source/Tantivy snapshot remains queryable through a new
    /// BM25-only publication; embedding payloads stay intact for diagnosis.
    fn detach_legacy_embedding_publication_identity(&mut self) -> Result<(), QghError> {
        if !table_exists(&self.conn, "embedding_generations")? {
            return Ok(());
        }
        type LegacyPublication = (i64, String, i64, i64, i64);
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let legacy = tx
            .query_row(
                "SELECT rp.publication_id, rp.source_snapshot_sync_run_id,
                        rp.source_snapshot_epoch, rp.tantivy_generation,
                        rp.embedding_generation_id
                 FROM retrieval_publication_pointer p
                 JOIN retrieval_publications rp
                   ON rp.publication_id = p.publication_id
                 LEFT JOIN embedding_generations eg
                   ON eg.id = rp.embedding_generation_id
                 WHERE p.id = 1
                   AND rp.embedding_generation_id IS NOT NULL
                   AND (
                       rp.model_manifest_hash IS NULL
                       OR rp.model_manifest_hash = ''
                       OR rp.runtime_fingerprint_hash IS NULL
                       OR rp.runtime_fingerprint_hash = ''
                       OR rp.chunker_fingerprint IS NULL
                       OR rp.chunker_fingerprint = ''
                       OR rp.context_template_version IS NULL
                       OR rp.context_template_version = ''
                       OR rp.output_dimension IS NULL
                       OR rp.output_dimension <= 0
                       OR eg.id IS NULL
                       OR eg.model_manifest_hash IS NULL
                       OR eg.model_manifest_hash = ''
                       OR eg.runtime_fingerprint_hash IS NULL
                       OR eg.runtime_fingerprint_hash = ''
                       OR eg.chunker_fingerprint IS NULL
                       OR eg.chunker_fingerprint = ''
                       OR eg.context_template_version IS NULL
                       OR eg.context_template_version = ''
                       OR eg.output_dimension IS NULL
                       OR eg.output_dimension <= 0
                       OR rp.model_manifest_hash IS NOT eg.model_manifest_hash
                       OR rp.runtime_fingerprint_hash IS NOT eg.runtime_fingerprint_hash
                       OR rp.chunker_fingerprint IS NOT eg.chunker_fingerprint
                       OR rp.context_template_version IS NOT eg.context_template_version
                       OR rp.output_dimension IS NOT eg.output_dimension
                   )",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()?;
        let Some((legacy_publication_id, source_sync_run_id, source_epoch, tantivy, generation_id)):
            Option<LegacyPublication> = legacy
        else {
            tx.commit()?;
            return Ok(());
        };
        tx.execute(
            "INSERT INTO retrieval_publications
                (source_snapshot_sync_run_id, tantivy_generation,
                 source_snapshot_epoch, active, created_at)
             VALUES (?1, ?2, ?3, 1, ?4)",
            params![source_sync_run_id, tantivy, source_epoch, now_rfc3339()],
        )?;
        let successor_id = tx.last_insert_rowid();
        tx.execute(
            "UPDATE retrieval_publications
             SET active = CASE WHEN publication_id = ?1 THEN 1 ELSE 0 END",
            params![successor_id],
        )?;
        let pointer_moved = tx.execute(
            "UPDATE retrieval_publication_pointer
             SET publication_id = ?1
             WHERE id = 1 AND publication_id = ?2",
            params![successor_id, legacy_publication_id],
        )?;
        if pointer_moved != 1 {
            return Err(QghError::validation(
                "publication.cas_conflict",
                "Retrieval publication changed during legacy identity repair.",
            ));
        }
        tx.execute(
            "UPDATE embedding_generations
             SET state = 'failed', failure_code = ?2, updated_at = ?3
             WHERE id = ?1",
            params![
                generation_id,
                "embedding.legacy_identity_incomplete",
                now_rfc3339()
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    #[cfg(feature = "vector-search")]
    pub fn enable_vector(&mut self) -> Result<(), QghError> {
        register_sqlite_vec_extension(&self.conn)?;
        self.migrate_vector_schema()
    }

    #[cfg(feature = "vector-search")]
    pub fn enable_vector_for_read(&self) -> Result<(), QghError> {
        register_sqlite_vec_extension(&self.conn)
    }

    /// Durably marks `target` pending before attempting destructive work. A
    /// pending source is immediately ineligible for Store-backed query/get.
    /// Completion retains only the stable identity fields in `source_entities`
    /// (`source_id`, type, host, repo, node/GitHub IDs), lifecycle timestamps,
    /// and a content-free tombstone reason; content and derived data are removed.
    pub fn purge(
        &mut self,
        target: PurgeTarget,
        trigger: PurgeTrigger,
    ) -> Result<PurgeOutcome, QghError> {
        let queued = self.queue_purges(&[(target.clone(), trigger)])?;
        if queued == 0 {
            return Ok(PurgeOutcome {
                target,
                purged_sources: 0,
                purged_issues: 0,
                purged_comments: 0,
                discarded_embedding_generations: 0,
                discarded_tantivy_generations: 0,
                sensitive_wal_truncated: false,
            });
        }
        self.finish_pending_purge(target, trigger)
    }

    /// Atomically persists a batch of remotely confirmed lifecycle triggers as
    /// pending before any destructive purge stage begins. All actionable
    /// targets share one write-epoch bump and one publication invalidation.
    /// Identical duplicates are collapsed; conflicting triggers for the same
    /// target fail before mutation. The returned count excludes completed true
    /// no-ops.
    pub fn queue_purges(
        &mut self,
        requests: &[(PurgeTarget, PurgeTrigger)],
    ) -> Result<usize, QghError> {
        if requests.is_empty() {
            return Ok(0);
        }

        #[cfg(test)]
        let fail_after_first = std::mem::take(&mut self.purge_queue_failure_after_first);

        self.conn
            .execute_batch("BEGIN IMMEDIATE")
            .map_err(|_| purge_error())?;
        let result = (|| -> Result<(usize, i64), QghError> {
            let mut deduplicated = BTreeMap::new();
            for (target, trigger) in requests {
                validate_purge_target(target)?;
                let target = canonicalize_purge_target_identity(&self.conn, target)?;
                let (kind, value) = target.kind_and_value();
                let key = (kind.to_string(), value);
                if let Some((_, existing_trigger)) = deduplicated.get(&key) {
                    if existing_trigger != trigger {
                        return Err(conflicting_purge_trigger_error(
                            target.kind(),
                            *existing_trigger,
                            *trigger,
                        ));
                    }
                    continue;
                }
                deduplicated.insert(key, (target, *trigger));
            }
            let repository_targets = deduplicated
                .values()
                .filter_map(|(target, _)| match target {
                    PurgeTarget::Repository { repo } => Some(repo.to_ascii_lowercase()),
                    _ => None,
                })
                .collect::<BTreeSet<_>>();
            let issue_targets = deduplicated
                .values()
                .filter_map(|(target, _)| match target {
                    PurgeTarget::Issue { repo, issue_number } => {
                        Some((repo.to_ascii_lowercase(), *issue_number))
                    }
                    _ => None,
                })
                .collect::<BTreeSet<_>>();
            let mut actionable = Vec::new();
            for ((kind, value), (target, trigger)) in deduplicated {
                if purge_target_is_subsumed(
                    &self.conn,
                    &target,
                    &repository_targets,
                    &issue_targets,
                )
                .map_err(|_| purge_error())?
                {
                    continue;
                }
                if self.purge_is_noop(&target).map_err(|_| purge_error())? {
                    continue;
                }
                let pending_trigger = self
                    .conn
                    .query_row(
                        "SELECT trigger FROM purge_requests
                         WHERE target_kind = ?1 AND target_value = ?2
                           AND purge_pending = 1",
                        params![kind, value],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
                    .map_err(|_| purge_error())?;
                if let Some(pending_trigger) = pending_trigger {
                    if pending_trigger != trigger.as_str() {
                        let existing = PurgeTrigger::from_stored(&pending_trigger)
                            .map_err(|_| purge_error())?;
                        return Err(conflicting_purge_trigger_error(
                            target.kind(),
                            existing,
                            trigger,
                        ));
                    }
                }
                actionable.push((kind, value, target, trigger));
            }
            if actionable.is_empty() {
                return Ok((0, read_content_write_epoch(&self.conn)?));
            }

            self.conn
                .execute(
                    "UPDATE profile_meta
                     SET value = CAST(value AS INTEGER) + 1
                     WHERE key = 'content_write_epoch'",
                    [],
                )
                .map_err(|_| purge_error())?;
            let content_write_epoch =
                read_content_write_epoch(&self.conn).map_err(|_| purge_error())?;
            mark_successor_repair_required(&self.conn, content_write_epoch)
                .map_err(|_| purge_error())?;
            let now = now_rfc3339();
            for (kind, value, target, trigger) in &actionable {
                self.conn
                    .execute(
                        "INSERT INTO purge_requests
                            (target_kind, target_value, trigger, purge_pending,
                             current_stage, failure_stage, completion_ready, created_at, updated_at)
                         VALUES (?1, ?2, ?3, 1, 'secure_delete', NULL, 0, ?4, ?4)
                         ON CONFLICT(target_kind, target_value) DO UPDATE SET
                            trigger = excluded.trigger,
                            purge_pending = 1,
                            current_stage = 'secure_delete',
                            failure_stage = NULL,
                            completion_ready = 0,
                            updated_at = excluded.updated_at",
                        params![kind, value, trigger.as_str(), now],
                    )
                    .map_err(|_| purge_error())?;
                capture_purge_target_sources(&self.conn, target, kind, value)
                    .map_err(|_| purge_error())?;
                #[cfg(test)]
                if fail_after_first {
                    return Err(purge_error());
                }
            }
            self.conn
                .execute(
                    "UPDATE source_entities SET lifecycle_state = 'purge_pending'
                     WHERE lifecycle_state = 'active'
                       AND EXISTS (
                           SELECT 1
                           FROM purge_target_sources pts
                           JOIN purge_requests pr
                             ON pr.target_kind = pts.target_kind
                            AND pr.target_value = pts.target_value
                           WHERE pr.purge_pending = 1
                             AND pts.source_id = source_entities.source_id
                       )",
                    [],
                )
                .map_err(|_| purge_error())?;
            self.conn
                .execute(
                    "UPDATE source_versions SET lifecycle_state = 'purge_pending'
                     WHERE lifecycle_state = 'active'
                       AND source_id IN (
                           SELECT source_id FROM source_entities
                           WHERE lifecycle_state = 'purge_pending'
                       )",
                    [],
                )
                .map_err(|_| purge_error())?;
            bump_source_snapshot_epoch(&self.conn).map_err(|_| purge_error())?;
            invalidate_publications_for_pending_purge(&self.conn).map_err(|_| purge_error())?;
            Ok((actionable.len(), content_write_epoch))
        })();

        match result {
            Ok((queued, content_write_epoch)) => {
                if self.conn.execute_batch("COMMIT").is_err() {
                    let _ = self.conn.execute_batch("ROLLBACK");
                    return Err(purge_error());
                }
                self.content_write_epoch = content_write_epoch;
                Ok(queued)
            }
            Err(error) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                if matches!(
                    error.code.as_str(),
                    "purge.conflicting_triggers" | "purge.invalid_target"
                ) {
                    Err(error)
                } else {
                    Err(purge_error())
                }
            }
        }
    }

    /// Returns whether a lifecycle purge invalidated the published lexical
    /// snapshot and still requires a successfully activated successor. This is
    /// durable and intentionally independent from both purge-pending state and
    /// the presence of active sources.
    pub fn successor_repair_required(&self) -> Result<bool, QghError> {
        read_successor_repair_required(&self.conn)
    }

    /// Persists a content-free successful sync-run identity for the current
    /// post-purge write epoch. Callers must publish the successor using the
    /// returned identity; an older remote sync run is not authoritative for the
    /// post-purge source snapshot. Repeated calls in the same epoch are
    /// idempotent. Fresh profiles and already repaired profiles return `None`.
    pub fn record_purge_successor_snapshot(&mut self) -> Result<Option<String>, QghError> {
        let expected_epoch = self.content_write_epoch;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !read_successor_repair_required(&tx)? {
            tx.commit()?;
            return Ok(None);
        }
        let pending_count: i64 = tx.query_row(
            "SELECT count(*) FROM purge_requests WHERE purge_pending = 1",
            [],
            |row| row.get(0),
        )?;
        if pending_count != 0 {
            return Err(QghError::new(
                "purge.successor_snapshot_pending",
                "A successor snapshot cannot be recorded until every pending purge completes.",
                6,
            ));
        }
        let current_epoch = read_content_write_epoch(&tx)?;
        let source_snapshot_epoch = read_source_snapshot_epoch(&tx)?;
        if current_epoch != expected_epoch {
            return Err(write_fence_error());
        }
        let existing = tx
            .query_row(
                "SELECT id FROM sync_runs
                 WHERE snapshot_kind = 'purge_successor'
                   AND content_write_epoch = ?1
                   AND source_snapshot_epoch = ?2
                   AND completed_successfully = 1
                 ORDER BY rowid DESC LIMIT 1",
                params![current_epoch, source_snapshot_epoch],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(sync_run_id) = existing {
            tx.commit()?;
            return Ok(Some(sync_run_id));
        }

        let sync_run_id = format!("sync-purge-successor-{}", now_run_id_suffix());
        let now = now_rfc3339();
        tx.execute(
            "INSERT INTO sync_runs
                (id, started_at, completed_at, completed_successfully,
                 fetched_issue_count, upserted_issue_count,
                 fetched_comment_count, upserted_comment_count,
                 skipped_pull_request_count, snapshot_kind, content_write_epoch,
                 source_snapshot_epoch)
             VALUES (?1, ?2, ?2, 1, 0, 0, 0, 0, 0, 'purge_successor', ?3, ?4)",
            params![sync_run_id, now, current_epoch, source_snapshot_epoch],
        )?;
        tx.commit()?;
        Ok(Some(sync_run_id))
    }

    /// Persists a content-free retrieval identity for local artifact rebuilds
    /// at the current source epoch. Unlike a remote sync, this never advances
    /// GitHub freshness. Each explicit rebuild gets a new identity so capture
    /// cannot accidentally select a newer remote-sync row at the same source
    /// epoch. A pending purge successor keeps its dedicated identity.
    pub fn record_local_rebuild_snapshot(&mut self) -> Result<SourceSnapshotIdentity, QghError> {
        let expected_epoch = self.content_write_epoch;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_content_write_allowed(&tx, expected_epoch)?;
        if read_successor_repair_required(&tx)? {
            return Err(QghError::validation(
                "publication.successor_snapshot_required",
                "A pending purge successor must use its dedicated source snapshot.",
            ));
        }
        let content_write_epoch = read_content_write_epoch(&tx)?;
        let source_snapshot_epoch = read_source_snapshot_epoch(&tx)?;
        let sync_run_id = format!("sync-local-rebuild-{}", now_run_id_suffix());
        let now = now_rfc3339();
        tx.execute(
            "INSERT INTO sync_runs
                (id, started_at, completed_at, completed_successfully,
                 fetched_issue_count, upserted_issue_count,
                 fetched_comment_count, upserted_comment_count,
                 skipped_pull_request_count, snapshot_kind, content_write_epoch,
                 source_snapshot_epoch)
             VALUES (?1, ?2, ?2, 1, 0, 0, 0, 0, 0, 'local_rebuild', ?3, ?4)",
            params![sync_run_id, now, content_write_epoch, source_snapshot_epoch],
        )?;
        tx.commit()?;
        Ok(SourceSnapshotIdentity {
            sync_run_id,
            epoch: source_snapshot_epoch,
        })
    }

    fn purge_is_noop(&self, target: &PurgeTarget) -> Result<bool, QghError> {
        let (kind, value) = target.kind_and_value();
        let pending = self.conn.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM purge_requests
                 WHERE target_kind = ?1 AND target_value = ?2 AND purge_pending = 1
             )",
            params![kind, value],
            |row| row.get::<_, bool>(0),
        )?;
        if pending {
            return Ok(false);
        }
        let completed = self.conn.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM purge_requests
                 WHERE target_kind = ?1 AND target_value = ?2
                   AND purge_pending = 0 AND completion_ready = 1
                   AND current_stage = 'finalize'
             )",
            params![kind, value],
            |row| row.get::<_, bool>(0),
        )?;
        if !completed {
            return Ok(false);
        }
        if let PurgeTarget::Repository { repo } = target {
            return self.repository_purge_is_noop(repo);
        }

        let mapped_source_ids = self
            .conn
            .prepare(
                "SELECT source_id FROM purge_target_sources
                 WHERE target_kind = ?1 AND target_value = ?2",
            )?
            .query_map(params![kind, value], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        if !mapped_source_ids.is_empty() {
            return Ok(false);
        }
        let mut source_ids = BTreeSet::new();
        match target {
            PurgeTarget::Source { source_id } => {
                if self
                    .conn
                    .query_row(
                        "SELECT 1 FROM source_entities WHERE source_id = ?1",
                        params![source_id],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some()
                {
                    source_ids.insert(source_id.clone());
                }
            }
            PurgeTarget::Issue { repo, issue_number } => {
                for table in ["issue_metadata", "comment_metadata"] {
                    let ids = self
                        .conn
                        .prepare(&format!(
                            "SELECT source_id FROM {table}
                             WHERE lower(repo) = lower(?1) AND issue_number = ?2"
                        ))?
                        .query_map(params![repo, issue_number], |row| row.get::<_, String>(0))?
                        .collect::<Result<Vec<_>, _>>()?;
                    source_ids.extend(ids);
                }
                let cursor = format!("comments:{repo}#{issue_number}");
                if self
                    .conn
                    .query_row(
                        "SELECT 1 FROM sync_cursors WHERE lower(endpoint) = lower(?1)",
                        params![cursor],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some()
                {
                    return Ok(false);
                }
            }
            PurgeTarget::Repository { .. } => unreachable!("handled above"),
        }
        for source_id in source_ids {
            if self.source_has_sensitive_purge_state(&source_id)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn source_has_sensitive_purge_state(&self, source_id: &str) -> Result<bool, QghError> {
        let lifecycle_state = self
            .conn
            .query_row(
                "SELECT lifecycle_state FROM source_entities WHERE source_id = ?1",
                params![source_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if lifecycle_state
            .as_deref()
            .is_some_and(|state| state != "tombstoned")
        {
            return Ok(true);
        }
        for table in [
            "issue_metadata",
            "comment_metadata",
            "source_versions",
            "source_aliases",
            "index_tasks",
        ] {
            if self
                .conn
                .query_row(
                    &format!("SELECT 1 FROM {table} WHERE source_id = ?1 LIMIT 1"),
                    params![source_id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some()
            {
                return Ok(true);
            }
        }
        if table_exists(&self.conn, "chunks")?
            && self
                .conn
                .query_row(
                    "SELECT 1 FROM chunks WHERE source_id = ?1 LIMIT 1",
                    params![source_id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some()
        {
            return Ok(true);
        }
        Ok(false)
    }

    fn repository_purge_is_noop(&self, repo: &str) -> Result<bool, QghError> {
        let pending_or_owned_state = self.conn.query_row(
            "SELECT
                 EXISTS(
                     SELECT 1 FROM purge_requests
                     WHERE target_kind = 'repository'
                       AND lower(target_value) = lower(?1)
                       AND purge_pending = 1
                 )
                 OR EXISTS(SELECT 1 FROM repositories WHERE lower(repo) = lower(?1))
                 OR EXISTS(
                     SELECT 1 FROM source_entities
                     WHERE lower(repo) = lower(?1) AND lifecycle_state != 'tombstoned'
                 )
                 OR EXISTS(SELECT 1 FROM issue_metadata WHERE lower(repo) = lower(?1))
                 OR EXISTS(SELECT 1 FROM comment_metadata WHERE lower(repo) = lower(?1))
                 OR EXISTS(
                     SELECT 1 FROM source_versions sv
                     JOIN source_entities se ON se.source_id = sv.source_id
                     WHERE lower(se.repo) = lower(?1)
                 )
                 OR EXISTS(
                     SELECT 1 FROM repository_sync_state WHERE lower(repo) = lower(?1)
                 )
                 OR EXISTS(
                     SELECT 1 FROM sync_cursors
                     WHERE lower(endpoint) = lower('issues:' || ?1)
                        OR lower(endpoint) = lower('history:' || ?1)
                        OR lower(endpoint) = lower('repo-comments:' || ?1)
                        OR lower(substr(endpoint, 1, length('comments:' || ?1 || '#')))
                           = lower('comments:' || ?1 || '#')
                 )",
            params![repo],
            |row| row.get::<_, bool>(0),
        )?;
        if pending_or_owned_state {
            return Ok(false);
        }
        if table_exists(&self.conn, "chunks")? {
            let has_chunks = self.conn.query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM chunks c
                     JOIN source_entities se ON se.source_id = c.source_id
                     WHERE lower(se.repo) = lower(?1)
                 )",
                params![repo],
                |row| row.get::<_, bool>(0),
            )?;
            if has_chunks {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Retries every durable pending request using its stored target and
    /// content-free trigger. Completed requests are not returned again.
    pub fn retry_pending_purges(&mut self) -> Result<Vec<PurgeOutcome>, QghError> {
        let pending = self.pending_purges()?;
        let mut outcomes = Vec::with_capacity(pending.len());
        let mut failed = false;
        for request in pending {
            match self.finish_pending_purge(request.target, request.trigger) {
                Ok(outcome) => outcomes.push(outcome),
                Err(_) => failed = true,
            }
        }
        if failed {
            return Err(QghError::new(
                "purge.retry_failed",
                "One or more pending purges did not complete; all targets were attempted.",
                6,
            ));
        }
        Ok(outcomes)
    }

    fn finish_pending_purge(
        &mut self,
        target: PurgeTarget,
        trigger: PurgeTrigger,
    ) -> Result<PurgeOutcome, QghError> {
        self.set_purge_stage(&target, PurgeFailureStage::SecureDelete)?;
        if self.should_fail_purge_stage(PurgeFailureStage::SecureDelete) {
            self.record_purge_failure(&target, PurgeFailureStage::SecureDelete)?;
            return Err(purge_error());
        }
        let secure_delete: i64 = match self
            .conn
            .pragma_query_value(None, "secure_delete", |row| row.get(0))
        {
            Ok(value) => value,
            Err(_) => {
                self.record_purge_failure(&target, PurgeFailureStage::SecureDelete)?;
                return Err(purge_error());
            }
        };
        if secure_delete != 1 {
            self.record_purge_failure(&target, PurgeFailureStage::SecureDelete)?;
            return Err(purge_error());
        }

        self.set_purge_stage(&target, PurgeFailureStage::Tantivy)?;
        if self.should_fail_purge_stage(PurgeFailureStage::Tantivy) {
            self.record_purge_failure(&target, PurgeFailureStage::Tantivy)?;
            return Err(purge_error());
        }
        let (discarded_tantivy_generations, _tantivy_cleanup_required) =
            match self.purge_tantivy_generations(&target) {
                Ok(result) => result,
                Err(_) => {
                    self.record_purge_failure(&target, PurgeFailureStage::Tantivy)?;
                    return Err(purge_error());
                }
            };

        self.set_purge_stage(&target, PurgeFailureStage::Storage)?;
        if self.should_fail_purge_stage(PurgeFailureStage::Storage) {
            self.record_purge_failure(&target, PurgeFailureStage::Storage)?;
            return Err(purge_error());
        }
        let (purged_issues, purged_comments) = match self.purge_target_entity_counts(&target) {
            Ok(counts) => counts,
            Err(_) => {
                self.record_purge_failure(&target, PurgeFailureStage::Storage)?;
                return Err(purge_error());
            }
        };
        let (purged_sources, discarded_embedding_generations) =
            match self.purge_sensitive_storage(&target, trigger) {
                Ok(counts) => counts,
                Err(_) => {
                    self.record_purge_failure(&target, PurgeFailureStage::Storage)?;
                    return Err(purge_error());
                }
            };
        self.set_purge_stage(&target, PurgeFailureStage::WalCheckpoint)?;
        if self.should_fail_purge_stage(PurgeFailureStage::WalCheckpoint)
            || self.checkpoint_and_truncate_wal().is_err()
        {
            self.record_purge_failure(&target, PurgeFailureStage::WalCheckpoint)?;
            return Err(purge_error());
        }
        if self.mark_purge_completion_ready(&target).is_err() {
            self.record_purge_failure(&target, PurgeFailureStage::Finalize)?;
            return Err(purge_error());
        }
        if self.should_fail_purge_stage(PurgeFailureStage::Finalize) {
            self.record_purge_failure(&target, PurgeFailureStage::Finalize)?;
            return Err(purge_error());
        }
        self.content_write_epoch = match self.clear_purge_pending(&target) {
            Ok(epoch) => epoch,
            Err(_) => {
                self.record_purge_failure(&target, PurgeFailureStage::Finalize)?;
                return Err(purge_error());
            }
        };
        Ok(PurgeOutcome {
            target,
            purged_sources,
            purged_issues,
            purged_comments,
            discarded_embedding_generations,
            discarded_tantivy_generations,
            sensitive_wal_truncated: true,
        })
    }

    /// Returns only stable target identity, trigger, and coarse failure stage;
    /// source content and underlying storage errors never cross this interface.
    pub fn pending_purges(&self) -> Result<Vec<PendingPurgeView>, QghError> {
        let mut stmt = self.conn.prepare(
            "SELECT target_kind, target_value, trigger, current_stage, failure_stage
             FROM purge_requests
             WHERE purge_pending = 1
             ORDER BY target_kind, target_value",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })?;
        rows.map(|row| {
            let (kind, value, trigger, current_stage, failure_stage) = row?;
            Ok(PendingPurgeView {
                target: PurgeTarget::from_stored(&kind, value)?,
                trigger: PurgeTrigger::from_stored(&trigger)?,
                current_stage: PurgeFailureStage::from_stored(&current_stage)?,
                failure_stage: failure_stage
                    .as_deref()
                    .map(PurgeFailureStage::from_stored)
                    .transpose()?,
            })
        })
        .collect()
    }

    #[cfg(test)]
    fn fail_next_purge_at(&mut self, stage: PurgeFailureStage) {
        self.purge_failure_stage = Some(stage);
    }

    #[cfg(test)]
    fn fail_next_purge_queue_after_first(&mut self) {
        self.purge_queue_failure_after_first = true;
    }

    #[cfg(test)]
    fn swap_generation_after_purge_validation(&mut self, generation: i64) {
        self.purge_swap_generation_after_validation = Some(generation);
    }

    #[cfg(test)]
    fn swap_quarantine_after_purge_open(&mut self, generation: i64) {
        self.purge_swap_quarantine_after_open = Some(generation);
    }

    #[cfg(test)]
    fn inject_generation_swap_after_validation(
        &mut self,
        generation: i64,
        generation_path: &Path,
    ) -> Result<(), QghError> {
        if self.purge_swap_generation_after_validation != Some(generation) {
            return Ok(());
        }
        self.purge_swap_generation_after_validation = None;
        let displaced_path = self
            .index_root
            .join(format!(".qgh-test-displaced-generation-{generation}"));
        fs::rename(generation_path, &displaced_path).map_err(|_| purge_error())?;
        fs::create_dir(generation_path).map_err(|_| purge_error())?;
        fs::write(generation_path.join("foreign-sentinel"), b"preserve")
            .map_err(|_| purge_error())?;
        Ok(())
    }

    #[cfg(test)]
    fn inject_quarantine_swap_after_open(
        &mut self,
        generation: i64,
        quarantine_path: &Path,
    ) -> Result<(), QghError> {
        if self.purge_swap_quarantine_after_open != Some(generation) {
            return Ok(());
        }
        self.purge_swap_quarantine_after_open = None;
        let displaced_path = self
            .index_root
            .join(format!(".qgh-test-displaced-quarantine-{generation}"));
        fs::rename(quarantine_path, &displaced_path).map_err(|_| purge_error())?;
        fs::create_dir(quarantine_path).map_err(|_| purge_error())?;
        fs::write(quarantine_path.join("foreign-sentinel"), b"preserve")
            .map_err(|_| purge_error())?;
        Ok(())
    }

    fn should_fail_purge_stage(&mut self, stage: PurgeFailureStage) -> bool {
        #[cfg(test)]
        {
            if self.purge_failure_stage == Some(stage) {
                self.purge_failure_stage = None;
                return true;
            }
        }
        #[cfg(not(test))]
        let _ = stage;
        false
    }

    fn enforce_pending_purge_guards(&mut self) -> Result<(), QghError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT OR IGNORE INTO purge_target_sources
                (target_kind, target_value, source_id)
             SELECT pr.target_kind, pr.target_value, se.source_id
             FROM purge_requests pr
             JOIN source_entities se ON se.source_id = pr.target_value
             WHERE pr.purge_pending = 1 AND pr.target_kind = 'source'",
            [],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO purge_target_sources
                (target_kind, target_value, source_id)
             SELECT pr.target_kind, pr.target_value, se.source_id
             FROM purge_requests pr
             JOIN source_entities se ON se.repo = pr.target_value
             WHERE pr.purge_pending = 1 AND pr.target_kind = 'repository'",
            [],
        )?;
        for table in ["issue_metadata", "comment_metadata"] {
            tx.execute(
                &format!(
                    "INSERT OR IGNORE INTO purge_target_sources
                        (target_kind, target_value, source_id)
                     SELECT pr.target_kind, pr.target_value, metadata.source_id
                     FROM purge_requests pr
                     JOIN {table} metadata
                       ON pr.target_value = metadata.repo || '#' || metadata.issue_number
                     WHERE pr.purge_pending = 1 AND pr.target_kind = 'issue'"
                ),
                [],
            )?;
        }
        tx.execute(
            "UPDATE source_entities
             SET lifecycle_state = 'purge_pending'
             WHERE EXISTS (
                 SELECT 1
                 FROM purge_target_sources pts
                 JOIN purge_requests pr
                   ON pr.target_kind = pts.target_kind
                  AND pr.target_value = pts.target_value
                 WHERE pr.purge_pending = 1
                   AND pts.source_id = source_entities.source_id
             )",
            [],
        )?;
        tx.execute(
            "UPDATE source_versions
             SET lifecycle_state = 'purge_pending'
             WHERE source_id IN (
                 SELECT source_id FROM source_entities
                 WHERE lifecycle_state = 'purge_pending'
             )",
            [],
        )?;
        let pending_count: i64 = tx.query_row(
            "SELECT count(*) FROM purge_requests WHERE purge_pending = 1",
            [],
            |row| row.get(0),
        )?;
        if pending_count > 0 {
            invalidate_publications_for_pending_purge(&tx)?;
        }
        tx.commit()?;
        Ok(())
    }

    fn record_purge_failure(
        &self,
        target: &PurgeTarget,
        stage: PurgeFailureStage,
    ) -> Result<(), QghError> {
        let (kind, value) = target.kind_and_value();
        self.conn.execute(
            "UPDATE purge_requests
             SET purge_pending = 1, current_stage = ?3, failure_stage = ?3,
                 updated_at = ?4
             WHERE target_kind = ?1 AND target_value = ?2",
            params![kind, value, stage.as_str(), now_rfc3339()],
        )?;
        Ok(())
    }

    fn set_purge_stage(
        &self,
        target: &PurgeTarget,
        stage: PurgeFailureStage,
    ) -> Result<(), QghError> {
        let (kind, value) = target.kind_and_value();
        self.conn.execute(
            "UPDATE purge_requests
             SET current_stage = ?3, failure_stage = NULL, updated_at = ?4
             WHERE target_kind = ?1 AND target_value = ?2 AND purge_pending = 1",
            params![kind, value, stage.as_str(), now_rfc3339()],
        )?;
        Ok(())
    }

    fn mark_purge_completion_ready(&self, target: &PurgeTarget) -> Result<(), QghError> {
        let (kind, value) = target.kind_and_value();
        let changed = self.conn.execute(
            "UPDATE purge_requests
             SET current_stage = 'finalize', failure_stage = NULL,
                 completion_ready = 1, updated_at = ?3
             WHERE target_kind = ?1 AND target_value = ?2 AND purge_pending = 1",
            params![kind, value, now_rfc3339()],
        )?;
        if changed != 1 {
            return Err(purge_error());
        }
        Ok(())
    }

    fn content_write_transaction(&mut self) -> Result<Transaction<'_>, QghError> {
        let expected_epoch = self.content_write_epoch;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_content_write_allowed(&tx, expected_epoch)?;
        Ok(tx)
    }

    fn content_write_transaction_with_pending_purge(
        &mut self,
    ) -> Result<Transaction<'_>, QghError> {
        let expected_epoch = self.content_write_epoch;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if read_content_write_epoch(&tx)? != expected_epoch {
            return Err(write_fence_error());
        }
        Ok(tx)
    }

    fn clear_purge_pending(&mut self, target: &PurgeTarget) -> Result<i64, QghError> {
        let (kind, value) = target.kind_and_value();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE profile_meta
             SET value = CAST(value AS INTEGER) + 1
             WHERE key = 'content_write_epoch'",
            [],
        )?;
        let content_write_epoch = read_content_write_epoch(&tx)?;
        let completed_at = now_rfc3339();
        let changed = tx.execute(
            "UPDATE purge_requests
             SET purge_pending = 0, failure_stage = NULL, updated_at = ?3
             WHERE target_kind = ?1 AND target_value = ?2
               AND purge_pending = 1 AND completion_ready = 1
               AND current_stage = 'finalize'",
            params![kind, value, completed_at],
        )?;
        if changed != 1 {
            return Err(purge_error());
        }
        if let PurgeTarget::Repository { repo } = target {
            tx.execute(
                "DELETE FROM repositories WHERE lower(repo) = lower(?1)",
                params![repo],
            )?;
        }
        tx.execute(
            "INSERT INTO purge_requests
                (target_kind, target_value, trigger, purge_pending,
                 current_stage, failure_stage, completion_ready, created_at, updated_at)
             SELECT 'source', pts.source_id, parent.trigger, 0,
                    'finalize', NULL, 1, ?3, ?3
             FROM purge_target_sources pts
             JOIN purge_requests parent
               ON parent.target_kind = pts.target_kind
              AND parent.target_value = pts.target_value
             WHERE pts.target_kind = ?1 AND pts.target_value = ?2
               AND parent.purge_pending = 0
               AND parent.current_stage = 'finalize'
               AND parent.completion_ready = 1
             ON CONFLICT(target_kind, target_value) DO NOTHING",
            params![kind, value, completed_at],
        )?;
        tx.execute(
            "DELETE FROM purge_target_sources
             WHERE target_kind = ?1 AND target_value = ?2",
            params![kind, value],
        )?;
        tx.commit()?;
        Ok(content_write_epoch)
    }

    fn checkpoint_and_truncate_wal(&self) -> Result<(), QghError> {
        let (busy, log_frames, checkpointed_frames): (i64, i64, i64) =
            self.conn
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })?;
        if busy != 0 || log_frames != checkpointed_frames {
            return Err(purge_error());
        }
        Ok(())
    }

    fn purge_sensitive_storage(
        &mut self,
        target: &PurgeTarget,
        trigger: PurgeTrigger,
    ) -> Result<(usize, usize), QghError> {
        let source_ids = self.purge_target_source_ids(target)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let has_chunks = table_exists(&tx, "chunks")?;
        let mut affected_generations = BTreeSet::new();
        if table_exists(&tx, "embedding_generations")? {
            let building_generations = tx
                .prepare("SELECT id FROM embedding_generations WHERE state = 'building'")?
                .query_map([], |row| row.get::<_, i64>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            affected_generations.extend(building_generations);
        }
        if has_chunks && table_exists(&tx, "embedding_generation_chunks")? {
            for source_id in &source_ids {
                let generation_ids = tx
                    .prepare(
                        "SELECT DISTINCT egc.generation_id
                         FROM embedding_generation_chunks egc
                         LEFT JOIN chunks c ON c.id = egc.chunk_id
                         LEFT JOIN source_versions sv ON sv.id = egc.source_version_id
                         WHERE c.source_id = ?1 OR sv.source_id = ?1",
                    )?
                    .query_map(params![source_id], |row| row.get::<_, i64>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                affected_generations.extend(generation_ids);
                if table_exists(&tx, "embedding_generation_vector_rows")? {
                    let mapped_generation_ids = tx
                        .prepare(
                            "SELECT DISTINCT m.generation_id
                             FROM embedding_generation_vector_rows m
                             JOIN chunks c ON c.id = m.chunk_id
                             WHERE c.source_id = ?1",
                        )?
                        .query_map(params![source_id], |row| row.get::<_, i64>(0))?
                        .collect::<Result<Vec<_>, _>>()?;
                    affected_generations.extend(mapped_generation_ids);
                }
            }
        }
        let owned_generation_vector_mappings =
            if table_exists(&tx, "embedding_generation_vector_rows")? {
                validate_purge_generation_vector_mapping_ownership(&tx, &affected_generations)?
            } else {
                BTreeMap::new()
            };
        let has_legacy_json_embeddings = has_chunks
            && table_exists(&tx, "chunk_embeddings")?
            && source_ids.iter().try_fold(false, |found, source_id| {
                if found {
                    return Ok::<bool, QghError>(true);
                }
                Ok(tx
                    .query_row(
                        "SELECT 1
                         FROM chunk_embeddings ce
                         JOIN chunks c ON c.id = ce.chunk_id
                         WHERE c.source_id = ?1 LIMIT 1",
                        params![source_id],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some())
            })?;
        #[cfg(feature = "vector-search")]
        let has_legacy_vector_embeddings = has_chunks
            && vector_table_dimension(&tx)?.is_some()
            && source_ids.iter().try_fold(false, |found, source_id| {
                if found {
                    return Ok::<bool, QghError>(true);
                }
                Ok(tx
                    .query_row(
                        &format!(
                            "SELECT 1
                             FROM {CHUNK_EMBEDDING_VECTORS_TABLE} v
                             JOIN chunks c ON c.id = v.rowid
                             WHERE c.source_id = ?1 LIMIT 1"
                        ),
                        params![source_id],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some())
            })?;
        #[cfg(not(feature = "vector-search"))]
        let has_legacy_vector_embeddings =
            !source_ids.is_empty() && vec0_shadow_payload_exists(&tx)?;
        let has_legacy_embeddings = has_legacy_json_embeddings || has_legacy_vector_embeddings;

        if has_legacy_embeddings {
            #[cfg(feature = "vector-search")]
            if vector_table_dimension(&tx)?.is_some() {
                tx.execute(&format!("DELETE FROM {CHUNK_EMBEDDING_VECTORS_TABLE}"), [])?;
            }
            #[cfg(not(feature = "vector-search"))]
            clear_vec0_shadow_payload_for_base(&tx, CHUNK_EMBEDDING_VECTORS_TABLE)?;
            if table_exists(&tx, "chunk_embeddings")? {
                tx.execute("DELETE FROM chunk_embeddings", [])?;
            }
            if table_exists(&tx, "embedding_fingerprints")? {
                tx.execute("UPDATE embedding_fingerprints SET active = 0", [])?;
            }
        } else if has_chunks && table_exists(&tx, "chunk_embeddings")? {
            for source_id in &source_ids {
                let chunk_ids = tx
                    .prepare("SELECT id FROM chunks WHERE source_id = ?1")?
                    .query_map(params![source_id], |row| row.get::<_, i64>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                for chunk_id in chunk_ids {
                    tx.execute(
                        "DELETE FROM chunk_embeddings WHERE chunk_id = ?1",
                        params![chunk_id],
                    )?;
                    #[cfg(feature = "vector-search")]
                    if vector_table_dimension(&tx)?.is_some() {
                        tx.execute(
                            &format!(
                                "DELETE FROM {CHUNK_EMBEDDING_VECTORS_TABLE} WHERE rowid = ?1"
                            ),
                            params![chunk_id],
                        )?;
                    }
                }
            }
        }

        for generation_id in &affected_generations {
            if table_exists(&tx, "embedding_generation_vector_rows")? {
                let mappings = owned_generation_vector_mappings
                    .get(generation_id)
                    .ok_or_else(purge_error)?;
                for (table, _dimension, rowid) in mappings {
                    #[cfg(feature = "vector-search")]
                    {
                        tx.execute(
                            &format!("DELETE FROM {table} WHERE rowid = ?1"),
                            params![rowid],
                        )?;
                    }
                    #[cfg(not(feature = "vector-search"))]
                    delete_vec0_shadow_row(&tx, table, *_dimension, *rowid)?;
                }
                tx.execute(
                    "DELETE FROM embedding_generation_vector_rows WHERE generation_id = ?1",
                    params![generation_id],
                )?;
            }
            tx.execute(
                "DELETE FROM embedding_generation_chunks WHERE generation_id = ?1",
                params![generation_id],
            )?;
            if table_exists(&tx, "retrieval_publications")? {
                if table_exists(&tx, "retrieval_publication_pointer")? {
                    tx.execute(
                        "DELETE FROM retrieval_publication_pointer
                         WHERE publication_id IN (
                             SELECT publication_id FROM retrieval_publications
                             WHERE embedding_generation_id = ?1
                         )",
                        params![generation_id],
                    )?;
                }
                tx.execute(
                    "DELETE FROM retrieval_publications WHERE embedding_generation_id = ?1",
                    params![generation_id],
                )?;
            }
            tx.execute(
                "DELETE FROM embedding_generations WHERE id = ?1",
                params![generation_id],
            )?;
        }

        let observed_at = now_rfc3339();
        for source_id in &source_ids {
            let tombstone_reason = purge_tombstone_reason(&tx, source_id, trigger)?;
            if has_chunks {
                tx.execute(
                    "DELETE FROM chunks WHERE source_id = ?1",
                    params![source_id],
                )?;
            }
            tx.execute(
                "DELETE FROM issue_metadata WHERE source_id = ?1",
                params![source_id],
            )?;
            tx.execute(
                "DELETE FROM comment_metadata WHERE source_id = ?1",
                params![source_id],
            )?;
            tx.execute(
                "DELETE FROM source_aliases WHERE source_id = ?1",
                params![source_id],
            )?;
            tx.execute(
                "DELETE FROM source_versions WHERE source_id = ?1",
                params![source_id],
            )?;
            tx.execute(
                "DELETE FROM index_tasks WHERE source_id = ?1",
                params![source_id],
            )?;
            tx.execute(
                "UPDATE source_entities
                 SET lifecycle_state = 'tombstoned',
                     created_at = ?2,
                     updated_at = ?2,
                     last_seen_at = ?2
                 WHERE source_id = ?1",
                params![source_id, observed_at],
            )?;
            tx.execute(
                "INSERT INTO tombstones (source_id, reason, observed_at)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(source_id) DO UPDATE SET
                    reason = excluded.reason,
                    observed_at = excluded.observed_at",
                params![source_id, tombstone_reason, observed_at],
            )?;
        }
        match target {
            PurgeTarget::Issue { repo, issue_number } => {
                tx.execute(
                    "DELETE FROM sync_cursors
                     WHERE lower(endpoint) = lower('comments:' || ?1 || '#' || ?2)",
                    params![repo, issue_number],
                )?;
            }
            PurgeTarget::Repository { repo } => {
                tx.execute(
                    "DELETE FROM sync_cursors
                     WHERE lower(endpoint) = lower('issues:' || ?1)
                        OR lower(endpoint) = lower('history:' || ?1)
                        OR lower(endpoint) = lower('repo-comments:' || ?1)
                        OR lower(substr(endpoint, 1, length('comments:' || ?1 || '#')))
                           = lower('comments:' || ?1 || '#')",
                    params![repo],
                )?;
                tx.execute(
                    "DELETE FROM repository_sync_state WHERE lower(repo) = lower(?1)",
                    params![repo],
                )?;
            }
            PurgeTarget::Source { .. } => {}
        }
        tx.commit()?;
        Ok((source_ids.len(), affected_generations.len()))
    }

    fn purge_tantivy_generations(
        &mut self,
        target: &PurgeTarget,
    ) -> Result<(usize, bool), QghError> {
        self.quiesce_index_build_leases()?;
        if !self.purge_target_has_sensitive_content(target)? {
            return Ok((0, false));
        }
        let generations = {
            let mut stmt = self.conn.prepare(
                "SELECT generation.generation, generation.path,
                        generation.source_count, generation.source_inventory_hash,
                        ownership.owner_token
                 FROM index_generations generation
                 JOIN index_build_leases ownership
                   ON ownership.generation = generation.generation
                  AND ownership.owner_pid = 0
                 ORDER BY generation.generation",
            )?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            let registered_count: i64 =
                self.conn
                    .query_row("SELECT count(*) FROM index_generations", [], |row| {
                        row.get(0)
                    })?;
            if i64::try_from(rows.len()).ok() != Some(registered_count) {
                return Err(purge_error());
            }
            rows
        };
        self.remove_registered_tantivy_generations(&generations)?;

        let tx = self.conn.transaction()?;
        if table_exists(&tx, "embedding_generations")? {
            tx.execute(
                "UPDATE embedding_generations SET state = 'ready'
                 WHERE state = 'active'",
                [],
            )?;
        }
        tx.execute("DELETE FROM retrieval_publication_pointer", [])?;
        tx.execute("DELETE FROM retrieval_publications", [])?;
        tx.execute("DELETE FROM index_build_leases WHERE owner_pid = 0", [])?;
        tx.execute("DELETE FROM index_generations", [])?;
        tx.commit()?;
        Ok((generations.len(), true))
    }

    fn quiesce_index_build_leases(&mut self) -> Result<(), QghError> {
        let leases = {
            let mut stmt = self.conn.prepare(
                "SELECT generation, owner_pid, owner_token
                 FROM index_build_leases
                 WHERE owner_pid != 0
                 ORDER BY generation",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        if leases
            .iter()
            .any(|(_, owner_pid, _)| index_builder_process_is_live(*owner_pid))
        {
            return Err(purge_error());
        }
        for (generation, _, owner_token) in leases {
            self.cleanup_index_generation_for_token(generation, &owner_token)?;
        }
        Ok(())
    }

    fn remove_registered_tantivy_generations(
        &mut self,
        generations: &[(i64, String, i64, Option<String>, String)],
    ) -> Result<(), QghError> {
        self.validate_index_root_confinement()?;
        if !self.index_root.exists() {
            return Ok(());
        }
        if fs::symlink_metadata(&self.index_root)
            .map_err(|_| purge_error())?
            .file_type()
            .is_symlink()
        {
            return Err(purge_error());
        }
        for (generation, stored_path, source_count, source_inventory_hash, owner_token) in
            generations
        {
            let expected_path = self.index_root.join(format!("generation-{generation}"));
            if Path::new(stored_path) != expected_path {
                return Err(purge_error());
            }
            let Some(source_count) = usize::try_from(*source_count).ok() else {
                return Err(purge_error());
            };
            let Some(source_inventory_hash) = source_inventory_hash.as_deref() else {
                return Err(purge_error());
            };
            let quarantine_path =
                tantivy_purge_quarantine_path(&self.index_root, *generation, owner_token);
            match (expected_path.exists(), quarantine_path.exists()) {
                (false, false) => continue,
                (true, true) => return Err(purge_error()),
                (false, true) => {
                    validate_quarantined_tantivy_generation(
                        &quarantine_path,
                        *generation,
                        owner_token,
                        source_count,
                        source_inventory_hash,
                    )?;
                    let identity = filesystem_identity(&quarantine_path)?;
                    self.remove_quarantined_tantivy_generation(
                        *generation,
                        owner_token,
                        &quarantine_path,
                        identity,
                    )?;
                    continue;
                }
                (true, false) => {}
            }
            validate_managed_tantivy_generation_path(
                &self.profile_dir,
                &self.index_root,
                *generation,
                &expected_path,
            )
            .map_err(|_| purge_error())?;
            validate_tantivy_generation_artifact(
                &expected_path,
                source_count,
                source_inventory_hash,
            )
            .map_err(|_| purge_error())?;
            crate::index::validate_owned_generation_directory(
                &expected_path,
                *generation,
                owner_token,
            )
            .map_err(|_| purge_error())?;
            let expected_identity = filesystem_identity(&expected_path)?;
            #[cfg(test)]
            self.inject_generation_swap_after_validation(*generation, &expected_path)?;
            crate::index::rename_without_replacement(&expected_path, &quarantine_path)
                .map_err(|_| purge_error())?;
            sync_directory(&self.index_root)?;
            let post_rename = (|| -> Result<(), QghError> {
                if filesystem_identity(&quarantine_path)? != expected_identity {
                    return Err(purge_error());
                }
                validate_quarantined_tantivy_generation(
                    &quarantine_path,
                    *generation,
                    owner_token,
                    source_count,
                    source_inventory_hash,
                )
            })();
            if post_rename.is_err() {
                let _ = crate::index::rename_without_replacement(&quarantine_path, &expected_path);
                let _ = sync_directory(&self.index_root);
                return Err(purge_error());
            }
            self.remove_quarantined_tantivy_generation(
                *generation,
                owner_token,
                &quarantine_path,
                expected_identity,
            )?;
        }
        Ok(())
    }

    fn remove_quarantined_tantivy_generation(
        &mut self,
        generation: i64,
        owner_token: &str,
        quarantine_path: &Path,
        expected_identity: FilesystemIdentity,
    ) -> Result<(), QghError> {
        let directory = open_anchored_directory(quarantine_path)?;
        if filesystem_identity_from_file(&directory)? != expected_identity {
            return Err(purge_error());
        }
        #[cfg(test)]
        self.inject_quarantine_swap_after_open(generation, quarantine_path)?;
        if filesystem_identity(quarantine_path)? != expected_identity {
            return Err(purge_error());
        }
        crate::index::validate_owned_generation_directory(quarantine_path, generation, owner_token)
            .map_err(|_| purge_error())?;
        if filesystem_identity(quarantine_path)? != expected_identity {
            return Err(purge_error());
        }
        remove_anchored_directory_contents(&directory)?;
        if filesystem_identity(quarantine_path)? != expected_identity {
            return Err(purge_error());
        }
        fs::remove_dir(quarantine_path).map_err(|_| purge_error())?;
        sync_directory(&self.index_root)
    }

    fn validate_index_root_confinement(&self) -> Result<(), QghError> {
        if self.index_root != self.profile_dir.join("tantivy") {
            return Err(purge_error());
        }
        let profile_root = fs::canonicalize(&self.profile_dir).map_err(|_| purge_error())?;
        if self.index_root.exists() {
            let metadata = fs::symlink_metadata(&self.index_root).map_err(|_| purge_error())?;
            if metadata.file_type().is_symlink() {
                return Err(purge_error());
            }
            let index_root = fs::canonicalize(&self.index_root).map_err(|_| purge_error())?;
            if !index_root.starts_with(&profile_root) {
                return Err(purge_error());
            }
            return Ok(());
        }
        let parent = self.index_root.parent().ok_or_else(purge_error)?;
        let parent = fs::canonicalize(parent).map_err(|_| purge_error())?;
        if !parent.starts_with(&profile_root) {
            return Err(purge_error());
        }
        Ok(())
    }

    fn cleanup_owned_index_generation(&mut self, generation: i64) -> Result<(), QghError> {
        let Some(owner_token) = self.index_build_tokens.get(&generation).cloned() else {
            return Ok(());
        };
        self.cleanup_index_generation_for_token(generation, &owner_token)?;
        self.index_build_tokens.remove(&generation);
        Ok(())
    }

    fn cleanup_index_generation_for_token(
        &mut self,
        generation: i64,
        owner_token: &str,
    ) -> Result<bool, QghError> {
        self.validate_index_root_confinement()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let lease_matches = tx
            .query_row(
                "SELECT 1 FROM index_build_leases
                 WHERE generation = ?1 AND owner_token = ?2",
                params![generation, owner_token],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !lease_matches {
            tx.commit()?;
            return Ok(false);
        }
        let expected_path = self.index_root.join(format!("generation-{generation}"));
        let generation_state = tx
            .query_row(
                "SELECT path, source_count, source_inventory_hash
                 FROM index_generations WHERE generation = ?1",
                params![generation],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((stored_path, source_count, source_inventory_hash)) = generation_state else {
            return Err(purge_error());
        };
        if Path::new(&stored_path) != expected_path {
            return Err(purge_error());
        }
        let shadow_path = self.index_root.join(format!("shadow-{generation}"));
        if shadow_path.exists()
            && crate::index::validate_owned_build_directory(&shadow_path, generation, owner_token)
                .is_err()
        {
            crate::index::validate_owned_generation_directory(
                &shadow_path,
                generation,
                owner_token,
            )
            .map_err(|_| purge_error())?;
        }
        if expected_path.exists() {
            let source_count = usize::try_from(source_count).map_err(|_| purge_error())?;
            let source_inventory_hash = source_inventory_hash.ok_or_else(purge_error)?;
            validate_managed_tantivy_generation_path(
                &self.profile_dir,
                &self.index_root,
                generation,
                &expected_path,
            )
            .map_err(|_| purge_error())?;
            crate::index::validate_owned_generation_directory(
                &expected_path,
                generation,
                owner_token,
            )
            .map_err(|_| purge_error())?;
            validate_tantivy_generation_artifact(
                &expected_path,
                source_count,
                &source_inventory_hash,
            )
            .map_err(|_| purge_error())?;
        }
        if shadow_path.exists() {
            fs::remove_dir_all(&shadow_path).map_err(|_| purge_error())?;
        }
        if expected_path.exists() {
            fs::remove_dir_all(&expected_path).map_err(|_| purge_error())?;
        }
        tx.execute(
            "DELETE FROM retrieval_publication_pointer
             WHERE publication_id IN (
                 SELECT publication_id FROM retrieval_publications
                 WHERE tantivy_generation = ?1
             )",
            params![generation],
        )?;
        tx.execute(
            "DELETE FROM retrieval_publications WHERE tantivy_generation = ?1",
            params![generation],
        )?;
        tx.execute(
            "DELETE FROM index_generations WHERE generation = ?1",
            params![generation],
        )?;
        tx.execute(
            "DELETE FROM index_build_leases
             WHERE generation = ?1 AND owner_token = ?2",
            params![generation, owner_token],
        )?;
        tx.commit()?;
        Ok(true)
    }

    fn purge_target_has_sensitive_content(&self, target: &PurgeTarget) -> Result<bool, QghError> {
        // The durable target mapping is captured before destructive work. Its
        // presence is sufficient proof that a managed Tantivy generation may
        // still contain the target even if SQLite cleanup was partially done.
        Ok(!self.purge_target_source_ids(target)?.is_empty())
    }

    fn purge_target_source_ids(&self, target: &PurgeTarget) -> Result<Vec<String>, QghError> {
        let (kind, value) = target.kind_and_value();
        let mut stmt = self.conn.prepare(
            "SELECT source_id FROM purge_target_sources
             WHERE target_kind = ?1 AND target_value = ?2
             ORDER BY source_id",
        )?;
        let source_ids = stmt
            .query_map(params![kind, value], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(QghError::from)?;
        Ok(source_ids)
    }

    fn purge_target_entity_counts(&self, target: &PurgeTarget) -> Result<(usize, usize), QghError> {
        let (kind, value) = target.kind_and_value();
        let mut stmt = self.conn.prepare(
            "SELECT se.entity_type, count(DISTINCT pts.source_id)
             FROM purge_target_sources pts
             JOIN source_entities se ON se.source_id = pts.source_id
             WHERE pts.target_kind = ?1 AND pts.target_value = ?2
             GROUP BY se.entity_type",
        )?;
        let rows = stmt.query_map(params![kind, value], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        let mut issues = 0usize;
        let mut comments = 0usize;
        for row in rows {
            let (entity_type, count) = row?;
            match entity_type.as_str() {
                "issue" => issues += count as usize,
                "issue_comment" => comments += count as usize,
                _ => return Err(purge_error()),
            }
        }
        Ok((issues, comments))
    }

    #[cfg(not(feature = "vector-search"))]
    pub fn enable_vector(&mut self) -> Result<(), QghError> {
        Err(QghError::validation(
            "embedding.vector_capability_unavailable",
            "This qgh binary was built without the vector-search feature.",
        ))
    }

    #[cfg(not(feature = "vector-search"))]
    pub fn enable_vector_for_read(&self) -> Result<(), QghError> {
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
        self.upsert_sources_for_run_with_pending_guard(
            sync_run_id,
            issues,
            comments,
            skipped_pull_requests,
            cursor_updates,
            false,
        )
    }

    /// Continues a fetched page after confirmed purge evidence is durable.
    /// Matching source/repository writes remain `purge_pending`; callers must
    /// refresh the queued target mapping before finishing the purge batch.
    pub fn upsert_sources_for_run_under_pending_purge(
        &mut self,
        sync_run_id: &str,
        issues: &[IssueRecord],
        comments: &[CommentRecord],
        skipped_pull_requests: usize,
        cursor_updates: &[CursorUpdate],
    ) -> Result<SyncSummary, QghError> {
        self.upsert_sources_for_run_with_pending_guard(
            sync_run_id,
            issues,
            comments,
            skipped_pull_requests,
            cursor_updates,
            true,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn upsert_sources_for_run_with_pending_guard(
        &mut self,
        sync_run_id: &str,
        issues: &[IssueRecord],
        comments: &[CommentRecord],
        skipped_pull_requests: usize,
        cursor_updates: &[CursorUpdate],
        allow_pending_purge: bool,
    ) -> Result<SyncSummary, QghError> {
        let now = now_rfc3339();
        let tx = if allow_pending_purge {
            self.content_write_transaction_with_pending_purge()?
        } else {
            self.content_write_transaction()?
        };
        let mut source_snapshot_changed = false;
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
            let repo = canonical_repository_identity(&tx, &issue.repo)?;
            source_snapshot_changed |= !authoritative_issue_matches(&tx, issue, &repo)?;
            let previous_title = tx
                .query_row(
                    "SELECT title FROM issue_metadata WHERE source_id = ?1",
                    params![issue.source_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
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
                    repo,
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
                params![repo, issue.host],
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
                    repo,
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
            if previous_title.as_deref() != Some(issue.title.as_str()) {
                let comment_source_ids = {
                    let mut stmt = tx.prepare(
                        "SELECT source_id FROM comment_metadata
                         WHERE parent_issue_source_id = ?1
                           AND parent_issue_title != ?2",
                    )?;
                    let rows = stmt.query_map(params![issue.source_id, issue.title], |row| {
                        row.get::<_, String>(0)
                    })?;
                    rows.collect::<Result<Vec<_>, _>>()?
                };
                if !comment_source_ids.is_empty() {
                    tx.execute(
                        "UPDATE comment_metadata
                         SET parent_issue_title = ?2
                         WHERE parent_issue_source_id = ?1
                           AND parent_issue_title != ?2",
                        params![issue.source_id, issue.title],
                    )?;
                    for source_id in comment_source_ids {
                        tx.execute(
                            "INSERT INTO index_tasks
                                (source_id, task_type, created_at, completed_at)
                             VALUES (?1, 'upsert', ?2, NULL)",
                            params![source_id, now],
                        )?;
                    }
                }
            }
            apply_pending_purge_guard(&tx, &issue.source_id, &repo, issue.number)?;
        }

        for comment in comments {
            let repo = canonical_repository_identity(&tx, &comment.repo)?;
            source_snapshot_changed |= !authoritative_comment_matches(&tx, comment, &repo)?;
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
                    repo,
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
                    repo,
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
            apply_pending_purge_guard(&tx, &comment.source_id, &repo, comment.parent_issue_number)?;
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

        if source_snapshot_changed {
            bump_source_snapshot_epoch(&tx)?;
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

    pub fn mark_sync_run_completed(
        &mut self,
        sync_run_id: &str,
    ) -> Result<SourceSnapshotIdentity, QghError> {
        let tx = self.content_write_transaction()?;
        let source_snapshot_epoch = read_source_snapshot_epoch(&tx)?;
        let changed = tx.execute(
            "UPDATE sync_runs
             SET completed_at = ?1, completed_successfully = 1,
                 source_snapshot_epoch = ?3
             WHERE id = ?2",
            params![now_rfc3339(), sync_run_id, source_snapshot_epoch],
        )?;
        if changed == 0 {
            return Err(QghError::storage(format!(
                "Cannot mark missing sync run `{sync_run_id}` completed."
            )));
        }
        tx.commit()?;
        Ok(SourceSnapshotIdentity {
            sync_run_id: sync_run_id.to_string(),
            epoch: source_snapshot_epoch,
        })
    }

    pub fn upsert_target_issue_refresh(
        &mut self,
        issue: &IssueRecord,
        comments: &[CommentRecord],
    ) -> Result<TargetedSyncSummary, QghError> {
        let existing_comments =
            self.active_comment_versions_for_issue(&issue.repo, issue.number)?;
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

        let summary = self.upsert_sources(std::slice::from_ref(issue), comments, 0, &[])?;

        Ok(TargetedSyncSummary {
            sync_run_id: summary.sync_run_id,
            fetched_issues: 1,
            upserted_issues: 1,
            fetched_comments: comments.len(),
            upserted_comments: comments.len(),
            added_comments,
            updated_comments,
            deleted_comments: 0,
            tombstoned_issues: 0,
            tombstoned_comments: 0,
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
        let tx = self.content_write_transaction()?;
        let version_exists = tx
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

        if stored_chunks_match(&tx, source_version_id, chunks)? {
            tx.commit()?;
            return self.chunks_for_source_version(source_version_id);
        }

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
        bump_source_snapshot_epoch(&tx)?;
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

    pub fn source_version_chunks_match_fingerprint(
        &self,
        source_version_id: i64,
        expected_fingerprint: &str,
    ) -> Result<bool, QghError> {
        if !embedding_schema_exists(&self.conn)? {
            return Ok(false);
        }
        let (count, mismatched): (i64, i64) = self.conn.query_row(
            "SELECT count(*),
                    coalesce(sum(
                        CASE
                            WHEN chunker_fingerprint IS ?2 THEN 0
                            ELSE 1
                        END
                    ), 0)
             FROM chunks WHERE source_version_id = ?1",
            params![source_version_id, expected_fingerprint],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(count > 0 && mismatched == 0)
    }

    pub fn cleanup_inactive_embedding_artifacts(&mut self) -> Result<usize, QghError> {
        let expected_epoch = self.content_write_epoch;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_content_write_allowed(&tx, expected_epoch)?;
        if !embedding_schema_exists(&tx)? {
            tx.commit()?;
            return Ok(0);
        }
        let protected_generation_clause = if table_exists(&tx, "embedding_generation_chunks")? {
            " AND c.id NOT IN (
                SELECT egc.chunk_id
                FROM embedding_generation_chunks egc
                JOIN retrieval_publications rp
                  ON rp.embedding_generation_id = egc.generation_id
                WHERE rp.publication_id = (
                       SELECT publication_id FROM retrieval_publication_pointer WHERE id = 1
                   )
                   OR rp.publication_id = (
                       SELECT publication_id FROM retrieval_publications
                       WHERE embedding_generation_id IS NOT NULL
                         AND publication_id != coalesce(
                             (SELECT publication_id FROM retrieval_publication_pointer WHERE id = 1),
                             -1
                         )
                       ORDER BY created_at DESC, publication_id DESC LIMIT 1
                   )
            )"
        } else {
            ""
        };
        let stale_chunk_filter = format!(
            "SELECT c.id
             FROM chunks c
             LEFT JOIN source_entities se ON se.source_id = c.source_id
             LEFT JOIN issue_metadata im ON im.source_id = c.source_id
             LEFT JOIN comment_metadata cm ON cm.source_id = c.source_id
             WHERE se.lifecycle_state IS NULL
                OR se.lifecycle_state != 'active'
                OR (c.source_version_id != coalesce(im.latest_version_id, cm.latest_version_id, -1)
                    {protected_generation_clause})"
        );

        let vector_table_exists = vector_table_dimension(&tx)?.is_some();
        if vector_table_exists {
            tx.execute(
                &format!(
                    "DELETE FROM {CHUNK_EMBEDDING_VECTORS_TABLE}
                     WHERE rowid NOT IN (SELECT id FROM chunks)
                        OR rowid IN ({stale_chunk_filter})"
                ),
                [],
            )?;
        }
        tx.execute(
            &format!(
                "DELETE FROM chunk_embeddings
                 WHERE chunk_id IN ({stale_chunk_filter})"
            ),
            [],
        )?;
        let deleted_chunks = tx.execute(
            &format!("DELETE FROM chunks WHERE id IN ({stale_chunk_filter})"),
            [],
        )?;
        tx.commit()?;
        Ok(deleted_chunks)
    }

    pub fn cleanup_tombstoned_embedding_artifacts(&mut self) -> Result<usize, QghError> {
        if !embedding_schema_exists(&self.conn)? {
            return Ok(0);
        }
        const TOMBSTONED_CHUNKS: &str = "SELECT c.id FROM chunks c
             LEFT JOIN source_entities se ON se.source_id = c.source_id
             WHERE se.lifecycle_state IS NULL OR se.lifecycle_state != 'active'";
        let vector_table_exists = vector_table_dimension(&self.conn)?.is_some();
        let tx = self.conn.transaction()?;
        if vector_table_exists {
            tx.execute(
                &format!(
                    "DELETE FROM {CHUNK_EMBEDDING_VECTORS_TABLE}
                     WHERE rowid IN ({TOMBSTONED_CHUNKS})"
                ),
                [],
            )?;
        }
        tx.execute(
            &format!("DELETE FROM chunk_embeddings WHERE chunk_id IN ({TOMBSTONED_CHUNKS})"),
            [],
        )?;
        let deleted = tx.execute(
            &format!("DELETE FROM chunks WHERE id IN ({TOMBSTONED_CHUNKS})"),
            [],
        )?;
        tx.commit()?;
        Ok(deleted)
    }

    pub fn active_contextual_embedding_chunks(
        &self,
    ) -> Result<Vec<ContextualEmbeddingChunk>, QghError> {
        if !embedding_schema_exists(&self.conn)? {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(
            "SELECT c.id, c.source_id, c.source_version_id, c.body, c.chunk_index,
                    c.token_start, c.token_end, c.byte_start, c.byte_end,
                    c.chunker_version, c.chunker_fingerprint, c.heading_path_json,
                    se.entity_type, se.host, se.repo,
                    coalesce(im.issue_number, cm.issue_number),
                    im.title, cm.parent_issue_title
             FROM chunks c
             JOIN source_entities se ON se.source_id = c.source_id
             LEFT JOIN issue_metadata im ON im.source_id = c.source_id
             LEFT JOIN comment_metadata cm ON cm.source_id = c.source_id
             WHERE se.lifecycle_state = 'active'
               AND c.source_version_id = coalesce(im.latest_version_id, cm.latest_version_id)
             ORDER BY c.id",
        )?;
        let mut rows = stmt.query([])?;
        let mut chunks = Vec::new();
        while let Some(row) = rows.next()? {
            chunks.push(contextual_embedding_chunk_from_row(row)?);
        }
        Ok(chunks)
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
        let tx = self.content_write_transaction()?;
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
        let tx = self.content_write_transaction()?;
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
        self.ensure_vector_storage_for_fingerprint_inner(fingerprint, || {})
    }

    fn ensure_vector_storage_for_fingerprint_inner<F>(
        &mut self,
        fingerprint: &EmbeddingFingerprint,
        after_candidate_read: F,
    ) -> Result<usize, QghError>
    where
        F: FnOnce(),
    {
        let fingerprint_hash = fingerprint.hash();
        let tx = self.content_write_transaction()?;
        Self::ensure_vector_storage_inner(&tx, fingerprint.dimension)?;
        let rows = {
            let mut stmt = tx.prepare(
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
        after_candidate_read();

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

    pub fn generation_vector_search(
        &self,
        generation_id: i64,
        query_vector: &[f32],
        filters: &VectorSearchFilters,
        limit: usize,
    ) -> Result<Vec<VectorSearchHit>, QghError> {
        if limit == 0 || query_vector.is_empty() || filters.source_types.is_empty() {
            return Ok(Vec::new());
        }
        if !query_vector.iter().all(|value| value.is_finite())
            || query_vector.iter().all(|value| *value == 0.0)
        {
            return Err(QghError::validation(
                "embedding.invalid_query_vector",
                "Query embedding must contain finite, non-zero values.",
            ));
        }
        let (state, output_dimension, total_chunks, completed_chunks): (String, i64, i64, i64) =
            self.conn.query_row(
                "SELECT state, output_dimension, total_chunks, completed_chunks
                 FROM embedding_generations WHERE id = ?1",
                params![generation_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?;
        let generation_rows: i64 = self.conn.query_row(
            "SELECT count(*) FROM embedding_generation_chunks WHERE generation_id = ?1",
            params![generation_id],
            |row| row.get(0),
        )?;
        let vector_rows: i64 = self.conn.query_row(
            "SELECT count(*) FROM embedding_generation_vector_rows WHERE generation_id = ?1",
            params![generation_id],
            |row| row.get(0),
        )?;
        let output_dimension = usize::try_from(output_dimension)
            .ok()
            .filter(|dimension| *dimension > 0)
            .ok_or_else(embedding_generation_corrupt_error)?;
        if !matches!(state.as_str(), "active" | "ready")
            || output_dimension != query_vector.len()
            || completed_chunks != total_chunks
            || generation_rows != total_chunks
            || vector_rows != total_chunks
        {
            return Err(QghError::validation(
                "embedding.generation_corrupt",
                "The published embedding generation is incomplete or inconsistent.",
            ));
        }
        validate_embedding_generation_vector_artifacts(
            &self.conn,
            generation_id,
            output_dimension,
            total_chunks,
        )?;
        let vector_table = generation_vector_table_name(output_dimension);
        let candidate_limit = limit.saturating_mul(4).max(limit).max(1);
        let mut prefilter_sql = String::from(
            "SELECT c2.id FROM chunks c2
             JOIN source_entities se ON se.source_id = c2.source_id
             LEFT JOIN issue_metadata im ON im.source_id = c2.source_id
             LEFT JOIN comment_metadata cm ON cm.source_id = c2.source_id
             WHERE se.lifecycle_state = 'active'",
        );
        let mut params = vec![
            Value::Blob(encode_embedding_blob(query_vector)),
            Value::Integer(candidate_limit as i64),
            Value::Integer(generation_id),
        ];
        push_vector_filter_sql(filters, &mut prefilter_sql, &mut params);
        params.push(Value::Integer(generation_id));
        params.push(Value::Integer(limit as i64));
        let sql = format!(
            "WITH vector_candidates AS (
                SELECT m.chunk_id, v.distance
                FROM {vector_table} v
                JOIN embedding_generation_vector_rows m ON m.vector_rowid = v.rowid
                WHERE v.embedding MATCH ? AND v.k = ?
                  AND v.rowid IN (
                      SELECT filtered.vector_rowid
                      FROM embedding_generation_vector_rows filtered
                      WHERE filtered.generation_id = ?
                        AND filtered.chunk_id IN ({prefilter_sql})
                  )
                  AND m.generation_id = ?
                ORDER BY v.distance
             )
             SELECT gc.vector_blob, gc.vector_dimension, gc.vector_checksum,
                    c.id, c.source_id,
                    c.source_version_id, c.body, c.chunk_index, c.token_start,
                    c.token_end, c.byte_start, c.byte_end, c.chunker_version,
                    c.chunker_fingerprint, c.heading_path_json, sv.body_hash,
                    vector_candidates.distance
             FROM vector_candidates
             JOIN embedding_generation_chunks gc
               ON gc.generation_id = {generation_id}
              AND gc.chunk_id = vector_candidates.chunk_id
             JOIN chunks c ON c.id = gc.chunk_id
             JOIN source_versions sv ON sv.id = c.source_version_id
             ORDER BY vector_candidates.distance, c.id
             LIMIT ?",
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut candidates = Vec::new();
        for row in stmt.query_map(params_from_iter(params.iter()), |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, i64>(1)? as usize,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, i64>(9)?,
                row.get::<_, i64>(10)?,
                row.get::<_, i64>(11)?,
                row.get::<_, String>(12)?,
                row.get::<_, String>(13)?,
                row.get::<_, String>(14)?,
                row.get::<_, String>(15)?,
                row.get::<_, f32>(16)?,
            ))
        })? {
            let (
                blob,
                dimension,
                checksum,
                chunk_id,
                source_id,
                source_version_id,
                body,
                chunk_index,
                token_start,
                token_end,
                byte_start,
                byte_end,
                chunker_version,
                chunker_fingerprint,
                heading_path_json,
                source_version_hash,
                distance,
            ) = row?;
            if dimension != query_vector.len() || embedding_blob_checksum(&blob) != checksum {
                return Err(QghError::validation(
                    "embedding.generation_corrupt",
                    "A published embedding row failed checksum or dimension validation.",
                ));
            }
            let vector = decode_embedding_blob(&blob, dimension)?;
            if !vector.iter().all(|value| value.is_finite())
                || vector.iter().all(|value| *value == 0.0)
            {
                return Err(QghError::validation(
                    "embedding.generation_corrupt",
                    "A published embedding row contains invalid values.",
                ));
            }
            candidates.push(VectorSearchHit {
                source_id: source_id.clone(),
                chunk: StoredChunk {
                    chunk_id,
                    source_id,
                    source_version_id,
                    body,
                    chunk_index: chunk_index as usize,
                    token_start: token_start as usize,
                    token_end: token_end as usize,
                    byte_start: byte_start as usize,
                    byte_end: byte_end as usize,
                    chunker_version,
                    chunker_fingerprint,
                    heading_path: serde_json::from_str(&heading_path_json).unwrap_or_default(),
                },
                source_version_hash,
                vector_distance: distance,
            });
        }
        let mut seen = BTreeSet::new();
        Ok(candidates
            .into_iter()
            .filter(|hit| seen.insert(hit.source_id.clone()))
            .take(limit)
            .collect())
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
        let tx = self.content_write_transaction()?;
        let previous_reason = tx
            .query_row(
                "SELECT reason FROM tombstones WHERE source_id = ?1",
                params![source_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
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
        // Tombstoned content must fail closed and must not remain in qgh-managed
        // derived storage. Keep only the minimal identity/tombstone metadata.
        let chunk_ids = if table_exists(&tx, "chunks")? {
            tx.prepare("SELECT id FROM chunks WHERE source_id = ?1")?
                .query_map(params![source_id], |row| row.get::<_, i64>(0))?
                .collect::<Result<Vec<_>, _>>()?
        } else {
            Vec::new()
        };
        if !chunk_ids.is_empty() {
            let placeholders = std::iter::repeat_n("?", chunk_ids.len())
                .collect::<Vec<_>>()
                .join(",");
            let values = chunk_ids
                .iter()
                .map(|id| Value::Integer(*id))
                .collect::<Vec<_>>();
            if embedding_schema_exists(&tx)? {
                let _ = tx.execute(
                    &format!("DELETE FROM chunk_embeddings WHERE chunk_id IN ({placeholders})"),
                    rusqlite::params_from_iter(values.iter()),
                );
                if vector_table_dimension(&tx)?.is_some() {
                    let _ = tx.execute(
                        &format!("DELETE FROM {CHUNK_EMBEDDING_VECTORS_TABLE} WHERE rowid IN ({placeholders})"),
                        rusqlite::params_from_iter(values.iter()),
                    );
                }
            }
        }
        if table_exists(&tx, "chunks")? {
            tx.execute(
                "DELETE FROM chunks WHERE source_id = ?1",
                params![source_id],
            )?;
        }
        if changed > 0 {
            tx.execute(
                "INSERT INTO index_tasks (source_id, task_type, created_at, completed_at)
                 VALUES (?1, 'delete', ?2, NULL)",
                params![source_id, observed_at],
            )?;
        }
        if changed > 0 || previous_reason.as_deref() != Some(reason) {
            bump_source_snapshot_epoch(&tx)?;
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
                 WHERE lower(im.repo) = lower(?1)
                   AND im.issue_number = ?2 AND se.lifecycle_state = 'active'",
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

    /// Returns repositories that still own profile state. A repository remains
    /// visible while its purge is pending and disappears atomically with purge
    /// completion, so callers can safely drive retry/backfill decisions.
    pub fn known_repositories(&self) -> Result<Vec<String>, QghError> {
        let mut repositories = BTreeSet::new();
        for query in [
            "SELECT repo FROM repositories",
            "SELECT DISTINCT repo FROM source_entities WHERE lifecycle_state != 'tombstoned'",
            "SELECT repo FROM repository_sync_state",
        ] {
            let mut stmt = self.conn.prepare(query)?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            repositories.extend(
                rows.into_iter()
                    .filter(|repo| valid_repository_identity(repo)),
            );
        }
        let mut cursor_stmt = self.conn.prepare("SELECT endpoint FROM sync_cursors")?;
        let cursor_endpoints = cursor_stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        for endpoint in cursor_endpoints {
            if let Some(repo) = repository_from_cursor_endpoint(&endpoint) {
                repositories.insert(repo.to_string());
            }
        }
        Ok(repositories.into_iter().collect())
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
             WHERE lower(cm.repo) = lower(?1)
               AND cm.issue_number = ?2 AND se.lifecycle_state = 'active'
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
        self.validate_index_root_confinement()?;
        let expected_path = self.index_root.join(format!("generation-{generation}"));
        if Path::new(path) != expected_path {
            return Err(QghError::validation(
                "purge.index_path_invalid",
                "Tantivy generation path does not match the reserved profile path.",
            ));
        }
        let now = now_rfc3339();
        let expected_epoch = self.content_write_epoch;
        let owner_token = self.index_build_tokens.get(&generation).cloned();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Err(error) = ensure_content_write_allowed(&tx, expected_epoch) {
            drop(tx);
            let _ = self.cleanup_owned_index_generation(generation);
            return Err(error);
        }
        let generation_state = tx
            .query_row(
                "SELECT write_epoch, source_inventory_hash
                 FROM index_generations WHERE generation = ?1",
                params![generation],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()?;
        let Some((generation_epoch, source_inventory_hash)) = generation_state else {
            drop(tx);
            let _ = self.cleanup_owned_index_generation(generation);
            return Err(write_fence_error());
        };
        if generation_epoch != expected_epoch {
            drop(tx);
            let _ = self.cleanup_owned_index_generation(generation);
            return Err(write_fence_error());
        }
        let Some(source_inventory_hash) = source_inventory_hash else {
            drop(tx);
            let _ = self.cleanup_owned_index_generation(generation);
            return Err(source_inventory_mismatch_error());
        };
        if let Err(error) = validate_managed_tantivy_generation_path(
            &self.profile_dir,
            &self.index_root,
            generation,
            Path::new(path),
        ) {
            drop(tx);
            let _ = self.cleanup_owned_index_generation(generation);
            return Err(error);
        }
        let Some(owner_token_value) = owner_token.as_deref() else {
            drop(tx);
            return Err(write_fence_error());
        };
        if let Err(error) = crate::index::validate_owned_generation_directory(
            &expected_path,
            generation,
            owner_token_value,
        ) {
            drop(tx);
            let _ = self.cleanup_owned_index_generation(generation);
            return Err(error);
        }
        if let Err(error) = validate_tantivy_generation_artifact(
            &expected_path,
            source_count,
            &source_inventory_hash,
        ) {
            drop(tx);
            let _ = self.cleanup_owned_index_generation(generation);
            return Err(error);
        }
        let lease_matches = if let Some(owner_token) = owner_token.as_deref() {
            tx.query_row(
                "SELECT 1 FROM index_build_leases
                 WHERE generation = ?1 AND write_epoch = ?2 AND owner_token = ?3",
                params![generation, expected_epoch, owner_token],
                |_| Ok(()),
            )
            .optional()?
            .is_some()
        } else {
            false
        };
        if !lease_matches {
            drop(tx);
            let _ = self.cleanup_owned_index_generation(generation);
            return Err(write_fence_error());
        }
        tx.execute("UPDATE index_generations SET active = 0", [])?;
        tx.execute(
            "UPDATE index_generations
             SET path = ?2, source_count = ?3, created_at = ?4, active = 1
             WHERE generation = ?1 AND write_epoch = ?5",
            params![generation, path, source_count as i64, now, expected_epoch],
        )?;
        tx.execute(
            "UPDATE index_tasks SET completed_at = ?1 WHERE completed_at IS NULL",
            params![now],
        )?;
        let ownership_retained = tx.execute(
            "UPDATE index_build_leases
             SET owner_pid = 0
             WHERE generation = ?1 AND owner_token = ?2",
            params![generation, owner_token_value],
        )?;
        if ownership_retained != 1 {
            return Err(write_fence_error());
        }
        tx.commit()?;
        self.index_build_tokens.remove(&generation);
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

    pub fn index_path_for_generation(&self, generation: i64) -> Result<Option<String>, QghError> {
        self.conn
            .query_row(
                "SELECT path FROM index_generations WHERE generation = ?1",
                params![generation],
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
                   AND snapshot_kind = 'remote_sync'
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

    pub(crate) fn reserve_index_generation(
        &mut self,
        index_root: &Path,
        source_count: usize,
    ) -> Result<(i64, PathBuf), QghError> {
        let sources = self.active_index_sources()?;
        if sources.len() != source_count {
            return Err(source_inventory_mismatch_error());
        }
        let inventory_hash = crate::index::source_inventory_digest(&sources);
        let source_snapshot_epoch = read_source_snapshot_epoch(&self.conn)?;
        let identity = self
            .conn
            .query_row(
                "SELECT id, source_snapshot_epoch FROM sync_runs
                 WHERE completed_successfully = 1
                   AND source_snapshot_epoch = ?1
                 ORDER BY rowid DESC LIMIT 1",
                params![source_snapshot_epoch],
                |row| {
                    Ok(SourceSnapshotIdentity {
                        sync_run_id: row.get(0)?,
                        epoch: row.get(1)?,
                    })
                },
            )
            .optional()?;
        let expected_publication_id = self
            .active_retrieval_publication()?
            .map(|publication| publication.publication_id);
        self.reserve_index_generation_bound(
            index_root,
            source_count,
            identity.as_ref(),
            expected_publication_id,
            Some(&inventory_hash),
        )
    }

    pub(crate) fn reserve_index_generation_for_snapshot(
        &mut self,
        index_root: &Path,
        snapshot: &RetrievalBuildSnapshot,
    ) -> Result<(i64, PathBuf), QghError> {
        self.validate_retrieval_build_snapshot(snapshot)?;
        self.reserve_index_generation_bound(
            index_root,
            snapshot.sources.len(),
            Some(&snapshot.identity),
            snapshot.expected_publication_id,
            Some(&snapshot.source_inventory_hash),
        )
    }

    fn reserve_index_generation_bound(
        &mut self,
        index_root: &Path,
        source_count: usize,
        identity: Option<&SourceSnapshotIdentity>,
        expected_publication_id: Option<i64>,
        source_inventory_hash: Option<&str>,
    ) -> Result<(i64, PathBuf), QghError> {
        if index_root != self.index_root {
            return Err(QghError::validation(
                "purge.index_root_invalid",
                "Tantivy generation root does not match the profile store.",
            ));
        }
        self.validate_index_root_confinement()?;
        let now = now_rfc3339();
        let write_epoch = self.content_write_epoch;
        let owner_pid = i64::from(std::process::id());
        let tx = self.content_write_transaction()?;
        let source_snapshot_epoch = read_source_snapshot_epoch(&tx)?;
        let current_publication_id = tx
            .query_row(
                "SELECT publication_id FROM retrieval_publication_pointer WHERE id = 1",
                [],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        if current_publication_id != expected_publication_id {
            return Err(QghError::validation(
                "publication.cas_conflict",
                "Retrieval publication changed before index generation reservation.",
            ));
        }
        let identity_matches = identity.is_some_and(|identity| {
            identity.epoch == source_snapshot_epoch
                && tx
                    .query_row(
                        "SELECT EXISTS(
                             SELECT 1 FROM sync_runs
                             WHERE id = ?1 AND completed_successfully = 1
                               AND source_snapshot_epoch = ?2
                         )",
                        params![identity.sync_run_id, identity.epoch],
                        |row| row.get::<_, bool>(0),
                    )
                    .unwrap_or(false)
        });
        if source_count > 0 && !identity_matches {
            return Err(incomplete_source_snapshot_error());
        }
        if identity.is_some() && !identity_matches {
            return Err(changed_source_snapshot_error());
        }
        let source_snapshot_sync_run_id = identity.map(|identity| identity.sync_run_id.as_str());
        let generation = tx.query_row(
            "SELECT CAST(value AS INTEGER) FROM profile_meta
             WHERE key = 'next_index_generation'",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        tx.execute(
            "UPDATE profile_meta SET value = CAST(?1 + 1 AS TEXT)
             WHERE key = 'next_index_generation'",
            params![generation],
        )?;
        let owner_token = format!(
            "index-build-{owner_pid}-{generation}-{}",
            now_run_id_suffix()
        );
        let generation_path = index_root.join(format!("generation-{generation}"));
        tx.execute(
            "INSERT INTO index_generations
                (generation, path, source_count, created_at, active, write_epoch,
                 source_snapshot_sync_run_id, source_snapshot_epoch,
                 source_inventory_hash, expected_publication_id)
             VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6, ?7, ?8, ?9)",
            params![
                generation,
                generation_path.to_string_lossy(),
                source_count as i64,
                now,
                write_epoch,
                source_snapshot_sync_run_id,
                source_snapshot_epoch,
                source_inventory_hash,
                expected_publication_id,
            ],
        )?;
        tx.execute(
            "INSERT INTO index_build_leases
                (generation, write_epoch, owner_pid, owner_token, created_at,
                 source_snapshot_sync_run_id, source_snapshot_epoch)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                generation,
                write_epoch,
                owner_pid,
                owner_token,
                now,
                source_snapshot_sync_run_id,
                source_snapshot_epoch,
            ],
        )?;
        tx.commit()?;
        if let Err(error) =
            crate::index::prepare_owned_rebuild(index_root, generation, &owner_token)
        {
            let cleanup = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            cleanup.execute(
                "DELETE FROM index_build_leases
                 WHERE generation = ?1 AND owner_token = ?2",
                params![generation, owner_token],
            )?;
            cleanup.execute(
                "DELETE FROM index_generations
                 WHERE generation = ?1 AND active = 0",
                params![generation],
            )?;
            cleanup.commit()?;
            return Err(error);
        }
        self.index_build_tokens.insert(generation, owner_token);
        Ok((generation, generation_path))
    }

    pub(crate) fn rebuild_reserved_index_generation(
        &self,
        generation: i64,
        sources: &[IndexSource],
    ) -> Result<PathBuf, QghError> {
        let owner_token = self
            .index_build_tokens
            .get(&generation)
            .ok_or_else(write_fence_error)?;
        let lease_matches = self.conn.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM index_build_leases
                 WHERE generation = ?1 AND owner_token = ?2
             )",
            params![generation, owner_token],
            |row| row.get::<_, bool>(0),
        )?;
        if !lease_matches {
            return Err(write_fence_error());
        }
        crate::index::rebuild_owned(&self.index_root, generation, owner_token, sources)
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
                   AND snapshot_kind = 'remote_sync'
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

    pub fn active_index_generation(&self) -> Result<Option<i64>, QghError> {
        self.conn
            .query_row(
                "SELECT generation FROM index_generations WHERE active = 1 ORDER BY generation DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(QghError::from)
    }

    pub fn latest_successful_sync_run_id(&self) -> Result<Option<String>, QghError> {
        Ok(self
            .latest_successful_source_snapshot()?
            .map(|snapshot| snapshot.sync_run_id))
    }

    pub fn latest_successful_source_snapshot(
        &self,
    ) -> Result<Option<SourceSnapshotIdentity>, QghError> {
        self.conn
            .query_row(
                "SELECT id, source_snapshot_epoch FROM sync_runs
                 WHERE completed_successfully = 1
                   AND snapshot_kind = 'remote_sync'
                   AND source_snapshot_epoch IS NOT NULL
                 ORDER BY completed_at DESC, id DESC LIMIT 1",
                [],
                |row| {
                    Ok(SourceSnapshotIdentity {
                        sync_run_id: row.get(0)?,
                        epoch: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(QghError::from)
    }

    pub fn capture_retrieval_build_snapshot(
        &self,
    ) -> Result<Option<RetrievalBuildSnapshot>, QghError> {
        self.conn.execute_batch("BEGIN")?;
        let captured = (|| -> Result<Option<RetrievalBuildSnapshot>, QghError> {
            let pending: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM purge_requests WHERE purge_pending = 1)",
                [],
                |row| row.get(0),
            )?;
            if pending {
                return Err(read_fence_error());
            }
            let epoch = read_source_snapshot_epoch(&self.conn)?;
            let identity = self
                .conn
                .query_row(
                    "SELECT id, source_snapshot_epoch FROM sync_runs
                     WHERE completed_successfully = 1
                       AND source_snapshot_epoch = ?1
                     ORDER BY rowid DESC LIMIT 1",
                    params![epoch],
                    |row| {
                        Ok(SourceSnapshotIdentity {
                            sync_run_id: row.get(0)?,
                            epoch: row.get(1)?,
                        })
                    },
                )
                .optional()?;
            let Some(identity) = identity else {
                let source_count: i64 = self.conn.query_row(
                    "SELECT count(*) FROM source_entities WHERE lifecycle_state = 'active'",
                    [],
                    |row| row.get(0),
                )?;
                return if source_count == 0 {
                    Ok(None)
                } else {
                    Err(incomplete_source_snapshot_error())
                };
            };
            let expected_publication_id = self
                .conn
                .query_row(
                    "SELECT publication_id FROM retrieval_publication_pointer WHERE id = 1",
                    [],
                    |row| row.get(0),
                )
                .optional()?;
            let sources = self.active_index_sources()?;
            let embedding_chunks = self.active_contextual_embedding_chunks()?;
            Ok(Some(RetrievalBuildSnapshot {
                identity,
                expected_publication_id,
                source_inventory_hash: crate::index::source_inventory_digest(&sources),
                embedding_inventory_hash: embedding_inventory_hash(&embedding_chunks),
                sources,
                embedding_chunks,
            }))
        })();
        match captured {
            Ok(snapshot) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(snapshot)
            }
            Err(error) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    fn validate_retrieval_build_snapshot(
        &self,
        snapshot: &RetrievalBuildSnapshot,
    ) -> Result<(), QghError> {
        if crate::index::source_inventory_digest(&snapshot.sources)
            != snapshot.source_inventory_hash
        {
            return Err(source_inventory_mismatch_error());
        }
        if embedding_inventory_hash(&snapshot.embedding_chunks) != snapshot.embedding_inventory_hash
        {
            return Err(embedding_inventory_mismatch_error());
        }
        let current_epoch = read_source_snapshot_epoch(&self.conn)?;
        if snapshot.identity.epoch != current_epoch {
            return Err(changed_source_snapshot_error());
        }
        let identity_valid = self.conn.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM sync_runs
                 WHERE id = ?1 AND completed_successfully = 1
                   AND source_snapshot_epoch = ?2
             )",
            params![snapshot.identity.sync_run_id, snapshot.identity.epoch],
            |row| row.get::<_, bool>(0),
        )?;
        if !identity_valid {
            return Err(changed_source_snapshot_error());
        }
        let current_sources = self.active_index_sources()?;
        if current_sources.len() != snapshot.sources.len()
            || crate::index::source_inventory_digest(&current_sources)
                != snapshot.source_inventory_hash
        {
            return Err(source_inventory_mismatch_error());
        }
        let current_chunks = self.active_contextual_embedding_chunks()?;
        if current_chunks.len() != snapshot.embedding_chunks.len()
            || embedding_inventory_hash(&current_chunks) != snapshot.embedding_inventory_hash
        {
            return Err(embedding_inventory_mismatch_error());
        }
        Ok(())
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

    pub(crate) fn begin_embedding_generation(
        &mut self,
        snapshot: &RetrievalBuildSnapshot,
        spec: &EmbeddingGenerationSpec,
    ) -> Result<i64, QghError> {
        if spec.output_dimension == 0 {
            return Err(QghError::validation(
                "embedding.generation_invalid_spec",
                "Embedding generation dimension must be positive.",
            ));
        }
        if spec.model_manifest_hash.is_empty()
            || spec.runtime_fingerprint_hash.is_empty()
            || spec.chunker_fingerprint.is_empty()
        {
            return Err(QghError::validation(
                "embedding.generation_invalid_spec",
                "Embedding generation identity fields must be non-empty.",
            ));
        }
        if spec.context_template_version != crate::context::METADATA_CONTEXT_TEMPLATE_VERSION {
            return Err(QghError::validation(
                "embedding.context_template_unsupported",
                "Embedding generations require the production metadata context template.",
            ));
        }
        self.validate_retrieval_build_snapshot(snapshot)?;
        let total_chunks = i64::try_from(snapshot.embedding_chunks.len()).map_err(|_| {
            QghError::validation(
                "embedding.generation_invalid_spec",
                "Embedding chunk inventory exceeds the supported generation size.",
            )
        })?;
        let write_epoch = self.content_write_epoch;
        let tx = self.content_write_transaction()?;
        let source_snapshot_epoch = read_source_snapshot_epoch(&tx)?;
        let source_snapshot_valid = tx.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM sync_runs
                 WHERE id = ?1 AND completed_successfully = 1
                   AND source_snapshot_epoch = ?2
             )",
            params![snapshot.identity.sync_run_id, source_snapshot_epoch],
            |row| row.get::<_, bool>(0),
        )?;
        if !source_snapshot_valid {
            return Err(QghError::validation(
                "embedding.source_snapshot_incomplete",
                "Embedding generation requires a completed source snapshot at the current epoch.",
            ));
        }
        let source_snapshot_hash = source_snapshot_identity_hash(&snapshot.identity);
        if let Some(id) = tx
            .query_row(
                "SELECT id FROM embedding_generations
                 WHERE state = 'building'
                   AND model_manifest_hash = ?1
                   AND runtime_fingerprint_hash = ?2
                   AND chunker_fingerprint = ?3
                   AND context_template_version = ?4
                   AND output_dimension = ?5
                   AND source_sync_run_id = ?6
                   AND source_snapshot_hash = ?7
                   AND write_epoch = ?8
                   AND source_snapshot_epoch = ?9
                   AND total_chunks = ?10
                   AND embedding_inventory_hash = ?11
                 ORDER BY id DESC LIMIT 1",
                params![
                    spec.model_manifest_hash,
                    spec.runtime_fingerprint_hash,
                    spec.chunker_fingerprint,
                    spec.context_template_version,
                    spec.output_dimension as i64,
                    snapshot.identity.sync_run_id,
                    source_snapshot_hash,
                    write_epoch,
                    source_snapshot_epoch,
                    total_chunks,
                    snapshot.embedding_inventory_hash,
                ],
                |row| row.get(0),
            )
            .optional()?
        {
            tx.commit()?;
            return Ok(id);
        }
        let now = now_rfc3339();
        tx.execute(
            "INSERT INTO embedding_generations
                (state, model_manifest_hash, runtime_fingerprint_hash, chunker_fingerprint,
                 context_template_version, output_dimension, source_sync_run_id,
                 source_snapshot_hash, embedding_inventory_hash, total_chunks,
                 created_at, updated_at, write_epoch, source_snapshot_epoch)
             VALUES ('building', ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10, ?11, ?12)",
            params![
                spec.model_manifest_hash,
                spec.runtime_fingerprint_hash,
                spec.chunker_fingerprint,
                spec.context_template_version,
                spec.output_dimension as i64,
                snapshot.identity.sync_run_id,
                source_snapshot_hash,
                snapshot.embedding_inventory_hash,
                total_chunks,
                now,
                write_epoch,
                source_snapshot_epoch,
            ],
        )?;
        let generation_id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(generation_id)
    }

    pub fn stage_embedding_generation_batch(
        &mut self,
        generation_id: i64,
        chunks: &[EmbeddingGenerationChunk],
    ) -> Result<usize, QghError> {
        let expected_epoch = self.content_write_epoch;
        let tx = self.content_write_transaction()?;
        let (dimension, state, generation_epoch, generation_source_epoch): (
            usize,
            String,
            i64,
            i64,
        ) = tx.query_row(
            "SELECT output_dimension, state, write_epoch, source_snapshot_epoch
             FROM embedding_generations WHERE id = ?1",
            params![generation_id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)? as usize,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                ))
            },
        )?;
        if generation_epoch != expected_epoch {
            return Err(write_fence_error());
        }
        if generation_source_epoch != read_source_snapshot_epoch(&tx)? {
            return Err(changed_source_snapshot_error());
        }
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
        if chunks.iter().any(|chunk| {
            !chunk.vector.iter().all(|value| value.is_finite())
                || chunk.vector.iter().all(|value| *value == 0.0)
        }) {
            return Err(QghError::validation(
                "embedding.generation_invalid_vector",
                "Staged vectors must contain finite, non-zero values.",
            ));
        }
        let vector_table = generation_vector_table_name(dimension);
        tx.execute(
            &format!(
                "CREATE VIRTUAL TABLE IF NOT EXISTS {vector_table}
                     USING vec0(embedding float[{dimension}])"
            ),
            [],
        )?;
        for chunk in chunks {
            let bytes = encode_embedding_blob(&chunk.vector);
            let checksum = embedding_blob_checksum(&bytes);
            if let Some((mapping_id, stored_dimension, stored_table, old_rowid)) = tx
                .query_row(
                    "SELECT id, dimension, vector_table, vector_rowid
                         FROM embedding_generation_vector_rows
                         WHERE generation_id = ?1 AND chunk_id = ?2",
                    params![generation_id, chunk.chunk_id],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, i64>(3)?,
                        ))
                    },
                )
                .optional()?
            {
                let owned_table = validate_generation_vector_mapping_ownership(
                    mapping_id,
                    stored_dimension,
                    &stored_table,
                    old_rowid,
                    dimension,
                )?;
                tx.execute(
                    &format!("DELETE FROM {owned_table} WHERE rowid = ?1"),
                    params![old_rowid],
                )?;
            }
            tx.execute(
                "DELETE FROM embedding_generation_vector_rows
                     WHERE generation_id = ?1 AND chunk_id = ?2",
                params![generation_id, chunk.chunk_id],
            )?;
            tx.execute(
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
                tx.execute(
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
                tx.last_insert_rowid()
            };
            tx.execute(
                &format!("INSERT INTO {vector_table}(rowid, embedding) VALUES (?1, ?2)"),
                params![mapping_id, encode_embedding_blob(&chunk.vector)],
            )?;
            tx.execute(
                "UPDATE embedding_generation_vector_rows
                 SET vector_rowid = ?1 WHERE id = ?1",
                params![mapping_id],
            )?;
        }
        let now = now_rfc3339();
        tx.execute(
            "UPDATE embedding_generations
             SET completed_chunks = (SELECT count(*) FROM embedding_generation_chunks WHERE generation_id = ?1),
                 checkpoint_chunk_id = (SELECT max(chunk_id) FROM embedding_generation_chunks WHERE generation_id = ?1),
                 updated_at = ?2
             WHERE id = ?1",
            params![generation_id, now],
        )?;
        tx.commit()?;
        Ok(chunks.len())
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
        let authoritative_chunks = self.active_contextual_embedding_chunks()?;
        let authoritative_inventory_hash = embedding_inventory_hash(&authoritative_chunks);
        let expected_epoch = self.content_write_epoch;
        let tx = self.content_write_transaction()?;
        let (
            state,
            stored_dimension,
            total_chunks,
            completed_chunks,
            model_manifest_hash,
            runtime_fingerprint_hash,
            chunker_fingerprint,
            context_template_version,
            generation_epoch,
            source_sync_run_id,
            source_snapshot_hash,
            generation_source_epoch,
            generation_inventory_hash,
        ): EmbeddingGenerationValidationRow = tx.query_row(
            "SELECT state, output_dimension, total_chunks, completed_chunks,
                        model_manifest_hash, runtime_fingerprint_hash,
                        chunker_fingerprint, context_template_version,
                        write_epoch, source_sync_run_id, source_snapshot_hash,
                        source_snapshot_epoch, embedding_inventory_hash
                 FROM embedding_generations WHERE id = ?1",
            params![generation_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get::<_, i64>(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                    row.get(11)?,
                    row.get(12)?,
                ))
            },
        )?;
        let dimension = usize::try_from(stored_dimension)
            .ok()
            .filter(|dimension| *dimension > 0)
            .ok_or_else(embedding_generation_corrupt_error)?;
        let building = state == "building";
        if !building && !matches!(state.as_str(), "ready" | "active") {
            return Err(embedding_generation_corrupt_error());
        }
        if model_manifest_hash.is_empty() || runtime_fingerprint_hash.is_empty() {
            return Err(embedding_generation_corrupt_error());
        }
        if generation_epoch != expected_epoch {
            return Err(write_fence_error());
        }
        let current_source_epoch = read_source_snapshot_epoch(&tx)?;
        let expected_source_hash = source_snapshot_identity_hash(&SourceSnapshotIdentity {
            sync_run_id: source_sync_run_id,
            epoch: generation_source_epoch,
        });
        if generation_source_epoch != current_source_epoch
            || source_snapshot_hash != expected_source_hash
        {
            return Err(changed_source_snapshot_error());
        }
        if usize::try_from(total_chunks).ok() != Some(authoritative_chunks.len())
            || generation_inventory_hash.as_deref() != Some(authoritative_inventory_hash.as_str())
        {
            if building {
                mark_embedding_generation_failed(
                    &tx,
                    generation_id,
                    "embedding.generation_inventory_mismatch",
                )?;
                tx.commit()?;
            }
            return Err(embedding_inventory_mismatch_error());
        }
        if completed_chunks != total_chunks {
            if building {
                mark_embedding_generation_failed(
                    &tx,
                    generation_id,
                    "embedding.generation_incomplete",
                )?;
                tx.commit()?;
                return Err(QghError::validation(
                    "embedding.generation_incomplete",
                    "Embedding generation is incomplete and cannot be activated.",
                ));
            }
            return Err(embedding_generation_corrupt_error());
        }
        if !embedding_generation_content_rows_valid(
            &tx,
            generation_id,
            dimension,
            total_chunks,
            &model_manifest_hash,
            &chunker_fingerprint,
            &context_template_version,
        )? {
            if building {
                mark_embedding_generation_failed(
                    &tx,
                    generation_id,
                    "embedding.generation_validation_failed",
                )?;
                tx.commit()?;
                return Err(QghError::validation(
                    "embedding.generation_validation_failed",
                    "Embedding generation validation failed.",
                ));
            }
            return Err(embedding_generation_corrupt_error());
        }
        if let Err(error) = validate_embedding_generation_vector_artifacts(
            &tx,
            generation_id,
            dimension,
            total_chunks,
        ) {
            if building {
                mark_embedding_generation_failed(
                    &tx,
                    generation_id,
                    "embedding.generation_validation_failed",
                )?;
                tx.commit()?;
                return Err(QghError::validation(
                    "embedding.generation_validation_failed",
                    "Embedding generation validation failed.",
                ));
            }
            return Err(error);
        }
        if building {
            tx.execute(
                "UPDATE embedding_generations
                 SET state = 'ready', updated_at = ?2, failure_code = NULL
                 WHERE id = ?1 AND state = 'building'",
                params![generation_id, now_rfc3339()],
            )?;
        }
        tx.commit()?;
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

    pub fn active_embedding_generation_coverage(
        &self,
    ) -> Result<Option<(i64, i64, i64, bool)>, QghError> {
        if !table_exists(&self.conn, "embedding_generations")?
            || !table_exists(&self.conn, "embedding_generation_chunks")?
            || !table_exists(&self.conn, "retrieval_publications")?
            || !table_exists(&self.conn, "retrieval_publication_pointer")?
        {
            return Ok(None);
        }
        self.conn
            .query_row(
                "SELECT eg.id, eg.total_chunks, eg.completed_chunks,
                        (eg.state IN ('active', 'ready')
                         AND eg.total_chunks = eg.completed_chunks
                         AND eg.total_chunks = (
                             SELECT count(*) FROM embedding_generation_chunks
                             WHERE generation_id = eg.id
                         )
                         AND eg.total_chunks = (
                             SELECT count(*) FROM embedding_generation_vector_rows
                             WHERE generation_id = eg.id
                         )
                         AND eg.embedding_inventory_hash IS NOT NULL)
                 FROM retrieval_publication_pointer p
             JOIN retrieval_publications rp ON rp.publication_id = p.publication_id
             JOIN embedding_generations eg ON eg.id = rp.embedding_generation_id
             WHERE p.id = 1",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get::<_, i64>(3)? != 0,
                    ))
                },
            )
            .optional()
            .map_err(QghError::from)
    }

    /// Deeply validates the vector artifacts owned by the embedding generation
    /// pinned by the active retrieval publication. This is intentionally
    /// read-only so diagnostics cannot promote, fail, or repair generations.
    #[cfg(feature = "vector-search")]
    pub fn validate_active_embedding_generation_artifacts(&self) -> Result<bool, QghError> {
        let Some(publication) = self.active_retrieval_publication()? else {
            return Ok(false);
        };
        let Some(generation_id) = publication.embedding_generation_id else {
            return Ok(false);
        };
        self.validate_query_publication_snapshot(Some(&publication))?;
        let (
            stored_dimension,
            total_chunks,
            completed_chunks,
            model_manifest_hash,
            chunker_fingerprint,
            context_template_version,
            generation_epoch,
            generation_inventory_hash,
        ): (i64, i64, i64, String, String, String, i64, Option<String>) = self.conn.query_row(
            "SELECT output_dimension, total_chunks, completed_chunks,
                        model_manifest_hash, chunker_fingerprint,
                        context_template_version, write_epoch,
                        embedding_inventory_hash
                 FROM embedding_generations WHERE id = ?1",
            params![generation_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            },
        )?;
        let dimension = usize::try_from(stored_dimension)
            .ok()
            .filter(|dimension| *dimension > 0)
            .ok_or_else(embedding_generation_corrupt_error)?;
        let authoritative_chunks = self.active_contextual_embedding_chunks()?;
        let authoritative_inventory_hash = embedding_inventory_hash(&authoritative_chunks);
        if generation_epoch != self.content_write_epoch
            || completed_chunks != total_chunks
            || usize::try_from(total_chunks).ok() != Some(authoritative_chunks.len())
            || generation_inventory_hash.as_deref() != Some(authoritative_inventory_hash.as_str())
        {
            return Err(embedding_generation_corrupt_error());
        }
        if !embedding_generation_content_rows_valid(
            &self.conn,
            generation_id,
            dimension,
            total_chunks,
            &model_manifest_hash,
            &chunker_fingerprint,
            &context_template_version,
        )? {
            return Err(embedding_generation_corrupt_error());
        }
        register_sqlite_vec_extension(&self.conn)?;
        validate_embedding_generation_vector_artifacts(
            &self.conn,
            generation_id,
            dimension,
            total_chunks,
        )?;
        Ok(true)
    }

    #[cfg(not(feature = "vector-search"))]
    pub fn validate_active_embedding_generation_artifacts(&self) -> Result<bool, QghError> {
        Ok(false)
    }

    #[cfg(test)]
    pub(crate) fn fail_next_retrieval_publication_activation(&mut self, error: QghError) {
        self.activation_failure = Some(error);
    }

    pub fn activate_retrieval_publication(
        &mut self,
        source_snapshot_sync_run_id: &str,
        tantivy_generation: i64,
        embedding_generation_id: Option<i64>,
        expected_publication_id: Option<i64>,
    ) -> Result<i64, QghError> {
        #[cfg(test)]
        if let Some(error) = self.activation_failure.take() {
            return Err(error);
        }
        self.validate_index_root_confinement()?;
        let expected_epoch = self.content_write_epoch;
        let expected_generation_path = self
            .index_root
            .join(format!("generation-{tantivy_generation}"));
        let authoritative_sources = self.active_index_sources()?;
        let authoritative_source_inventory_hash =
            crate::index::source_inventory_digest(&authoritative_sources);
        let authoritative_embedding_chunks = self.active_contextual_embedding_chunks()?;
        let authoritative_embedding_inventory_hash =
            embedding_inventory_hash(&authoritative_embedding_chunks);
        let owner_token = self.index_build_tokens.get(&tantivy_generation).cloned();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Err(error) = ensure_content_write_allowed(&tx, expected_epoch) {
            drop(tx);
            let _ = self.cleanup_owned_index_generation(tantivy_generation);
            return Err(error);
        }
        let current_source_snapshot_epoch = read_source_snapshot_epoch(&tx)?;
        let embedding_metadata = if let Some(generation_id) = embedding_generation_id {
            let raw_metadata = tx
                .query_row(
                    "SELECT state, model_manifest_hash, runtime_fingerprint_hash,
                            chunker_fingerprint,
                            context_template_version, output_dimension, total_chunks,
                            completed_chunks, write_epoch, source_sync_run_id,
                            source_snapshot_hash, source_snapshot_epoch,
                            embedding_inventory_hash
                     FROM embedding_generations WHERE id = ?1",
                    params![generation_id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, Option<String>>(1)?,
                            row.get::<_, Option<String>>(2)?,
                            row.get::<_, Option<String>>(3)?,
                            row.get::<_, Option<String>>(4)?,
                            row.get::<_, Option<i64>>(5)?,
                            row.get::<_, i64>(6)?,
                            row.get::<_, i64>(7)?,
                            row.get::<_, i64>(8)?,
                            row.get::<_, String>(9)?,
                            row.get::<_, String>(10)?,
                            row.get::<_, Option<i64>>(11)?,
                            row.get::<_, Option<String>>(12)?,
                        ))
                    },
                )
                .optional()?;
            let Some((
                state,
                Some(manifest),
                Some(runtime),
                Some(chunker),
                Some(context),
                Some(stored_dimension),
                total_chunks,
                completed_chunks,
                generation_epoch,
                sync_run_id,
                snapshot_hash,
                source_epoch,
                inventory_hash,
            )) = raw_metadata
            else {
                drop(tx);
                let _ = self.cleanup_owned_index_generation(tantivy_generation);
                return Err(embedding_snapshot_mismatch_error());
            };
            let Some(dimension) = usize::try_from(stored_dimension)
                .ok()
                .filter(|dimension| *dimension > 0)
            else {
                drop(tx);
                let _ = self.cleanup_owned_index_generation(tantivy_generation);
                return Err(embedding_snapshot_mismatch_error());
            };
            Some((
                state,
                manifest,
                runtime,
                chunker,
                context,
                dimension,
                total_chunks,
                completed_chunks,
                generation_epoch,
                sync_run_id,
                snapshot_hash,
                source_epoch,
                inventory_hash,
            ))
        } else {
            None
        };
        if let Some((
            state,
            model_manifest_hash,
            runtime_fingerprint_hash,
            chunker_fingerprint,
            context_template_version,
            dimension,
            total_chunks,
            completed_chunks,
            generation_epoch,
            generation_sync_run_id,
            generation_snapshot_hash,
            generation_source_epoch,
            generation_inventory_hash,
        )) = &embedding_metadata
        {
            if *generation_epoch != expected_epoch {
                drop(tx);
                let _ = self.cleanup_owned_index_generation(tantivy_generation);
                return Err(write_fence_error());
            }
            if state != "ready" || total_chunks != completed_chunks {
                drop(tx);
                let _ = self.cleanup_owned_index_generation(tantivy_generation);
                return Err(QghError::validation(
                    "publication.embedding_not_ready",
                    "Only a complete ready embedding generation can be published.",
                ));
            }
            let embedding_snapshot_matches = generation_source_epoch.is_some_and(|epoch| {
                let expected_hash = source_snapshot_identity_hash(&SourceSnapshotIdentity {
                    sync_run_id: generation_sync_run_id.clone(),
                    epoch,
                });
                epoch == current_source_snapshot_epoch && generation_snapshot_hash == &expected_hash
            });
            if model_manifest_hash.is_empty()
                || runtime_fingerprint_hash.is_empty()
                || chunker_fingerprint.is_empty()
                || *dimension == 0
                || context_template_version != crate::context::METADATA_CONTEXT_TEMPLATE_VERSION
                || generation_sync_run_id != source_snapshot_sync_run_id
                || !embedding_snapshot_matches
                || usize::try_from(*total_chunks).ok() != Some(authoritative_embedding_chunks.len())
                || generation_inventory_hash.as_deref()
                    != Some(authoritative_embedding_inventory_hash.as_str())
            {
                drop(tx);
                let _ = self.cleanup_owned_index_generation(tantivy_generation);
                return Err(QghError::validation(
                    "publication.embedding_snapshot_mismatch",
                    "Embedding and lexical generations do not share one source snapshot.",
                ));
            }
            if let Err(error) = validate_embedding_generation_vector_artifacts(
                &tx,
                embedding_generation_id.expect("metadata exists only for an embedding generation"),
                *dimension,
                *total_chunks,
            ) {
                drop(tx);
                let _ = self.cleanup_owned_index_generation(tantivy_generation);
                return Err(error);
            }
        }
        let current = tx
            .query_row(
                "SELECT publication_id FROM retrieval_publication_pointer WHERE id = 1",
                [],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        if current != expected_publication_id {
            drop(tx);
            let _ = self.cleanup_owned_index_generation(tantivy_generation);
            return Err(QghError::validation(
                "publication.cas_conflict",
                "Retrieval publication changed before activation.",
            ));
        }
        let index_state = tx
            .query_row(
                "SELECT write_epoch, active, path, source_count,
                    source_snapshot_sync_run_id, source_snapshot_epoch,
                    source_inventory_hash, expected_publication_id
                 FROM index_generations WHERE generation = ?1",
                params![tantivy_generation],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, bool>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<i64>>(5)?,
                        row.get::<_, Option<String>>(6)?,
                        row.get::<_, Option<i64>>(7)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            index_epoch,
            index_active,
            index_path,
            source_count,
            index_snapshot_sync_run_id,
            index_snapshot_epoch,
            index_source_inventory_hash,
            index_expected_publication_id,
        )) = index_state
        else {
            drop(tx);
            let _ = self.cleanup_owned_index_generation(tantivy_generation);
            return Err(QghError::validation(
                "publication.tantivy_generation_missing",
                "The retrieval publication references a missing Tantivy generation.",
            ));
        };
        if index_epoch != expected_epoch {
            drop(tx);
            let _ = self.cleanup_owned_index_generation(tantivy_generation);
            return Err(write_fence_error());
        }
        if index_expected_publication_id != expected_publication_id {
            drop(tx);
            let _ = self.cleanup_owned_index_generation(tantivy_generation);
            return Err(QghError::validation(
                "publication.cas_conflict",
                "The index generation was reserved against a different retrieval publication.",
            ));
        }
        let index_snapshot_matches = index_snapshot_sync_run_id.as_deref()
            == Some(source_snapshot_sync_run_id)
            && index_snapshot_epoch == Some(current_source_snapshot_epoch)
            && tx.query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM sync_runs
                     WHERE id = ?1
                       AND completed_successfully = 1
                       AND source_snapshot_epoch = ?2
                 )",
                params![source_snapshot_sync_run_id, current_source_snapshot_epoch],
                |row| row.get::<_, bool>(0),
            )?;
        if !index_snapshot_matches {
            drop(tx);
            let _ = self.cleanup_owned_index_generation(tantivy_generation);
            return Err(changed_source_snapshot_error());
        }
        if usize::try_from(source_count).ok() != Some(authoritative_sources.len())
            || index_source_inventory_hash.as_deref()
                != Some(authoritative_source_inventory_hash.as_str())
        {
            drop(tx);
            let _ = self.cleanup_owned_index_generation(tantivy_generation);
            return Err(source_inventory_mismatch_error());
        }
        if Path::new(&index_path) != expected_generation_path {
            drop(tx);
            let _ = self.cleanup_owned_index_generation(tantivy_generation);
            return Err(tantivy_artifact_not_ready_error());
        }
        let Some(source_count) = usize::try_from(source_count).ok() else {
            drop(tx);
            let _ = self.cleanup_owned_index_generation(tantivy_generation);
            return Err(tantivy_artifact_not_ready_error());
        };
        if let Err(error) = validate_managed_tantivy_generation_path(
            &self.profile_dir,
            &self.index_root,
            tantivy_generation,
            Path::new(&index_path),
        ) {
            drop(tx);
            let _ = self.cleanup_owned_index_generation(tantivy_generation);
            return Err(error);
        }
        let Some(owner_token) = owner_token.as_deref() else {
            drop(tx);
            return Err(write_fence_error());
        };
        if let Err(error) = crate::index::validate_owned_generation_directory(
            &expected_generation_path,
            tantivy_generation,
            owner_token,
        ) {
            drop(tx);
            let _ = self.cleanup_owned_index_generation(tantivy_generation);
            return Err(error);
        }
        if let Err(error) = validate_tantivy_generation_artifact(
            &expected_generation_path,
            source_count,
            &authoritative_source_inventory_hash,
        ) {
            drop(tx);
            let _ = self.cleanup_owned_index_generation(tantivy_generation);
            return Err(error);
        }
        let lease_matches = tx
            .query_row(
                "SELECT 1 FROM index_build_leases
                 WHERE generation = ?1 AND write_epoch = ?2 AND owner_token = ?3",
                params![tantivy_generation, expected_epoch, owner_token],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if index_active || !lease_matches {
            drop(tx);
            let _ = self.cleanup_owned_index_generation(tantivy_generation);
            return Err(write_fence_error());
        }
        let successor_repair_required = read_successor_repair_required(&tx)?;
        if successor_repair_required {
            let authoritative_snapshot = tx.query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM sync_runs
                     WHERE id = ?1
                       AND completed_successfully = 1
                       AND snapshot_kind = 'purge_successor'
                       AND content_write_epoch = ?2
                 )",
                params![source_snapshot_sync_run_id, expected_epoch],
                |row| row.get::<_, bool>(0),
            )?;
            if !authoritative_snapshot {
                drop(tx);
                let _ = self.cleanup_owned_index_generation(tantivy_generation);
                return Err(QghError::validation(
                    "publication.successor_snapshot_required",
                    "A post-purge publication requires the persisted successor snapshot for the current write epoch.",
                ));
            }
        }
        let now = now_rfc3339();
        let (manifest, runtime, chunker, context, dimension) = embedding_metadata
            .as_ref()
            .map(
                |(_, manifest, runtime, chunker, context, dimension, _, _, _, _, _, _, _)| {
                    (
                        Some(manifest.as_str()),
                        Some(runtime.as_str()),
                        Some(chunker.as_str()),
                        Some(context.as_str()),
                        Some(*dimension as i64),
                    )
                },
            )
            .unwrap_or((None, None, None, None, None));
        tx.execute(
            "INSERT INTO retrieval_publications
                (source_snapshot_sync_run_id, tantivy_generation,
                 embedding_generation_id, model_manifest_hash,
                 runtime_fingerprint_hash, chunker_fingerprint,
                 context_template_version, output_dimension,
                 source_snapshot_epoch, active, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 1, ?10)",
            params![
                source_snapshot_sync_run_id,
                tantivy_generation,
                embedding_generation_id,
                manifest,
                runtime,
                chunker,
                context,
                dimension,
                current_source_snapshot_epoch,
                now
            ],
        )?;
        let publication_id = tx.last_insert_rowid();
        tx.execute(
            "UPDATE retrieval_publications SET active = 0 WHERE publication_id != ?1",
            params![publication_id],
        )?;
        tx.execute(
            "INSERT INTO retrieval_publication_pointer(id, publication_id)
             VALUES (1, ?1)
             ON CONFLICT(id) DO UPDATE SET publication_id = excluded.publication_id",
            params![publication_id],
        )?;
        tx.execute(
            "UPDATE index_generations SET active = CASE WHEN generation = ?1 THEN 1 ELSE 0 END",
            params![tantivy_generation],
        )?;
        tx.execute(
            "UPDATE index_tasks SET completed_at = ?1 WHERE completed_at IS NULL",
            params![now],
        )?;
        if let Some(generation_id) = embedding_generation_id {
            tx.execute(
                "UPDATE embedding_generations SET state = 'active', updated_at = ?2 WHERE id = ?1",
                params![generation_id, now_rfc3339()],
            )?;
        }
        if let Some(previous) = current {
            if let Some(previous_generation) = tx.query_row(
                "SELECT embedding_generation_id FROM retrieval_publications WHERE publication_id = ?1",
                params![previous],
                |row| row.get::<_, Option<i64>>(0),
            )? {
                tx.execute(
                    "UPDATE embedding_generations SET state = 'ready', updated_at = ?2
                     WHERE id = ?1 AND state = 'active'",
                    params![previous_generation, now_rfc3339()],
                )?;
            }
        }
        let ownership_retained = tx.execute(
            "UPDATE index_build_leases
             SET owner_pid = 0
             WHERE generation = ?1 AND owner_token = ?2",
            params![tantivy_generation, owner_token],
        )?;
        if ownership_retained != 1 {
            return Err(write_fence_error());
        }
        if successor_repair_required {
            clear_successor_repair_required(&tx)?;
        }
        tx.commit()?;
        self.index_build_tokens.remove(&tantivy_generation);
        Ok(publication_id)
    }

    pub fn active_retrieval_publication(
        &self,
    ) -> Result<Option<RetrievalPublicationView>, QghError> {
        self.conn
            .query_row(
                "SELECT rp.publication_id, rp.source_snapshot_sync_run_id,
                        rp.source_snapshot_epoch, rp.tantivy_generation,
                        rp.embedding_generation_id,
                        rp.model_manifest_hash, rp.runtime_fingerprint_hash,
                        rp.chunker_fingerprint,
                        rp.context_template_version, rp.output_dimension
                 FROM retrieval_publication_pointer p
                 JOIN retrieval_publications rp ON rp.publication_id = p.publication_id
                 WHERE p.id = 1",
                [],
                |row| {
                    Ok(RetrievalPublicationView {
                        publication_id: row.get(0)?,
                        source_snapshot_sync_run_id: row.get(1)?,
                        source_snapshot_epoch: row.get(2)?,
                        tantivy_generation: row.get(3)?,
                        embedding_generation_id: row.get(4)?,
                        model_manifest_hash: row.get(5)?,
                        runtime_fingerprint_hash: row.get(6)?,
                        chunker_fingerprint: row.get(7)?,
                        context_template_version: row.get(8)?,
                        output_dimension: row
                            .get::<_, Option<i64>>(9)?
                            .and_then(|value| usize::try_from(value).ok()),
                    })
                },
            )
            .optional()
            .map_err(QghError::from)
    }

    /// Resolves the only Tantivy artifact that retrieval may use. `None` is a
    /// valid state only for a never-published profile with no active sources.
    /// The resolver is read-only: invalid publication state is reported and is
    /// never repaired or detached here.
    pub fn resolve_active_tantivy_artifact(&self) -> Result<Option<PathBuf>, QghError> {
        let publication = self.active_retrieval_publication()?;
        let (pointer_count, active_publication_count, active_generation_count) =
            self.conn.query_row(
                "SELECT
                     (SELECT count(*) FROM retrieval_publication_pointer),
                     (SELECT count(*) FROM retrieval_publications WHERE active = 1),
                     (SELECT count(*) FROM index_generations WHERE active = 1)",
                [],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )?;
        let expected_active_count = if publication.is_some() { 1 } else { 0 };
        if pointer_count != expected_active_count
            || active_publication_count != expected_active_count
            || active_generation_count != expected_active_count
        {
            return Err(tantivy_artifact_not_ready_error());
        }
        self.validate_query_publication_snapshot(publication.as_ref())?;
        let Some(publication) = publication else {
            return Ok(None);
        };

        let generation = self
            .conn
            .query_row(
                "SELECT generation.path, generation.source_count,
                        generation.source_inventory_hash, generation.active,
                        generation.source_snapshot_sync_run_id,
                        generation.source_snapshot_epoch, publication.active
                 FROM index_generations generation
                 JOIN retrieval_publications publication
                   ON publication.publication_id = ?1
                  AND publication.tantivy_generation = generation.generation
                 WHERE generation.generation = ?2",
                params![publication.publication_id, publication.tantivy_generation],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, bool>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<i64>>(5)?,
                        row.get::<_, bool>(6)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            stored_path,
            source_count,
            inventory_hash,
            generation_active,
            sync_run_id,
            epoch,
            publication_active,
        )) = generation
        else {
            return Err(tantivy_artifact_not_ready_error());
        };
        if !publication_active
            || !generation_active
            || sync_run_id.as_deref() != Some(publication.source_snapshot_sync_run_id.as_str())
            || epoch != Some(publication.source_snapshot_epoch)
        {
            return Err(tantivy_artifact_not_ready_error());
        }
        let expected_path = validate_managed_tantivy_generation_path(
            &self.profile_dir,
            &self.index_root,
            publication.tantivy_generation,
            Path::new(&stored_path),
        )?;
        let Some(source_count) = usize::try_from(source_count).ok() else {
            return Err(source_inventory_mismatch_error());
        };
        let Some(inventory_hash) = inventory_hash
            .as_deref()
            .filter(|hash| hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
        else {
            return Err(source_inventory_mismatch_error());
        };
        validate_tantivy_generation_artifact(&expected_path, source_count, inventory_hash)?;
        Ok(Some(expected_path))
    }

    pub fn validate_query_publication_snapshot(
        &self,
        publication: Option<&RetrievalPublicationView>,
    ) -> Result<(), QghError> {
        let current_epoch = read_source_snapshot_epoch(&self.conn)?;
        let has_active_sources: bool = self.conn.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM source_entities WHERE lifecycle_state = 'active'
             )",
            [],
            |row| row.get(0),
        )?;
        let Some(publication) = publication else {
            return if has_active_sources {
                Err(incomplete_source_snapshot_error())
            } else {
                Ok(())
            };
        };
        if publication.source_snapshot_epoch != current_epoch {
            return Err(changed_source_snapshot_error());
        }
        let source_snapshot_valid = self.conn.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM sync_runs
                 WHERE id = ?1 AND completed_successfully = 1
                   AND source_snapshot_epoch = ?2
             )",
            params![publication.source_snapshot_sync_run_id, current_epoch],
            |row| row.get::<_, bool>(0),
        )?;
        if !source_snapshot_valid {
            return Err(changed_source_snapshot_error());
        }
        let index_snapshot = self
            .conn
            .query_row(
                "SELECT source_snapshot_sync_run_id, source_snapshot_epoch,
                        source_count, source_inventory_hash
                 FROM index_generations WHERE generation = ?1",
                params![publication.tantivy_generation],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .optional()?;
        if !index_snapshot.is_some_and(|(sync_run_id, epoch, count, inventory_hash)| {
            sync_run_id.as_deref() == Some(publication.source_snapshot_sync_run_id.as_str())
                && epoch == Some(current_epoch)
                && count >= 0
                && inventory_hash.is_some()
        }) {
            return Err(changed_source_snapshot_error());
        }
        let identity_field_count = [
            publication.embedding_generation_id.is_some(),
            publication.model_manifest_hash.is_some(),
            publication.runtime_fingerprint_hash.is_some(),
            publication.chunker_fingerprint.is_some(),
            publication.context_template_version.is_some(),
            publication.output_dimension.is_some(),
        ]
        .into_iter()
        .filter(|present| *present)
        .count();
        if identity_field_count != 0 && identity_field_count != 6 {
            return Err(embedding_snapshot_mismatch_error());
        }
        let Some(embedding_generation_id) = publication.embedding_generation_id else {
            return Ok(());
        };
        if !table_exists(&self.conn, "embedding_generations")? {
            return Err(embedding_snapshot_mismatch_error());
        }
        let embedding = self
            .conn
            .query_row(
                "SELECT state, model_manifest_hash, runtime_fingerprint_hash,
                        chunker_fingerprint,
                        context_template_version, output_dimension,
                        source_sync_run_id, source_snapshot_hash,
                        source_snapshot_epoch, embedding_inventory_hash,
                        total_chunks, completed_chunks
                 FROM embedding_generations WHERE id = ?1",
                params![embedding_generation_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<i64>>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, Option<i64>>(8)?,
                        row.get::<_, Option<String>>(9)?,
                        row.get::<_, i64>(10)?,
                        row.get::<_, i64>(11)?,
                    ))
                },
            )
            .optional()?;
        let expected_snapshot_hash = source_snapshot_identity_hash(&SourceSnapshotIdentity {
            sync_run_id: publication.source_snapshot_sync_run_id.clone(),
            epoch: current_epoch,
        });
        if !embedding.is_some_and(
            |(
                state,
                manifest,
                runtime,
                chunker,
                context,
                dimension,
                sync_run_id,
                snapshot_hash,
                epoch,
                inventory_hash,
                total_chunks,
                completed_chunks,
            )| {
                let dimension = dimension.and_then(|value| usize::try_from(value).ok());
                state == "active"
                    && manifest.as_deref().is_some_and(|manifest| {
                        !manifest.is_empty()
                            && Some(manifest) == publication.model_manifest_hash.as_deref()
                    })
                    && runtime.as_deref().is_some_and(|runtime| {
                        !runtime.is_empty()
                            && Some(runtime) == publication.runtime_fingerprint_hash.as_deref()
                    })
                    && chunker.as_deref().is_some_and(|chunker| {
                        !chunker.is_empty()
                            && Some(chunker) == publication.chunker_fingerprint.as_deref()
                    })
                    && context.as_deref() == Some(crate::context::METADATA_CONTEXT_TEMPLATE_VERSION)
                    && publication.context_template_version.as_deref() == context.as_deref()
                    && publication.output_dimension == dimension.filter(|dimension| *dimension > 0)
                    && sync_run_id == publication.source_snapshot_sync_run_id
                    && snapshot_hash == expected_snapshot_hash
                    && epoch == Some(current_epoch)
                    && inventory_hash.is_some()
                    && total_chunks == completed_chunks
            },
        ) {
            return Err(embedding_snapshot_mismatch_error());
        }
        Ok(())
    }

    pub fn cleanup_embedding_generations(
        &mut self,
        stale_building_before: &str,
        previous_ready_before: &str,
    ) -> Result<usize, QghError> {
        #[cfg(test)]
        let promote_after_scan = self.cleanup_promote_generation_after_scan.take();
        #[cfg(test)]
        let fail_after_first_generation_delete =
            std::mem::take(&mut self.cleanup_fail_after_first_generation_delete);
        let expected_epoch = self.content_write_epoch;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_content_write_allowed(&tx, expected_epoch)?;
        let active_publication_id = tx
            .query_row(
                "SELECT publication_id FROM retrieval_publication_pointer WHERE id = 1",
                [],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        let active_generation = tx
            .query_row(
                "SELECT rp.embedding_generation_id
                 FROM retrieval_publication_pointer p
                 JOIN retrieval_publications rp ON rp.publication_id = p.publication_id
                 WHERE p.id = 1",
                [],
                |row| row.get::<_, Option<i64>>(0),
            )
            .optional()?
            .flatten();
        let previous_generation = tx
            .query_row(
                "SELECT rp.embedding_generation_id
                 FROM retrieval_publications rp
                 JOIN embedding_generations eg ON eg.id = rp.embedding_generation_id
                 WHERE rp.embedding_generation_id IS NOT NULL
                   AND rp.publication_id != coalesce(?1, -1)
                   AND eg.created_at >= ?2
                 ORDER BY rp.created_at DESC, rp.publication_id DESC
                 LIMIT 1",
                params![active_publication_id, previous_ready_before],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        let mut keep = BTreeSet::new();
        if let Some(id) = active_generation {
            keep.insert(id);
        }
        if let Some(id) = previous_generation {
            keep.insert(id);
        }
        let candidate_rows = tx
            .prepare(
                "SELECT id, output_dimension FROM embedding_generations
             WHERE (state = 'building' AND updated_at < ?1)
                OR state IN ('failed', 'ready')
             ORDER BY id",
            )?
            .query_map(params![stale_building_before], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut candidates = Vec::new();
        for (generation_id, stored_dimension) in candidate_rows {
            if keep.contains(&generation_id) {
                continue;
            }
            let dimension = usize::try_from(stored_dimension)
                .ok()
                .filter(|dimension| *dimension > 0)
                .ok_or_else(embedding_generation_corrupt_error)?;
            let mappings = tx
                .prepare(
                    "SELECT id, dimension, vector_table, vector_rowid
                     FROM embedding_generation_vector_rows
                     WHERE generation_id = ?1
                     ORDER BY id",
                )?
                .query_map(params![generation_id], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            let (generation_chunk_count, owned_mapping_count): (i64, i64) = tx.query_row(
                "SELECT
                    (SELECT count(*) FROM embedding_generation_chunks
                     WHERE generation_id = ?1),
                    (SELECT count(*)
                     FROM embedding_generation_vector_rows m
                     JOIN embedding_generation_chunks gc
                       ON gc.generation_id = m.generation_id AND gc.chunk_id = m.chunk_id
                     WHERE m.generation_id = ?1)",
                params![generation_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            if i64::try_from(mappings.len()).ok() != Some(generation_chunk_count)
                || owned_mapping_count != generation_chunk_count
            {
                return Err(embedding_generation_corrupt_error());
            }
            let mut owned_mappings = Vec::with_capacity(mappings.len());
            for (mapping_id, mapping_dimension, stored_table, vector_rowid) in mappings {
                let owned_table = validate_generation_vector_mapping_ownership(
                    mapping_id,
                    mapping_dimension,
                    &stored_table,
                    vector_rowid,
                    dimension,
                )?;
                if !generation_vector_table_schema_matches(&tx, &owned_table, dimension)? {
                    return Err(embedding_generation_corrupt_error());
                }
                #[cfg(feature = "vector-search")]
                if !tx.query_row(
                    &format!("SELECT EXISTS(SELECT 1 FROM {owned_table} WHERE rowid = ?1)"),
                    params![vector_rowid],
                    |row| row.get::<_, bool>(0),
                )? {
                    return Err(embedding_generation_corrupt_error());
                }
                owned_mappings.push((owned_table, dimension, vector_rowid));
            }
            candidates.push((generation_id, owned_mappings));
        }
        #[cfg(test)]
        if let Some(generation_id) = promote_after_scan {
            tx.execute("UPDATE retrieval_publications SET active = 0", [])?;
            tx.execute(
                "INSERT INTO retrieval_publications
                    (source_snapshot_sync_run_id, tantivy_generation,
                     embedding_generation_id, model_manifest_hash,
                     runtime_fingerprint_hash, chunker_fingerprint,
                     context_template_version, output_dimension,
                     source_snapshot_epoch, active, created_at)
                 SELECT source_sync_run_id, 0, id, model_manifest_hash,
                        runtime_fingerprint_hash, chunker_fingerprint,
                        context_template_version, output_dimension,
                        source_snapshot_epoch, 1, ?2
                 FROM embedding_generations WHERE id = ?1",
                params![generation_id, now_rfc3339()],
            )?;
            let publication_id = tx.last_insert_rowid();
            tx.execute(
                "INSERT INTO retrieval_publication_pointer(id, publication_id)
                 VALUES (1, ?1)
                 ON CONFLICT(id) DO UPDATE SET publication_id = excluded.publication_id",
                params![publication_id],
            )?;
            tx.execute(
                "UPDATE embedding_generations SET state = 'active' WHERE id = ?1",
                params![generation_id],
            )?;
        }
        let mut removed = 0;
        for (generation_id, mappings) in candidates {
            if embedding_generation_cleanup_protected(&tx, generation_id, previous_ready_before)? {
                continue;
            }
            for (table, dimension, rowid) in mappings {
                #[cfg(feature = "vector-search")]
                {
                    let _ = dimension;
                    tx.execute(
                        &format!("DELETE FROM {table} WHERE rowid = ?1"),
                        params![rowid],
                    )?;
                }
                #[cfg(not(feature = "vector-search"))]
                delete_vec0_shadow_row(&tx, &table, dimension, rowid)?;
            }
            tx.execute(
                "DELETE FROM retrieval_publications
                 WHERE embedding_generation_id = ?1
                   AND publication_id NOT IN (
                       SELECT publication_id FROM retrieval_publication_pointer WHERE id = 1
                   )",
                params![generation_id],
            )?;
            tx.execute(
                "DELETE FROM embedding_generation_vector_rows WHERE generation_id = ?1",
                params![generation_id],
            )?;
            tx.execute(
                "DELETE FROM embedding_generation_chunks WHERE generation_id = ?1",
                params![generation_id],
            )?;
            removed += tx.execute(
                "DELETE FROM embedding_generations WHERE id = ?1",
                params![generation_id],
            )?;
            #[cfg(test)]
            if fail_after_first_generation_delete && removed > 0 {
                return Err(QghError::validation(
                    "embedding.generation_cleanup_injected_failure",
                    "Embedding generation cleanup failed at an injected test boundary.",
                ));
            }
        }
        tx.commit()?;
        Ok(removed)
    }

    fn ensure_vector_storage(&mut self, dimension: usize) -> Result<(), QghError> {
        if dimension == 0 {
            return Err(QghError::storage(
                "Cannot create sqlite-vec storage for zero-dimensional embeddings.",
            ));
        }
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = ensure_content_write_allowed(&self.conn, self.content_write_epoch)
            .and_then(|()| Self::ensure_vector_storage_inner(&self.conn, dimension));
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

    fn ensure_vector_storage_inner(conn: &Connection, dimension: usize) -> Result<(), QghError> {
        if dimension == 0 {
            return Err(QghError::storage(
                "Cannot create sqlite-vec storage for zero-dimensional embeddings.",
            ));
        }
        if let Some(existing_dimension) = vector_table_dimension(conn)? {
            if existing_dimension == dimension {
                return Ok(());
            }
            conn.execute(&format!("DROP TABLE {CHUNK_EMBEDDING_VECTORS_TABLE}"), [])?;
        }
        conn.execute(
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
                runtime_fingerprint_hash TEXT NOT NULL,
                chunker_fingerprint TEXT NOT NULL,
                context_template_version TEXT NOT NULL,
                output_dimension INTEGER NOT NULL,
                source_sync_run_id TEXT NOT NULL,
                source_snapshot_hash TEXT NOT NULL,
                embedding_inventory_hash TEXT,
                total_chunks INTEGER NOT NULL,
                completed_chunks INTEGER NOT NULL DEFAULT 0,
                checkpoint_chunk_id INTEGER,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                failure_code TEXT,
                write_epoch INTEGER NOT NULL DEFAULT 0,
                source_snapshot_epoch INTEGER
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
                runtime_fingerprint_hash TEXT,
                chunker_fingerprint TEXT,
                context_template_version TEXT,
                output_dimension INTEGER,
                source_snapshot_epoch INTEGER,
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
        ensure_column(
            &self.conn,
            "embedding_generations",
            "model_manifest_hash",
            "TEXT",
        )?;
        ensure_column(
            &self.conn,
            "embedding_generations",
            "runtime_fingerprint_hash",
            "TEXT",
        )?;
        ensure_column(
            &self.conn,
            "embedding_generations",
            "chunker_fingerprint",
            "TEXT",
        )?;
        ensure_column(
            &self.conn,
            "embedding_generations",
            "context_template_version",
            "TEXT",
        )?;
        ensure_column(
            &self.conn,
            "embedding_generations",
            "output_dimension",
            "INTEGER",
        )?;
        ensure_column(
            &self.conn,
            "embedding_generations",
            "write_epoch",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        ensure_column(
            &self.conn,
            "embedding_generations",
            "source_snapshot_epoch",
            "INTEGER",
        )?;
        ensure_column(
            &self.conn,
            "embedding_generations",
            "embedding_inventory_hash",
            "TEXT",
        )?;
        ensure_column(
            &self.conn,
            "retrieval_publications",
            "runtime_fingerprint_hash",
            "TEXT",
        )?;
        ensure_column(
            &self.conn,
            "retrieval_publications",
            "source_snapshot_epoch",
            "INTEGER",
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
            INSERT INTO profile_meta (key, value)
                VALUES ('content_write_epoch', '0')
                ON CONFLICT(key) DO NOTHING;
            INSERT INTO profile_meta (key, value)
                VALUES ('source_snapshot_epoch', '0')
                ON CONFLICT(key) DO NOTHING;

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
                skipped_pull_request_count INTEGER NOT NULL,
                snapshot_kind TEXT NOT NULL DEFAULT 'remote_sync',
                content_write_epoch INTEGER,
                source_snapshot_epoch INTEGER
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

            CREATE TABLE IF NOT EXISTS purge_requests (
                target_kind TEXT NOT NULL CHECK (target_kind IN ('source', 'issue', 'repository')),
                target_value TEXT NOT NULL,
                trigger TEXT NOT NULL CHECK (
                    trigger IN (
                        'confirmed_delete', 'confirmed_tombstone',
                        'permission_loss', 'allowlist_removal'
                    )
                ),
                purge_pending INTEGER NOT NULL CHECK (purge_pending IN (0, 1)),
                current_stage TEXT NOT NULL DEFAULT 'secure_delete' CHECK (
                    current_stage IN (
                        'secure_delete', 'tantivy', 'storage', 'wal_checkpoint', 'finalize'
                    )
                ),
                failure_stage TEXT CHECK (
                    failure_stage IS NULL OR failure_stage IN (
                        'secure_delete', 'tantivy', 'storage', 'wal_checkpoint', 'finalize'
                    )
                ),
                completion_ready INTEGER NOT NULL DEFAULT 0 CHECK (completion_ready IN (0, 1)),
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                PRIMARY KEY (target_kind, target_value)
            );

            CREATE TABLE IF NOT EXISTS purge_target_sources (
                target_kind TEXT NOT NULL,
                target_value TEXT NOT NULL,
                source_id TEXT NOT NULL,
                PRIMARY KEY (target_kind, target_value, source_id)
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
                active INTEGER NOT NULL,
                write_epoch INTEGER NOT NULL DEFAULT 0,
                source_snapshot_sync_run_id TEXT,
                source_snapshot_epoch INTEGER,
                source_inventory_hash TEXT,
                expected_publication_id INTEGER
            );

            CREATE TABLE IF NOT EXISTS index_build_leases (
                generation INTEGER PRIMARY KEY,
                write_epoch INTEGER NOT NULL,
                owner_pid INTEGER NOT NULL,
                owner_token TEXT NOT NULL UNIQUE,
                created_at TEXT NOT NULL,
                source_snapshot_sync_run_id TEXT,
                source_snapshot_epoch INTEGER
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
                runtime_fingerprint_hash TEXT,
                chunker_fingerprint TEXT,
                context_template_version TEXT,
                output_dimension INTEGER,
                source_snapshot_epoch INTEGER,
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
        if table_exists(&self.conn, "embedding_generations")? {
            ensure_column(
                &self.conn,
                "embedding_generations",
                "model_manifest_hash",
                "TEXT",
            )?;
            ensure_column(
                &self.conn,
                "embedding_generations",
                "runtime_fingerprint_hash",
                "TEXT",
            )?;
            ensure_column(
                &self.conn,
                "embedding_generations",
                "chunker_fingerprint",
                "TEXT",
            )?;
            ensure_column(
                &self.conn,
                "embedding_generations",
                "context_template_version",
                "TEXT",
            )?;
            ensure_column(
                &self.conn,
                "embedding_generations",
                "output_dimension",
                "INTEGER",
            )?;
            ensure_column(
                &self.conn,
                "embedding_generations",
                "write_epoch",
                "INTEGER NOT NULL DEFAULT 0",
            )?;
            ensure_column(
                &self.conn,
                "embedding_generations",
                "embedding_inventory_hash",
                "TEXT",
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
        ensure_column(
            &self.conn,
            "sync_runs",
            "snapshot_kind",
            "TEXT NOT NULL DEFAULT 'remote_sync'",
        )?;
        ensure_column(&self.conn, "sync_runs", "content_write_epoch", "INTEGER")?;
        ensure_column(&self.conn, "sync_runs", "source_snapshot_epoch", "INTEGER")?;
        ensure_column(
            &self.conn,
            "index_generations",
            "write_epoch",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        ensure_column(
            &self.conn,
            "index_generations",
            "source_snapshot_sync_run_id",
            "TEXT",
        )?;
        ensure_column(
            &self.conn,
            "index_generations",
            "source_snapshot_epoch",
            "INTEGER",
        )?;
        ensure_column(
            &self.conn,
            "index_generations",
            "source_inventory_hash",
            "TEXT",
        )?;
        ensure_column(
            &self.conn,
            "index_generations",
            "expected_publication_id",
            "INTEGER",
        )?;
        ensure_column(
            &self.conn,
            "index_build_leases",
            "source_snapshot_sync_run_id",
            "TEXT",
        )?;
        ensure_column(
            &self.conn,
            "index_build_leases",
            "source_snapshot_epoch",
            "INTEGER",
        )?;
        ensure_column(
            &self.conn,
            "retrieval_publications",
            "runtime_fingerprint_hash",
            "TEXT",
        )?;
        ensure_column(
            &self.conn,
            "retrieval_publications",
            "source_snapshot_epoch",
            "INTEGER",
        )?;
        detach_incomplete_publication_snapshots(&self.conn)?;
        self.conn.execute(
            "INSERT INTO profile_meta (key, value)
             SELECT 'next_index_generation',
                    CAST(coalesce(max(generation), 0) + 1 AS TEXT)
             FROM index_generations
             WHERE 1
             ON CONFLICT(key) DO UPDATE SET
                value = CAST(max(
                    CAST(profile_meta.value AS INTEGER),
                    CAST(excluded.value AS INTEGER)
                ) AS TEXT)",
            [],
        )?;
        ensure_column(
            &self.conn,
            "purge_requests",
            "current_stage",
            "TEXT NOT NULL DEFAULT 'secure_delete'",
        )?;
        ensure_column(
            &self.conn,
            "purge_requests",
            "completion_ready",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        self.conn.execute(
            "INSERT INTO profile_meta (key, value)
             SELECT 'successor_repair_required',
                    CASE
                        WHEN EXISTS(
                            SELECT 1 FROM purge_requests WHERE purge_pending = 1
                        ) OR (
                            EXISTS(
                                SELECT 1 FROM purge_requests
                                WHERE purge_pending = 0 AND completion_ready = 1
                            ) AND NOT EXISTS(
                                SELECT 1 FROM retrieval_publication_pointer WHERE id = 1
                            )
                        ) THEN '1'
                        ELSE '0'
                    END
             ON CONFLICT(key) DO NOTHING",
            [],
        )?;
        if read_successor_repair_required(&self.conn)? {
            let epoch = read_content_write_epoch(&self.conn)?;
            mark_successor_repair_required(&self.conn, epoch)?;
        }
        self.conn.execute(
            "INSERT OR IGNORE INTO repository_sync_state (repo, last_successful_sync_at)
             SELECT DISTINCT repo, (
                 SELECT max(completed_at) FROM sync_runs
                 WHERE snapshot_kind = 'remote_sync'
             )
             FROM source_entities
             WHERE lifecycle_state = 'active'
               AND (
                   SELECT max(completed_at) FROM sync_runs
                   WHERE snapshot_kind = 'remote_sync'
               ) IS NOT NULL",
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
        self.queue_untracked_legacy_tombstones()?;
        self.conn.execute(
            "INSERT INTO schema_migrations (version, applied_at)
             VALUES (?1, ?2)
             ON CONFLICT(version) DO NOTHING",
            params!["qgh.db.v1", now_rfc3339()],
        )?;
        self.conn.execute(
            "INSERT INTO schema_migrations (version, applied_at)
             VALUES (?1, ?2)
             ON CONFLICT(version) DO NOTHING",
            params!["qgh.purge.v1", now_rfc3339()],
        )?;
        Ok(())
    }

    fn queue_untracked_legacy_tombstones(&self) -> Result<usize, QghError> {
        type LegacyTombstoneCandidate = (String, String, String, String, String);
        let candidates = (|| -> Result<Vec<LegacyTombstoneCandidate>, rusqlite::Error> {
            let mut stmt = self.conn.prepare(
                "SELECT t.source_id, t.reason, se.host, se.entity_type, se.node_id
                 FROM tombstones t
                 JOIN source_entities se ON se.source_id = t.source_id
                 WHERE NOT EXISTS (
                     SELECT 1 FROM purge_requests pr
                     WHERE pr.target_kind = 'source'
                       AND pr.target_value = t.source_id
                 )
                 ORDER BY t.source_id",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })?;
            rows.collect::<Result<Vec<_>, _>>()
        })()
        .map_err(|_| purge_error())?;
        let now = now_rfc3339();
        let mut inserted = 0;
        for (source_id, reason, host, entity_type, node_id) in candidates {
            let canonical_entity_type = match entity_type.as_str() {
                "issue" => "issue",
                "issue_comment" => "issue-comment",
                _ => continue,
            };
            let target = PurgeTarget::Source {
                source_id: source_id.clone(),
            };
            if source_id != format!("qgh://{host}/{canonical_entity_type}/{node_id}")
                || validate_purge_target(&target).is_err()
            {
                continue;
            }
            let trigger = match reason.as_str() {
                "transferred" => PurgeTrigger::ConfirmedTombstone,
                "permission_loss" => PurgeTrigger::PermissionLoss,
                "allowlist_removal" => PurgeTrigger::AllowlistRemoval,
                _ => PurgeTrigger::ConfirmedDelete,
            };
            let changed = self
                .conn
                .execute(
                    "INSERT INTO purge_requests
                        (target_kind, target_value, trigger, purge_pending,
                         current_stage, failure_stage, completion_ready, created_at, updated_at)
                     VALUES ('source', ?1, ?2, 1, 'secure_delete', NULL, 0, ?3, ?3)
                     ON CONFLICT(target_kind, target_value) DO NOTHING",
                    params![source_id, trigger.as_str(), now],
                )
                .map_err(|_| purge_error())?;
            if changed == 0 {
                continue;
            }
            self.conn
                .execute(
                    "INSERT INTO purge_target_sources
                        (target_kind, target_value, source_id)
                     VALUES ('source', ?1, ?1)
                     ON CONFLICT(target_kind, target_value, source_id) DO NOTHING",
                    params![source_id],
                )
                .map_err(|_| purge_error())?;
            inserted += 1;
        }
        if inserted > 0 {
            self.conn
                .execute(
                    "UPDATE profile_meta
                     SET value = CAST(value AS INTEGER) + 1
                     WHERE key = 'content_write_epoch'",
                    [],
                )
                .map_err(|_| purge_error())?;
            let content_write_epoch =
                read_content_write_epoch(&self.conn).map_err(|_| purge_error())?;
            mark_successor_repair_required(&self.conn, content_write_epoch)
                .map_err(|_| purge_error())?;
            invalidate_publications_for_pending_purge(&self.conn).map_err(|_| purge_error())?;
        }
        Ok(inserted)
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let generations = self.index_build_tokens.keys().copied().collect::<Vec<_>>();
        for generation in generations {
            let _ = self.cleanup_owned_index_generation(generation);
        }
    }
}

fn capture_purge_target_sources(
    conn: &Connection,
    target: &PurgeTarget,
    kind: &str,
    value: &str,
) -> Result<(), QghError> {
    match target {
        PurgeTarget::Source { .. } => {
            conn.execute(
                "INSERT OR IGNORE INTO purge_target_sources
                    (target_kind, target_value, source_id)
                 SELECT ?1, ?2, source_id FROM source_entities WHERE source_id = ?2",
                params![kind, value],
            )?;
        }
        PurgeTarget::Issue { repo, issue_number } => {
            for table in ["issue_metadata", "comment_metadata"] {
                conn.execute(
                    &format!(
                        "INSERT OR IGNORE INTO purge_target_sources
                            (target_kind, target_value, source_id)
                         SELECT ?1, ?2, source_id FROM {table}
                         WHERE lower(repo) = lower(?3) AND issue_number = ?4"
                    ),
                    params![kind, value, repo, issue_number],
                )?;
            }
        }
        PurgeTarget::Repository { repo } => {
            conn.execute(
                "INSERT OR IGNORE INTO purge_target_sources
                    (target_kind, target_value, source_id)
                 SELECT ?1, ?2, source_id FROM source_entities
                 WHERE lower(repo) = lower(?3)",
                params![kind, value, repo],
            )?;
        }
    }
    Ok(())
}

fn purge_target_is_subsumed(
    conn: &Connection,
    target: &PurgeTarget,
    repository_targets: &BTreeSet<String>,
    issue_targets: &BTreeSet<(String, i64)>,
) -> Result<bool, QghError> {
    match target {
        PurgeTarget::Repository { .. } => Ok(false),
        PurgeTarget::Issue { repo, .. } => {
            Ok(repository_targets.contains(&repo.to_ascii_lowercase()))
        }
        PurgeTarget::Source { source_id } => {
            let scope = conn
                .query_row(
                    "SELECT se.repo, coalesce(im.issue_number, cm.issue_number)
                     FROM source_entities se
                     LEFT JOIN issue_metadata im ON im.source_id = se.source_id
                     LEFT JOIN comment_metadata cm ON cm.source_id = se.source_id
                     WHERE se.source_id = ?1",
                    params![source_id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?)),
                )
                .optional()?;
            Ok(scope.is_some_and(|(repo, issue_number)| {
                let repo = repo.to_ascii_lowercase();
                repository_targets.contains(&repo)
                    || issue_number.is_some_and(|number| issue_targets.contains(&(repo, number)))
            }))
        }
    }
}

fn canonicalize_purge_target_identity(
    conn: &Connection,
    target: &PurgeTarget,
) -> Result<PurgeTarget, QghError> {
    Ok(match target {
        PurgeTarget::Source { .. } => target.clone(),
        PurgeTarget::Issue { repo, issue_number } => PurgeTarget::Issue {
            repo: canonical_repository_identity(conn, repo)?,
            issue_number: *issue_number,
        },
        PurgeTarget::Repository { repo } => PurgeTarget::Repository {
            repo: canonical_repository_identity(conn, repo)?,
        },
    })
}

fn canonical_repository_identity(conn: &Connection, repo: &str) -> Result<String, QghError> {
    Ok(conn
        .query_row(
            "SELECT repo FROM (
                 SELECT target_value AS repo, 0 AS priority
                 FROM purge_requests
                 WHERE target_kind = 'repository' AND purge_pending = 1
                 UNION ALL
                 SELECT repo, 1 AS priority FROM repositories
                 UNION ALL
                 SELECT repo, 2 AS priority FROM source_entities
                 UNION ALL
                 SELECT repo, 3 AS priority FROM repository_sync_state
             )
             WHERE lower(repo) = lower(?1)
             ORDER BY priority, repo COLLATE BINARY DESC
             LIMIT 1",
            params![repo],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .unwrap_or_else(|| repo.to_string()))
}

fn conflicting_purge_trigger_error(
    target_kind: &str,
    existing: PurgeTrigger,
    conflicting: PurgeTrigger,
) -> QghError {
    QghError::validation(
        "purge.conflicting_triggers",
        "A purge target cannot be queued with conflicting confirmed triggers.",
    )
    .with_details(serde_json::json!({
        "target_kind": target_kind,
        "existing_trigger": existing.as_str(),
        "conflicting_trigger": conflicting.as_str()
    }))
}

fn validate_purge_target(target: &PurgeTarget) -> Result<(), QghError> {
    let (_, value) = target.kind_and_value();
    if value.is_empty() {
        return Err(QghError::validation(
            "purge.invalid_target",
            "Purge target must not be empty.",
        ));
    }
    match target {
        PurgeTarget::Source { source_id } => {
            let valid = source_id
                .strip_prefix("qgh://")
                .and_then(|identity| {
                    let mut parts = identity.split('/');
                    Some((parts.next()?, parts.next()?, parts.next()?, parts.next()))
                })
                .is_some_and(|(host, entity_type, node_id, extra)| {
                    !host.is_empty()
                        && matches!(entity_type, "issue" | "issue-comment")
                        && !node_id.is_empty()
                        && extra.is_none()
                        && !source_id.chars().any(char::is_whitespace)
                        && !source_id.chars().any(char::is_control)
                });
            if !valid {
                return Err(QghError::validation(
                    "purge.invalid_target",
                    "Source purge target must use a canonical qgh source identity.",
                ));
            }
        }
        PurgeTarget::Issue { repo, issue_number } => {
            if !valid_repository_identity(repo) || *issue_number <= 0 {
                return Err(QghError::validation(
                    "purge.invalid_target",
                    "Issue purge target must use owner/repo and a positive issue number.",
                ));
            }
        }
        PurgeTarget::Repository { repo } if !valid_repository_identity(repo) => {
            return Err(QghError::validation(
                "purge.invalid_target",
                "Repository purge target must use owner/repo format.",
            ));
        }
        PurgeTarget::Repository { .. } => {}
    }
    Ok(())
}

fn valid_repository_identity(repo: &str) -> bool {
    repo.split_once('/').is_some_and(|(owner, name)| {
        !owner.is_empty()
            && !name.is_empty()
            && !name.contains('/')
            && !repo.contains('#')
            && !repo.chars().any(char::is_whitespace)
            && !repo.chars().any(char::is_control)
    })
}

fn repository_from_cursor_endpoint(endpoint: &str) -> Option<&str> {
    for prefix in ["issues:", "history:", "repo-comments:"] {
        if let Some(repo) = endpoint.strip_prefix(prefix) {
            return valid_repository_identity(repo).then_some(repo);
        }
    }
    let target = endpoint.strip_prefix("comments:")?;
    let (repo, issue_number) = target.rsplit_once('#')?;
    let issue_number = issue_number.parse::<i64>().ok()?;
    (issue_number > 0 && valid_repository_identity(repo)).then_some(repo)
}

fn validate_managed_tantivy_generation_path(
    profile_dir: &Path,
    index_root: &Path,
    generation: i64,
    stored_path: &Path,
) -> Result<PathBuf, QghError> {
    if generation < 0 || index_root != profile_dir.join("tantivy") {
        return Err(tantivy_artifact_not_ready_error());
    }
    let expected_path = index_root.join(format!("generation-{generation}"));
    if stored_path != expected_path {
        return Err(tantivy_artifact_not_ready_error());
    }
    let profile_metadata =
        fs::symlink_metadata(profile_dir).map_err(|_| tantivy_artifact_not_ready_error())?;
    let root_metadata =
        fs::symlink_metadata(index_root).map_err(|_| tantivy_artifact_not_ready_error())?;
    if profile_metadata.file_type().is_symlink()
        || !profile_metadata.is_dir()
        || root_metadata.file_type().is_symlink()
        || !root_metadata.is_dir()
    {
        return Err(tantivy_artifact_not_ready_error());
    }
    let canonical_profile =
        fs::canonicalize(profile_dir).map_err(|_| tantivy_artifact_not_ready_error())?;
    let canonical_root =
        fs::canonicalize(index_root).map_err(|_| tantivy_artifact_not_ready_error())?;
    if canonical_root != canonical_profile.join("tantivy") {
        return Err(tantivy_artifact_not_ready_error());
    }
    let generation_metadata =
        fs::symlink_metadata(&expected_path).map_err(|_| tantivy_artifact_not_ready_error())?;
    if generation_metadata.file_type().is_symlink() || !generation_metadata.is_dir() {
        return Err(tantivy_artifact_not_ready_error());
    }
    let canonical_generation =
        fs::canonicalize(&expected_path).map_err(|_| tantivy_artifact_not_ready_error())?;
    if canonical_generation != canonical_root.join(format!("generation-{generation}")) {
        return Err(tantivy_artifact_not_ready_error());
    }
    validate_tantivy_tree_confinement(&expected_path, &canonical_generation)?;
    Ok(expected_path)
}

fn validate_tantivy_tree_confinement(
    directory: &Path,
    canonical_generation: &Path,
) -> Result<(), QghError> {
    for entry in fs::read_dir(directory).map_err(|_| tantivy_artifact_not_ready_error())? {
        let entry = entry.map_err(|_| tantivy_artifact_not_ready_error())?;
        let path = entry.path();
        let metadata =
            fs::symlink_metadata(&path).map_err(|_| tantivy_artifact_not_ready_error())?;
        if metadata.file_type().is_symlink() {
            return Err(tantivy_artifact_not_ready_error());
        }
        let canonical_path =
            fs::canonicalize(&path).map_err(|_| tantivy_artifact_not_ready_error())?;
        if !canonical_path.starts_with(canonical_generation) {
            return Err(tantivy_artifact_not_ready_error());
        }
        if metadata.is_dir() {
            validate_tantivy_tree_confinement(&path, canonical_generation)?;
        } else if !metadata.is_file() {
            return Err(tantivy_artifact_not_ready_error());
        }
    }
    Ok(())
}

fn validate_tantivy_generation_artifact(
    generation_path: &Path,
    expected_source_count: usize,
    expected_source_inventory_hash: &str,
) -> Result<(), QghError> {
    let metadata =
        fs::symlink_metadata(generation_path).map_err(|_| tantivy_artifact_not_ready_error())?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(tantivy_artifact_not_ready_error());
    }
    let index = tantivy::Index::open_in_dir(generation_path)
        .map_err(|_| tantivy_artifact_not_ready_error())?;
    let reader = index
        .reader()
        .map_err(|_| tantivy_artifact_not_ready_error())?;
    let observed_source_count = reader.searcher().num_docs() as usize;
    if observed_source_count != expected_source_count {
        return Err(tantivy_artifact_not_ready_error());
    }
    let artifact_inventory_hash = crate::index::committed_source_inventory_digest(&index)
        .map_err(|_| tantivy_artifact_not_ready_error())?;
    if artifact_inventory_hash.as_deref() != Some(expected_source_inventory_hash) {
        return Err(source_inventory_mismatch_error());
    }
    Ok(())
}

fn tantivy_purge_quarantine_path(index_root: &Path, generation: i64, owner_token: &str) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(b"qgh.tantivy-purge-quarantine.v1");
    hasher.update(generation.to_le_bytes());
    hash_text(&mut hasher, owner_token);
    let digest = digest_hex(hasher);
    index_root.join(format!(
        ".qgh-purge-generation-{generation}-{}",
        &digest[..32]
    ))
}

fn validate_quarantined_tantivy_generation(
    quarantine_path: &Path,
    generation: i64,
    owner_token: &str,
    expected_source_count: usize,
    expected_source_inventory_hash: &str,
) -> Result<(), QghError> {
    crate::index::validate_owned_generation_directory(quarantine_path, generation, owner_token)
        .map_err(|_| purge_error())?;
    validate_tantivy_generation_artifact(
        quarantine_path,
        expected_source_count,
        expected_source_inventory_hash,
    )
    .map_err(|_| purge_error())
}

fn tantivy_artifact_not_ready_error() -> QghError {
    QghError::validation(
        "publication.tantivy_artifact_not_ready",
        "The reserved Tantivy generation is missing or does not match its persisted source count.",
    )
}

fn purge_error() -> QghError {
    QghError::new(
        "purge.failed",
        "Purge did not complete; retry using the persisted safe failure stage.",
        6,
    )
}

fn purge_tombstone_reason(
    conn: &Connection,
    source_id: &str,
    trigger: PurgeTrigger,
) -> Result<String, QghError> {
    if matches!(
        trigger,
        PurgeTrigger::ConfirmedDelete | PurgeTrigger::ConfirmedTombstone
    ) {
        let existing = conn
            .query_row(
                "SELECT reason FROM tombstones WHERE source_id = ?1",
                params![source_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if existing.as_deref().is_some_and(|reason| {
            matches!(
                reason,
                "deleted" | "transferred" | "permission_loss" | "allowlist_removal"
            )
        }) {
            return Ok(existing.expect("checked as present"));
        }
    }
    Ok(trigger.tombstone_reason().to_string())
}

/// Unix can distinguish an abandoned builder without a TTL. Permission errors
/// and unknown OS failures are treated as live. Other platforms conservatively
/// keep every positive-PID lease live until its owner explicitly releases it.
#[cfg(unix)]
fn index_builder_process_is_live(owner_pid: i64) -> bool {
    use std::os::raw::c_int;

    if owner_pid <= 0 || owner_pid > i64::from(c_int::MAX) {
        return false;
    }
    unsafe extern "C" {
        fn kill(pid: c_int, signal: c_int) -> c_int;
    }
    let result = unsafe { kill(owner_pid as c_int, 0) };
    if result == 0 {
        return true;
    }
    // POSIX ESRCH is 3. EPERM and every unknown error stay fail-closed/live.
    std::io::Error::last_os_error().raw_os_error() != Some(3)
}

#[cfg(not(unix))]
fn index_builder_process_is_live(owner_pid: i64) -> bool {
    owner_pid > 0
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

fn record_schema_migration(conn: &Connection, version: &str) -> Result<(), QghError> {
    conn.execute(
        "INSERT INTO schema_migrations (version, applied_at)
         VALUES (?1, ?2)
         ON CONFLICT(version) DO NOTHING",
        params![version, now_rfc3339()],
    )?;
    Ok(())
}

fn finish_schema_migration(tx: Transaction<'_>, version: &str) -> Result<(), QghError> {
    record_schema_migration(&tx, version)?;
    tx.commit()?;
    Ok(())
}

fn generation_vector_table_name(dimension: usize) -> String {
    format!("embedding_generation_vectors_d{dimension}")
}

fn validate_generation_vector_mapping_ownership(
    mapping_id: i64,
    stored_dimension: i64,
    stored_table: &str,
    vector_rowid: i64,
    generation_dimension: usize,
) -> Result<String, QghError> {
    let expected_dimension = i64::try_from(generation_dimension)
        .ok()
        .filter(|dimension| *dimension > 0)
        .ok_or_else(embedding_generation_corrupt_error)?;
    let expected_table = generation_vector_table_name(generation_dimension);
    if mapping_id <= 0
        || vector_rowid != mapping_id
        || stored_dimension != expected_dimension
        || stored_table != expected_table
    {
        return Err(embedding_generation_corrupt_error());
    }
    Ok(expected_table)
}

fn validate_purge_generation_vector_mapping_ownership(
    conn: &Connection,
    generation_ids: &BTreeSet<i64>,
) -> Result<BTreeMap<i64, Vec<(String, usize, i64)>>, QghError> {
    let mut validated = BTreeMap::new();
    for generation_id in generation_ids {
        let stored_generation_dimension: i64 = conn.query_row(
            "SELECT output_dimension FROM embedding_generations WHERE id = ?1",
            params![generation_id],
            |row| row.get(0),
        )?;
        let generation_dimension = usize::try_from(stored_generation_dimension)
            .ok()
            .filter(|dimension| *dimension > 0)
            .ok_or_else(embedding_generation_corrupt_error)?;
        let generation_chunk_count: i64 = conn.query_row(
            "SELECT count(*) FROM embedding_generation_chunks WHERE generation_id = ?1",
            params![generation_id],
            |row| row.get(0),
        )?;
        let mappings = conn
            .prepare(
                "SELECT m.id, m.chunk_id, m.dimension, m.vector_table, m.vector_rowid,
                        EXISTS(
                            SELECT 1 FROM embedding_generation_chunks gc
                            WHERE gc.generation_id = m.generation_id
                              AND gc.chunk_id = m.chunk_id
                        )
                 FROM embedding_generation_vector_rows m
                 WHERE m.generation_id = ?1
                 ORDER BY m.id",
            )?
            .query_map(params![generation_id], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, bool>(5)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        if i64::try_from(mappings.len()).ok() != Some(generation_chunk_count) {
            return Err(embedding_generation_corrupt_error());
        }
        let mut owned = Vec::with_capacity(mappings.len());
        for (mapping_id, _chunk_id, dimension, table, rowid, owns_chunk) in mappings {
            if !owns_chunk {
                return Err(embedding_generation_corrupt_error());
            }
            let owned_table = validate_generation_vector_mapping_ownership(
                mapping_id,
                dimension,
                &table,
                rowid,
                generation_dimension,
            )?;
            #[cfg(feature = "vector-search")]
            {
                if !generation_vector_table_schema_matches(
                    conn,
                    &owned_table,
                    generation_dimension,
                )? || !conn.query_row(
                    &format!("SELECT EXISTS(SELECT 1 FROM {owned_table} WHERE rowid = ?1)"),
                    params![rowid],
                    |row| row.get::<_, bool>(0),
                )? {
                    return Err(embedding_generation_corrupt_error());
                }
            }
            #[cfg(not(feature = "vector-search"))]
            validate_vec0_shadow_row_ownership(conn, &owned_table, generation_dimension, rowid)?;
            owned.push((owned_table, generation_dimension, rowid));
        }
        validated.insert(*generation_id, owned);
    }
    Ok(validated)
}

fn embedding_generation_content_rows_valid(
    conn: &Connection,
    generation_id: i64,
    dimension: usize,
    total_chunks: i64,
    model_manifest_hash: &str,
    chunker_fingerprint: &str,
    context_template_version: &str,
) -> Result<bool, QghError> {
    let mut stmt = conn.prepare(
        "SELECT gc.vector_blob, gc.vector_checksum, gc.vector_dimension,
                gc.source_version_id, gc.source_version_hash, gc.context_hash,
                sv.body_hash, coalesce(im.latest_version_id, cm.latest_version_id), c.body,
                se.entity_type, se.host, se.repo,
                im.issue_number, cm.issue_number, im.title, cm.parent_issue_title
         FROM embedding_generation_chunks gc
         JOIN source_versions sv ON sv.id = gc.source_version_id
         JOIN chunks c ON c.id = gc.chunk_id
         JOIN source_entities se ON se.source_id = c.source_id
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
            row.get::<_, String>(9)?,
            row.get::<_, String>(10)?,
            row.get::<_, String>(11)?,
            row.get::<_, Option<i64>>(12)?,
            row.get::<_, Option<i64>>(13)?,
            row.get::<_, Option<String>>(14)?,
            row.get::<_, Option<String>>(15)?,
        ))
    })?;
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
            chunk_body,
            entity_type,
            host,
            repo,
            issue_number,
            parent_issue_number,
            issue_title,
            parent_issue_title,
        ) = row?;
        let repository = format!("{host}/{repo}");
        let prepared_input = match entity_type.as_str() {
            "issue" => issue_number
                .zip(issue_title.as_deref())
                .map(|(number, title)| {
                    prepare_embedding_input(
                        EmbeddingSourceContext::Issue {
                            repository: &repository,
                            issue_number: number,
                            title,
                        },
                        &chunk_body,
                    )
                }),
            "issue_comment" => {
                parent_issue_number
                    .zip(parent_issue_title.as_deref())
                    .map(|(number, title)| {
                        prepare_embedding_input(
                            EmbeddingSourceContext::Comment {
                                repository: &repository,
                                parent_issue_number: number,
                                parent_issue_title: title,
                            },
                            &chunk_body,
                        )
                    })
            }
            _ => None,
        };
        if stored_dimension != dimension
            || decode_embedding_blob(&bytes, dimension).is_err()
            || embedding_blob_checksum(&bytes) != checksum
            || source_hash != body_hash
            || latest_version_id != Some(source_version_id)
            || prepared_input.as_ref().is_none_or(|prepared| {
                context_hash != prepared.context_hash(model_manifest_hash, chunker_fingerprint)
                    || prepared.context_template_version() != context_template_version
            })
        {
            return Ok(false);
        }
    }
    Ok(validated_rows == total_chunks)
}

fn validate_embedding_generation_vector_artifacts(
    conn: &Connection,
    generation_id: i64,
    generation_dimension: usize,
    total_chunks: i64,
) -> Result<(), QghError> {
    if total_chunks < 0 {
        return Err(embedding_generation_corrupt_error());
    }
    let chunk_count: i64 = conn.query_row(
        "SELECT count(*) FROM embedding_generation_chunks WHERE generation_id = ?1",
        params![generation_id],
        |row| row.get(0),
    )?;
    let mapping_count: i64 = conn.query_row(
        "SELECT count(*) FROM embedding_generation_vector_rows WHERE generation_id = ?1",
        params![generation_id],
        |row| row.get(0),
    )?;
    if chunk_count != total_chunks || mapping_count != total_chunks {
        return Err(embedding_generation_corrupt_error());
    }
    if total_chunks == 0 {
        return Ok(());
    }
    let expected_table = generation_vector_table_name(generation_dimension);
    if !generation_vector_table_schema_matches(conn, &expected_table, generation_dimension)? {
        return Err(embedding_generation_corrupt_error());
    }
    let mapping_sql = format!(
        "SELECT gc.vector_blob, gc.vector_checksum, gc.vector_dimension,
                m.id, m.dimension, m.vector_table, m.vector_rowid, v.embedding
         FROM embedding_generation_chunks gc
         LEFT JOIN embedding_generation_vector_rows m
           ON m.generation_id = gc.generation_id AND m.chunk_id = gc.chunk_id
         LEFT JOIN {expected_table} v ON v.rowid = m.vector_rowid
         WHERE gc.generation_id = ?1
         ORDER BY gc.chunk_id"
    );
    let mappings = conn
        .prepare(&mapping_sql)?
        .query_map(params![generation_id], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<i64>>(6)?,
                row.get::<_, Option<Vec<u8>>>(7)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if i64::try_from(mappings.len()).ok() != Some(total_chunks) {
        return Err(embedding_generation_corrupt_error());
    }
    for (
        expected_blob,
        expected_checksum,
        blob_dimension,
        mapping_id,
        stored_dimension,
        stored_table,
        vector_rowid,
        indexed_blob,
    ) in mappings
    {
        if usize::try_from(blob_dimension).ok() != Some(generation_dimension)
            || decode_embedding_blob(&expected_blob, generation_dimension).is_err()
            || embedding_blob_checksum(&expected_blob) != expected_checksum
        {
            return Err(embedding_generation_corrupt_error());
        }
        let (Some(mapping_id), Some(stored_dimension), Some(stored_table), Some(vector_rowid)) =
            (mapping_id, stored_dimension, stored_table, vector_rowid)
        else {
            return Err(embedding_generation_corrupt_error());
        };
        let owned_table = validate_generation_vector_mapping_ownership(
            mapping_id,
            stored_dimension,
            &stored_table,
            vector_rowid,
            generation_dimension,
        )?;
        debug_assert_eq!(owned_table, expected_table);
        if indexed_blob.as_deref() != Some(expected_blob.as_slice()) {
            return Err(embedding_generation_corrupt_error());
        }
    }
    Ok(())
}

fn embedding_generation_cleanup_protected(
    conn: &Connection,
    generation_id: i64,
    previous_ready_before: &str,
) -> Result<bool, QghError> {
    let active: bool = conn.query_row(
        "SELECT EXISTS(
             SELECT 1
             FROM retrieval_publication_pointer p
             JOIN retrieval_publications rp ON rp.publication_id = p.publication_id
             WHERE p.id = 1 AND rp.embedding_generation_id = ?1
         )",
        params![generation_id],
        |row| row.get(0),
    )?;
    if active {
        return Ok(true);
    }
    let previous = conn
        .query_row(
            "SELECT rp.embedding_generation_id
             FROM retrieval_publications rp
             JOIN embedding_generations eg ON eg.id = rp.embedding_generation_id
             WHERE rp.embedding_generation_id IS NOT NULL
               AND rp.publication_id != coalesce(
                   (SELECT publication_id FROM retrieval_publication_pointer WHERE id = 1),
                   -1
               )
               AND eg.created_at >= ?1
             ORDER BY rp.created_at DESC, rp.publication_id DESC
             LIMIT 1",
            params![previous_ready_before],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    Ok(previous == Some(generation_id))
}

fn generation_vector_table_schema_matches(
    conn: &Connection,
    expected_table: &str,
    expected_dimension: usize,
) -> Result<bool, QghError> {
    let sql = conn
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = ?1",
            params![expected_table],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let Some(sql) = sql else {
        return Ok(false);
    };
    let lower = sql.to_ascii_lowercase();
    let Some(start) = lower.find("float[") else {
        return Ok(false);
    };
    let dimension_start = start + "float[".len();
    let Some(end) = lower[dimension_start..].find(']') else {
        return Ok(false);
    };
    Ok(lower.contains("using vec0")
        && lower[dimension_start..dimension_start + end]
            .parse::<usize>()
            .ok()
            == Some(expected_dimension))
}

#[cfg(not(feature = "vector-search"))]
fn vec0_shadow_payload_exists(conn: &Connection) -> Result<bool, QghError> {
    for table in [
        CHUNK_EMBEDDING_VECTOR_CHUNKS_META_TABLE,
        CHUNK_EMBEDDING_VECTOR_ROWIDS_TABLE,
        CHUNK_EMBEDDING_VECTOR_CHUNKS_TABLE,
    ] {
        if table_exists(conn, table)? {
            let sql = format!("SELECT 1 FROM \"{table}\" LIMIT 1");
            if conn.query_row(&sql, [], |_| Ok(())).optional()?.is_some() {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

#[cfg(not(feature = "vector-search"))]
fn validate_vec0_shadow_row_ownership(
    conn: &Connection,
    base: &str,
    dimension: usize,
    vector_rowid: i64,
) -> Result<(), QghError> {
    if base != generation_vector_table_name(dimension) || !is_qgh_generation_vector_table(base) {
        return Err(purge_error());
    }
    let chunks_table = format!("{base}_chunks");
    let rowids_table = format!("{base}_rowids");
    let vectors_table = format!("{base}_vector_chunks00");
    for table in [&chunks_table, &rowids_table, &vectors_table] {
        if !table_exists(conn, table)? {
            return Err(purge_error());
        }
    }
    let Some((stored_id, chunk_id, chunk_offset)) = conn
        .query_row(
            &format!(
                "SELECT id, chunk_id, chunk_offset FROM \"{rowids_table}\"
                 WHERE rowid = ?1"
            ),
            params![vector_rowid],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()?
    else {
        return Err(purge_error());
    };
    let chunk_offset = usize::try_from(chunk_offset).map_err(|_| purge_error())?;
    if stored_id != vector_rowid {
        return Err(purge_error());
    }
    let (size, validity, rowids): (i64, Vec<u8>, Vec<u8>) = conn.query_row(
        &format!("SELECT size, validity, rowids FROM \"{chunks_table}\" WHERE rowid = ?1"),
        params![chunk_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let vectors: Vec<u8> = conn.query_row(
        &format!("SELECT vectors FROM \"{vectors_table}\" WHERE rowid = ?1"),
        params![chunk_id],
        |row| row.get(0),
    )?;
    let size = usize::try_from(size).map_err(|_| purge_error())?;
    let rowid_width = std::mem::size_of::<i64>();
    let vector_width = dimension
        .checked_mul(std::mem::size_of::<f32>())
        .ok_or_else(purge_error)?;
    let rowid_start = chunk_offset
        .checked_mul(rowid_width)
        .ok_or_else(purge_error)?;
    let rowid_end = rowid_start
        .checked_add(rowid_width)
        .ok_or_else(purge_error)?;
    let vector_start = chunk_offset
        .checked_mul(vector_width)
        .ok_or_else(purge_error)?;
    let vector_end = vector_start
        .checked_add(vector_width)
        .ok_or_else(purge_error)?;
    let validity_byte = chunk_offset / 8;
    let validity_mask = 1_u8 << (chunk_offset % 8);
    if chunk_offset >= size
        || validity
            .get(validity_byte)
            .is_none_or(|byte| byte & validity_mask == 0)
        || rowids.get(rowid_start..rowid_end) != Some(vector_rowid.to_ne_bytes().as_slice())
        || vectors.get(vector_start..vector_end).is_none()
    {
        return Err(purge_error());
    }
    Ok(())
}

#[cfg(not(feature = "vector-search"))]
fn delete_vec0_shadow_row(
    conn: &Connection,
    base: &str,
    dimension: usize,
    vector_rowid: i64,
) -> Result<(), QghError> {
    validate_vec0_shadow_row_ownership(conn, base, dimension, vector_rowid)?;
    if base != generation_vector_table_name(dimension) || !is_qgh_generation_vector_table(base) {
        return Err(purge_error());
    }
    let chunks_table = format!("{base}_chunks");
    let rowids_table = format!("{base}_rowids");
    let vectors_table = format!("{base}_vector_chunks00");
    for table in [&chunks_table, &rowids_table, &vectors_table] {
        if !table_exists(conn, table)? {
            return Err(purge_error());
        }
    }
    let Some((chunk_id, chunk_offset)) = conn
        .query_row(
            &format!(
                "SELECT chunk_id, chunk_offset FROM \"{rowids_table}\"
                 WHERE rowid = ?1"
            ),
            params![vector_rowid],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)? as usize)),
        )
        .optional()?
    else {
        return Err(purge_error());
    };
    let (mut validity, mut rowids): (Vec<u8>, Vec<u8>) = conn.query_row(
        &format!("SELECT validity, rowids FROM \"{chunks_table}\" WHERE rowid = ?1"),
        params![chunk_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let mut vectors: Vec<u8> = conn.query_row(
        &format!("SELECT vectors FROM \"{vectors_table}\" WHERE rowid = ?1"),
        params![chunk_id],
        |row| row.get(0),
    )?;
    let validity_index = chunk_offset / 8;
    let rowid_start = chunk_offset
        .checked_mul(std::mem::size_of::<i64>())
        .ok_or_else(purge_error)?;
    let vector_width = dimension
        .checked_mul(std::mem::size_of::<f32>())
        .ok_or_else(purge_error)?;
    let vector_start = chunk_offset
        .checked_mul(vector_width)
        .ok_or_else(purge_error)?;
    if validity_index >= validity.len()
        || rowid_start + std::mem::size_of::<i64>() > rowids.len()
        || vector_start + vector_width > vectors.len()
    {
        return Err(purge_error());
    }
    if rowids[rowid_start..rowid_start + std::mem::size_of::<i64>()] != vector_rowid.to_ne_bytes() {
        return Err(purge_error());
    }
    validity[validity_index] &= !(1_u8 << (chunk_offset % 8));
    rowids[rowid_start..rowid_start + std::mem::size_of::<i64>()].fill(0);
    vectors[vector_start..vector_start + vector_width].fill(0);
    if validity.iter().all(|byte| *byte == 0) {
        conn.execute(
            &format!("DELETE FROM \"{chunks_table}\" WHERE rowid = ?1"),
            params![chunk_id],
        )?;
        conn.execute(
            &format!("DELETE FROM \"{vectors_table}\" WHERE rowid = ?1"),
            params![chunk_id],
        )?;
    } else {
        conn.execute(
            &format!("UPDATE \"{chunks_table}\" SET validity = ?2, rowids = ?3 WHERE rowid = ?1"),
            params![chunk_id, validity, rowids],
        )?;
        conn.execute(
            &format!("UPDATE \"{vectors_table}\" SET vectors = ?2 WHERE rowid = ?1"),
            params![chunk_id, vectors],
        )?;
    }
    conn.execute(
        &format!("DELETE FROM \"{rowids_table}\" WHERE rowid = ?1"),
        params![vector_rowid],
    )?;
    Ok(())
}

#[cfg(not(feature = "vector-search"))]
fn clear_vec0_shadow_payload_for_base(conn: &Connection, base: &str) -> Result<(), QghError> {
    if base != CHUNK_EMBEDDING_VECTORS_TABLE && !is_qgh_generation_vector_table(base) {
        return Err(purge_error());
    }
    for suffix in ["_chunks", "_rowids", "_vector_chunks00"] {
        let table = format!("{base}{suffix}");
        if table_exists(conn, &table)? {
            conn.execute(&format!("DELETE FROM \"{table}\""), [])?;
        }
    }
    Ok(())
}

#[cfg(not(feature = "vector-search"))]
fn is_qgh_generation_vector_table(table: &str) -> bool {
    table
        .strip_prefix("embedding_generation_vectors_d")
        .is_some_and(|dimension| {
            !dimension.is_empty() && dimension.bytes().all(|byte| byte.is_ascii_digit())
        })
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

fn apply_pending_purge_guard(
    tx: &rusqlite::Transaction<'_>,
    source_id: &str,
    repo: &str,
    issue_number: i64,
) -> Result<(), rusqlite::Error> {
    let pending = tx
        .query_row(
            "SELECT 1 FROM purge_requests
             WHERE purge_pending = 1
               AND ((target_kind = 'source' AND target_value = ?1)
                 OR (target_kind = 'repository' AND lower(target_value) = lower(?2))
                 OR (target_kind = 'issue'
                     AND lower(target_value) = lower(?2 || '#' || ?3)))
             LIMIT 1",
            params![source_id, repo, issue_number],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if pending {
        tx.execute(
            "UPDATE source_entities SET lifecycle_state = 'purge_pending'
             WHERE source_id = ?1",
            params![source_id],
        )?;
        tx.execute(
            "UPDATE source_versions SET lifecycle_state = 'purge_pending'
             WHERE source_id = ?1",
            params![source_id],
        )?;
    }
    Ok(())
}

fn read_content_write_epoch(conn: &Connection) -> Result<i64, QghError> {
    conn.query_row(
        "SELECT CAST(value AS INTEGER) FROM profile_meta
         WHERE key = 'content_write_epoch'",
        [],
        |row| row.get(0),
    )
    .map_err(QghError::from)
}

fn read_source_snapshot_epoch(conn: &Connection) -> Result<i64, QghError> {
    conn.query_row(
        "SELECT CAST(value AS INTEGER) FROM profile_meta
         WHERE key = 'source_snapshot_epoch'",
        [],
        |row| row.get(0),
    )
    .map_err(QghError::from)
}

fn bump_source_snapshot_epoch(conn: &Connection) -> Result<i64, QghError> {
    conn.execute(
        "UPDATE profile_meta
         SET value = CAST(value AS INTEGER) + 1
         WHERE key = 'source_snapshot_epoch'",
        [],
    )?;
    read_source_snapshot_epoch(conn)
}

fn source_snapshot_identity_hash(identity: &SourceSnapshotIdentity) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"qgh.source_snapshot.v1");
    hasher.update([0]);
    hasher.update(identity.sync_run_id.as_bytes());
    hasher.update([0]);
    hasher.update(identity.epoch.to_le_bytes());
    digest_hex(hasher)
}

fn embedding_inventory_hash(chunks: &[ContextualEmbeddingChunk]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"qgh.embedding_inventory.v1");
    hasher.update((chunks.len() as u64).to_le_bytes());
    for contextual in chunks {
        let chunk = &contextual.chunk;
        hasher.update(chunk.chunk_id.to_le_bytes());
        hash_text(&mut hasher, &chunk.source_id);
        hasher.update(chunk.source_version_id.to_le_bytes());
        hasher.update((chunk.chunk_index as u64).to_le_bytes());
        hash_text(&mut hasher, &chunk.chunker_version);
        hash_text(&mut hasher, &chunk.chunker_fingerprint);
        hash_text(&mut hasher, contextual.prepared_input.as_str());
        hash_text(
            &mut hasher,
            contextual.prepared_input.context_template_version(),
        );
    }
    digest_hex(hasher)
}

fn hash_text(hasher: &mut Sha256, value: &str) {
    hasher.update((value.len() as u64).to_le_bytes());
    hasher.update(value.as_bytes());
}

fn digest_hex(hasher: Sha256) -> String {
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn read_successor_repair_required(conn: &Connection) -> Result<bool, QghError> {
    let value = conn
        .query_row(
            "SELECT value FROM profile_meta WHERE key = 'successor_repair_required'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    match value.as_deref() {
        Some("0") => Ok(false),
        Some("1") => Ok(true),
        _ => Err(QghError::new(
            "purge.successor_repair_state_invalid",
            "The durable successor repair state is missing or invalid.",
            6,
        )),
    }
}

fn mark_successor_repair_required(
    conn: &Connection,
    content_write_epoch: i64,
) -> Result<(), QghError> {
    for (key, value) in [
        ("successor_repair_required", "1".to_string()),
        (
            "successor_repair_requested_epoch",
            content_write_epoch.to_string(),
        ),
        ("successor_repair_reason", "purge".to_string()),
    ] {
        conn.execute(
            "INSERT INTO profile_meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
    }
    Ok(())
}

fn clear_successor_repair_required(conn: &Connection) -> Result<(), QghError> {
    conn.execute(
        "UPDATE profile_meta SET value = '0'
         WHERE key = 'successor_repair_required'",
        [],
    )?;
    conn.execute(
        "DELETE FROM profile_meta
         WHERE key IN ('successor_repair_requested_epoch', 'successor_repair_reason')",
        [],
    )?;
    Ok(())
}

fn ensure_content_write_allowed(conn: &Connection, expected_epoch: i64) -> Result<(), QghError> {
    let current_epoch = read_content_write_epoch(conn)?;
    let pending_count: i64 = conn.query_row(
        "SELECT count(*) FROM purge_requests WHERE purge_pending = 1",
        [],
        |row| row.get(0),
    )?;
    if current_epoch != expected_epoch || pending_count != 0 {
        return Err(write_fence_error());
    }
    Ok(())
}

fn detach_incomplete_publication_snapshots(conn: &Connection) -> Result<(), QghError> {
    if table_exists(conn, "embedding_generations")? {
        conn.execute(
            "UPDATE embedding_generations
             SET state = 'ready'
             WHERE state = 'active'
               AND id IN (
                   SELECT rp.embedding_generation_id
                   FROM retrieval_publication_pointer p
                   JOIN retrieval_publications rp
                     ON rp.publication_id = p.publication_id
                   LEFT JOIN index_generations ig
                     ON ig.generation = rp.tantivy_generation
                   WHERE p.id = 1
                     AND rp.embedding_generation_id IS NOT NULL
                     AND (
                         rp.source_snapshot_epoch IS NULL
                         OR ig.generation IS NULL
                         OR ig.source_snapshot_sync_run_id IS NULL
                         OR ig.source_snapshot_epoch IS NULL
                         OR ig.source_inventory_hash IS NULL
                     )
               )",
            [],
        )?;
    }
    conn.execute_batch(
        "DELETE FROM retrieval_publication_pointer
         WHERE id = 1
           AND EXISTS (
               SELECT 1
               FROM retrieval_publications rp
               LEFT JOIN index_generations ig
                 ON ig.generation = rp.tantivy_generation
               WHERE rp.publication_id = retrieval_publication_pointer.publication_id
                 AND (
                     rp.source_snapshot_epoch IS NULL
                     OR ig.generation IS NULL
                     OR ig.source_snapshot_sync_run_id IS NULL
                     OR ig.source_snapshot_epoch IS NULL
                     OR ig.source_inventory_hash IS NULL
                 )
           );
         UPDATE retrieval_publications
         SET active = 0
         WHERE active = 1
           AND (
               source_snapshot_epoch IS NULL
               OR EXISTS (
                   SELECT 1
                   FROM index_generations ig
                   WHERE ig.generation = retrieval_publications.tantivy_generation
                     AND (
                         ig.source_snapshot_sync_run_id IS NULL
                         OR ig.source_snapshot_epoch IS NULL
                         OR ig.source_inventory_hash IS NULL
                     )
               )
               OR NOT EXISTS (
                   SELECT 1
                   FROM index_generations ig
                   WHERE ig.generation = retrieval_publications.tantivy_generation
               )
           );
         UPDATE index_generations
         SET active = 0
         WHERE active = 1
           AND (
               source_snapshot_sync_run_id IS NULL
               OR source_snapshot_epoch IS NULL
               OR source_inventory_hash IS NULL
           );",
    )?;
    Ok(())
}

fn write_fence_error() -> QghError {
    QghError::new(
        "purge.write_fenced",
        "Content write was fenced by lifecycle purge state.",
        6,
    )
}

fn incomplete_source_snapshot_error() -> QghError {
    QghError::new(
        "publication.source_snapshot_incomplete",
        "A retrieval build requires a completed source snapshot at the current source epoch.",
        6,
    )
}

fn changed_source_snapshot_error() -> QghError {
    QghError::new(
        "publication.source_snapshot_changed",
        "The authoritative source snapshot changed before retrieval publication activation.",
        6,
    )
}

fn source_inventory_mismatch_error() -> QghError {
    QghError::new(
        "publication.source_inventory_mismatch",
        "The retrieval source inventory does not match its captured snapshot.",
        6,
    )
}

fn embedding_inventory_mismatch_error() -> QghError {
    QghError::new(
        "embedding.generation_inventory_mismatch",
        "The embedding generation does not match the authoritative contextual chunk inventory.",
        6,
    )
}

fn embedding_generation_corrupt_error() -> QghError {
    QghError::validation(
        "embedding.generation_corrupt",
        "The embedding generation artifacts are incomplete or inconsistent.",
    )
}

fn mark_embedding_generation_failed(
    conn: &Connection,
    generation_id: i64,
    failure_code: &str,
) -> Result<(), QghError> {
    conn.execute(
        "UPDATE embedding_generations
         SET state = 'failed', failure_code = ?2, updated_at = ?3
         WHERE id = ?1 AND state = 'building'",
        params![generation_id, failure_code, now_rfc3339()],
    )?;
    Ok(())
}

fn embedding_snapshot_mismatch_error() -> QghError {
    QghError::new(
        "publication.embedding_snapshot_mismatch",
        "Embedding and lexical generations do not share one validated source snapshot.",
        6,
    )
}

fn read_fence_error() -> QghError {
    QghError::new(
        "purge.read_fenced",
        "Loaded content was fenced by lifecycle purge state.",
        6,
    )
}

fn allowlist_reconciliation_required_error() -> QghError {
    QghError::new(
        "purge.allowlist_reconciliation_required",
        "Stored repository state no longer matches the configured profile allowlist.",
        6,
    )
    .with_hint("Run qgh sync to reconcile removed repositories before reading this profile.")
}

fn invalidate_publications_for_pending_purge(conn: &Connection) -> Result<(), QghError> {
    conn.execute("DELETE FROM retrieval_publication_pointer", [])?;
    conn.execute("UPDATE retrieval_publications SET active = 0", [])?;
    conn.execute("UPDATE index_generations SET active = 0", [])?;
    if table_exists(conn, "embedding_generations")? {
        conn.execute(
            "UPDATE embedding_generations SET state = 'ready'
             WHERE state = 'active'",
            [],
        )?;
    }
    Ok(())
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
    if let Some((version_id, stored_github_updated_at, stored_lifecycle_state)) = tx
        .query_row(
            "SELECT id, github_updated_at, lifecycle_state FROM source_versions
             WHERE source_id = ?1 AND body_hash = ?2
             ORDER BY id DESC
             LIMIT 1",
            params![source_id, body_hash],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?
    {
        if stored_github_updated_at == github_updated_at && stored_lifecycle_state == "active" {
            return Ok(version_id);
        }
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

fn authoritative_issue_matches(
    conn: &Connection,
    issue: &IssueRecord,
    canonical_repo: &str,
) -> Result<bool, QghError> {
    let labels_json = serde_json::to_string(&issue.labels)
        .map_err(|_| QghError::storage("Failed to compare issue metadata."))?;
    let assignees_json = serde_json::to_string(&issue.assignees)
        .map_err(|_| QghError::storage("Failed to compare issue metadata."))?;
    conn.query_row(
        "SELECT EXISTS(
             SELECT 1
             FROM source_entities se
             JOIN issue_metadata im ON im.source_id = se.source_id
             JOIN source_versions sv ON sv.id = im.latest_version_id
             WHERE se.source_id = ?1
               AND se.entity_type = 'issue'
               AND se.host = ?2 AND se.repo = ?3
               AND se.node_id = ?4 AND se.github_id = ?5
               AND se.lifecycle_state = 'active'
               AND se.created_at = ?6 AND se.updated_at = ?7
               AND im.repo = ?3 AND im.issue_number = ?8
               AND im.title = ?9 AND im.body = ?10 AND im.state = ?11
               AND im.labels_json = ?12 AND im.milestone IS ?13
               AND im.assignees_json = ?14 AND im.author IS ?15
               AND im.created_at = ?6 AND im.updated_at = ?7
               AND im.closed_at IS ?16 AND im.canonical_url = ?17
               AND sv.body_hash = ?18 AND sv.github_updated_at = ?7
         )",
        params![
            issue.source_id,
            issue.host,
            canonical_repo,
            issue.node_id,
            issue.github_id,
            issue.created_at,
            issue.updated_at,
            issue.number,
            issue.title,
            issue.body,
            issue.state,
            labels_json,
            issue.milestone,
            assignees_json,
            issue.author,
            issue.closed_at,
            issue.canonical_url,
            issue.body_hash,
        ],
        |row| row.get(0),
    )
    .map_err(QghError::from)
}

fn authoritative_comment_matches(
    conn: &Connection,
    comment: &CommentRecord,
    canonical_repo: &str,
) -> Result<bool, QghError> {
    conn.query_row(
        "SELECT EXISTS(
             SELECT 1
             FROM source_entities se
             JOIN comment_metadata cm ON cm.source_id = se.source_id
             JOIN source_versions sv ON sv.id = cm.latest_version_id
             WHERE se.source_id = ?1
               AND se.entity_type = 'issue_comment'
               AND se.host = ?2 AND se.repo = ?3
               AND se.node_id = ?4 AND se.github_id = ?5
               AND se.lifecycle_state = 'active'
               AND se.created_at = ?6 AND se.updated_at = ?7
               AND cm.repo = ?3 AND cm.issue_number = ?8
               AND cm.body = ?9 AND cm.author IS ?10
               AND cm.created_at = ?6 AND cm.updated_at = ?7
               AND cm.canonical_url = ?11
               AND cm.parent_issue_source_id = ?12
               AND cm.parent_issue_title = ?13
               AND cm.parent_issue_canonical_url = ?14
               AND sv.body_hash = ?15 AND sv.github_updated_at = ?7
         )",
        params![
            comment.source_id,
            comment.host,
            canonical_repo,
            comment.node_id,
            comment.github_id,
            comment.created_at,
            comment.updated_at,
            comment.parent_issue_number,
            comment.body,
            comment.author,
            comment.canonical_url,
            comment.parent_issue_source_id,
            comment.parent_issue_title,
            comment.parent_issue_canonical_url,
            comment.body_hash,
        ],
        |row| row.get(0),
    )
    .map_err(QghError::from)
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

fn stored_chunks_match(
    conn: &Connection,
    source_version_id: i64,
    expected: &[MarkdownChunk],
) -> Result<bool, QghError> {
    let mut stmt = conn.prepare(
        "SELECT id, source_id, source_version_id, body, chunk_index, token_start,
                token_end, byte_start, byte_end, chunker_version,
                coalesce(chunker_fingerprint, ''), heading_path_json
         FROM chunks WHERE source_version_id = ?1 ORDER BY chunk_index, id",
    )?;
    let stored = stmt
        .query_map(params![source_version_id], stored_chunk_from_row)?
        .collect::<Result<Vec<_>, _>>()?;
    if stored.len() != expected.len() {
        return Ok(false);
    }
    Ok(stored.iter().zip(expected).all(|(stored, expected)| {
        stored.body == expected.body
            && stored.chunk_index == expected.chunk_index
            && stored.token_start == expected.token_start
            && stored.token_end == expected.token_end
            && stored.byte_start == expected.byte_start
            && stored.byte_end == expected.byte_end
            && stored.chunker_version == expected.chunker_version
            && stored.chunker_fingerprint == expected.chunker_fingerprint
            && stored.heading_path == expected.heading_path
    }))
}

fn contextual_embedding_chunk_from_row(
    row: &rusqlite::Row<'_>,
) -> Result<ContextualEmbeddingChunk, QghError> {
    let chunk = stored_chunk_from_row(row)?;
    let entity_type: String = row.get(12)?;
    let host: String = row.get(13)?;
    let repo: String = row.get(14)?;
    let issue_number: i64 = row.get(15)?;
    let issue_title: Option<String> = row.get(16)?;
    let parent_issue_title: Option<String> = row.get(17)?;
    let repository = format!("{host}/{repo}");
    let context = match entity_type.as_str() {
        "issue" => EmbeddingSourceContext::Issue {
            repository: &repository,
            issue_number,
            title: issue_title.as_deref().ok_or_else(|| {
                QghError::storage("Active issue embedding metadata is incomplete.")
            })?,
        },
        "issue_comment" => EmbeddingSourceContext::Comment {
            repository: &repository,
            parent_issue_number: issue_number,
            parent_issue_title: parent_issue_title.as_deref().ok_or_else(|| {
                QghError::storage("Active comment embedding metadata is incomplete.")
            })?,
        },
        _ => {
            return Err(QghError::storage(
                "Active embedding source has an unsupported entity type.",
            ));
        }
    };
    let prepared_input = prepare_embedding_input(context, &chunk.body);
    Ok(ContextualEmbeddingChunk {
        chunk,
        prepared_input,
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
    fn purge_status_labels_are_stable_and_content_free() {
        assert_eq!(
            PurgeTarget::Issue {
                repo: "owner/repo".to_string(),
                issue_number: 47,
            }
            .kind(),
            "issue"
        );
        assert_eq!(
            PurgeTrigger::ConfirmedTombstone.as_str(),
            "confirmed_tombstone"
        );
        assert_eq!(PurgeFailureStage::WalCheckpoint.as_str(), "wal_checkpoint");
    }

    #[test]
    fn fresh_empty_profile_does_not_require_successor_repair_and_migrates_missing_marker() {
        let paths = temp_profile_paths("successor-repair-fresh-empty");
        let mut store = Store::open(&paths).unwrap();
        assert!(!store.successor_repair_required().unwrap());
        assert!(store.capture_retrieval_build_snapshot().unwrap().is_none());
        store.validate_query_publication_snapshot(None).unwrap();
        assert_eq!(store.resolve_active_tantivy_artifact().unwrap(), None);
        assert!(store.record_purge_successor_snapshot().unwrap().is_none());
        assert!(store.latest_successful_sync_run_id().unwrap().is_none());
        store
            .conn
            .execute(
                "DELETE FROM profile_meta
                 WHERE key IN ('successor_repair_required', 'source_snapshot_epoch')",
                [],
            )
            .unwrap();
        drop(store);

        let mut reopened = Store::open(&paths).unwrap();
        assert!(!reopened.successor_repair_required().unwrap());
        assert!(reopened
            .capture_retrieval_build_snapshot()
            .unwrap()
            .is_none());
        reopened.validate_query_publication_snapshot(None).unwrap();
        assert_eq!(reopened.resolve_active_tantivy_artifact().unwrap(), None);
        assert!(reopened
            .record_purge_successor_snapshot()
            .unwrap()
            .is_none());
        assert!(reopened.latest_successful_sync_run_id().unwrap().is_none());

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn empty_purge_requires_durable_successor_until_successful_activation() {
        let paths = temp_profile_paths("successor-repair-empty-purge");
        let mut store = Store::open(&paths).unwrap();
        let target = PurgeTarget::Repository {
            repo: "owner/empty".to_string(),
        };

        store
            .purge(target.clone(), PurgeTrigger::AllowlistRemoval)
            .unwrap();
        assert!(store.successor_repair_required().unwrap());
        drop(store);

        let mut reopened = Store::open(&paths).unwrap();
        assert!(reopened.successor_repair_required().unwrap());
        let snapshot = reopened
            .record_purge_successor_snapshot()
            .unwrap()
            .expect("post-purge snapshot");
        assert_eq!(
            reopened.record_purge_successor_snapshot().unwrap(),
            Some(snapshot.clone())
        );
        let persisted: (String, i64, i64) = reopened
            .conn
            .query_row(
                "SELECT snapshot_kind, content_write_epoch, completed_successfully
                 FROM sync_runs WHERE id = ?1",
                params![snapshot],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(persisted.0, "purge_successor");
        assert_eq!(persisted.1, reopened.content_write_epoch);
        assert_eq!(persisted.2, 1);

        let (reserved_only, _) = reopened
            .reserve_index_generation(&paths.index_root, 0)
            .unwrap();
        let error = reopened
            .activate_retrieval_publication(&snapshot, reserved_only, None, None)
            .unwrap_err();
        assert_eq!(error.code, "publication.tantivy_artifact_not_ready");
        assert!(reopened.successor_repair_required().unwrap());

        let (deleted_generation, _) = reopened
            .reserve_index_generation(&paths.index_root, 0)
            .unwrap();
        let deleted_path = reopened
            .rebuild_reserved_index_generation(deleted_generation, &[])
            .unwrap();
        fs::remove_dir_all(deleted_path).unwrap();
        let error = reopened
            .activate_retrieval_publication(&snapshot, deleted_generation, None, None)
            .unwrap_err();
        assert_eq!(error.code, "publication.tantivy_artifact_not_ready");
        assert!(reopened.successor_repair_required().unwrap());

        let (generation, _) = reopened
            .reserve_index_generation(&paths.index_root, 0)
            .unwrap();
        reopened
            .rebuild_reserved_index_generation(generation, &[])
            .unwrap();
        reopened
            .activate_retrieval_publication(&snapshot, generation, None, None)
            .unwrap();
        assert!(!reopened.successor_repair_required().unwrap());

        assert_eq!(
            reopened
                .queue_purges(&[(target, PurgeTrigger::AllowlistRemoval)])
                .unwrap(),
            0
        );
        assert!(!reopened.successor_repair_required().unwrap());

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn purge_successor_rejects_pending_old_and_stale_epoch_snapshots() {
        let paths = temp_profile_paths("successor-repair-snapshot-fences");
        let mut store = Store::open(&paths).unwrap();
        let old_sync_run_id = "sync-old-remote";
        store
            .upsert_sources_for_run(old_sync_run_id, &[], &[], 0, &[])
            .unwrap();
        store.mark_sync_run_completed(old_sync_run_id).unwrap();
        let first_target = PurgeTarget::Repository {
            repo: "owner/first-empty".to_string(),
        };
        store
            .queue_purges(&[(first_target, PurgeTrigger::AllowlistRemoval)])
            .unwrap();

        let pending = store.record_purge_successor_snapshot().unwrap_err();
        assert_eq!(pending.code, "purge.successor_snapshot_pending");
        store.retry_pending_purges().unwrap();
        let first_snapshot = store
            .record_purge_successor_snapshot()
            .unwrap()
            .expect("first lifecycle snapshot");
        assert_ne!(first_snapshot, old_sync_run_id);

        let (old_generation, _) = store
            .reserve_index_generation(&paths.index_root, 0)
            .unwrap();
        store
            .rebuild_reserved_index_generation(old_generation, &[])
            .unwrap();
        let old_error = store
            .activate_retrieval_publication(old_sync_run_id, old_generation, None, None)
            .unwrap_err();
        assert_eq!(old_error.code, "publication.source_snapshot_changed");
        assert!(store.successor_repair_required().unwrap());

        store
            .purge(
                PurgeTarget::Repository {
                    repo: "owner/second-empty".to_string(),
                },
                PurgeTrigger::AllowlistRemoval,
            )
            .unwrap();
        let (stale_generation, _) = store
            .reserve_index_generation(&paths.index_root, 0)
            .unwrap();
        store
            .rebuild_reserved_index_generation(stale_generation, &[])
            .unwrap();
        let stale_error = store
            .activate_retrieval_publication(&first_snapshot, stale_generation, None, None)
            .unwrap_err();
        assert_eq!(stale_error.code, "publication.source_snapshot_changed");
        assert!(store.successor_repair_required().unwrap());

        let current_snapshot = store
            .record_purge_successor_snapshot()
            .unwrap()
            .expect("current lifecycle snapshot");
        assert_ne!(current_snapshot, first_snapshot);

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn purge_successor_snapshot_does_not_advance_remote_freshness() {
        let paths = temp_profile_paths("successor-repair-remote-freshness");
        let mut store = Store::open(&paths).unwrap();
        let remote_sync_run_id = "sync-remote-freshness";
        let repo = "owner/retained";
        let cursor = CursorUpdate {
            endpoint: format!("issues:{repo}"),
            cursor: Some("cursor-safe".to_string()),
            etag: None,
            not_modified: false,
        };
        store
            .upsert_sources_for_run(
                remote_sync_run_id,
                &[test_issue(
                    "qgh://github.com/issue/I_REMOTE_FRESHNESS",
                    repo,
                    "retained",
                )],
                &[],
                0,
                &[cursor],
            )
            .unwrap();
        store.mark_sync_run_completed(remote_sync_run_id).unwrap();
        let remote_completed_at = "2026-01-01T00:00:00Z".to_string();
        store
            .conn
            .execute(
                "UPDATE sync_runs SET started_at = ?1, completed_at = ?1 WHERE id = ?2",
                params![remote_completed_at, remote_sync_run_id],
            )
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE repository_sync_state SET last_successful_sync_at = ?1
                 WHERE repo = ?2",
                params![remote_completed_at, repo],
            )
            .unwrap();

        store
            .purge(
                PurgeTarget::Repository {
                    repo: "owner/empty".to_string(),
                },
                PurgeTrigger::AllowlistRemoval,
            )
            .unwrap();
        let successor = store
            .record_purge_successor_snapshot()
            .unwrap()
            .expect("lifecycle snapshot");

        assert_ne!(successor, remote_sync_run_id);
        assert_eq!(
            store.latest_successful_sync_run_id().unwrap().as_deref(),
            Some(remote_sync_run_id)
        );
        assert_eq!(
            store.status().unwrap().last_sync_at,
            Some(remote_completed_at.clone())
        );
        assert_eq!(
            store
                .oldest_successful_sync_at_for_repos(&[repo.to_string()])
                .unwrap(),
            Some(remote_completed_at.clone())
        );
        let backoff = store
            .record_backoff_state("rate_limit", repo, 60, None)
            .unwrap();
        assert_eq!(
            backoff.last_successful_sync,
            Some(remote_completed_at.clone())
        );
        let repo_sync_at: String = store
            .conn
            .query_row(
                "SELECT last_successful_sync_at FROM repository_sync_state WHERE repo = ?1",
                params![repo],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(repo_sync_at, remote_completed_at);

        store
            .conn
            .execute(
                "DELETE FROM repository_sync_state WHERE repo = ?1",
                params![repo],
            )
            .unwrap();
        drop(store);
        let reopened = Store::open(&paths).unwrap();
        let migrated_repo_sync_at: String = reopened
            .conn
            .query_row(
                "SELECT last_successful_sync_at FROM repository_sync_state WHERE repo = ?1",
                params![repo],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(migrated_repo_sync_at, remote_completed_at);

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn purge_queue_marks_every_target_pending_atomically_before_finish() {
        let paths = temp_profile_paths("purge-queue-atomic-batch");
        let mut store = Store::open(&paths).unwrap();
        let first_id = "qgh://github.com/issue/I_QUEUE_ATOMIC_FIRST";
        let second_id = "qgh://github.com/issue/I_QUEUE_ATOMIC_SECOND";
        store
            .upsert_sources_for_run(
                "sync-purge-queue-atomic",
                &[
                    test_issue(first_id, "owner/first", "queue-first-sensitive"),
                    test_issue(second_id, "owner/second", "queue-second-sensitive"),
                ],
                &[],
                0,
                &[],
            )
            .unwrap();
        seal_latest_test_sync(&mut store);
        let (generation, _) = store
            .reserve_index_generation(&paths.index_root, 2)
            .unwrap();
        rebuild_reserved_generation(&store, &paths, generation);
        store
            .activate_retrieval_publication("sync-purge-queue-atomic", generation, None, None)
            .unwrap();
        let epoch_before = read_content_write_epoch(&store.conn).unwrap();

        let queued = store
            .queue_purges(&[
                (
                    PurgeTarget::Source {
                        source_id: first_id.to_string(),
                    },
                    PurgeTrigger::ConfirmedDelete,
                ),
                (
                    PurgeTarget::Repository {
                        repo: "owner/second".to_string(),
                    },
                    PurgeTrigger::PermissionLoss,
                ),
            ])
            .unwrap();

        assert_eq!(queued, 2);
        assert!(store.successor_repair_required().unwrap());
        assert_eq!(
            read_content_write_epoch(&store.conn).unwrap(),
            epoch_before + 1
        );
        assert_eq!(store.pending_purges().unwrap().len(), 2);
        assert!(store.active_retrieval_publication().unwrap().is_none());
        for source_id in [first_id, second_id] {
            let state: String = store
                .conn
                .query_row(
                    "SELECT lifecycle_state FROM source_entities WHERE source_id = ?1",
                    params![source_id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(state, "purge_pending");
            let body_count: i64 = store
                .conn
                .query_row(
                    "SELECT count(*) FROM issue_metadata WHERE source_id = ?1",
                    params![source_id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(body_count, 1, "queue must not destroy content");
        }

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn repository_queue_subsumes_overlapping_source_and_counts_each_entity_once() {
        let paths = temp_profile_paths("purge-queue-subsumes-source");
        let mut store = Store::open(&paths).unwrap();
        let issue_id = "qgh://github.com/issue/I_QUEUE_SUBSUME";
        let comment_id = "qgh://github.com/issue-comment/IC_QUEUE_SUBSUME";
        store
            .upsert_sources_for_run(
                "sync-purge-queue-subsumes-source",
                &[test_issue(issue_id, "owner/repo", "issue-sensitive")],
                &[test_comment(
                    comment_id,
                    issue_id,
                    "owner/repo",
                    "comment-sensitive",
                )],
                0,
                &[],
            )
            .unwrap();

        let queued = store
            .queue_purges(&[
                (
                    PurgeTarget::Source {
                        source_id: comment_id.to_string(),
                    },
                    PurgeTrigger::ConfirmedDelete,
                ),
                (
                    PurgeTarget::Repository {
                        repo: "OWNER/REPO".to_string(),
                    },
                    PurgeTrigger::PermissionLoss,
                ),
            ])
            .unwrap();
        assert_eq!(queued, 1);
        assert_eq!(store.pending_purges().unwrap().len(), 1);

        let outcomes = store.retry_pending_purges().unwrap();
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].purged_sources, 2);
        assert_eq!(outcomes[0].purged_issues, 1);
        assert_eq!(outcomes[0].purged_comments, 1);
        assert!(store.known_repositories().unwrap().is_empty());

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn mixed_case_issue_purge_guards_and_recaptures_later_comment() {
        let paths = temp_profile_paths("purge-queue-mixed-case-issue");
        let mut store = Store::open(&paths).unwrap();
        let issue_id = "qgh://github.com/issue/I_QUEUE_CASE_ISSUE";
        let first_comment_id = "qgh://github.com/issue-comment/IC_QUEUE_CASE_FIRST";
        let later_comment_id = "qgh://github.com/issue-comment/IC_QUEUE_CASE_LATER";
        store
            .upsert_sources_for_run(
                "sync-purge-queue-mixed-case-issue",
                &[test_issue(issue_id, "owner/repo", "issue-sensitive")],
                &[test_comment(
                    first_comment_id,
                    issue_id,
                    "owner/repo",
                    "first-comment-sensitive",
                )],
                0,
                &[],
            )
            .unwrap();
        let request = (
            PurgeTarget::Issue {
                repo: "OWNER/REPO".to_string(),
                issue_number: 47,
            },
            PurgeTrigger::ConfirmedDelete,
        );
        store.queue_purges(std::slice::from_ref(&request)).unwrap();

        store
            .upsert_sources_for_run_under_pending_purge(
                "sync-purge-queue-mixed-case-later-comment",
                &[],
                &[test_comment(
                    later_comment_id,
                    issue_id,
                    "OWNER/REPO",
                    "later-comment-sensitive",
                )],
                0,
                &[],
            )
            .unwrap();
        store.queue_purges(&[request]).unwrap();

        let outcomes = store.retry_pending_purges().unwrap();
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].purged_issues, 1);
        assert_eq!(outcomes[0].purged_comments, 2);
        assert_eq!(
            store
                .get_tombstone(later_comment_id)
                .unwrap()
                .unwrap()
                .reason,
            "deleted"
        );

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn purge_queue_keeps_global_read_fence_after_partial_finish_failure() {
        let paths = temp_profile_paths("purge-queue-partial-finish");
        let mut store = Store::open(&paths).unwrap();
        let first_id = "qgh://github.com/issue/I_QUEUE_PARTIAL_FIRST";
        let second_id = "qgh://github.com/issue/I_QUEUE_PARTIAL_SECOND";
        store
            .upsert_sources_for_run(
                "sync-purge-queue-partial",
                &[
                    test_issue(first_id, "owner/first", "partial-first"),
                    test_issue(second_id, "owner/second", "partial-second"),
                ],
                &[],
                0,
                &[],
            )
            .unwrap();
        let first = PurgeTarget::Source {
            source_id: first_id.to_string(),
        };
        let second = PurgeTarget::Source {
            source_id: second_id.to_string(),
        };
        store
            .queue_purges(&[
                (first.clone(), PurgeTrigger::ConfirmedDelete),
                (second.clone(), PurgeTrigger::ConfirmedDelete),
            ])
            .unwrap();

        store
            .finish_pending_purge(first, PurgeTrigger::ConfirmedDelete)
            .unwrap();
        store.fail_next_purge_at(PurgeFailureStage::Storage);
        store
            .finish_pending_purge(second.clone(), PurgeTrigger::ConfirmedDelete)
            .unwrap_err();

        assert_eq!(store.pending_purges().unwrap().len(), 1);
        assert_eq!(store.pending_purges().unwrap()[0].target, second);
        let error = store.begin_read_snapshot().unwrap_err();
        assert_eq!(error.code, "purge.read_fenced");

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn purge_queue_transaction_failure_rolls_back_every_target_content_free() {
        let paths = temp_profile_paths("purge-queue-rollback");
        let mut store = Store::open(&paths).unwrap();
        let first_id = "qgh://github.com/issue/I_QUEUE_ROLLBACK_FIRST";
        let second_id = "qgh://github.com/issue/I_QUEUE_ROLLBACK_SECOND";
        let sensitive_marker = "SENSITIVE_QUEUE_ROLLBACK_MARKER";
        store
            .upsert_sources_for_run(
                "sync-purge-queue-rollback",
                &[
                    test_issue(first_id, "owner/first", sensitive_marker),
                    test_issue(second_id, "owner/second", sensitive_marker),
                ],
                &[],
                0,
                &[],
            )
            .unwrap();
        seal_latest_test_sync(&mut store);
        let (generation, _) = store
            .reserve_index_generation(&paths.index_root, 2)
            .unwrap();
        rebuild_reserved_generation(&store, &paths, generation);
        let publication = store
            .activate_retrieval_publication("sync-purge-queue-rollback", generation, None, None)
            .unwrap();
        let epoch_before = read_content_write_epoch(&store.conn).unwrap();
        store.fail_next_purge_queue_after_first();

        let error = store
            .queue_purges(&[
                (
                    PurgeTarget::Source {
                        source_id: first_id.to_string(),
                    },
                    PurgeTrigger::ConfirmedDelete,
                ),
                (
                    PurgeTarget::Source {
                        source_id: second_id.to_string(),
                    },
                    PurgeTrigger::ConfirmedDelete,
                ),
            ])
            .unwrap_err();

        assert_eq!(error.code, "purge.failed");
        let rendered = serde_json::to_string(&error).unwrap();
        assert!(!rendered.contains(sensitive_marker));
        assert!(!rendered.contains(first_id));
        assert!(!rendered.contains(second_id));
        assert!(store.pending_purges().unwrap().is_empty());
        assert!(!store.successor_repair_required().unwrap());
        assert_eq!(read_content_write_epoch(&store.conn).unwrap(), epoch_before);
        assert_eq!(
            store
                .active_retrieval_publication()
                .unwrap()
                .unwrap()
                .publication_id,
            publication
        );
        for source_id in [first_id, second_id] {
            let state: String = store
                .conn
                .query_row(
                    "SELECT lifecycle_state FROM source_entities WHERE source_id = ?1",
                    params![source_id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(state, "active");
        }

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn purge_queue_deduplicates_identical_targets_with_one_epoch_bump() {
        let paths = temp_profile_paths("purge-queue-deduplicate");
        let mut store = Store::open(&paths).unwrap();
        let source_id = "qgh://github.com/issue/I_QUEUE_DEDUPLICATE";
        store
            .upsert_sources_for_run(
                "sync-purge-queue-deduplicate",
                &[test_issue(source_id, "owner/repo", "deduplicate")],
                &[],
                0,
                &[],
            )
            .unwrap();
        let target = PurgeTarget::Source {
            source_id: source_id.to_string(),
        };
        let epoch_before = read_content_write_epoch(&store.conn).unwrap();

        let queued = store
            .queue_purges(&[
                (target.clone(), PurgeTrigger::ConfirmedDelete),
                (target, PurgeTrigger::ConfirmedDelete),
            ])
            .unwrap();

        assert_eq!(queued, 1);
        assert_eq!(store.pending_purges().unwrap().len(), 1);
        assert_eq!(
            read_content_write_epoch(&store.conn).unwrap(),
            epoch_before + 1
        );

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn purge_queue_rejects_conflicting_triggers_without_mutation() {
        let paths = temp_profile_paths("purge-queue-conflicting-trigger");
        let mut store = Store::open(&paths).unwrap();
        let source_id = "qgh://github.com/issue/I_QUEUE_CONFLICT";
        store
            .upsert_sources_for_run(
                "sync-purge-queue-conflict",
                &[test_issue(source_id, "owner/repo", "conflict")],
                &[],
                0,
                &[],
            )
            .unwrap();
        seal_latest_test_sync(&mut store);
        let (generation, _) = store
            .reserve_index_generation(&paths.index_root, 1)
            .unwrap();
        rebuild_reserved_generation(&store, &paths, generation);
        let publication = store
            .activate_retrieval_publication("sync-purge-queue-conflict", generation, None, None)
            .unwrap();
        let epoch_before = read_content_write_epoch(&store.conn).unwrap();
        let target = PurgeTarget::Source {
            source_id: source_id.to_string(),
        };

        let error = store
            .queue_purges(&[
                (target.clone(), PurgeTrigger::ConfirmedDelete),
                (target, PurgeTrigger::PermissionLoss),
            ])
            .unwrap_err();

        assert_eq!(error.code, "purge.conflicting_triggers");
        assert!(store.pending_purges().unwrap().is_empty());
        assert!(!store.successor_repair_required().unwrap());
        assert_eq!(read_content_write_epoch(&store.conn).unwrap(), epoch_before);
        assert_eq!(
            store
                .active_retrieval_publication()
                .unwrap()
                .unwrap()
                .publication_id,
            publication
        );

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn completed_purge_queue_batch_is_noop_for_epoch_and_publication() {
        let paths = temp_profile_paths("purge-queue-completed-noop");
        let mut store = Store::open(&paths).unwrap();
        let source_id = "qgh://github.com/issue/I_QUEUE_COMPLETED_NOOP";
        store
            .upsert_sources_for_run(
                "sync-purge-queue-completed",
                &[test_issue(source_id, "owner/repo", "completed")],
                &[],
                0,
                &[],
            )
            .unwrap();
        let target = PurgeTarget::Source {
            source_id: source_id.to_string(),
        };
        store
            .purge(target.clone(), PurgeTrigger::ConfirmedDelete)
            .unwrap();
        let snapshot = store
            .record_purge_successor_snapshot()
            .unwrap()
            .expect("completed purge successor snapshot");
        let (generation, _) = store
            .reserve_index_generation(&paths.index_root, 0)
            .unwrap();
        rebuild_reserved_generation(&store, &paths, generation);
        let publication = store
            .activate_retrieval_publication(&snapshot, generation, None, None)
            .unwrap();
        let epoch_before = read_content_write_epoch(&store.conn).unwrap();

        let queued = store
            .queue_purges(&[(target, PurgeTrigger::ConfirmedDelete)])
            .unwrap();

        assert_eq!(queued, 0);
        assert!(store.pending_purges().unwrap().is_empty());
        assert!(!store.successor_repair_required().unwrap());
        assert_eq!(read_content_write_epoch(&store.conn).unwrap(), epoch_before);
        assert_eq!(
            store
                .active_retrieval_publication()
                .unwrap()
                .unwrap()
                .publication_id,
            publication
        );

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn targeted_refresh_upsert_never_tombstones_missing_comments_by_itself() {
        let paths = temp_profile_paths("targeted-refresh-no-destructive-fallback");
        let mut store = Store::open(&paths).unwrap();
        let issue_id = "qgh://github.com/issue/I_TARGET_REFRESH_SAFE";
        let retained_id = "qgh://github.com/issue-comment/IC_TARGET_REFRESH_RETAINED";
        let missing_id = "qgh://github.com/issue-comment/IC_TARGET_REFRESH_MISSING";
        let added_id = "qgh://github.com/issue-comment/IC_TARGET_REFRESH_ADDED";
        let issue = test_issue(issue_id, "owner/repo", "issue-body");
        store
            .upsert_sources_for_run(
                "sync-targeted-refresh-initial",
                std::slice::from_ref(&issue),
                &[
                    test_comment(retained_id, issue_id, "owner/repo", "retained-old"),
                    test_comment(missing_id, issue_id, "owner/repo", "missing-stays"),
                ],
                0,
                &[],
            )
            .unwrap();

        let summary = store
            .upsert_target_issue_refresh(
                &issue,
                &[
                    test_comment(retained_id, issue_id, "owner/repo", "retained-new"),
                    test_comment(added_id, issue_id, "owner/repo", "added-new"),
                ],
            )
            .unwrap();

        assert_eq!(summary.added_comments, 1);
        assert_eq!(summary.updated_comments, 1);
        assert_eq!(summary.deleted_comments, 0);
        assert_eq!(summary.tombstoned_comments, 0);
        assert!(store.get_tombstone(missing_id).unwrap().is_none());
        assert_eq!(
            store.get_comment(missing_id).unwrap().unwrap().body,
            "Comment missing-stays"
        );
        assert_eq!(
            store.get_comment(retained_id).unwrap().unwrap().body,
            "Comment retained-new"
        );
        assert_eq!(
            store.get_comment(added_id).unwrap().unwrap().body,
            "Comment added-new"
        );

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn known_repositories_includes_cursor_and_sync_only_state_for_repository_purge() {
        let paths = temp_profile_paths("known-repositories-owned-state-union");
        let mut store = Store::open(&paths).unwrap();
        let target_repo = "owner/empty";
        let retained_repo = "owner/retained";
        for repo in [target_repo, retained_repo] {
            store
                .conn
                .execute(
                    "INSERT INTO repository_sync_state (repo, last_successful_sync_at)
                     VALUES (?1, ?2)",
                    params![repo, now_rfc3339()],
                )
                .unwrap();
        }
        for endpoint in [
            "issues:owner/empty",
            "history:owner/empty",
            "repo-comments:owner/empty",
            "comments:owner/empty#47",
            "issues:owner/retained",
            "comments:not-a-repo",
            "comments:owner/invalid#zero",
        ] {
            store
                .conn
                .execute(
                    "INSERT INTO sync_cursors (endpoint, cursor, etag)
                     VALUES (?1, NULL, NULL)",
                    params![endpoint],
                )
                .unwrap();
        }
        assert_eq!(
            store.known_repositories().unwrap(),
            vec![target_repo.to_string(), retained_repo.to_string()]
        );

        store
            .purge(
                PurgeTarget::Repository {
                    repo: target_repo.to_string(),
                },
                PurgeTrigger::AllowlistRemoval,
            )
            .unwrap();

        assert_eq!(
            store.known_repositories().unwrap(),
            vec![retained_repo.to_string()]
        );
        let target_sync_state: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM repository_sync_state WHERE repo = ?1",
                params![target_repo],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(target_sync_state, 0);
        let target_cursors: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM sync_cursors WHERE endpoint LIKE '%owner/empty%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(target_cursors, 0);
        let retained_sync_state: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM repository_sync_state WHERE repo = ?1",
                params![retained_repo],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retained_sync_state, 1);

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn known_repositories_excludes_completed_repo_tombstone_identities() {
        let paths = temp_profile_paths("known-repositories-ignore-completed-tombstones");
        let mut store = Store::open(&paths).unwrap();
        let repo = "owner/completed";
        let source_id = "qgh://github.com/issue/I_KNOWN_COMPLETED_TOMBSTONE";
        store
            .upsert_sources_for_run(
                "sync-known-completed-tombstone",
                &[test_issue(source_id, repo, "completed-sensitive")],
                &[],
                0,
                &[],
            )
            .unwrap();

        store
            .purge(
                PurgeTarget::Repository {
                    repo: repo.to_string(),
                },
                PurgeTrigger::AllowlistRemoval,
            )
            .unwrap();

        let retained_identity: (String, String) = store
            .conn
            .query_row(
                "SELECT repo, lifecycle_state FROM source_entities WHERE source_id = ?1",
                params![source_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            retained_identity,
            (repo.to_string(), "tombstoned".to_string())
        );
        assert!(store.known_repositories().unwrap().is_empty());

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn legacy_tombstone_is_queued_on_reopen_then_retry_purges_residual_state() {
        let paths = temp_profile_paths("purge-legacy-tombstone-migration");
        let mut store = Store::open(&paths).unwrap();
        store.enable_vector().unwrap();
        let source_id = "qgh://github.com/issue/I_PURGE_LEGACY_TOMBSTONE";
        let marker = "PRIVATE_LEGACY_TOMBSTONE_MARKER_38c1";
        store
            .upsert_sources_for_run(
                "sync-legacy-tombstone-migration",
                &[test_issue(source_id, "owner/repo", marker)],
                &[],
                0,
                &[],
            )
            .unwrap();
        let chunk_id = insert_chunk(&mut store, source_id, marker);
        let embedding_generation =
            stage_test_generation(&mut store, "manifest-legacy-tombstone", &[chunk_id]);
        let (tantivy_generation, generation_path) = store
            .reserve_index_generation(&paths.index_root, 1)
            .unwrap();
        rebuild_reserved_generation(&store, &paths, tantivy_generation);
        store
            .activate_retrieval_publication(
                "sync-legacy-tombstone-migration",
                tantivy_generation,
                Some(embedding_generation),
                None,
            )
            .unwrap();
        store.tombstone_source(source_id, "transferred").unwrap();
        let epoch_before = read_content_write_epoch(&store.conn).unwrap();
        assert!(store.pending_purges().unwrap().is_empty());
        assert!(generation_path.exists());
        drop(store);

        let mut reopened = Store::open(&paths).unwrap();
        reopened.enable_vector().unwrap();

        assert_eq!(
            reopened.pending_purges().unwrap(),
            vec![PendingPurgeView {
                target: PurgeTarget::Source {
                    source_id: source_id.to_string(),
                },
                trigger: PurgeTrigger::ConfirmedTombstone,
                current_stage: PurgeFailureStage::SecureDelete,
                failure_stage: None,
            }]
        );
        assert_eq!(
            read_content_write_epoch(&reopened.conn).unwrap(),
            epoch_before + 1
        );
        assert!(reopened.active_retrieval_publication().unwrap().is_none());
        assert!(generation_path.exists());
        assert!(reopened
            .embedding_generation_state(embedding_generation)
            .is_ok());
        let residual_versions: i64 = reopened
            .conn
            .query_row(
                "SELECT count(*) FROM source_versions WHERE source_id = ?1",
                params![source_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(residual_versions, 1);

        let outcomes = reopened.retry_pending_purges().unwrap();

        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].purged_sources, 1);
        assert!(reopened.pending_purges().unwrap().is_empty());
        assert!(!generation_path.exists());
        assert!(reopened
            .embedding_generation_state(embedding_generation)
            .is_err());
        let residual_versions: i64 = reopened
            .conn
            .query_row(
                "SELECT count(*) FROM source_versions WHERE source_id = ?1",
                params![source_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(residual_versions, 0);
        assert_eq!(
            reopened.get_tombstone(source_id).unwrap().unwrap().reason,
            "transferred"
        );

        drop(reopened);
        let db_bytes = fs::read(&paths.db_path).unwrap();
        assert!(!db_bytes
            .windows(marker.len())
            .any(|bytes| bytes == marker.as_bytes()));
        let wal_path = PathBuf::from(format!("{}-wal", paths.db_path.display()));
        if wal_path.exists() {
            let wal_bytes = fs::read(wal_path).unwrap();
            assert!(!wal_bytes
                .windows(marker.len())
                .any(|bytes| bytes == marker.as_bytes()));
        }

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn legacy_comment_tombstone_is_queued_with_canonical_source_target() {
        let paths = temp_profile_paths("purge-legacy-comment-tombstone");
        let mut store = Store::open(&paths).unwrap();
        let issue_id = "qgh://github.com/issue/I_PURGE_LEGACY_COMMENT_PARENT";
        let comment_id = "qgh://github.com/issue-comment/IC_PURGE_LEGACY_COMMENT_TARGET";
        store
            .upsert_sources_for_run(
                "sync-legacy-comment-tombstone",
                &[test_issue(issue_id, "owner/repo", "comment-parent")],
                &[test_comment(
                    comment_id,
                    issue_id,
                    "owner/repo",
                    "comment-target",
                )],
                0,
                &[],
            )
            .unwrap();
        store
            .tombstone_source(comment_id, "permission_loss")
            .unwrap();
        let epoch_before = read_content_write_epoch(&store.conn).unwrap();
        drop(store);

        let reopened = Store::open(&paths).unwrap();

        assert_eq!(
            reopened.pending_purges().unwrap(),
            vec![PendingPurgeView {
                target: PurgeTarget::Source {
                    source_id: comment_id.to_string(),
                },
                trigger: PurgeTrigger::PermissionLoss,
                current_stage: PurgeFailureStage::SecureDelete,
                failure_stage: None,
            }]
        );
        assert_eq!(
            read_content_write_epoch(&reopened.conn).unwrap(),
            epoch_before + 1
        );

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn legacy_tombstone_batch_maps_reasons_and_bumps_epoch_once() {
        let paths = temp_profile_paths("purge-legacy-tombstone-batch");
        let mut store = Store::open(&paths).unwrap();
        let moved_id = "qgh://github.com/issue/I_LEGACY_BATCH_A_MOVED";
        let permission_id = "qgh://github.com/issue/I_LEGACY_BATCH_B_PERMISSION";
        let allowlist_id = "qgh://github.com/issue/I_LEGACY_BATCH_C_ALLOWLIST";
        let deleted_id = "qgh://github.com/issue/I_LEGACY_BATCH_D_DELETED";
        store
            .upsert_sources_for_run(
                "sync-legacy-tombstone-batch",
                &[
                    test_issue(moved_id, "owner/moved", "batch-moved"),
                    test_issue(permission_id, "owner/permission", "batch-permission"),
                    test_issue(allowlist_id, "owner/allowlist", "batch-allowlist"),
                    test_issue(deleted_id, "owner/deleted", "batch-deleted"),
                ],
                &[],
                0,
                &[],
            )
            .unwrap();
        for (source_id, reason) in [
            (moved_id, "moved"),
            (permission_id, "permission_denied"),
            (allowlist_id, "allowlist_removal"),
            (deleted_id, "gone"),
        ] {
            store.tombstone_source(source_id, reason).unwrap();
        }
        let epoch_before = read_content_write_epoch(&store.conn).unwrap();
        drop(store);

        let reopened = Store::open(&paths).unwrap();

        assert_eq!(
            reopened.pending_purges().unwrap(),
            vec![
                PendingPurgeView {
                    target: PurgeTarget::Source {
                        source_id: moved_id.to_string(),
                    },
                    trigger: PurgeTrigger::ConfirmedTombstone,
                    current_stage: PurgeFailureStage::SecureDelete,
                    failure_stage: None,
                },
                PendingPurgeView {
                    target: PurgeTarget::Source {
                        source_id: permission_id.to_string(),
                    },
                    trigger: PurgeTrigger::PermissionLoss,
                    current_stage: PurgeFailureStage::SecureDelete,
                    failure_stage: None,
                },
                PendingPurgeView {
                    target: PurgeTarget::Source {
                        source_id: allowlist_id.to_string(),
                    },
                    trigger: PurgeTrigger::AllowlistRemoval,
                    current_stage: PurgeFailureStage::SecureDelete,
                    failure_stage: None,
                },
                PendingPurgeView {
                    target: PurgeTarget::Source {
                        source_id: deleted_id.to_string(),
                    },
                    trigger: PurgeTrigger::ConfirmedDelete,
                    current_stage: PurgeFailureStage::SecureDelete,
                    failure_stage: None,
                },
            ]
        );
        assert_eq!(
            read_content_write_epoch(&reopened.conn).unwrap(),
            epoch_before + 1
        );
        let mapped_count: i64 = reopened
            .conn
            .query_row("SELECT count(*) FROM purge_target_sources", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(mapped_count, 4);
        let issue_metadata_count: i64 = reopened
            .conn
            .query_row("SELECT count(*) FROM issue_metadata", [], |row| row.get(0))
            .unwrap();
        assert_eq!(issue_metadata_count, 4);
        for (source_id, reason) in [
            (moved_id, "transferred"),
            (permission_id, "permission_loss"),
            (allowlist_id, "allowlist_removal"),
            (deleted_id, "deleted"),
        ] {
            assert_eq!(
                reopened.get_tombstone(source_id).unwrap().unwrap().reason,
                reason
            );
        }

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn legacy_tombstone_batch_failure_is_atomic_and_content_free() {
        let paths = temp_profile_paths("purge-legacy-tombstone-batch-failure");
        let mut store = Store::open(&paths).unwrap();
        let first_id = "qgh://github.com/issue/I_LEGACY_ATOMIC_A";
        let second_id = "qgh://github.com/issue/I_LEGACY_ATOMIC_B";
        let private_failure = "PRIVATE_LEGACY_MIGRATION_FAILURE_5f2a";
        store
            .upsert_sources_for_run(
                "sync-legacy-tombstone-batch-failure",
                &[
                    test_issue(first_id, "owner/first", "atomic-first"),
                    test_issue(second_id, "owner/second", "atomic-second"),
                ],
                &[],
                0,
                &[],
            )
            .unwrap();
        store.tombstone_source(first_id, "moved").unwrap();
        store
            .tombstone_source(second_id, "permission_denied")
            .unwrap();
        let epoch_before = read_content_write_epoch(&store.conn).unwrap();
        store
            .conn
            .execute_batch(&format!(
                "CREATE TRIGGER fail_legacy_purge_queue
                 BEFORE INSERT ON purge_requests
                 WHEN NEW.target_value = '{second_id}'
                 BEGIN
                     SELECT RAISE(ABORT, '{private_failure}');
                 END;"
            ))
            .unwrap();
        drop(store);

        let error = match Store::open(&paths) {
            Ok(_) => panic!("legacy tombstone migration unexpectedly succeeded"),
            Err(error) => error,
        };

        assert_eq!(error.code, "purge.failed");
        let serialized = serde_json::to_string(&error).unwrap();
        assert!(!serialized.contains(private_failure));
        assert!(!serialized.contains(first_id));
        assert!(!serialized.contains(second_id));
        let conn = Connection::open(&paths.db_path).unwrap();
        let queued: i64 = conn
            .query_row("SELECT count(*) FROM purge_requests", [], |row| row.get(0))
            .unwrap();
        assert_eq!(queued, 0);
        let mapped: i64 = conn
            .query_row("SELECT count(*) FROM purge_target_sources", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(mapped, 0);
        assert_eq!(read_content_write_epoch(&conn).unwrap(), epoch_before);
        assert_eq!(
            conn.query_row(
                "SELECT reason FROM tombstones WHERE source_id = ?1",
                params![first_id],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
            "moved"
        );
        conn.execute_batch("DROP TRIGGER fail_legacy_purge_queue")
            .unwrap();
        drop(conn);

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn legacy_tombstone_migration_does_not_reset_existing_pending_source_request() {
        let paths = temp_profile_paths("purge-legacy-existing-pending");
        let mut store = Store::open(&paths).unwrap();
        let source_id = "qgh://github.com/issue/I_LEGACY_EXISTING_PENDING";
        store
            .upsert_sources_for_run(
                "sync-legacy-existing-pending",
                &[test_issue(source_id, "owner/repo", "existing-pending")],
                &[],
                0,
                &[],
            )
            .unwrap();
        store
            .tombstone_source(source_id, "permission_loss")
            .unwrap();
        store
            .conn
            .pragma_update(None, "secure_delete", "OFF")
            .unwrap();
        store
            .purge(
                PurgeTarget::Source {
                    source_id: source_id.to_string(),
                },
                PurgeTrigger::ConfirmedDelete,
            )
            .unwrap_err();
        let request_before: (String, i64, String, Option<String>, i64, String, String) = store
            .conn
            .query_row(
                "SELECT trigger, purge_pending, current_stage, failure_stage,
                        completion_ready, created_at, updated_at
                 FROM purge_requests
                 WHERE target_kind = 'source' AND target_value = ?1",
                params![source_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .unwrap();
        let epoch_before = read_content_write_epoch(&store.conn).unwrap();
        drop(store);

        let reopened = Store::open(&paths).unwrap();

        let request_after: (String, i64, String, Option<String>, i64, String, String) = reopened
            .conn
            .query_row(
                "SELECT trigger, purge_pending, current_stage, failure_stage,
                        completion_ready, created_at, updated_at
                 FROM purge_requests
                 WHERE target_kind = 'source' AND target_value = ?1",
                params![source_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(request_after, request_before);
        assert_eq!(
            reopened.pending_purges().unwrap(),
            vec![PendingPurgeView {
                target: PurgeTarget::Source {
                    source_id: source_id.to_string(),
                },
                trigger: PurgeTrigger::ConfirmedDelete,
                current_stage: PurgeFailureStage::SecureDelete,
                failure_stage: Some(PurgeFailureStage::SecureDelete),
            }]
        );
        assert_eq!(
            read_content_write_epoch(&reopened.conn).unwrap(),
            epoch_before
        );

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn empty_store_reopen_does_not_create_purge_work_or_bump_epoch() {
        let paths = temp_profile_paths("purge-empty-store-migration-noop");
        let store = Store::open(&paths).unwrap();
        let epoch_before = read_content_write_epoch(&store.conn).unwrap();
        assert_eq!(epoch_before, 0);
        drop(store);

        let reopened = Store::open(&paths).unwrap();

        assert_eq!(read_content_write_epoch(&reopened.conn).unwrap(), 0);
        assert!(reopened.pending_purges().unwrap().is_empty());
        let request_count: i64 = reopened
            .conn
            .query_row("SELECT count(*) FROM purge_requests", [], |row| row.get(0))
            .unwrap();
        assert_eq!(request_count, 0);
        let mapping_count: i64 = reopened
            .conn
            .query_row("SELECT count(*) FROM purge_target_sources", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(mapping_count, 0);

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn purge_pending_immediately_blocks_store_get_and_query_eligibility() {
        let paths = temp_profile_paths("purge-pending-eligibility");
        let mut store = Store::open(&paths).unwrap();
        let source_id = "qgh://github.com/issue/I_PURGE_PENDING";
        store
            .upsert_sources_for_run(
                "sync-purge-pending",
                &[test_issue(source_id, "owner/repo", "PRIVATE_PURGE_MARKER")],
                &[],
                0,
                &[],
            )
            .unwrap();
        assert!(store.get_source(source_id).unwrap().is_some());
        assert_eq!(store.active_index_sources().unwrap().len(), 1);

        store
            .conn
            .pragma_update(None, "secure_delete", "OFF")
            .unwrap();
        let error = store
            .purge(
                PurgeTarget::Source {
                    source_id: source_id.to_string(),
                },
                PurgeTrigger::ConfirmedDelete,
            )
            .unwrap_err();

        assert_eq!(error.code, "purge.failed");
        assert!(store.get_source(source_id).unwrap().is_none());
        assert!(store.active_index_sources().unwrap().is_empty());
        assert_eq!(
            store.pending_purges().unwrap(),
            vec![PendingPurgeView {
                target: PurgeTarget::Source {
                    source_id: source_id.to_string(),
                },
                trigger: PurgeTrigger::ConfirmedDelete,
                current_stage: PurgeFailureStage::SecureDelete,
                failure_stage: Some(PurgeFailureStage::SecureDelete),
            }]
        );

        store
            .conn
            .execute(
                "UPDATE source_entities SET lifecycle_state = 'active' WHERE source_id = ?1",
                params![source_id],
            )
            .unwrap();
        drop(store);
        let reopened = Store::open(&paths).unwrap();
        assert!(reopened.get_source(source_id).unwrap().is_none());
        assert_eq!(reopened.pending_purges().unwrap().len(), 1);

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn purge_start_atomically_invalidates_publication_before_secure_delete_failure() {
        let paths = temp_profile_paths("purge-atomic-publication-invalidation");
        let mut store = Store::open(&paths).unwrap();
        let source_id = "qgh://github.com/issue/I_PURGE_ATOMIC_PUBLICATION";
        store
            .upsert_sources_for_run(
                "sync-purge-atomic-publication",
                &[test_issue(
                    source_id,
                    "owner/repo",
                    "atomic-publication-marker",
                )],
                &[],
                0,
                &[],
            )
            .unwrap();
        seal_latest_test_sync(&mut store);
        let (generation, _) = store
            .reserve_index_generation(&paths.index_root, 1)
            .unwrap();
        rebuild_reserved_generation(&store, &paths, generation);
        store
            .activate_retrieval_publication("sync-purge-atomic-publication", generation, None, None)
            .unwrap();
        store
            .conn
            .pragma_update(None, "secure_delete", "OFF")
            .unwrap();

        store
            .purge(
                PurgeTarget::Source {
                    source_id: source_id.to_string(),
                },
                PurgeTrigger::ConfirmedDelete,
            )
            .unwrap_err();

        assert!(store.active_retrieval_publication().unwrap().is_none());
        assert!(store.active_index_generation().unwrap().is_none());
        assert_eq!(store.pending_purges().unwrap().len(), 1);

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn read_snapshot_revalidation_fences_content_loaded_before_purge_commit() {
        let paths = temp_profile_paths("purge-read-snapshot-fence");
        let mut writer = Store::open(&paths).unwrap();
        let source_id = "qgh://github.com/issue/I_PURGE_READ_SNAPSHOT";
        let marker = "PRIVATE_READ_SNAPSHOT_MARKER_18de";
        writer
            .upsert_sources_for_run(
                "sync-purge-read-snapshot",
                &[test_issue(source_id, "owner/repo", marker)],
                &[],
                0,
                &[],
            )
            .unwrap();
        let reader = Store::open(&paths).unwrap();
        let fence = reader.begin_read_snapshot().unwrap();
        let loaded = reader.get_source(source_id).unwrap();
        assert!(
            loaded.is_some(),
            "old snapshot did not load fixture content"
        );

        let purge_error = writer
            .purge(
                PurgeTarget::Source {
                    source_id: source_id.to_string(),
                },
                PurgeTrigger::ConfirmedDelete,
            )
            .unwrap_err();
        assert_eq!(purge_error.code, "purge.failed");

        let error = reader.end_read_snapshot_and_validate(fence).unwrap_err();
        assert_eq!(error.code, "purge.read_fenced");
        assert!(reader.get_source(source_id).unwrap().is_none());
        writer.retry_pending_purges().unwrap();

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn read_snapshot_can_be_validated_or_explicitly_rolled_back() {
        let paths = temp_profile_paths("purge-read-snapshot-clean-end");
        let store = Store::open(&paths).unwrap();

        let fence = store.begin_read_snapshot().unwrap();
        assert!(store.status().is_ok());
        store.end_read_snapshot_and_validate(fence).unwrap();

        let _fence = store.begin_read_snapshot().unwrap();
        store.rollback_read_snapshot().unwrap();
        assert!(store.begin_read_snapshot().is_ok());
        store.rollback_read_snapshot().unwrap();

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn purge_fences_store_opened_before_it_from_reingesting_content() {
        let paths = temp_profile_paths("purge-stale-writer-before");
        let mut stale_writer = Store::open(&paths).unwrap();
        let mut purger = Store::open(&paths).unwrap();
        let source_id = "qgh://github.com/issue/I_PURGE_STALE_WRITER_BEFORE";
        stale_writer
            .upsert_sources_for_run(
                "sync-stale-writer-before",
                &[test_issue(source_id, "owner/repo", "before-purge")],
                &[],
                0,
                &[],
            )
            .unwrap();
        purger
            .purge(
                PurgeTarget::Source {
                    source_id: source_id.to_string(),
                },
                PurgeTrigger::ConfirmedDelete,
            )
            .unwrap();

        let error = stale_writer
            .upsert_sources_for_run(
                "sync-stale-writer-after",
                &[test_issue(source_id, "owner/repo", "resurrection-marker")],
                &[],
                0,
                &[],
            )
            .unwrap_err();

        assert_eq!(error.code, "purge.write_fenced");
        let fresh_reader = Store::open(&paths).unwrap();
        assert!(fresh_reader.get_source(source_id).unwrap().is_none());

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn purge_fences_stale_tantivy_generation_reservation() {
        let paths = temp_profile_paths("purge-stale-tantivy-reserve");
        let mut stale_builder = Store::open(&paths).unwrap();
        let mut purger = Store::open(&paths).unwrap();
        let source_id = "qgh://github.com/issue/I_PURGE_STALE_TANTIVY_RESERVE";
        stale_builder
            .upsert_sources_for_run(
                "sync-stale-tantivy-reserve",
                &[test_issue(source_id, "owner/repo", "stale-tantivy-reserve")],
                &[],
                0,
                &[],
            )
            .unwrap();
        purger
            .purge(
                PurgeTarget::Source {
                    source_id: source_id.to_string(),
                },
                PurgeTrigger::ConfirmedDelete,
            )
            .unwrap();

        let error = stale_builder
            .reserve_index_generation(&paths.index_root, 0)
            .unwrap_err();
        assert_eq!(error.code, "purge.write_fenced");

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn purge_rejects_late_stale_tantivy_publish_and_removes_orphan() {
        let paths = temp_profile_paths("purge-late-stale-tantivy-publish");
        let mut stale_builder = Store::open(&paths).unwrap();
        let mut purger = Store::open(&paths).unwrap();
        let source_id = "qgh://github.com/issue/I_PURGE_LATE_TANTIVY";
        stale_builder
            .upsert_sources_for_run(
                "sync-late-stale-tantivy",
                &[test_issue(source_id, "owner/repo", "late-tantivy-marker")],
                &[],
                0,
                &[],
            )
            .unwrap();
        seal_latest_test_sync(&mut stale_builder);
        let (generation, generation_path) = stale_builder
            .reserve_index_generation(&paths.index_root, 1)
            .unwrap();
        stale_builder
            .rebuild_reserved_index_generation(
                generation,
                &stale_builder.active_index_sources().unwrap(),
            )
            .unwrap();
        let purge_error = purger
            .purge(
                PurgeTarget::Source {
                    source_id: source_id.to_string(),
                },
                PurgeTrigger::ConfirmedDelete,
            )
            .unwrap_err();
        assert_eq!(purge_error.code, "purge.failed");
        let error = stale_builder
            .activate_retrieval_publication("sync-late-stale-tantivy", generation, None, None)
            .unwrap_err();

        assert_eq!(error.code, "purge.write_fenced");
        assert!(!generation_path.exists());
        purger.retry_pending_purges().unwrap();
        let fresh = Store::open(&paths).unwrap();
        assert!(fresh.active_retrieval_publication().unwrap().is_none());

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn purge_stays_pending_while_live_index_builder_owns_generation() {
        let paths = temp_profile_paths("purge-live-index-build-lease");
        let mut builder = Store::open(&paths).unwrap();
        let source_id = "qgh://github.com/issue/I_PURGE_LIVE_INDEX_LEASE";
        builder
            .upsert_sources_for_run(
                "sync-purge-live-index-lease",
                &[test_issue(source_id, "owner/repo", "live-index-private")],
                &[],
                0,
                &[],
            )
            .unwrap();
        seal_latest_test_sync(&mut builder);
        let (generation, generation_path) = builder
            .reserve_index_generation(&paths.index_root, 1)
            .unwrap();
        builder
            .rebuild_reserved_index_generation(generation, &builder.active_index_sources().unwrap())
            .unwrap();
        let mut purger = Store::open(&paths).unwrap();

        let error = purger
            .purge(
                PurgeTarget::Source {
                    source_id: source_id.to_string(),
                },
                PurgeTrigger::ConfirmedDelete,
            )
            .unwrap_err();

        assert_eq!(error.code, "purge.failed");
        assert_eq!(
            purger.pending_purges().unwrap()[0].failure_stage,
            Some(PurgeFailureStage::Tantivy)
        );
        assert!(generation_path.exists());
        let stale_error = builder
            .activate_retrieval_publication("sync-purge-live-index-lease", generation, None, None)
            .unwrap_err();
        assert_eq!(stale_error.code, "purge.write_fenced");
        assert!(!generation_path.exists());
        purger.retry_pending_purges().unwrap();
        let successor_snapshot = purger
            .record_purge_successor_snapshot()
            .unwrap()
            .expect("purge successor snapshot");
        let (successor_generation, successor_path) = purger
            .reserve_index_generation(&paths.index_root, 0)
            .unwrap();
        assert!(successor_generation > generation);
        rebuild_reserved_generation(&purger, &paths, successor_generation);
        let repeated_stale_error = builder
            .mark_index_published(generation, &generation_path.to_string_lossy(), 1)
            .unwrap_err();

        assert_eq!(repeated_stale_error.code, "purge.write_fenced");
        assert!(successor_path.exists());
        purger
            .activate_retrieval_publication(&successor_snapshot, successor_generation, None, None)
            .unwrap();

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(unix)]
    #[test]
    fn purge_reclaims_dead_process_index_lease_without_ttl() {
        let paths = temp_profile_paths("purge-dead-index-build-lease");
        let mut store = Store::open(&paths).unwrap();
        let source_id = "qgh://github.com/issue/I_PURGE_DEAD_INDEX_LEASE";
        store
            .upsert_sources_for_run(
                "sync-purge-dead-index-lease",
                &[test_issue(source_id, "owner/repo", "dead-index-private")],
                &[],
                0,
                &[],
            )
            .unwrap();
        seal_latest_test_sync(&mut store);
        let (generation, generation_path) = store
            .reserve_index_generation(&paths.index_root, 1)
            .unwrap();
        store
            .rebuild_reserved_index_generation(generation, &store.active_index_sources().unwrap())
            .unwrap();
        store.index_build_tokens.remove(&generation);
        store
            .conn
            .execute(
                "UPDATE index_build_leases SET owner_pid = -1 WHERE generation = ?1",
                params![generation],
            )
            .unwrap();

        let outcome = store
            .purge(
                PurgeTarget::Source {
                    source_id: source_id.to_string(),
                },
                PurgeTrigger::ConfirmedDelete,
            )
            .unwrap();

        assert_eq!(outcome.purged_sources, 1);
        assert!(!generation_path.exists());
        let lease_count: i64 = store
            .conn
            .query_row("SELECT count(*) FROM index_build_leases", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(lease_count, 0);

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(unix)]
    #[test]
    fn purge_keeps_pending_when_owned_shadow_is_swapped_for_foreign_symlink() {
        use std::os::unix::fs::symlink;

        let paths = temp_profile_paths("purge-shadow-symlink-swap");
        let mut store = Store::open(&paths).unwrap();
        let source_id = "qgh://github.com/issue/I_PURGE_SHADOW_SWAP";
        store
            .upsert_sources_for_run(
                "sync-purge-shadow-swap",
                &[test_issue(source_id, "owner/repo", "private-shadow-swap")],
                &[],
                0,
                &[],
            )
            .unwrap();
        seal_latest_test_sync(&mut store);
        let (generation, generation_path) = store
            .reserve_index_generation(&paths.index_root, 1)
            .unwrap();
        let shadow_path = paths.index_root.join(format!("shadow-{generation}"));
        fs::remove_dir_all(&shadow_path).unwrap();
        let external = paths.profile_dir.join("foreign-backup");
        fs::create_dir_all(&external).unwrap();
        let sentinel = external.join("preserve");
        fs::write(&sentinel, "foreign-content").unwrap();
        symlink(&external, &shadow_path).unwrap();
        store.index_build_tokens.remove(&generation);
        store
            .conn
            .execute(
                "UPDATE index_build_leases SET owner_pid = -1 WHERE generation = ?1",
                params![generation],
            )
            .unwrap();

        let error = store
            .purge(
                PurgeTarget::Source {
                    source_id: source_id.to_string(),
                },
                PurgeTrigger::ConfirmedDelete,
            )
            .unwrap_err();

        assert_eq!(error.code, "purge.failed");
        assert!(!error
            .message
            .contains(&external.to_string_lossy().to_string()));
        assert!(sentinel.exists());
        assert!(fs::symlink_metadata(&shadow_path)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(!generation_path.exists());
        assert_eq!(
            store.pending_purges().unwrap()[0].failure_stage,
            Some(PurgeFailureStage::Tantivy)
        );

        fs::remove_file(&shadow_path).unwrap();
        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn purge_fences_legacy_index_publish_and_removes_orphan() {
        let paths = temp_profile_paths("purge-stale-legacy-index-publish");
        let mut stale_builder = Store::open(&paths).unwrap();
        let mut purger = Store::open(&paths).unwrap();
        let source_id = "qgh://github.com/issue/I_PURGE_LEGACY_PUBLISH";
        stale_builder
            .upsert_sources_for_run(
                "sync-stale-legacy-publish",
                &[test_issue(source_id, "owner/repo", "legacy-publish-marker")],
                &[],
                0,
                &[],
            )
            .unwrap();
        seal_latest_test_sync(&mut stale_builder);
        let (generation, generation_path) = stale_builder
            .reserve_index_generation(&paths.index_root, 1)
            .unwrap();
        stale_builder
            .rebuild_reserved_index_generation(
                generation,
                &stale_builder.active_index_sources().unwrap(),
            )
            .unwrap();
        purger
            .purge(
                PurgeTarget::Source {
                    source_id: source_id.to_string(),
                },
                PurgeTrigger::ConfirmedDelete,
            )
            .unwrap_err();
        let error = stale_builder
            .mark_index_published(generation, &generation_path.to_string_lossy(), 1)
            .unwrap_err();

        assert_eq!(error.code, "purge.write_fenced");
        assert!(!generation_path.exists());
        purger.retry_pending_purges().unwrap();

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn purge_completion_fences_store_opened_while_pending() {
        let paths = temp_profile_paths("purge-stale-writer-during");
        let mut purger = Store::open(&paths).unwrap();
        let target_id = "qgh://github.com/issue/I_PURGE_STALE_WRITER_DURING";
        purger
            .upsert_sources_for_run(
                "sync-stale-writer-during",
                &[test_issue(target_id, "owner/repo", "during-purge")],
                &[],
                0,
                &[],
            )
            .unwrap();
        purger.fail_next_purge_at(PurgeFailureStage::Storage);
        purger
            .purge(
                PurgeTarget::Source {
                    source_id: target_id.to_string(),
                },
                PurgeTrigger::ConfirmedDelete,
            )
            .unwrap_err();
        let mut opened_while_pending = Store::open(&paths).unwrap();

        purger.retry_pending_purges().unwrap();

        let error = opened_while_pending
            .upsert_sources_for_run(
                "sync-opened-during-after",
                &[test_issue(
                    "qgh://github.com/issue/I_UNRELATED_STALE_WRITER",
                    "owner/other",
                    "stale-during-marker",
                )],
                &[],
                0,
                &[],
            )
            .unwrap_err();
        assert_eq!(error.code, "purge.write_fenced");

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn purge_fences_stale_chunk_writer_on_second_connection() {
        let paths = temp_profile_paths("purge-stale-chunk-writer");
        let mut stale_writer = Store::open(&paths).unwrap();
        stale_writer.enable_vector().unwrap();
        let mut purger = Store::open(&paths).unwrap();
        purger.enable_vector().unwrap();
        let target_id = "qgh://github.com/issue/I_PURGE_CHUNK_TARGET";
        let retained_id = "qgh://github.com/issue/I_PURGE_CHUNK_RETAINED";
        stale_writer
            .upsert_sources_for_run(
                "sync-stale-chunk-writer",
                &[
                    test_issue(target_id, "owner/repo", "chunk-target"),
                    test_issue(retained_id, "owner/other", "chunk-retained"),
                ],
                &[],
                0,
                &[],
            )
            .unwrap();
        let retained_version = stale_writer
            .latest_source_version_id(retained_id)
            .unwrap()
            .unwrap();
        purger
            .purge(
                PurgeTarget::Source {
                    source_id: target_id.to_string(),
                },
                PurgeTrigger::ConfirmedDelete,
            )
            .unwrap();

        let error = stale_writer
            .replace_chunks_for_source_version(
                retained_id,
                retained_version,
                &[test_chunk("stale-chunk-marker")],
            )
            .unwrap_err();

        assert_eq!(error.code, "purge.write_fenced");
        let fresh = Store::open(&paths).unwrap();
        assert!(!fresh.source_version_has_chunks(retained_version).unwrap());

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn purge_discards_unproven_building_generation_and_fences_staging() {
        let paths = temp_profile_paths("purge-stale-embedding-builder");
        let mut builder = Store::open(&paths).unwrap();
        builder.enable_vector().unwrap();
        let mut purger = Store::open(&paths).unwrap();
        purger.enable_vector().unwrap();
        let target_id = "qgh://github.com/issue/I_PURGE_EMBED_BUILD_TARGET";
        let retained_id = "qgh://github.com/issue/I_PURGE_EMBED_BUILD_RETAINED";
        builder
            .upsert_sources_for_run(
                "sync-stale-embedding-builder",
                &[
                    test_issue(target_id, "owner/repo", "embed-build-target"),
                    test_issue(retained_id, "owner/other", "embed-build-retained"),
                ],
                &[],
                0,
                &[],
            )
            .unwrap();
        let retained_chunk = insert_chunk(&mut builder, retained_id, "retained chunk");
        builder
            .mark_sync_run_completed("sync-stale-embedding-builder")
            .unwrap();
        let retained_version = builder
            .latest_source_version_id(retained_id)
            .unwrap()
            .unwrap();
        let snapshot = builder.capture_retrieval_build_snapshot().unwrap().unwrap();
        let generation_id = builder
            .begin_embedding_generation(
                &snapshot,
                &EmbeddingGenerationSpec {
                    model_manifest_hash: "manifest-stale-builder".to_string(),
                    runtime_fingerprint_hash: "runtime-stale-builder".to_string(),
                    chunker_fingerprint: crate::chunking::CHUNKER_FINGERPRINT.to_string(),
                    context_template_version: crate::context::METADATA_CONTEXT_TEMPLATE_VERSION
                        .to_string(),
                    output_dimension: 2,
                },
            )
            .unwrap();

        purger
            .purge(
                PurgeTarget::Source {
                    source_id: target_id.to_string(),
                },
                PurgeTrigger::ConfirmedDelete,
            )
            .unwrap();

        let error = builder
            .stage_embedding_generation_batch(
                generation_id,
                &[EmbeddingGenerationChunk {
                    chunk_id: retained_chunk,
                    source_version_id: retained_version,
                    source_version_hash: "embed-build-retained".to_string(),
                    context_hash: "stale-context".to_string(),
                    vector: vec![1.0, 2.0],
                }],
            )
            .unwrap_err();
        assert_eq!(error.code, "purge.write_fenced");
        let fresh = Store::open(&paths).unwrap();
        assert!(fresh.embedding_generation_state(generation_id).is_err());

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn purge_fences_stale_embedding_generation_validation() {
        let paths = temp_profile_paths("purge-stale-embedding-validation");
        let mut builder = Store::open(&paths).unwrap();
        builder.enable_vector().unwrap();
        let mut purger = Store::open(&paths).unwrap();
        purger.enable_vector().unwrap();
        let target_id = "qgh://github.com/issue/I_PURGE_VALIDATE_TARGET";
        let retained_id = "qgh://github.com/issue/I_PURGE_VALIDATE_RETAINED";
        builder
            .upsert_sources_for_run(
                "sync-stale-embedding-validation",
                &[
                    test_issue(target_id, "owner/repo", "validate-target"),
                    test_issue(retained_id, "owner/other", "validate-retained"),
                ],
                &[],
                0,
                &[],
            )
            .unwrap();
        let chunk_id = insert_chunk(&mut builder, retained_id, "validate retained chunk");
        seal_latest_test_sync(&mut builder);
        let snapshot = builder.capture_retrieval_build_snapshot().unwrap().unwrap();
        let source_version_id = builder
            .latest_source_version_id(retained_id)
            .unwrap()
            .unwrap();
        let source_version_hash = builder
            .source_version_hash(source_version_id)
            .unwrap()
            .unwrap();
        let manifest = "manifest-stale-validation";
        let generation_id = builder
            .begin_embedding_generation(
                &snapshot,
                &EmbeddingGenerationSpec {
                    model_manifest_hash: manifest.to_string(),
                    runtime_fingerprint_hash: format!("runtime-{manifest}"),
                    chunker_fingerprint: crate::chunking::CHUNKER_FINGERPRINT.to_string(),
                    context_template_version: crate::context::METADATA_CONTEXT_TEMPLATE_VERSION
                        .to_string(),
                    output_dimension: 2,
                },
            )
            .unwrap();
        builder
            .stage_embedding_generation_batch(
                generation_id,
                &[EmbeddingGenerationChunk {
                    chunk_id,
                    source_version_id,
                    source_version_hash,
                    context_hash: production_context_hash_for_chunk(
                        &builder,
                        manifest,
                        crate::chunking::CHUNKER_FINGERPRINT,
                        chunk_id,
                    ),
                    vector: vec![1.0, 2.0],
                }],
            )
            .unwrap();
        purger
            .purge(
                PurgeTarget::Source {
                    source_id: target_id.to_string(),
                },
                PurgeTrigger::ConfirmedDelete,
            )
            .unwrap();

        let error = builder
            .validate_embedding_generation(generation_id)
            .unwrap_err();
        assert_eq!(error.code, "purge.write_fenced");

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn purge_fences_stale_legacy_embedding_writer_before_vec0_setup() {
        let paths = temp_profile_paths("purge-stale-legacy-embedding");
        let mut stale_writer = Store::open(&paths).unwrap();
        stale_writer.enable_vector().unwrap();
        let mut purger = Store::open(&paths).unwrap();
        purger.enable_vector().unwrap();
        let target_id = "qgh://github.com/issue/I_PURGE_LEGACY_EMBED_TARGET";
        let retained_id = "qgh://github.com/issue/I_PURGE_LEGACY_EMBED_RETAINED";
        stale_writer
            .upsert_sources_for_run(
                "sync-stale-legacy-embedding",
                &[
                    test_issue(target_id, "owner/repo", "legacy-embed-target"),
                    test_issue(retained_id, "owner/other", "legacy-embed-retained"),
                ],
                &[],
                0,
                &[],
            )
            .unwrap();
        let retained_chunk = insert_chunk(&mut stale_writer, retained_id, "legacy retained");
        purger
            .purge(
                PurgeTarget::Source {
                    source_id: target_id.to_string(),
                },
                PurgeTrigger::ConfirmedDelete,
            )
            .unwrap();

        let error = stale_writer
            .replace_all_chunk_embeddings(
                &embedding_fingerprint("Example/stale-legacy-model"),
                &[(retained_chunk, vec![0.1, 0.2, 0.3])],
            )
            .unwrap_err();

        assert_eq!(error.code, "purge.write_fenced");
        let fresh = Store::open(&paths).unwrap();
        let count: i64 = fresh
            .conn
            .query_row("SELECT count(*) FROM chunk_embeddings", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0);

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn purge_removes_sensitive_rows_chunks_and_legacy_vectors() {
        let paths = temp_profile_paths("purge-sensitive-storage");
        let mut store = Store::open(&paths).unwrap();
        store.enable_vector().unwrap();
        let source_id = "qgh://github.com/issue/I_PURGE_STORAGE";
        let comment_id = "qgh://github.com/issue-comment/IC_PURGE_STORAGE";
        let marker = "PRIVATE_PURGE_STORAGE_MARKER_9f4c";
        store
            .upsert_sources_for_run(
                "sync-purge-storage",
                &[test_issue(source_id, "owner/repo", marker)],
                &[test_comment(comment_id, source_id, "owner/repo", marker)],
                0,
                &[],
            )
            .unwrap();
        let source_version_id = store.latest_source_version_id(source_id).unwrap().unwrap();
        let chunks = store
            .replace_chunks_for_source_version(source_id, source_version_id, &[test_chunk(marker)])
            .unwrap();
        store
            .replace_all_chunk_embeddings(
                &embedding_fingerprint("Example/purge-model"),
                &[(chunks[0].chunk_id, vec![0.1, 0.2, 0.3])],
            )
            .unwrap();
        let wal_path = PathBuf::from(format!("{}-wal", paths.db_path.display()));
        assert!(fs::metadata(&wal_path).unwrap().len() > 0);

        let outcome = store
            .purge(
                PurgeTarget::Repository {
                    repo: "owner/repo".to_string(),
                },
                PurgeTrigger::AllowlistRemoval,
            )
            .unwrap();

        assert_eq!(outcome.purged_sources, 2);
        assert_eq!(outcome.purged_issues, 1);
        assert_eq!(outcome.purged_comments, 1);
        assert!(outcome.sensitive_wal_truncated);
        assert!(store.get_source(source_id).unwrap().is_none());
        assert!(store.get_source(comment_id).unwrap().is_none());
        for table in [
            "issue_metadata",
            "comment_metadata",
            "source_versions",
            "source_aliases",
            "chunks",
            "chunk_embeddings",
        ] {
            let count: i64 = store
                .conn
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 0, "sensitive rows remain in {table}");
        }
        assert_eq!(vector_row_count(&store.conn), 0);
        assert!(store.pending_purges().unwrap().is_empty());
        let secure_delete: i64 = store
            .conn
            .pragma_query_value(None, "secure_delete", |row| row.get(0))
            .unwrap();
        assert_eq!(secure_delete, 1);
        if wal_path.exists() {
            // Finalizing the content-free completion marker may create a new
            // WAL frame after the sensitive WAL was truncated.
            assert!(!fs::read(&wal_path)
                .unwrap()
                .windows(marker.len())
                .any(|bytes| bytes == marker.as_bytes()));
        }

        drop(store);
        let db_bytes = fs::read(&paths.db_path).unwrap();
        assert!(!db_bytes
            .windows(marker.len())
            .any(|bytes| bytes == marker.as_bytes()));
        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(not(feature = "vector-search"))]
    #[test]
    fn bm25_only_purge_clears_persisted_qgh_vec0_shadow_payloads() {
        let paths = temp_profile_paths("purge-bm25-vec0-shadows");
        let mut store = Store::open(&paths).unwrap();
        let source_id = "qgh://github.com/issue/I_PURGE_BM25_VEC0";
        let marker = b"PRIVATE_BM25_VEC0_PAYLOAD_813c";
        store
            .upsert_sources_for_run(
                "sync-purge-bm25-vec0",
                &[test_issue(source_id, "owner/repo", "bm25-vec0-private")],
                &[],
                0,
                &[],
            )
            .unwrap();
        store
            .conn
            .execute_batch(&format!(
                "CREATE TABLE chunks (id INTEGER PRIMARY KEY, source_id TEXT NOT NULL);
                 INSERT INTO chunks (id, source_id) VALUES (41, '{source_id}');
                 CREATE TABLE {CHUNK_EMBEDDING_VECTOR_CHUNKS_META_TABLE} (
                    chunk_id INTEGER PRIMARY KEY, size INTEGER NOT NULL,
                    validity BLOB NOT NULL, rowids BLOB NOT NULL
                 );
                 CREATE TABLE {CHUNK_EMBEDDING_VECTOR_ROWIDS_TABLE} (
                    rowid INTEGER PRIMARY KEY, id, chunk_id INTEGER, chunk_offset INTEGER
                 );
                 CREATE TABLE {CHUNK_EMBEDDING_VECTOR_CHUNKS_TABLE} (
                    rowid PRIMARY KEY, vectors BLOB NOT NULL
                 );"
            ))
            .unwrap();
        store
            .conn
            .execute(
                &format!(
                    "INSERT INTO {CHUNK_EMBEDDING_VECTOR_CHUNKS_META_TABLE}
                        (chunk_id, size, validity, rowids)
                     VALUES (1, 1, ?1, ?1)"
                ),
                params![marker.as_slice()],
            )
            .unwrap();
        store
            .conn
            .execute(
                &format!(
                    "INSERT INTO {CHUNK_EMBEDDING_VECTOR_ROWIDS_TABLE}
                        (rowid, id, chunk_id, chunk_offset)
                     VALUES (41, 41, 1, 0)"
                ),
                [],
            )
            .unwrap();
        store
            .conn
            .execute(
                &format!(
                    "INSERT INTO {CHUNK_EMBEDDING_VECTOR_CHUNKS_TABLE} (rowid, vectors)
                     VALUES (1, ?1)"
                ),
                params![marker.as_slice()],
            )
            .unwrap();
        drop(store);
        let mut store = Store::open(&paths).unwrap();

        store
            .purge(
                PurgeTarget::Source {
                    source_id: source_id.to_string(),
                },
                PurgeTrigger::ConfirmedDelete,
            )
            .unwrap();

        for table in [
            CHUNK_EMBEDDING_VECTOR_CHUNKS_META_TABLE,
            CHUNK_EMBEDDING_VECTOR_ROWIDS_TABLE,
            CHUNK_EMBEDDING_VECTOR_CHUNKS_TABLE,
        ] {
            let count: i64 = store
                .conn
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 0, "vec0 shadow payload remained in {table}");
            assert!(table_exists(&store.conn, table).unwrap());
        }
        drop(store);
        let db = fs::read(&paths.db_path).unwrap();
        assert!(!db.windows(marker.len()).any(|bytes| bytes == marker));

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(not(feature = "vector-search"))]
    #[test]
    fn bm25_only_purge_preserves_proven_target_free_generation_shadow_rows() {
        let paths = temp_profile_paths("purge-bm25-targeted-generation-shadows");
        let mut store = Store::open(&paths).unwrap();
        let target_id = "qgh://github.com/issue/I_PURGE_BM25_GEN_TARGET";
        let other_id = "qgh://github.com/issue/I_PURGE_BM25_GEN_OTHER";
        store
            .upsert_sources_for_run(
                "sync-purge-bm25-generation-shadows",
                &[
                    test_issue(target_id, "owner/target", "target-generation-private"),
                    test_issue(other_id, "owner/other", "other-generation-safe"),
                ],
                &[],
                0,
                &[],
            )
            .unwrap();
        let target_version = store.latest_source_version_id(target_id).unwrap().unwrap();
        let other_version = store.latest_source_version_id(other_id).unwrap().unwrap();
        let vector_table = generation_vector_table_name(2);
        let shadow_chunks = format!("{vector_table}_chunks");
        let shadow_rowids = format!("{vector_table}_rowids");
        let shadow_vectors = format!("{vector_table}_vector_chunks00");
        store
            .conn
            .execute_batch(&format!(
                "CREATE TABLE chunks (
                    id INTEGER PRIMARY KEY, source_id TEXT NOT NULL,
                    source_version_id INTEGER NOT NULL
                 );
                 INSERT INTO chunks (id, source_id, source_version_id)
                    VALUES (41, '{target_id}', {target_version});
                 INSERT INTO chunks (id, source_id, source_version_id)
                    VALUES (42, '{other_id}', {other_version});
                 CREATE TABLE embedding_generations (
                    id INTEGER PRIMARY KEY, state TEXT NOT NULL,
                    output_dimension INTEGER NOT NULL,
                    write_epoch INTEGER NOT NULL DEFAULT 0
                 );
                 INSERT INTO embedding_generations (id, state, output_dimension)
                    VALUES (1, 'ready', 2);
                 INSERT INTO embedding_generations (id, state, output_dimension)
                    VALUES (2, 'ready', 2);
                 CREATE TABLE embedding_generation_chunks (
                    generation_id INTEGER NOT NULL, chunk_id INTEGER NOT NULL,
                    source_version_id INTEGER NOT NULL
                 );
                 INSERT INTO embedding_generation_chunks VALUES (1, 41, {target_version});
                 INSERT INTO embedding_generation_chunks VALUES (2, 42, {other_version});
                 CREATE TABLE embedding_generation_vector_rows (
                    id INTEGER PRIMARY KEY, generation_id INTEGER NOT NULL,
                    chunk_id INTEGER NOT NULL, dimension INTEGER NOT NULL,
                    vector_table TEXT NOT NULL, vector_rowid INTEGER NOT NULL
                 );
                 INSERT INTO embedding_generation_vector_rows
                    VALUES (101, 1, 41, 2, '{vector_table}', 101);
                 INSERT INTO embedding_generation_vector_rows
                    VALUES (102, 2, 42, 2, '{vector_table}', 102);
                 CREATE TABLE {shadow_chunks} (
                    chunk_id INTEGER PRIMARY KEY, size INTEGER NOT NULL,
                    validity BLOB NOT NULL, rowids BLOB NOT NULL
                 );
                 CREATE TABLE {shadow_rowids} (
                    rowid INTEGER PRIMARY KEY, id, chunk_id INTEGER, chunk_offset INTEGER
                 );
                 INSERT INTO {shadow_rowids} VALUES (101, 101, 1, 0);
                 INSERT INTO {shadow_rowids} VALUES (102, 102, 1, 1);
                 CREATE TABLE {shadow_vectors} (
                    rowid PRIMARY KEY, vectors BLOB NOT NULL
                 );"
            ))
            .unwrap();
        let target_marker = b"SECRETV!";
        let other_vector = encode_embedding_blob(&[0.3, 0.4]);
        let mut rowids_blob = Vec::new();
        rowids_blob.extend_from_slice(&101_i64.to_ne_bytes());
        rowids_blob.extend_from_slice(&102_i64.to_ne_bytes());
        let mut vectors_blob = target_marker.to_vec();
        vectors_blob.extend_from_slice(&other_vector);
        store
            .conn
            .execute(
                &format!(
                    "INSERT INTO {shadow_chunks} (chunk_id, size, validity, rowids)
                     VALUES (1, 2, ?1, ?2)"
                ),
                params![vec![0b0000_0011_u8], rowids_blob],
            )
            .unwrap();
        store
            .conn
            .execute(
                &format!("INSERT INTO {shadow_vectors} (rowid, vectors) VALUES (1, ?1)"),
                params![vectors_blob],
            )
            .unwrap();
        drop(store);
        let mut store = Store::open(&paths).unwrap();

        store
            .purge(
                PurgeTarget::Source {
                    source_id: target_id.to_string(),
                },
                PurgeTrigger::ConfirmedDelete,
            )
            .unwrap();

        assert!(store.embedding_generation_state(1).is_err());
        assert_eq!(store.embedding_generation_state(2).unwrap(), "ready");
        let other_mapping_count: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM embedding_generation_vector_rows
                 WHERE generation_id = 2 AND vector_rowid = 102",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(other_mapping_count, 1);
        let target_shadow_count: i64 = store
            .conn
            .query_row(
                &format!("SELECT count(*) FROM {shadow_rowids} WHERE rowid = 101"),
                [],
                |row| row.get(0),
            )
            .unwrap();
        let other_shadow_count: i64 = store
            .conn
            .query_row(
                &format!("SELECT count(*) FROM {shadow_rowids} WHERE rowid = 102"),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(target_shadow_count, 0);
        assert_eq!(other_shadow_count, 1);
        let (validity, rowids, vectors): (Vec<u8>, Vec<u8>, Vec<u8>) = store
            .conn
            .query_row(
                &format!(
                    "SELECT c.validity, c.rowids, v.vectors
                     FROM {shadow_chunks} c JOIN {shadow_vectors} v ON v.rowid = c.chunk_id
                     WHERE c.chunk_id = 1"
                ),
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(validity, vec![0b0000_0010_u8]);
        assert_eq!(&rowids[..8], &[0_u8; 8]);
        assert_eq!(&rowids[8..16], &102_i64.to_ne_bytes());
        assert_eq!(&vectors[..8], &[0_u8; 8]);
        assert_eq!(
            decode_embedding_blob(&vectors[8..16], 2).unwrap(),
            vec![0.3, 0.4]
        );
        drop(store);
        let db = fs::read(&paths.db_path).unwrap();
        assert!(!db
            .windows(target_marker.len())
            .any(|bytes| bytes == target_marker));
        let wal_path = PathBuf::from(format!("{}-wal", paths.db_path.display()));
        if wal_path.exists() {
            let wal = fs::read(wal_path).unwrap();
            assert!(!wal
                .windows(target_marker.len())
                .any(|bytes| bytes == target_marker));
        }

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(not(feature = "vector-search"))]
    #[test]
    fn bm25_shadow_delete_fails_closed_on_corrupt_rowid_ownership() {
        let conn = Connection::open_in_memory().unwrap();
        let base = generation_vector_table_name(2);
        let chunks = format!("{base}_chunks");
        let rowids = format!("{base}_rowids");
        let vectors = format!("{base}_vector_chunks00");
        conn.execute_batch(&format!(
            "CREATE TABLE {chunks} (
                chunk_id INTEGER PRIMARY KEY, size INTEGER NOT NULL,
                validity BLOB NOT NULL, rowids BLOB NOT NULL
             );
             CREATE TABLE {rowids} (
                rowid INTEGER PRIMARY KEY, id, chunk_id INTEGER, chunk_offset INTEGER
             );
             INSERT INTO {rowids} VALUES (101, 101, 1, 0);
             CREATE TABLE {vectors} (rowid PRIMARY KEY, vectors BLOB NOT NULL);"
        ))
        .unwrap();
        conn.execute(
            &format!(
                "INSERT INTO {chunks} (chunk_id, size, validity, rowids)
                 VALUES (1, 1, ?1, ?2)"
            ),
            params![vec![1_u8], 999_i64.to_ne_bytes().to_vec()],
        )
        .unwrap();
        conn.execute(
            &format!("INSERT INTO {vectors} (rowid, vectors) VALUES (1, ?1)"),
            params![b"PRIVATE!".as_slice()],
        )
        .unwrap();

        let error = delete_vec0_shadow_row(&conn, &base, 2, 101).unwrap_err();

        assert_eq!(error.code, "purge.failed");
        assert!(!serde_json::to_string(&error).unwrap().contains("PRIVATE"));
        let payload: Vec<u8> = conn
            .query_row(&format!("SELECT vectors FROM {vectors}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(payload, b"PRIVATE!");
        assert!(delete_vec0_shadow_row(&conn, &base, 2, 404).is_err());
    }

    #[cfg(not(feature = "vector-search"))]
    #[test]
    fn bm25_only_purge_rejects_unowned_generation_shadow_mapping_and_retries_after_repair() {
        let paths = temp_profile_paths("purge-bm25-unowned-generation-shadow");
        let mut store = Store::open(&paths).unwrap();
        let target_id = "qgh://github.com/issue/I_PURGE_BM25_UNOWNED_TARGET";
        let retained_id = "qgh://github.com/issue/I_PURGE_BM25_UNOWNED_RETAINED";
        let private_marker = "PRIVATE_BM25_UNOWNED_MAPPING_45d1";
        store
            .upsert_sources_for_run(
                "sync-purge-bm25-unowned-generation-shadow",
                &[
                    test_issue(target_id, "owner/target", private_marker),
                    test_issue(retained_id, "owner/retained", "retained-safe"),
                ],
                &[],
                0,
                &[],
            )
            .unwrap();
        let target_version = store.latest_source_version_id(target_id).unwrap().unwrap();
        let retained_version = store
            .latest_source_version_id(retained_id)
            .unwrap()
            .unwrap();
        let vector_table = generation_vector_table_name(2);
        let shadow_chunks = format!("{vector_table}_chunks");
        let shadow_rowids = format!("{vector_table}_rowids");
        let shadow_vectors = format!("{vector_table}_vector_chunks00");
        store
            .conn
            .execute_batch(&format!(
                "CREATE TABLE chunks (
                    id INTEGER PRIMARY KEY, source_id TEXT NOT NULL,
                    source_version_id INTEGER NOT NULL
                 );
                 INSERT INTO chunks VALUES (41, '{target_id}', {target_version});
                 INSERT INTO chunks VALUES (42, '{retained_id}', {retained_version});
                 CREATE TABLE embedding_generations (
                    id INTEGER PRIMARY KEY, state TEXT NOT NULL,
                    output_dimension INTEGER NOT NULL, write_epoch INTEGER NOT NULL DEFAULT 0
                 );
                 INSERT INTO embedding_generations VALUES (1, 'ready', 2, 0);
                 INSERT INTO embedding_generations VALUES (2, 'ready', 2, 0);
                 CREATE TABLE embedding_generation_chunks (
                    generation_id INTEGER NOT NULL, chunk_id INTEGER NOT NULL,
                    source_version_id INTEGER NOT NULL
                 );
                 INSERT INTO embedding_generation_chunks VALUES (1, 41, {target_version});
                 INSERT INTO embedding_generation_chunks VALUES (2, 42, {retained_version});
                 CREATE TABLE embedding_generation_vector_rows (
                    id INTEGER PRIMARY KEY, generation_id INTEGER NOT NULL,
                    chunk_id INTEGER NOT NULL, dimension INTEGER NOT NULL,
                    vector_table TEXT NOT NULL, vector_rowid INTEGER NOT NULL
                 );
                 INSERT INTO embedding_generation_vector_rows
                    VALUES (101, 1, 41, 2, '{vector_table}', 102);
                 INSERT INTO embedding_generation_vector_rows
                    VALUES (102, 2, 42, 2, '{vector_table}', 102);
                 CREATE TABLE {shadow_chunks} (
                    chunk_id INTEGER PRIMARY KEY, size INTEGER NOT NULL,
                    validity BLOB NOT NULL, rowids BLOB NOT NULL
                 );
                 CREATE TABLE {shadow_rowids} (
                    rowid INTEGER PRIMARY KEY, id, chunk_id INTEGER, chunk_offset INTEGER
                 );
                 INSERT INTO {shadow_rowids} VALUES (101, 101, 1, 0);
                 INSERT INTO {shadow_rowids} VALUES (102, 102, 1, 1);
                 CREATE TABLE {shadow_vectors} (
                    rowid PRIMARY KEY, vectors BLOB NOT NULL
                 );"
            ))
            .unwrap();
        let target_vector = encode_embedding_blob(&[0.1, 0.2]);
        let retained_vector = encode_embedding_blob(&[0.3, 0.4]);
        let mut rowids_blob = Vec::new();
        rowids_blob.extend_from_slice(&101_i64.to_ne_bytes());
        rowids_blob.extend_from_slice(&102_i64.to_ne_bytes());
        let mut vectors_blob = target_vector.clone();
        vectors_blob.extend_from_slice(&retained_vector);
        store
            .conn
            .execute(
                &format!(
                    "INSERT INTO {shadow_chunks} (chunk_id, size, validity, rowids)
                     VALUES (1, 2, ?1, ?2)"
                ),
                params![vec![0b0000_0011_u8], rowids_blob],
            )
            .unwrap();
        store
            .conn
            .execute(
                &format!("INSERT INTO {shadow_vectors} (rowid, vectors) VALUES (1, ?1)"),
                params![vectors_blob],
            )
            .unwrap();

        let error = store
            .purge(
                PurgeTarget::Source {
                    source_id: target_id.to_string(),
                },
                PurgeTrigger::ConfirmedDelete,
            )
            .unwrap_err();

        assert_eq!(error.code, "purge.failed");
        assert!(!serde_json::to_string(&error)
            .unwrap()
            .contains(private_marker));
        assert_eq!(
            store.pending_purges().unwrap()[0].failure_stage,
            Some(PurgeFailureStage::Storage)
        );
        assert_eq!(store.embedding_generation_state(1).unwrap(), "ready");
        assert_eq!(store.embedding_generation_state(2).unwrap(), "ready");
        let mapping_count: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM embedding_generation_vector_rows",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(mapping_count, 2);
        let retained_shadow_count: i64 = store
            .conn
            .query_row(
                &format!("SELECT count(*) FROM {shadow_rowids} WHERE rowid = 102"),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retained_shadow_count, 1);

        store
            .conn
            .execute(
                "UPDATE embedding_generation_vector_rows
                 SET vector_rowid = id WHERE generation_id = 1",
                [],
            )
            .unwrap();
        store.retry_pending_purges().unwrap();

        assert!(store.embedding_generation_state(1).is_err());
        assert_eq!(store.embedding_generation_state(2).unwrap(), "ready");
        let retained_shadow_count: i64 = store
            .conn
            .query_row(
                &format!("SELECT count(*) FROM {shadow_rowids} WHERE rowid = 102"),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retained_shadow_count, 1);
        assert!(store.pending_purges().unwrap().is_empty());

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn purge_discards_active_and_previous_embedding_generations_whole() {
        let paths = temp_profile_paths("purge-embedding-generations");
        let mut store = Store::open(&paths).unwrap();
        store.enable_vector().unwrap();
        let target_id = "qgh://github.com/issue/I_PURGE_GENERATION_TARGET";
        let other_id = "qgh://github.com/issue/I_PURGE_GENERATION_OTHER";
        store
            .upsert_sources_for_run(
                "sync-purge-generation-other",
                &[test_issue(
                    other_id,
                    "owner/repo",
                    "other-generation-marker",
                )],
                &[],
                0,
                &[],
            )
            .unwrap();
        let other_chunk = insert_chunk(&mut store, other_id, "other generation chunk");
        let target_free_generation =
            stage_test_generation(&mut store, "manifest-purge-target-free", &[other_chunk]);
        store
            .upsert_sources_for_run(
                "sync-purge-generation",
                &[
                    test_issue(target_id, "owner/repo", "target-generation-marker"),
                    test_issue(other_id, "owner/repo", "other-generation-marker"),
                ],
                &[],
                0,
                &[],
            )
            .unwrap();
        let target_chunk = insert_chunk(&mut store, target_id, "target generation chunk");
        let first_generation = stage_test_generation(
            &mut store,
            "manifest-purge-first",
            &[target_chunk, other_chunk],
        );
        let (first_tantivy_generation, _) = store
            .reserve_index_generation(&paths.index_root, 2)
            .unwrap();
        rebuild_reserved_generation(&store, &paths, first_tantivy_generation);
        let first_publication = store
            .activate_retrieval_publication(
                "sync-purge-generation",
                first_tantivy_generation,
                Some(first_generation),
                None,
            )
            .unwrap();
        let second_generation = stage_test_generation(
            &mut store,
            "manifest-purge-second",
            &[target_chunk, other_chunk],
        );
        let (second_tantivy_generation, _) = store
            .reserve_index_generation(&paths.index_root, 2)
            .unwrap();
        rebuild_reserved_generation(&store, &paths, second_tantivy_generation);
        store
            .activate_retrieval_publication(
                "sync-purge-generation",
                second_tantivy_generation,
                Some(second_generation),
                Some(first_publication),
            )
            .unwrap();
        let outcome = store
            .purge(
                PurgeTarget::Source {
                    source_id: target_id.to_string(),
                },
                PurgeTrigger::ConfirmedDelete,
            )
            .unwrap();

        assert_eq!(outcome.discarded_embedding_generations, 2);
        assert!(store.embedding_generation_state(first_generation).is_err());
        assert!(store.embedding_generation_state(second_generation).is_err());
        assert_eq!(
            store
                .embedding_generation_state(target_free_generation)
                .unwrap(),
            "ready"
        );
        assert!(store.active_retrieval_publication().unwrap().is_none());
        for table in [
            "embedding_generation_chunks",
            "embedding_generation_vector_rows",
        ] {
            let count: i64 = store
                .conn
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(
                count, 1,
                "target-free generation was not preserved in {table}"
            );
        }
        let table = generation_vector_table_name(2);
        let count: i64 = store
            .conn
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            count, 1,
            "target-free vec0 row was not preserved in {table}"
        );

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn purge_rejects_unowned_generation_vector_mapping_and_retries_after_repair() {
        let paths = temp_profile_paths("purge-unowned-generation-vector");
        let mut store = Store::open(&paths).unwrap();
        store.enable_vector().unwrap();
        let target_id = "qgh://github.com/issue/I_PURGE_UNOWNED_VECTOR_TARGET";
        let retained_id = "qgh://github.com/issue/I_PURGE_UNOWNED_VECTOR_RETAINED";
        let private_marker = "PRIVATE_UNOWNED_VECTOR_MAPPING_12c8";
        store
            .upsert_sources_for_run(
                "sync-purge-unowned-vector-retained",
                &[test_issue(
                    retained_id,
                    "owner/retained",
                    "retained-vector-safe",
                )],
                &[],
                0,
                &[],
            )
            .unwrap();
        let retained_chunk = insert_chunk(&mut store, retained_id, "retained vector chunk");
        let retained_generation = stage_test_generation(
            &mut store,
            "manifest-purge-unowned-retained",
            &[retained_chunk],
        );
        store
            .upsert_sources_for_run(
                "sync-purge-unowned-vector-target",
                &[
                    test_issue(target_id, "owner/target", private_marker),
                    test_issue(retained_id, "owner/retained", "retained-vector-safe"),
                ],
                &[],
                0,
                &[],
            )
            .unwrap();
        let target_chunk = insert_chunk(&mut store, target_id, private_marker);
        let affected_generation = stage_test_generation(
            &mut store,
            "manifest-purge-unowned-affected",
            &[target_chunk, retained_chunk],
        );
        let retained_mapping_id: i64 = store
            .conn
            .query_row(
                "SELECT id FROM embedding_generation_vector_rows
                 WHERE generation_id = ?1 AND chunk_id = ?2",
                params![retained_generation, retained_chunk],
                |row| row.get(0),
            )
            .unwrap();
        let target_mapping_id: i64 = store
            .conn
            .query_row(
                "SELECT id FROM embedding_generation_vector_rows
                 WHERE generation_id = ?1 AND chunk_id = ?2",
                params![affected_generation, target_chunk],
                |row| row.get(0),
            )
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE embedding_generation_vector_rows
                 SET vector_rowid = ?2 WHERE id = ?1",
                params![target_mapping_id, retained_mapping_id],
            )
            .unwrap();

        let error = store
            .purge(
                PurgeTarget::Source {
                    source_id: target_id.to_string(),
                },
                PurgeTrigger::ConfirmedDelete,
            )
            .unwrap_err();

        assert_eq!(error.code, "purge.failed");
        assert!(!serde_json::to_string(&error)
            .unwrap()
            .contains(private_marker));
        assert_eq!(
            store.pending_purges().unwrap()[0].failure_stage,
            Some(PurgeFailureStage::Storage)
        );
        assert_eq!(
            store
                .embedding_generation_state(affected_generation)
                .unwrap(),
            "ready"
        );
        assert_eq!(
            store
                .embedding_generation_state(retained_generation)
                .unwrap(),
            "ready"
        );
        let vector_table = generation_vector_table_name(2);
        let retained_vector_count: i64 = store
            .conn
            .query_row(
                &format!("SELECT count(*) FROM {vector_table} WHERE rowid = ?1"),
                params![retained_mapping_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retained_vector_count, 1);

        store
            .conn
            .execute(
                "UPDATE embedding_generation_vector_rows
                 SET vector_rowid = id WHERE id = ?1",
                params![target_mapping_id],
            )
            .unwrap();
        store.retry_pending_purges().unwrap();

        assert!(store
            .embedding_generation_state(affected_generation)
            .is_err());
        assert_eq!(
            store
                .embedding_generation_state(retained_generation)
                .unwrap(),
            "ready"
        );
        let retained_vector_count: i64 = store
            .conn
            .query_row(
                &format!("SELECT count(*) FROM {vector_table} WHERE rowid = ?1"),
                params![retained_mapping_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retained_vector_count, 1);
        assert!(store.pending_purges().unwrap().is_empty());

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn purge_discards_content_bearing_tantivy_generations_and_publication() {
        let paths = temp_profile_paths("purge-tantivy-generations");
        let mut store = Store::open(&paths).unwrap();
        let source_id = "qgh://github.com/issue/I_PURGE_TANTIVY";
        store
            .upsert_sources_for_run(
                "sync-purge-tantivy",
                &[test_issue(
                    source_id,
                    "owner/repo",
                    "tantivy-private-marker",
                )],
                &[],
                0,
                &[],
            )
            .unwrap();
        seal_latest_test_sync(&mut store);
        let (first_generation, first_path) = store
            .reserve_index_generation(&paths.index_root, 1)
            .unwrap();
        rebuild_reserved_generation(&store, &paths, first_generation);
        fs::create_dir_all(&paths.index_active).unwrap();
        let backup_path = paths.index_root.join("user-backup-1");
        let generation_backup_path = paths.index_root.join("generation-9001");
        let shadow_backup_path = paths.index_root.join("shadow-9002");
        fs::create_dir_all(&backup_path).unwrap();
        fs::create_dir_all(&generation_backup_path).unwrap();
        fs::create_dir_all(&shadow_backup_path).unwrap();
        let model_artifact = paths.cache_dir.join("models/model.onnx");
        fs::create_dir_all(model_artifact.parent().unwrap()).unwrap();
        fs::write(paths.index_active.join("segment"), "tantivy-private-marker").unwrap();
        fs::write(backup_path.join("keep"), "user-owned-backup").unwrap();
        fs::write(generation_backup_path.join("keep"), "user-owned-backup").unwrap();
        fs::write(shadow_backup_path.join("keep"), "user-owned-backup").unwrap();
        fs::write(&model_artifact, "model-artifact").unwrap();
        let first_publication = store
            .activate_retrieval_publication("sync-purge-tantivy", first_generation, None, None)
            .unwrap();
        let (second_generation, second_path) = store
            .reserve_index_generation(&paths.index_root, 1)
            .unwrap();
        rebuild_reserved_generation(&store, &paths, second_generation);
        store
            .activate_retrieval_publication(
                "sync-purge-tantivy",
                second_generation,
                None,
                Some(first_publication),
            )
            .unwrap();

        let outcome = store
            .purge(
                PurgeTarget::Source {
                    source_id: source_id.to_string(),
                },
                PurgeTrigger::ConfirmedDelete,
            )
            .unwrap();

        assert_eq!(outcome.discarded_tantivy_generations, 2);
        assert_eq!(outcome.purged_sources, 1);
        assert!(!first_path.exists());
        assert!(!second_path.exists());
        assert!(paths.index_active.exists());
        assert!(backup_path.exists());
        assert!(generation_backup_path.exists());
        assert!(shadow_backup_path.exists());
        assert!(model_artifact.exists());
        assert!(store.active_index_generation().unwrap().is_none());
        assert!(store.active_retrieval_publication().unwrap().is_none());
        assert!(store.index_path_for_generation(1).unwrap().is_none());
        assert!(store.index_path_for_generation(2).unwrap().is_none());
        assert!(store.get_source(source_id).unwrap().is_none());
        assert!(store.latest_source_version_id(source_id).unwrap().is_none());
        assert!(!embedding_schema_exists(&store.conn).unwrap());

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn purge_keeps_pending_when_registered_generation_commit_ownership_mismatches() {
        let paths = temp_profile_paths("purge-tantivy-commit-mismatch");
        let mut store = Store::open(&paths).unwrap();
        let source_id = "qgh://github.com/issue/I_PURGE_COMMIT_MISMATCH";
        store
            .upsert_sources_for_run(
                "sync-purge-commit-mismatch",
                &[test_issue(
                    source_id,
                    "owner/repo",
                    "private-commit-mismatch",
                )],
                &[],
                0,
                &[],
            )
            .unwrap();
        seal_latest_test_sync(&mut store);
        let (generation, generation_path) = store
            .reserve_index_generation(&paths.index_root, 1)
            .unwrap();
        rebuild_reserved_generation(&store, &paths, generation);
        let index = tantivy::Index::open_in_dir(&generation_path).unwrap();
        let mut writer = index
            .writer::<tantivy::TantivyDocument>(50_000_000)
            .unwrap();
        let mut commit = writer.prepare_commit().unwrap();
        commit.set_payload("not-qgh-owned");
        commit.commit().unwrap();
        writer.wait_merging_threads().unwrap();

        let error = store
            .purge(
                PurgeTarget::Source {
                    source_id: source_id.to_string(),
                },
                PurgeTrigger::ConfirmedDelete,
            )
            .unwrap_err();

        assert_eq!(error.code, "purge.failed");
        assert!(!error.message.contains("private-commit-mismatch"));
        assert!(generation_path.exists());
        assert_eq!(
            store.pending_purges().unwrap()[0].failure_stage,
            Some(PurgeFailureStage::Tantivy)
        );

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn purge_keeps_pending_and_preserves_foreign_entry_added_after_publication() {
        let paths = temp_profile_paths("purge-tantivy-foreign-entry");
        let mut store = Store::open(&paths).unwrap();
        let source_id = "qgh://github.com/issue/I_PURGE_FOREIGN_ENTRY";
        let private_marker = "private-foreign-entry-query";
        store
            .upsert_sources_for_run(
                "sync-purge-foreign-entry",
                &[test_issue(source_id, "owner/repo", private_marker)],
                &[],
                0,
                &[],
            )
            .unwrap();
        seal_latest_test_sync(&mut store);
        let (generation, generation_path) = store
            .reserve_index_generation(&paths.index_root, 1)
            .unwrap();
        rebuild_reserved_generation(&store, &paths, generation);
        store
            .activate_retrieval_publication("sync-purge-foreign-entry", generation, None, None)
            .unwrap();
        let foreign_entry = generation_path.join("foreign-backup-marker");
        fs::write(&foreign_entry, "preserve").unwrap();

        let error = store
            .purge(
                PurgeTarget::Source {
                    source_id: source_id.to_string(),
                },
                PurgeTrigger::ConfirmedDelete,
            )
            .unwrap_err();

        assert_eq!(error.code, "purge.failed");
        assert!(!error.message.contains(private_marker));
        assert!(!error
            .message
            .contains(&generation_path.to_string_lossy().to_string()));
        assert!(foreign_entry.exists());
        assert_eq!(
            store.pending_purges().unwrap()[0].failure_stage,
            Some(PurgeFailureStage::Tantivy)
        );

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn purge_resumes_from_durable_owned_quarantine_after_rename_crash() {
        let paths = temp_profile_paths("purge-tantivy-quarantine-resume");
        let mut store = Store::open(&paths).unwrap();
        let source_id = "qgh://github.com/issue/I_PURGE_QUARANTINE_RESUME";
        store
            .upsert_sources_for_run(
                "sync-purge-quarantine-resume",
                &[test_issue(source_id, "owner/repo", "private-resume-marker")],
                &[],
                0,
                &[],
            )
            .unwrap();
        seal_latest_test_sync(&mut store);
        let (generation, generation_path) = store
            .reserve_index_generation(&paths.index_root, 1)
            .unwrap();
        rebuild_reserved_generation(&store, &paths, generation);
        store
            .activate_retrieval_publication("sync-purge-quarantine-resume", generation, None, None)
            .unwrap();
        let (owner_pid, owner_token): (i64, String) = store
            .conn
            .query_row(
                "SELECT owner_pid, owner_token FROM index_build_leases
                 WHERE generation = ?1",
                params![generation],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(owner_pid, 0);
        let quarantine_path =
            tantivy_purge_quarantine_path(&paths.index_root, generation, &owner_token);
        crate::index::rename_without_replacement(&generation_path, &quarantine_path).unwrap();
        sync_directory(&paths.index_root).unwrap();

        let outcome = store
            .purge(
                PurgeTarget::Source {
                    source_id: source_id.to_string(),
                },
                PurgeTrigger::ConfirmedDelete,
            )
            .unwrap();

        assert_eq!(outcome.discarded_tantivy_generations, 1);
        assert!(!generation_path.exists());
        assert!(!quarantine_path.exists());
        assert!(store.pending_purges().unwrap().is_empty());
        let ownership_count: i64 = store
            .conn
            .query_row("SELECT count(*) FROM index_build_leases", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(ownership_count, 0);

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn purge_detects_generation_swap_after_validation_without_deleting_foreign_tree() {
        let paths = temp_profile_paths("purge-tantivy-generation-swap");
        let mut store = Store::open(&paths).unwrap();
        let source_id = "qgh://github.com/issue/I_PURGE_GENERATION_SWAP";
        let private_marker = "private-generation-swap-query";
        store
            .upsert_sources_for_run(
                "sync-purge-generation-swap",
                &[test_issue(source_id, "owner/repo", private_marker)],
                &[],
                0,
                &[],
            )
            .unwrap();
        seal_latest_test_sync(&mut store);
        let (generation, generation_path) = store
            .reserve_index_generation(&paths.index_root, 1)
            .unwrap();
        rebuild_reserved_generation(&store, &paths, generation);
        store
            .activate_retrieval_publication("sync-purge-generation-swap", generation, None, None)
            .unwrap();
        store.swap_generation_after_purge_validation(generation);

        let error = store
            .purge(
                PurgeTarget::Source {
                    source_id: source_id.to_string(),
                },
                PurgeTrigger::ConfirmedDelete,
            )
            .unwrap_err();

        assert_eq!(error.code, "purge.failed");
        assert!(!error.message.contains(private_marker));
        assert!(generation_path.join("foreign-sentinel").exists());
        assert!(paths
            .index_root
            .join(format!(".qgh-test-displaced-generation-{generation}"))
            .exists());
        assert_eq!(
            store.pending_purges().unwrap()[0].failure_stage,
            Some(PurgeFailureStage::Tantivy)
        );

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn purge_fails_closed_when_quarantine_path_is_swapped_after_fd_open() {
        let paths = temp_profile_paths("purge-tantivy-quarantine-swap");
        let mut store = Store::open(&paths).unwrap();
        let source_id = "qgh://github.com/issue/I_PURGE_QUARANTINE_SWAP";
        let private_marker = "private-quarantine-swap-query";
        store
            .upsert_sources_for_run(
                "sync-purge-quarantine-swap",
                &[test_issue(source_id, "owner/repo", private_marker)],
                &[],
                0,
                &[],
            )
            .unwrap();
        seal_latest_test_sync(&mut store);
        let (generation, _) = store
            .reserve_index_generation(&paths.index_root, 1)
            .unwrap();
        rebuild_reserved_generation(&store, &paths, generation);
        store
            .activate_retrieval_publication("sync-purge-quarantine-swap", generation, None, None)
            .unwrap();
        let owner_token: String = store
            .conn
            .query_row(
                "SELECT owner_token FROM index_build_leases WHERE generation = ?1",
                params![generation],
                |row| row.get(0),
            )
            .unwrap();
        let quarantine_path =
            tantivy_purge_quarantine_path(&paths.index_root, generation, &owner_token);
        store.swap_quarantine_after_purge_open(generation);

        let error = store
            .purge(
                PurgeTarget::Source {
                    source_id: source_id.to_string(),
                },
                PurgeTrigger::ConfirmedDelete,
            )
            .unwrap_err();

        assert_eq!(error.code, "purge.failed");
        assert!(!error.message.contains(private_marker));
        assert!(!error
            .message
            .contains(&quarantine_path.to_string_lossy().to_string()));
        assert!(quarantine_path.join("foreign-sentinel").exists());
        let displaced = paths
            .index_root
            .join(format!(".qgh-test-displaced-quarantine-{generation}"));
        assert!(displaced.exists());
        assert!(fs::read_dir(&displaced).unwrap().next().is_some());
        assert_eq!(
            store.pending_purges().unwrap()[0].failure_stage,
            Some(PurgeFailureStage::Tantivy)
        );

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn mapped_partially_cleaned_tombstone_still_discards_tantivy() {
        let paths = temp_profile_paths("purge-partially-cleaned-tombstone-tantivy");
        let mut store = Store::open(&paths).unwrap();
        let source_id = "qgh://github.com/issue/I_PURGE_PARTIAL_TOMBSTONE";
        let marker = "PRIVATE_PARTIAL_TOMBSTONE_TANTIVY_8a51";
        store
            .upsert_sources_for_run(
                "sync-partially-cleaned-tombstone",
                &[test_issue(source_id, "owner/repo", marker)],
                &[],
                0,
                &[],
            )
            .unwrap();
        seal_latest_test_sync(&mut store);
        let (generation, generation_path) = store
            .reserve_index_generation(&paths.index_root, 1)
            .unwrap();
        rebuild_reserved_generation(&store, &paths, generation);
        store
            .activate_retrieval_publication(
                "sync-partially-cleaned-tombstone",
                generation,
                None,
                None,
            )
            .unwrap();
        store.tombstone_source(source_id, "deleted").unwrap();
        for table in [
            "issue_metadata",
            "source_versions",
            "source_aliases",
            "index_tasks",
        ] {
            store
                .conn
                .execute(
                    &format!("DELETE FROM {table} WHERE source_id = ?1"),
                    params![source_id],
                )
                .unwrap();
        }
        assert!(store.get_tombstone(source_id).unwrap().is_some());
        assert!(generation_path.exists());

        let outcome = store
            .purge(
                PurgeTarget::Source {
                    source_id: source_id.to_string(),
                },
                PurgeTrigger::ConfirmedDelete,
            )
            .unwrap();

        assert_eq!(outcome.discarded_tantivy_generations, 1);
        assert!(!generation_path.exists());
        assert!(store
            .index_path_for_generation(generation)
            .unwrap()
            .is_none());
        assert!(store.pending_purges().unwrap().is_empty());

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(unix)]
    #[test]
    fn purge_rejects_symlinked_index_root_without_touching_external_files() {
        use std::os::unix::fs::symlink;

        let paths = temp_profile_paths("purge-index-root-symlink");
        let external =
            std::env::temp_dir().join(format!("qgh-purge-external-index-{}", now_run_id_suffix()));
        let external_generation = external.join("generation-1");
        fs::create_dir_all(&external_generation).unwrap();
        let external_marker = external_generation.join("private-segment");
        fs::write(&external_marker, "must-survive").unwrap();
        let mut store = Store::open(&paths).unwrap();
        let source_id = "qgh://github.com/issue/I_PURGE_SYMLINK_ROOT";
        store
            .upsert_sources_for_run(
                "sync-purge-symlink-root",
                &[test_issue(source_id, "owner/repo", "symlink-private")],
                &[],
                0,
                &[],
            )
            .unwrap();
        symlink(&external, &paths.index_root).unwrap();

        let error = store
            .purge(
                PurgeTarget::Source {
                    source_id: source_id.to_string(),
                },
                PurgeTrigger::ConfirmedDelete,
            )
            .unwrap_err();

        assert_eq!(error.code, "purge.failed");
        assert!(external_marker.exists());
        assert_eq!(
            store.pending_purges().unwrap()[0].failure_stage,
            Some(PurgeFailureStage::Tantivy)
        );
        fs::remove_file(&paths.index_root).unwrap();
        store.retry_pending_purges().unwrap();
        assert!(external_marker.exists());

        let _ = fs::remove_dir_all(paths.profile_dir);
        let _ = fs::remove_dir_all(external);
    }

    #[test]
    fn purge_mid_stage_failure_retains_safe_pending_state() {
        let paths = temp_profile_paths("purge-mid-stage-failure");
        let mut store = Store::open(&paths).unwrap();
        let source_id = "qgh://github.com/issue/I_PURGE_FAILURE";
        let marker = "PRIVATE_PURGE_FAILURE_MARKER_d8a1";
        store
            .upsert_sources_for_run(
                "sync-purge-failure",
                &[test_issue(source_id, "owner/repo", marker)],
                &[],
                0,
                &[],
            )
            .unwrap();
        store.fail_next_purge_at(PurgeFailureStage::WalCheckpoint);

        let error = store
            .purge(
                PurgeTarget::Source {
                    source_id: source_id.to_string(),
                },
                PurgeTrigger::ConfirmedDelete,
            )
            .unwrap_err();

        assert!(store.get_source(source_id).unwrap().is_none());
        assert_eq!(
            store.pending_purges().unwrap(),
            vec![PendingPurgeView {
                target: PurgeTarget::Source {
                    source_id: source_id.to_string(),
                },
                trigger: PurgeTrigger::ConfirmedDelete,
                current_stage: PurgeFailureStage::WalCheckpoint,
                failure_stage: Some(PurgeFailureStage::WalCheckpoint),
            }]
        );
        let serialized_error = serde_json::to_string(&error).unwrap();
        assert!(!serialized_error.contains(marker));
        assert_eq!(error.code, "purge.failed");

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn completed_purge_retains_only_documented_stable_source_identity() {
        let paths = temp_profile_paths("purge-minimal-identity");
        let mut store = Store::open(&paths).unwrap();
        let source_id = "qgh://github.com/issue/I_PURGE_MINIMAL_IDENTITY";
        store
            .upsert_sources_for_run(
                "sync-purge-minimal-identity",
                &[test_issue(
                    source_id,
                    "owner/repo",
                    "minimal-identity-private",
                )],
                &[],
                0,
                &[],
            )
            .unwrap();

        store
            .purge(
                PurgeTarget::Source {
                    source_id: source_id.to_string(),
                },
                PurgeTrigger::ConfirmedDelete,
            )
            .unwrap();

        let identity = store
            .conn
            .query_row(
                "SELECT source_id, entity_type, host, repo, node_id, github_id, lifecycle_state
                 FROM source_entities WHERE source_id = ?1",
                params![source_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, i64>(5)?,
                        row.get::<_, String>(6)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            identity,
            (
                source_id.to_string(),
                "issue".to_string(),
                "github.com".to_string(),
                "owner/repo".to_string(),
                "I_PURGE_MINIMAL_IDENTITY".to_string(),
                404,
                "tombstoned".to_string(),
            )
        );
        for table in [
            "issue_metadata",
            "comment_metadata",
            "source_versions",
            "source_aliases",
            "index_tasks",
        ] {
            let count: i64 = store
                .conn
                .query_row(
                    &format!("SELECT count(*) FROM {table} WHERE source_id = ?1"),
                    params![source_id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 0, "non-identity state remained in {table}");
        }
        assert_eq!(
            store.get_tombstone(source_id).unwrap().unwrap().reason,
            "deleted"
        );

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn purge_rejects_noncanonical_source_identity_before_state_change() {
        let paths = temp_profile_paths("purge-invalid-source-identity");
        let mut store = Store::open(&paths).unwrap();
        let epoch_before = read_content_write_epoch(&store.conn).unwrap();

        let error = store
            .purge(
                PurgeTarget::Source {
                    source_id: "github.com/owner/repo#47".to_string(),
                },
                PurgeTrigger::ConfirmedDelete,
            )
            .unwrap_err();

        assert_eq!(error.code, "purge.invalid_target");
        assert_eq!(read_content_write_epoch(&store.conn).unwrap(), epoch_before);
        assert!(store.pending_purges().unwrap().is_empty());

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn purge_retry_finishes_idempotently_and_clears_pending() {
        let paths = temp_profile_paths("purge-retry");
        let mut store = Store::open(&paths).unwrap();
        let source_id = "qgh://github.com/issue/I_PURGE_RETRY";
        store
            .upsert_sources_for_run(
                "sync-purge-retry",
                &[test_issue(source_id, "owner/repo", "purge-retry-marker")],
                &[],
                0,
                &[],
            )
            .unwrap();
        store.fail_next_purge_at(PurgeFailureStage::WalCheckpoint);
        store
            .purge(
                PurgeTarget::Source {
                    source_id: source_id.to_string(),
                },
                PurgeTrigger::PermissionLoss,
            )
            .unwrap_err();

        let outcomes = store.retry_pending_purges().unwrap();

        assert_eq!(outcomes.len(), 1);
        assert_eq!(
            outcomes[0].target,
            PurgeTarget::Source {
                source_id: source_id.to_string(),
            }
        );
        assert!(outcomes[0].sensitive_wal_truncated);
        assert!(store.pending_purges().unwrap().is_empty());
        assert!(store.get_source(source_id).unwrap().is_none());
        assert!(store.retry_pending_purges().unwrap().is_empty());

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn purge_retry_attempts_later_targets_after_first_failure() {
        let paths = temp_profile_paths("purge-retry-all-targets");
        let mut store = Store::open(&paths).unwrap();
        let first_id = "qgh://github.com/issue/I_PURGE_RETRY_A";
        let second_id = "qgh://github.com/issue/I_PURGE_RETRY_B";
        store
            .upsert_sources_for_run(
                "sync-purge-retry-all",
                &[
                    test_issue(first_id, "owner/first", "retry-first"),
                    test_issue(second_id, "owner/second", "retry-second"),
                ],
                &[],
                0,
                &[],
            )
            .unwrap();
        for source_id in [first_id, second_id] {
            store.fail_next_purge_at(PurgeFailureStage::Storage);
            store
                .purge(
                    PurgeTarget::Source {
                        source_id: source_id.to_string(),
                    },
                    PurgeTrigger::ConfirmedDelete,
                )
                .unwrap_err();
        }
        assert_eq!(store.pending_purges().unwrap().len(), 2);
        store.fail_next_purge_at(PurgeFailureStage::Storage);

        let error = store.retry_pending_purges().unwrap_err();

        assert_eq!(error.code, "purge.retry_failed");
        let pending = store.pending_purges().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending[0].target,
            PurgeTarget::Source {
                source_id: first_id.to_string(),
            }
        );
        assert!(store.get_tombstone(second_id).unwrap().is_some());

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn purge_finalize_failure_keeps_pending_after_sensitive_wal_truncation() {
        let paths = temp_profile_paths("purge-finalize-failure");
        let mut store = Store::open(&paths).unwrap();
        let source_id = "qgh://github.com/issue/I_PURGE_FINALIZE_FAILURE";
        let marker = "PRIVATE_PURGE_FINALIZE_MARKER_4c2e";
        store
            .upsert_sources_for_run(
                "sync-purge-finalize-failure",
                &[test_issue(source_id, "owner/repo", marker)],
                &[],
                0,
                &[],
            )
            .unwrap();
        store.fail_next_purge_at(PurgeFailureStage::Finalize);

        let error = store
            .purge(
                PurgeTarget::Source {
                    source_id: source_id.to_string(),
                },
                PurgeTrigger::ConfirmedDelete,
            )
            .unwrap_err();

        assert_eq!(error.code, "purge.failed");
        assert_eq!(
            store.pending_purges().unwrap(),
            vec![PendingPurgeView {
                target: PurgeTarget::Source {
                    source_id: source_id.to_string(),
                },
                trigger: PurgeTrigger::ConfirmedDelete,
                current_stage: PurgeFailureStage::Finalize,
                failure_stage: Some(PurgeFailureStage::Finalize),
            }]
        );
        assert!(store.get_source(source_id).unwrap().is_none());
        let wal_path = PathBuf::from(format!("{}-wal", paths.db_path.display()));
        if wal_path.exists() {
            let wal = fs::read(wal_path).unwrap();
            assert!(!wal
                .windows(marker.len())
                .any(|bytes| bytes == marker.as_bytes()));
        }

        drop(store);
        let mut reopened = Store::open(&paths).unwrap();
        reopened.retry_pending_purges().unwrap();
        assert!(reopened.pending_purges().unwrap().is_empty());

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn pending_repository_purge_blocks_reingest_and_preserves_other_repo() {
        let paths = temp_profile_paths("purge-repository-preservation");
        let mut store = Store::open(&paths).unwrap();
        let target_id = "qgh://github.com/issue/I_PURGE_REPO_TARGET";
        let new_target_id = "qgh://github.com/issue/I_PURGE_REPO_NEW";
        let other_id = "qgh://github.com/issue/I_PURGE_REPO_OTHER";
        store
            .upsert_sources_for_run(
                "sync-purge-repo-initial",
                &[
                    test_issue(target_id, "owner/target", "target-before-purge"),
                    test_issue(other_id, "owner/other", "other-before-purge"),
                ],
                &[],
                0,
                &[],
            )
            .unwrap();
        store.fail_next_purge_at(PurgeFailureStage::Storage);
        store
            .purge(
                PurgeTarget::Repository {
                    repo: "owner/target".to_string(),
                },
                PurgeTrigger::AllowlistRemoval,
            )
            .unwrap_err();

        let error = store
            .upsert_sources_for_run(
                "sync-purge-repo-race",
                &[
                    test_issue(target_id, "owner/target", "target-reingested"),
                    test_issue(new_target_id, "owner/target", "new-target-reingested"),
                    test_issue(other_id, "owner/other", "other-updated"),
                ],
                &[],
                0,
                &[],
            )
            .unwrap_err();
        assert_eq!(error.code, "purge.write_fenced");
        let target_body: String = store
            .conn
            .query_row(
                "SELECT body FROM issue_metadata WHERE source_id = ?1",
                params![target_id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!target_body.contains("target-reingested"));
        let new_target_count: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM source_entities WHERE source_id = ?1",
                params![new_target_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(new_target_count, 0);
        assert!(store
            .known_repositories()
            .unwrap()
            .contains(&"owner/target".to_string()));

        let eligible = store.active_index_sources().unwrap();
        assert_eq!(eligible.len(), 1);
        assert_eq!(eligible[0].source_id, other_id);
        store.retry_pending_purges().unwrap();
        assert!(!store
            .known_repositories()
            .unwrap()
            .contains(&"owner/target".to_string()));
        assert!(store.get_source(target_id).unwrap().is_none());
        assert!(store.get_source(new_target_id).unwrap().is_none());
        assert!(store.get_source(other_id).unwrap().is_some());
        store
            .upsert_sources_for_run(
                "sync-purge-repo-other-after-retry",
                &[test_issue(other_id, "owner/other", "other-updated")],
                &[],
                0,
                &[],
            )
            .unwrap();
        let other_body: String = store
            .conn
            .query_row(
                "SELECT body FROM issue_metadata WHERE source_id = ?1",
                params![other_id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(other_body.contains("other-updated"));

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn issue_purge_persists_target_and_cascades_to_all_known_comments() {
        let paths = temp_profile_paths("purge-issue-cascade");
        let mut store = Store::open(&paths).unwrap();
        let issue_id = "qgh://github.com/issue/I_PURGE_ISSUE";
        let first_comment_id = "qgh://github.com/issue-comment/IC_PURGE_ISSUE_1";
        let second_comment_id = "qgh://github.com/issue-comment/IC_PURGE_ISSUE_2";
        let retained_issue_id = "qgh://github.com/issue/I_PURGE_ISSUE_RETAINED";
        let mut retained_issue = test_issue(retained_issue_id, "owner/repo", "retained");
        retained_issue.number = 48;
        retained_issue.canonical_url = "https://github.com/owner/repo/issues/48".to_string();
        store
            .upsert_sources_for_run(
                "sync-purge-issue-cascade",
                &[
                    test_issue(issue_id, "owner/repo", "issue-private"),
                    retained_issue,
                ],
                &[
                    test_comment(
                        first_comment_id,
                        issue_id,
                        "owner/repo",
                        "comment-private-1",
                    ),
                    test_comment(
                        second_comment_id,
                        issue_id,
                        "owner/repo",
                        "comment-private-2",
                    ),
                ],
                0,
                &[],
            )
            .unwrap();
        store.fail_next_purge_at(PurgeFailureStage::Storage);

        store
            .purge(
                PurgeTarget::Issue {
                    repo: "owner/repo".to_string(),
                    issue_number: 47,
                },
                PurgeTrigger::ConfirmedDelete,
            )
            .unwrap_err();

        assert_eq!(
            store.pending_purges().unwrap(),
            vec![PendingPurgeView {
                target: PurgeTarget::Issue {
                    repo: "owner/repo".to_string(),
                    issue_number: 47,
                },
                trigger: PurgeTrigger::ConfirmedDelete,
                current_stage: PurgeFailureStage::Storage,
                failure_stage: Some(PurgeFailureStage::Storage),
            }]
        );
        let outcomes = store.retry_pending_purges().unwrap();
        assert_eq!(outcomes[0].purged_sources, 3);
        assert_eq!(outcomes[0].purged_issues, 1);
        assert_eq!(outcomes[0].purged_comments, 2);
        for source_id in [issue_id, first_comment_id, second_comment_id] {
            assert!(store.get_source(source_id).unwrap().is_none());
            assert_eq!(
                store.get_tombstone(source_id).unwrap().unwrap().reason,
                "deleted"
            );
        }
        assert!(store.get_source(retained_issue_id).unwrap().is_some());

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn confirmed_tombstone_preserves_existing_canonical_reason_and_defaults_transferred() {
        let paths = temp_profile_paths("purge-confirmed-tombstone");
        let mut store = Store::open(&paths).unwrap();
        let issue_id = "qgh://github.com/issue/I_PURGE_TRANSFER";
        let comment_id = "qgh://github.com/issue-comment/IC_PURGE_TRANSFER";
        store
            .upsert_sources_for_run(
                "sync-purge-transfer",
                &[test_issue(issue_id, "owner/repo", "transfer-private")],
                &[test_comment(
                    comment_id,
                    issue_id,
                    "owner/repo",
                    "transfer-comment-private",
                )],
                0,
                &[],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO tombstones (source_id, reason, observed_at)
                 VALUES (?1, 'deleted', ?2)",
                params![issue_id, now_rfc3339()],
            )
            .unwrap();

        store
            .purge(
                PurgeTarget::Issue {
                    repo: "owner/repo".to_string(),
                    issue_number: 47,
                },
                PurgeTrigger::ConfirmedTombstone,
            )
            .unwrap();

        assert_eq!(
            store.get_tombstone(issue_id).unwrap().unwrap().reason,
            "deleted"
        );
        assert_eq!(
            store.get_tombstone(comment_id).unwrap().unwrap().reason,
            "transferred"
        );

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn repository_purge_clears_all_cursors_and_reopen_does_not_repopulate_state() {
        let paths = temp_profile_paths("purge-repository-cursors");
        let mut store = Store::open(&paths).unwrap();
        let target_repo = "owner/target";
        let other_repo = "owner/other";
        let cursors = [
            "issues:owner/target",
            "history:owner/target",
            "repo-comments:owner/target",
            "comments:owner/target#47",
            "comments:owner/target#99",
            "issues:owner/other",
        ]
        .into_iter()
        .map(|endpoint| CursorUpdate {
            endpoint: endpoint.to_string(),
            cursor: Some("2026-01-01T00:00:00Z".to_string()),
            etag: Some("safe-etag".to_string()),
            not_modified: false,
        })
        .collect::<Vec<_>>();
        store
            .upsert_sources_for_run(
                "sync-purge-repository-cursors",
                &[
                    test_issue(
                        "qgh://github.com/issue/I_PURGE_CURSOR_TARGET",
                        target_repo,
                        "cursor-target",
                    ),
                    test_issue(
                        "qgh://github.com/issue/I_PURGE_CURSOR_OTHER",
                        other_repo,
                        "cursor-other",
                    ),
                ],
                &[],
                0,
                &cursors,
            )
            .unwrap();
        assert_eq!(
            store.known_repositories().unwrap(),
            vec![other_repo.to_string(), target_repo.to_string()]
        );

        store
            .purge(
                PurgeTarget::Repository {
                    repo: target_repo.to_string(),
                },
                PurgeTrigger::AllowlistRemoval,
            )
            .unwrap();

        assert_eq!(
            store
                .sync_cursors()
                .unwrap()
                .into_iter()
                .map(|cursor| cursor.endpoint)
                .collect::<Vec<_>>(),
            vec!["issues:owner/other".to_string()]
        );
        assert_eq!(
            store.known_repositories().unwrap(),
            vec![other_repo.to_string()]
        );
        drop(store);

        let reopened = Store::open(&paths).unwrap();
        assert_eq!(
            reopened.known_repositories().unwrap(),
            vec![other_repo.to_string()]
        );
        let target_sync_state: i64 = reopened
            .conn
            .query_row(
                "SELECT count(*) FROM repository_sync_state WHERE repo = ?1",
                params![target_repo],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(target_sync_state, 0);

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn repeated_completed_repository_purge_is_true_noop_for_epoch_and_publication() {
        let paths = temp_profile_paths("purge-repository-repeat-noop");
        let mut store = Store::open(&paths).unwrap();
        let target_id = "qgh://github.com/issue/I_PURGE_REPEAT_TARGET";
        let other_id = "qgh://github.com/issue/I_PURGE_REPEAT_OTHER";
        store
            .upsert_sources_for_run(
                "sync-purge-repeat",
                &[
                    test_issue(target_id, "owner/target", "repeat-target"),
                    test_issue(other_id, "owner/other", "repeat-other"),
                ],
                &[],
                0,
                &[],
            )
            .unwrap();
        store
            .purge(
                PurgeTarget::Repository {
                    repo: "owner/target".to_string(),
                },
                PurgeTrigger::AllowlistRemoval,
            )
            .unwrap();
        let successor_snapshot = store
            .record_purge_successor_snapshot()
            .unwrap()
            .expect("repository purge successor snapshot");
        let (generation, _) = store
            .reserve_index_generation(&paths.index_root, 1)
            .unwrap();
        rebuild_reserved_generation(&store, &paths, generation);
        let publication = store
            .activate_retrieval_publication(&successor_snapshot, generation, None, None)
            .unwrap();
        let epoch_before = read_content_write_epoch(&store.conn).unwrap();

        let outcome = store
            .purge(
                PurgeTarget::Repository {
                    repo: "owner/target".to_string(),
                },
                PurgeTrigger::AllowlistRemoval,
            )
            .unwrap();

        assert_eq!(outcome.purged_sources, 0);
        assert!(!outcome.sensitive_wal_truncated);
        assert_eq!(read_content_write_epoch(&store.conn).unwrap(), epoch_before);
        assert_eq!(
            store
                .active_retrieval_publication()
                .unwrap()
                .unwrap()
                .publication_id,
            publication
        );

        let epoch_before_reopen = read_content_write_epoch(&store.conn).unwrap();
        drop(store);
        let reopened = Store::open(&paths).unwrap();
        assert!(reopened.pending_purges().unwrap().is_empty());
        assert_eq!(
            read_content_write_epoch(&reopened.conn).unwrap(),
            epoch_before_reopen
        );
        assert_eq!(
            reopened
                .active_retrieval_publication()
                .unwrap()
                .unwrap()
                .publication_id,
            publication
        );
        let source_completion: (String, bool, String, bool) = reopened
            .conn
            .query_row(
                "SELECT trigger, purge_pending, current_stage, completion_ready
                 FROM purge_requests
                 WHERE target_kind = 'source' AND target_value = ?1",
                params![target_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            source_completion,
            (
                "allowlist_removal".to_string(),
                false,
                "finalize".to_string(),
                true,
            )
        );

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn first_empty_repository_purge_fences_later_stale_ingest() {
        let paths = temp_profile_paths("purge-first-empty-repository");
        let mut stale_writer = Store::open(&paths).unwrap();
        let mut purger = Store::open(&paths).unwrap();
        let epoch_before = read_content_write_epoch(&purger.conn).unwrap();

        let outcome = purger
            .purge(
                PurgeTarget::Repository {
                    repo: "owner/removed".to_string(),
                },
                PurgeTrigger::AllowlistRemoval,
            )
            .unwrap();

        assert!(outcome.sensitive_wal_truncated);
        assert!(read_content_write_epoch(&purger.conn).unwrap() >= epoch_before + 2);
        let error = stale_writer
            .upsert_sources_for_run(
                "sync-after-empty-repository-purge",
                &[test_issue(
                    "qgh://github.com/issue/I_PURGE_EMPTY_REPO",
                    "owner/removed",
                    "must-be-fenced",
                )],
                &[],
                0,
                &[],
            )
            .unwrap_err();
        assert_eq!(error.code, "purge.write_fenced");

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn repeated_completed_source_purge_preserves_unrelated_successor_publication() {
        let paths = temp_profile_paths("purge-source-repeat-noop");
        let mut store = Store::open(&paths).unwrap();
        let target_id = "qgh://github.com/issue/I_PURGE_SOURCE_REPEAT_TARGET";
        let other_id = "qgh://github.com/issue/I_PURGE_SOURCE_REPEAT_OTHER";
        store
            .upsert_sources_for_run(
                "sync-purge-source-repeat",
                &[
                    test_issue(target_id, "owner/target", "source-repeat-target"),
                    test_issue(other_id, "owner/other", "source-repeat-other"),
                ],
                &[],
                0,
                &[],
            )
            .unwrap();
        store
            .purge(
                PurgeTarget::Source {
                    source_id: target_id.to_string(),
                },
                PurgeTrigger::ConfirmedDelete,
            )
            .unwrap();
        let completion_evidence_before: (String, i64, String, Option<String>, i64, String, String) =
            store
                .conn
                .query_row(
                    "SELECT trigger, purge_pending, current_stage, failure_stage,
                            completion_ready, created_at, updated_at
                     FROM purge_requests
                     WHERE target_kind = 'source' AND target_value = ?1",
                    params![target_id],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                            row.get(6)?,
                        ))
                    },
                )
                .unwrap();
        let successor_snapshot = store
            .record_purge_successor_snapshot()
            .unwrap()
            .expect("source purge successor snapshot");
        let (generation, generation_path) = store
            .reserve_index_generation(&paths.index_root, 1)
            .unwrap();
        rebuild_reserved_generation(&store, &paths, generation);
        let publication = store
            .activate_retrieval_publication(&successor_snapshot, generation, None, None)
            .unwrap();
        let epoch_before = read_content_write_epoch(&store.conn).unwrap();

        let outcome = store
            .purge(
                PurgeTarget::Source {
                    source_id: target_id.to_string(),
                },
                PurgeTrigger::ConfirmedDelete,
            )
            .unwrap();

        assert_eq!(outcome.purged_sources, 0);
        assert!(!outcome.sensitive_wal_truncated);
        assert_eq!(read_content_write_epoch(&store.conn).unwrap(), epoch_before);
        assert!(generation_path.exists());
        assert_eq!(
            store
                .active_retrieval_publication()
                .unwrap()
                .unwrap()
                .publication_id,
            publication
        );
        assert!(store.get_source(other_id).unwrap().is_some());

        let epoch_before_reopen = read_content_write_epoch(&store.conn).unwrap();
        drop(store);
        let reopened = Store::open(&paths).unwrap();
        assert!(reopened.pending_purges().unwrap().is_empty());
        assert_eq!(
            read_content_write_epoch(&reopened.conn).unwrap(),
            epoch_before_reopen
        );
        assert!(generation_path.exists());
        assert_eq!(
            reopened
                .active_retrieval_publication()
                .unwrap()
                .unwrap()
                .publication_id,
            publication
        );
        assert!(reopened.get_source(other_id).unwrap().is_some());
        let completion_evidence_after: (String, i64, String, Option<String>, i64, String, String) =
            reopened
                .conn
                .query_row(
                    "SELECT trigger, purge_pending, current_stage, failure_stage,
                            completion_ready, created_at, updated_at
                     FROM purge_requests
                     WHERE target_kind = 'source' AND target_value = ?1",
                    params![target_id],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                            row.get(6)?,
                        ))
                    },
                )
                .unwrap();
        assert_eq!(completion_evidence_after, completion_evidence_before);

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn first_absent_source_purge_fences_store_opened_before_confirmation() {
        let paths = temp_profile_paths("purge-first-absent-source");
        let mut stale_writer = Store::open(&paths).unwrap();
        let mut purger = Store::open(&paths).unwrap();
        let source_id = "qgh://github.com/issue/I_PURGE_FIRST_ABSENT";
        let epoch_before = read_content_write_epoch(&purger.conn).unwrap();

        let outcome = purger
            .purge(
                PurgeTarget::Source {
                    source_id: source_id.to_string(),
                },
                PurgeTrigger::ConfirmedDelete,
            )
            .unwrap();

        assert!(outcome.sensitive_wal_truncated);
        assert!(read_content_write_epoch(&purger.conn).unwrap() >= epoch_before + 2);
        let error = stale_writer
            .upsert_sources_for_run(
                "sync-after-first-absent-purge",
                &[test_issue(source_id, "owner/repo", "must-be-fenced")],
                &[],
                0,
                &[],
            )
            .unwrap_err();
        assert_eq!(error.code, "purge.write_fenced");
        assert!(purger.pending_purges().unwrap().is_empty());

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn first_absent_issue_purge_does_not_remove_unrelated_index_files() {
        let paths = temp_profile_paths("purge-first-absent-issue-index");
        let mut store = Store::open(&paths).unwrap();
        let other_id = "qgh://github.com/issue/I_PURGE_ABSENT_ISSUE_OTHER";
        store
            .upsert_sources_for_run(
                "sync-purge-absent-issue-index",
                &[test_issue(other_id, "owner/other", "other-index-content")],
                &[],
                0,
                &[],
            )
            .unwrap();
        seal_latest_test_sync(&mut store);
        let (generation, generation_path) = store
            .reserve_index_generation(&paths.index_root, 1)
            .unwrap();
        rebuild_reserved_generation(&store, &paths, generation);
        store
            .activate_retrieval_publication("sync-purge-absent-issue-index", generation, None, None)
            .unwrap();

        store
            .purge(
                PurgeTarget::Issue {
                    repo: "owner/missing".to_string(),
                    issue_number: 47,
                },
                PurgeTrigger::ConfirmedDelete,
            )
            .unwrap();

        assert!(generation_path.exists());
        assert_eq!(
            store.index_path_for_generation(generation).unwrap(),
            Some(generation_path.to_string_lossy().to_string())
        );
        assert!(store.active_retrieval_publication().unwrap().is_none());

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn repeated_completed_issue_purge_preserves_unrelated_successor_publication() {
        let paths = temp_profile_paths("purge-issue-repeat-noop");
        let mut store = Store::open(&paths).unwrap();
        let target_id = "qgh://github.com/issue/I_PURGE_ISSUE_REPEAT_TARGET";
        let comment_id = "qgh://github.com/issue-comment/IC_PURGE_ISSUE_REPEAT_TARGET";
        let other_id = "qgh://github.com/issue/I_PURGE_ISSUE_REPEAT_OTHER";
        let mut other = test_issue(other_id, "owner/repo", "issue-repeat-other");
        other.number = 48;
        other.canonical_url = "https://github.com/owner/repo/issues/48".to_string();
        store
            .upsert_sources_for_run(
                "sync-purge-issue-repeat",
                &[
                    test_issue(target_id, "owner/repo", "issue-repeat-target"),
                    other,
                ],
                &[test_comment(
                    comment_id,
                    target_id,
                    "owner/repo",
                    "issue-repeat-comment",
                )],
                0,
                &[],
            )
            .unwrap();
        store
            .purge(
                PurgeTarget::Issue {
                    repo: "owner/repo".to_string(),
                    issue_number: 47,
                },
                PurgeTrigger::ConfirmedDelete,
            )
            .unwrap();
        let successor_snapshot = store
            .record_purge_successor_snapshot()
            .unwrap()
            .expect("issue purge successor snapshot");
        let (generation, generation_path) = store
            .reserve_index_generation(&paths.index_root, 1)
            .unwrap();
        rebuild_reserved_generation(&store, &paths, generation);
        let publication = store
            .activate_retrieval_publication(&successor_snapshot, generation, None, None)
            .unwrap();
        let epoch_before = read_content_write_epoch(&store.conn).unwrap();

        let outcome = store
            .purge(
                PurgeTarget::Issue {
                    repo: "owner/repo".to_string(),
                    issue_number: 47,
                },
                PurgeTrigger::ConfirmedDelete,
            )
            .unwrap();

        assert_eq!(outcome.purged_sources, 0);
        assert!(!outcome.sensitive_wal_truncated);
        assert_eq!(read_content_write_epoch(&store.conn).unwrap(), epoch_before);
        assert!(generation_path.exists());
        assert_eq!(
            store
                .active_retrieval_publication()
                .unwrap()
                .unwrap()
                .publication_id,
            publication
        );
        assert!(store.get_source(other_id).unwrap().is_some());

        let epoch_before_reopen = read_content_write_epoch(&store.conn).unwrap();
        drop(store);
        let reopened = Store::open(&paths).unwrap();
        assert!(reopened.pending_purges().unwrap().is_empty());
        assert_eq!(
            read_content_write_epoch(&reopened.conn).unwrap(),
            epoch_before_reopen
        );
        assert!(generation_path.exists());
        assert_eq!(
            reopened
                .active_retrieval_publication()
                .unwrap()
                .unwrap()
                .publication_id,
            publication
        );
        assert!(reopened.get_source(other_id).unwrap().is_some());

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn reserve_index_generation_allocates_distinct_inactive_rows() {
        let paths = temp_profile_paths("index-generation-reservation");
        let mut store = Store::open(&paths).unwrap();
        store
            .upsert_sources_for_run(
                "sync-index-generation-reservation",
                &[
                    test_issue(
                        "qgh://github.com/issue/I_INDEX_RESERVATION_FIRST",
                        "owner/repo",
                        "first",
                    ),
                    test_issue(
                        "qgh://github.com/issue/I_INDEX_RESERVATION_SECOND",
                        "owner/repo",
                        "second",
                    ),
                ],
                &[],
                0,
                &[],
            )
            .unwrap();
        seal_latest_test_sync(&mut store);

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
        rebuild_reserved_generation(&store, &paths, first_generation);
        rebuild_reserved_generation(&store, &paths, second_generation);

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
        let ownership_count: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM index_build_leases WHERE owner_pid = 0",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(ownership_count, 2);

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn reservation_collision_preserves_foreign_shadow_and_rolls_back_ownership_rows() {
        let paths = temp_profile_paths("index-reservation-foreign-shadow");
        let mut store = Store::open(&paths).unwrap();
        fs::create_dir_all(&paths.index_root).unwrap();
        let foreign_shadow = paths.index_root.join("shadow-1");
        fs::create_dir_all(&foreign_shadow).unwrap();
        let sentinel = foreign_shadow.join("user-backup");
        fs::write(&sentinel, "preserve").unwrap();

        let error = store
            .reserve_index_generation(&paths.index_root, 0)
            .unwrap_err();

        assert_eq!(error.code, "publication.tantivy_artifact_not_ready");
        assert!(sentinel.exists());
        let ownership_rows: i64 = store
            .conn
            .query_row(
                "SELECT
                    (SELECT count(*) FROM index_generations) +
                    (SELECT count(*) FROM index_build_leases)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(ownership_rows, 0);

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn index_generation_identity_is_never_reused_after_purge() {
        let paths = temp_profile_paths("index-generation-never-reused");
        let mut store = Store::open(&paths).unwrap();
        let target_id = "qgh://github.com/issue/I_GENERATION_NEVER_REUSE_TARGET";
        let other_id = "qgh://github.com/issue/I_GENERATION_NEVER_REUSE_OTHER";
        store
            .upsert_sources_for_run(
                "sync-generation-never-reused",
                &[
                    test_issue(target_id, "owner/target", "target-generation"),
                    test_issue(other_id, "owner/other", "other-generation"),
                ],
                &[],
                0,
                &[],
            )
            .unwrap();
        store
            .mark_sync_run_completed("sync-generation-never-reused")
            .unwrap();
        let (first_generation, first_path) = store
            .reserve_index_generation(&paths.index_root, 2)
            .unwrap();
        rebuild_reserved_generation(&store, &paths, first_generation);
        store
            .activate_retrieval_publication(
                "sync-generation-never-reused",
                first_generation,
                None,
                None,
            )
            .unwrap();
        store
            .purge(
                PurgeTarget::Source {
                    source_id: target_id.to_string(),
                },
                PurgeTrigger::ConfirmedDelete,
            )
            .unwrap();
        store
            .record_purge_successor_snapshot()
            .unwrap()
            .expect("post-purge source snapshot");

        let (second_generation, second_path) = store
            .reserve_index_generation(&paths.index_root, 1)
            .unwrap();

        assert!(second_generation > first_generation);
        assert_ne!(second_path, first_path);

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn index_generation_sequence_migrates_above_existing_rows() {
        let paths = temp_profile_paths("index-generation-sequence-migration");
        let store = Store::open(&paths).unwrap();
        store
            .conn
            .execute(
                "INSERT INTO index_generations
                    (generation, path, source_count, created_at, active, write_epoch)
                 VALUES (42, ?1, 0, ?2, 0, 0)",
                params![
                    paths.index_root.join("generation-42").to_string_lossy(),
                    now_rfc3339()
                ],
            )
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE profile_meta SET value = '1' WHERE key = 'next_index_generation'",
                [],
            )
            .unwrap();
        drop(store);

        let mut reopened = Store::open(&paths).unwrap();
        let (generation, _) = reopened
            .reserve_index_generation(&paths.index_root, 0)
            .unwrap();
        assert_eq!(generation, 43);

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn dropping_builder_releases_only_its_owned_generation_lease() {
        let paths = temp_profile_paths("index-build-lease-drop");
        let mut builder = Store::open(&paths).unwrap();
        let (generation, generation_path) = builder
            .reserve_index_generation(&paths.index_root, 0)
            .unwrap();
        builder
            .rebuild_reserved_index_generation(generation, &[])
            .unwrap();
        drop(builder);

        assert!(!generation_path.exists());
        let reopened = Store::open(&paths).unwrap();
        let lease_count: i64 = reopened
            .conn
            .query_row(
                "SELECT count(*) FROM index_build_leases WHERE generation = ?1",
                params![generation],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(lease_count, 0);
        assert!(reopened
            .index_path_for_generation(generation)
            .unwrap()
            .is_none());

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn dropping_builder_cleans_sealed_shadow_left_before_generation_promotion() {
        let paths = temp_profile_paths("index-sealed-shadow-drop");
        let mut builder = Store::open(&paths).unwrap();
        let (generation, generation_path) = builder
            .reserve_index_generation(&paths.index_root, 0)
            .unwrap();
        builder
            .rebuild_reserved_index_generation(generation, &[])
            .unwrap();
        let shadow_path = paths.index_root.join(format!("shadow-{generation}"));
        fs::rename(&generation_path, &shadow_path).unwrap();

        drop(builder);

        assert!(!generation_path.exists());
        assert!(!shadow_path.exists());
        let reopened = Store::open(&paths).unwrap();
        assert!(reopened
            .index_path_for_generation(generation)
            .unwrap()
            .is_none());

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
    fn vector_storage_rebuild_cannot_resurrect_rows_across_purge_commit() {
        use std::cell::{Cell, RefCell};
        use std::rc::Rc;

        let paths = temp_profile_paths("vector-storage-purge-race");
        let mut writer = Store::open(&paths).unwrap();
        writer.enable_vector().unwrap();
        let source_id = "qgh://github.com/issue/I_VECTOR_PURGE_RACE";
        let chunk_id =
            insert_test_issue_chunk(&mut writer, source_id, "sync-vector-storage-purge-race");
        let fingerprint = embedding_fingerprint("Example/vector-purge-race");
        writer
            .replace_all_chunk_embeddings(&fingerprint, &[(chunk_id, vec![0.1, 0.2, 0.3])])
            .unwrap();
        let mut purge_store = Store::open(&paths).unwrap();
        purge_store.enable_vector().unwrap();
        let purge_store = Rc::new(RefCell::new(purge_store));
        let first_purge_succeeded = Rc::new(Cell::new(false));
        let hook_store = Rc::clone(&purge_store);
        let hook_succeeded = Rc::clone(&first_purge_succeeded);

        writer
            .ensure_vector_storage_for_fingerprint_inner(&fingerprint, move || {
                hook_succeeded.set(
                    hook_store
                        .borrow_mut()
                        .purge(
                            PurgeTarget::Source {
                                source_id: source_id.to_string(),
                            },
                            PurgeTrigger::ConfirmedDelete,
                        )
                        .is_ok(),
                );
            })
            .unwrap();
        purge_store
            .borrow_mut()
            .purge(
                PurgeTarget::Source {
                    source_id: source_id.to_string(),
                },
                PurgeTrigger::ConfirmedDelete,
            )
            .unwrap();

        assert_eq!(vector_row_count(&writer.conn), 0);
        if first_purge_succeeded.get() {
            assert!(writer.get_source(source_id).unwrap().is_none());
        }

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
        // Tombstoning purges the chunk and its derived vectors eagerly; the
        // background cleanup remains idempotent.
        assert_eq!(store.cleanup_inactive_embedding_artifacts().unwrap(), 0);

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
        let snapshot = store.capture_retrieval_build_snapshot().unwrap().unwrap();
        let spec = EmbeddingGenerationSpec {
            model_manifest_hash: "manifest-a".to_string(),
            runtime_fingerprint_hash: "runtime-a".to_string(),
            chunker_fingerprint: "chunker-a".to_string(),
            context_template_version: crate::context::METADATA_CONTEXT_TEMPLATE_VERSION.to_string(),
            output_dimension: 2,
        };
        let generation_id = store.begin_embedding_generation(&snapshot, &spec).unwrap();
        store
            .stage_embedding_generation_batch(
                generation_id,
                &[EmbeddingGenerationChunk {
                    chunk_id,
                    source_version_id,
                    source_version_hash: "body-hash-generation-sync".to_string(),
                    context_hash: production_context_hash_for_chunk(
                        &store,
                        "manifest-a",
                        "chunker-a",
                        chunk_id,
                    ),
                    vector: vec![1.0, 2.0],
                }],
            )
            .unwrap();
        assert_eq!(
            store.begin_embedding_generation(&snapshot, &spec).unwrap(),
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
        let second_generation = store
            .begin_embedding_generation(&snapshot, &second_spec)
            .unwrap();
        store
            .stage_embedding_generation_batch(
                second_generation,
                &[EmbeddingGenerationChunk {
                    chunk_id,
                    source_version_id,
                    source_version_hash: "body-hash-generation-sync".to_string(),
                    context_hash: production_context_hash_for_chunk(
                        &store,
                        "manifest-b",
                        "chunker-a",
                        chunk_id,
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
        let (tantivy_generation, _) = store
            .reserve_index_generation(&paths.index_root, 1)
            .unwrap();
        rebuild_reserved_generation(&store, &paths, tantivy_generation);
        let publication = store
            .activate_retrieval_publication(
                "generation-sync",
                tantivy_generation,
                Some(second_generation),
                None,
            )
            .unwrap();
        let publication_view = store.active_retrieval_publication().unwrap().unwrap();
        assert_eq!(publication_view.publication_id, publication);
        assert_eq!(
            publication_view.embedding_generation_id,
            Some(second_generation)
        );
        assert_eq!(publication_view.output_dimension, Some(3));
        store
            .conn
            .execute(
                "DELETE FROM embedding_generation_vector_rows WHERE generation_id = ?1",
                params![second_generation],
            )
            .unwrap();
        assert_eq!(
            store
                .generation_vector_search(
                    second_generation,
                    &[1.0, 2.0, 3.0],
                    &VectorSearchFilters::default(),
                    5,
                )
                .unwrap_err()
                .code,
            "embedding.generation_corrupt"
        );
        let post_publication_snapshot = store.capture_retrieval_build_snapshot().unwrap().unwrap();
        let missing_generation = store
            .begin_embedding_generation(
                &post_publication_snapshot,
                &EmbeddingGenerationSpec {
                    model_manifest_hash: "manifest-missing".to_string(),
                    ..second_spec.clone()
                },
            )
            .unwrap();
        store
            .stage_embedding_generation_batch(
                missing_generation,
                &[EmbeddingGenerationChunk {
                    chunk_id,
                    source_version_id,
                    source_version_hash: "body-hash-generation-sync".to_string(),
                    context_hash: production_context_hash_for_chunk(
                        &store,
                        "manifest-missing",
                        "chunker-a",
                        chunk_id,
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
            "embedding.generation_inventory_mismatch"
        );
        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn authoritative_edit_during_build_rejects_activation_and_preserves_previous_publication() {
        let paths = temp_profile_paths("publication-source-epoch-drift");
        let mut store = Store::open(&paths).unwrap();
        let source_id = "qgh://github.com/issue/I_SOURCE_EPOCH_DRIFT";
        let first_issue = test_issue(source_id, "owner/repo", "first");
        store
            .upsert_sources_for_run(
                "sync-source-first",
                std::slice::from_ref(&first_issue),
                &[],
                0,
                &[],
            )
            .unwrap();
        store.mark_sync_run_completed("sync-source-first").unwrap();
        let (first_generation, _) = store
            .reserve_index_generation(&paths.index_root, 1)
            .unwrap();
        rebuild_reserved_generation(&store, &paths, first_generation);
        let first_publication = store
            .activate_retrieval_publication("sync-source-first", first_generation, None, None)
            .unwrap();

        let mut built_issue = first_issue.clone();
        built_issue.title = "Title built snapshot".to_string();
        built_issue.updated_at = "2026-01-03T00:00:00Z".to_string();
        built_issue.indexed_at = "2026-01-03T00:00:01Z".to_string();
        store
            .upsert_sources_for_run("sync-source-built", &[built_issue.clone()], &[], 0, &[])
            .unwrap();
        store.mark_sync_run_completed("sync-source-built").unwrap();
        let (stale_generation, _) = store
            .reserve_index_generation(&paths.index_root, 1)
            .unwrap();
        rebuild_reserved_generation(&store, &paths, stale_generation);

        let mut concurrent_edit = built_issue;
        concurrent_edit.body = "Body changed while index build was in flight".to_string();
        concurrent_edit.body_hash = "body-hash-concurrent-edit".to_string();
        concurrent_edit.updated_at = "2026-01-04T00:00:00Z".to_string();
        concurrent_edit.indexed_at = "2026-01-04T00:00:01Z".to_string();
        store
            .upsert_sources_for_run("sync-source-concurrent", &[concurrent_edit], &[], 0, &[])
            .unwrap();

        let error = store
            .activate_retrieval_publication(
                "sync-source-built",
                stale_generation,
                None,
                Some(first_publication),
            )
            .unwrap_err();

        assert_eq!(error.code, "publication.source_snapshot_changed");
        let active = store.active_retrieval_publication().unwrap().unwrap();
        assert_eq!(active.publication_id, first_publication);
        assert_eq!(active.tantivy_generation, first_generation);
        let query_error = store
            .validate_query_publication_snapshot(Some(&active))
            .unwrap_err();
        assert_eq!(query_error.code, "publication.source_snapshot_changed");

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn retrieval_publication_cas_keeps_bm25_embedding_null_and_rolls_back_conflicts() {
        let paths = temp_profile_paths("publication-cas");
        let mut store = Store::open(&paths).unwrap();
        store
            .upsert_sources_for_run("sync-one", &[], &[], 0, &[])
            .unwrap();
        store.mark_sync_run_completed("sync-one").unwrap();
        let (first_generation, _) = store
            .reserve_index_generation(&paths.index_root, 0)
            .unwrap();
        rebuild_reserved_generation(&store, &paths, first_generation);
        let first = store
            .activate_retrieval_publication("sync-one", first_generation, None, None)
            .unwrap();
        let active = store.active_retrieval_publication().unwrap().unwrap();
        assert_eq!(active.publication_id, first);
        assert_eq!(active.embedding_generation_id, None);
        assert_eq!(active.tantivy_generation, first_generation);
        store
            .upsert_sources_for_run("sync-two", &[], &[], 0, &[])
            .unwrap();
        store.mark_sync_run_completed("sync-two").unwrap();
        let (second_generation, _) = store
            .reserve_index_generation(&paths.index_root, 0)
            .unwrap();
        rebuild_reserved_generation(&store, &paths, second_generation);
        let second = store
            .activate_retrieval_publication("sync-two", second_generation, None, Some(first))
            .unwrap();
        assert_ne!(first, second);
        store
            .upsert_sources_for_run("sync-three", &[], &[], 0, &[])
            .unwrap();
        store.mark_sync_run_completed("sync-three").unwrap();
        let (third_generation, _) = store
            .reserve_index_generation(&paths.index_root, 0)
            .unwrap();
        rebuild_reserved_generation(&store, &paths, third_generation);
        let conflict =
            store.activate_retrieval_publication("sync-three", third_generation, None, Some(first));
        assert_eq!(conflict.unwrap_err().code, "publication.cas_conflict");
        assert_eq!(
            store.active_index_generation().unwrap(),
            Some(second_generation)
        );
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
    fn fresh_embedding_identity_keeps_manifest_runtime_and_context_distinct_after_reopen() {
        let paths = temp_profile_paths("embedding-distinct-persisted-identity");
        let mut store = Store::open(&paths).unwrap();
        store.enable_vector().unwrap();
        let source_id = "qgh://github.com/issue/I_DISTINCT_EMBEDDING_IDENTITY";
        let chunk_id =
            insert_test_issue_chunk(&mut store, source_id, "sync-distinct-embedding-identity");
        let source_version_id = store.latest_source_version_id(source_id).unwrap().unwrap();
        let snapshot = store.capture_retrieval_build_snapshot().unwrap().unwrap();
        let model_manifest_hash = "manifest-distinct-identity";
        let runtime_fingerprint_hash = "runtime-distinct-identity";
        let spec = EmbeddingGenerationSpec {
            model_manifest_hash: model_manifest_hash.to_string(),
            runtime_fingerprint_hash: runtime_fingerprint_hash.to_string(),
            chunker_fingerprint: crate::chunking::CHUNKER_FINGERPRINT.to_string(),
            context_template_version: crate::context::METADATA_CONTEXT_TEMPLATE_VERSION.to_string(),
            output_dimension: 2,
        };
        let generation_id = store.begin_embedding_generation(&snapshot, &spec).unwrap();
        let expected_context_hash = production_context_hash_for_chunk(
            &store,
            model_manifest_hash,
            crate::chunking::CHUNKER_FINGERPRINT,
            chunk_id,
        );
        let runtime_keyed_context_hash = production_context_hash_for_chunk(
            &store,
            runtime_fingerprint_hash,
            crate::chunking::CHUNKER_FINGERPRINT,
            chunk_id,
        );
        assert_ne!(expected_context_hash, runtime_keyed_context_hash);
        store
            .stage_embedding_generation_batch(
                generation_id,
                &[EmbeddingGenerationChunk {
                    chunk_id,
                    source_version_id,
                    source_version_hash: store
                        .source_version_hash(source_version_id)
                        .unwrap()
                        .unwrap(),
                    context_hash: expected_context_hash.clone(),
                    vector: vec![1.0, 2.0],
                }],
            )
            .unwrap();
        store.validate_embedding_generation(generation_id).unwrap();
        let (tantivy_generation, _) = store
            .reserve_index_generation_for_snapshot(&paths.index_root, &snapshot)
            .unwrap();
        store
            .rebuild_reserved_index_generation(tantivy_generation, snapshot.sources())
            .unwrap();
        store
            .activate_retrieval_publication(
                snapshot.identity().sync_run_id(),
                tantivy_generation,
                Some(generation_id),
                snapshot.expected_publication_id(),
            )
            .unwrap();
        assert_eq!(
            store
                .conn
                .query_row(
                    "SELECT model_manifest_hash, runtime_fingerprint_hash
                     FROM embedding_generations WHERE id = ?1",
                    params![generation_id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .unwrap(),
            (
                model_manifest_hash.to_string(),
                runtime_fingerprint_hash.to_string(),
            )
        );
        assert_eq!(
            store
                .embedding_generation_chunk_blob(generation_id, chunk_id)
                .unwrap()
                .dimension,
            2
        );
        let stored_context_hash: String = store
            .conn
            .query_row(
                "SELECT context_hash FROM embedding_generation_chunks
                 WHERE generation_id = ?1 AND chunk_id = ?2",
                params![generation_id, chunk_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored_context_hash, expected_context_hash);
        drop(store);

        let reopened = Store::open(&paths).unwrap();
        let publication = reopened.active_retrieval_publication().unwrap().unwrap();
        assert_eq!(publication.embedding_generation_id, Some(generation_id));
        assert_eq!(
            publication.model_manifest_hash.as_deref(),
            Some(model_manifest_hash)
        );
        assert_eq!(
            publication.runtime_fingerprint_hash.as_deref(),
            Some(runtime_fingerprint_hash)
        );
        reopened
            .validate_query_publication_snapshot(Some(&publication))
            .unwrap();

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn partial_embedding_publication_identity_fails_closed() {
        let (paths, mut store, _, generation_id, _) =
            ready_generation_fixture("partial-embedding-publication-identity");
        publish_test_retrieval(&mut store, &paths, Some(generation_id));
        store
            .conn
            .execute(
                "UPDATE retrieval_publications
                 SET runtime_fingerprint_hash = NULL WHERE active = 1",
                [],
            )
            .unwrap();

        let publication = store.active_retrieval_publication().unwrap().unwrap();
        let error = store
            .validate_query_publication_snapshot(Some(&publication))
            .unwrap_err();
        assert_eq!(error.code, "publication.embedding_snapshot_mismatch");

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn active_embedding_generation_artifact_validation_is_read_only() {
        let (paths, mut store, _, generation_id, _) =
            ready_generation_fixture("doctor-active-generation-artifacts");
        let publication_id = publish_test_retrieval(&mut store, &paths, Some(generation_id));
        let state_before = store.embedding_generation_state(generation_id).unwrap();

        assert!(store
            .validate_active_embedding_generation_artifacts()
            .unwrap());
        assert_eq!(
            store
                .active_retrieval_publication()
                .unwrap()
                .unwrap()
                .publication_id,
            publication_id
        );
        assert_eq!(
            store.embedding_generation_state(generation_id).unwrap(),
            state_before
        );

        store
            .conn
            .execute(
                "UPDATE issue_metadata SET title = 'changed after publication'",
                [],
            )
            .unwrap();
        assert!(store
            .validate_active_embedding_generation_artifacts()
            .is_err());
        store
            .conn
            .execute(
                "UPDATE issue_metadata SET title = 'Vector storage regression'",
                [],
            )
            .unwrap();
        assert!(store
            .validate_active_embedding_generation_artifacts()
            .unwrap());

        store
            .conn
            .execute(
                "UPDATE embedding_generation_chunks
                 SET vector_checksum = 'invalid' WHERE generation_id = ?1",
                params![generation_id],
            )
            .unwrap();
        assert!(store
            .validate_active_embedding_generation_artifacts()
            .is_err());
        assert_eq!(
            store
                .active_retrieval_publication()
                .unwrap()
                .unwrap()
                .publication_id,
            publication_id
        );
        assert_eq!(
            store.embedding_generation_state(generation_id).unwrap(),
            state_before
        );

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn activation_rejects_empty_runtime_identity_with_structured_error() {
        let (paths, mut store, _, generation_id, _) =
            ready_generation_fixture("partial-embedding-generation-identity");
        let (snapshot, tantivy_generation) = reserve_test_retrieval(&mut store, &paths);
        store
            .conn
            .execute(
                "UPDATE embedding_generations
                 SET runtime_fingerprint_hash = '' WHERE id = ?1",
                params![generation_id],
            )
            .unwrap();

        let error = store
            .activate_retrieval_publication(
                snapshot.identity().sync_run_id(),
                tantivy_generation,
                Some(generation_id),
                snapshot.expected_publication_id(),
            )
            .unwrap_err();
        assert_eq!(error.code, "publication.embedding_snapshot_mismatch");

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn base_open_detaches_legacy_embedding_identity_and_preserves_bm25_payload_idempotently() {
        let paths = temp_profile_paths("legacy-embedding-identity-base-migration");
        let mut store = Store::open(&paths).unwrap();
        let source_id = "qgh://github.com/issue/I_LEGACY_EMBEDDING_IDENTITY";
        store
            .upsert_sources_for_run(
                "sync-legacy-embedding-identity",
                &[test_issue(
                    source_id,
                    "owner/repo",
                    "legacy-embedding-identity-searchable",
                )],
                &[],
                0,
                &[],
            )
            .unwrap();
        store
            .mark_sync_run_completed("sync-legacy-embedding-identity")
            .unwrap();
        let snapshot = store.capture_retrieval_build_snapshot().unwrap().unwrap();
        let (tantivy_generation, generation_path) = store
            .reserve_index_generation_for_snapshot(&paths.index_root, &snapshot)
            .unwrap();
        store
            .rebuild_reserved_index_generation(tantivy_generation, snapshot.sources())
            .unwrap();
        let legacy_publication_id = store
            .activate_retrieval_publication(
                snapshot.identity().sync_run_id(),
                tantivy_generation,
                None,
                snapshot.expected_publication_id(),
            )
            .unwrap();
        let source_snapshot_epoch = snapshot.identity.epoch;
        drop(store);

        let legacy = Connection::open(&paths.db_path).unwrap();
        legacy
            .execute_batch(
                "CREATE TABLE embedding_generations (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    state TEXT NOT NULL,
                    model_manifest_hash TEXT NOT NULL,
                    chunker_fingerprint TEXT NOT NULL,
                    context_template_version TEXT NOT NULL,
                    output_dimension INTEGER NOT NULL,
                    source_sync_run_id TEXT NOT NULL,
                    source_snapshot_hash TEXT NOT NULL,
                    embedding_inventory_hash TEXT,
                    total_chunks INTEGER NOT NULL,
                    completed_chunks INTEGER NOT NULL DEFAULT 0,
                    checkpoint_chunk_id INTEGER,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    failure_code TEXT,
                    write_epoch INTEGER NOT NULL DEFAULT 0,
                    source_snapshot_epoch INTEGER
                 );
                 CREATE TABLE embedding_generation_chunks (
                    generation_id INTEGER NOT NULL,
                    chunk_id INTEGER NOT NULL,
                    source_version_id INTEGER NOT NULL,
                    source_version_hash TEXT NOT NULL,
                    context_hash TEXT NOT NULL,
                    vector_blob BLOB NOT NULL,
                    vector_checksum TEXT NOT NULL,
                    vector_dimension INTEGER NOT NULL,
                    created_at TEXT NOT NULL
                 );
                 CREATE TABLE embedding_generation_vector_rows (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    generation_id INTEGER NOT NULL,
                    chunk_id INTEGER NOT NULL,
                    dimension INTEGER NOT NULL,
                    vector_table TEXT NOT NULL,
                    vector_rowid INTEGER NOT NULL
                 );",
            )
            .unwrap();
        legacy
            .execute(
                "INSERT INTO embedding_generations
                    (id, state, model_manifest_hash, chunker_fingerprint,
                     context_template_version, output_dimension, source_sync_run_id,
                     source_snapshot_hash, total_chunks, completed_chunks,
                     created_at, updated_at, source_snapshot_epoch)
                 VALUES (77, 'active', 'legacy-runtime-hash-in-manifest-column',
                         'legacy-chunker', 'qgh.context.v1', 2,
                         'sync-legacy-embedding-identity', 'legacy-snapshot', 1, 1,
                         '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', ?1)",
                params![source_snapshot_epoch],
            )
            .unwrap();
        legacy
            .execute(
                "INSERT INTO embedding_generation_chunks
                    (generation_id, chunk_id, source_version_id, source_version_hash,
                     context_hash, vector_blob, vector_checksum, vector_dimension, created_at)
                 VALUES (77, 901, 902, 'legacy-version', 'legacy-raw-context-collision',
                         X'0000803F00000040', 'legacy-checksum', 2,
                         '2026-01-01T00:00:00Z')",
                [],
            )
            .unwrap();
        legacy
            .execute(
                "INSERT INTO embedding_generation_vector_rows
                    (generation_id, chunk_id, dimension, vector_table, vector_rowid)
                 VALUES (77, 901, 2, 'embedding_generation_vectors_d2', 903)",
                [],
            )
            .unwrap();
        legacy
            .execute(
                "UPDATE retrieval_publications
                 SET embedding_generation_id = 77,
                     model_manifest_hash = 'legacy-runtime-hash-in-manifest-column',
                     chunker_fingerprint = 'legacy-chunker',
                     context_template_version = 'qgh.context.v1',
                     output_dimension = 2
                 WHERE publication_id = ?1",
                params![legacy_publication_id],
            )
            .unwrap();
        drop(legacy);

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let repairs = [paths.clone(), paths.clone()]
            .into_iter()
            .map(|paths| {
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    Store::open(&paths)
                        .unwrap()
                        .active_retrieval_publication()
                        .unwrap()
                        .unwrap()
                        .publication_id
                })
            })
            .collect::<Vec<_>>();
        let repaired_publication_ids = repairs
            .into_iter()
            .map(|repair| repair.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(repaired_publication_ids[0], repaired_publication_ids[1]);

        let reopened = Store::open(&paths).unwrap();
        let successor = reopened.active_retrieval_publication().unwrap().unwrap();
        assert_ne!(successor.publication_id, legacy_publication_id);
        assert_eq!(successor.tantivy_generation, tantivy_generation);
        assert_eq!(
            successor.source_snapshot_sync_run_id,
            "sync-legacy-embedding-identity"
        );
        assert_eq!(successor.embedding_generation_id, None);
        assert_eq!(successor.model_manifest_hash, None);
        assert_eq!(successor.runtime_fingerprint_hash, None);
        assert_eq!(successor.chunker_fingerprint, None);
        assert_eq!(successor.context_template_version, None);
        assert_eq!(successor.output_dimension, None);
        reopened
            .validate_query_publication_snapshot(Some(&successor))
            .unwrap();
        assert_eq!(
            reopened
                .conn
                .query_row(
                    "SELECT state, failure_code FROM embedding_generations WHERE id = 77",
                    [],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .unwrap(),
            (
                "failed".to_string(),
                "embedding.legacy_identity_incomplete".to_string(),
            )
        );
        for table in [
            "embedding_generation_chunks",
            "embedding_generation_vector_rows",
        ] {
            assert_eq!(
                reopened
                    .conn
                    .query_row(
                        &format!("SELECT count(*) FROM {table} WHERE generation_id = 77"),
                        [],
                        |row| row.get::<_, i64>(0),
                    )
                    .unwrap(),
                1
            );
        }
        assert!(generation_path.exists());
        let hits = crate::index::search(&generation_path, "searchable", 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].source_id, source_id);
        assert!(!reopened
            .conn
            .query_row(
                "SELECT active FROM retrieval_publications WHERE publication_id = ?1",
                params![legacy_publication_id],
                |row| row.get::<_, bool>(0)
            )
            .unwrap());
        let successor_id = successor.publication_id;
        let publication_count: i64 = reopened
            .conn
            .query_row("SELECT count(*) FROM retrieval_publications", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(publication_count, 2);
        drop(reopened);

        let reopened_again = Store::open(&paths).unwrap();
        assert_eq!(
            reopened_again
                .active_retrieval_publication()
                .unwrap()
                .unwrap()
                .publication_id,
            successor_id
        );
        assert_eq!(
            reopened_again
                .conn
                .query_row("SELECT count(*) FROM retrieval_publications", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            publication_count
        );

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn open_detaches_pre_epoch_bm25_publication_and_fails_closed_structurally() {
        let paths = temp_profile_paths("publication-pre-epoch-migration");
        let mut store = Store::open(&paths).unwrap();
        let source_id = "qgh://github.com/issue/I_PRE_EPOCH_PUBLICATION";
        store
            .upsert_sources_for_run(
                "sync-pre-epoch",
                &[test_issue(source_id, "owner/repo", "pre-epoch")],
                &[],
                0,
                &[],
            )
            .unwrap();
        store.mark_sync_run_completed("sync-pre-epoch").unwrap();
        let snapshot = store.capture_retrieval_build_snapshot().unwrap().unwrap();
        let (generation, generation_path) = store
            .reserve_index_generation_for_snapshot(&paths.index_root, &snapshot)
            .unwrap();
        store
            .rebuild_reserved_index_generation(generation, snapshot.sources())
            .unwrap();
        store
            .activate_retrieval_publication(
                snapshot.identity().sync_run_id(),
                generation,
                None,
                snapshot.expected_publication_id(),
            )
            .unwrap();
        drop(store);

        let legacy = Connection::open(&paths.db_path).unwrap();
        legacy
            .execute(
                "UPDATE retrieval_publications SET source_snapshot_epoch = NULL WHERE active = 1",
                [],
            )
            .unwrap();
        legacy
            .execute(
                "UPDATE index_generations
                 SET source_snapshot_epoch = NULL, source_inventory_hash = NULL
                 WHERE active = 1",
                [],
            )
            .unwrap();
        drop(legacy);

        let reopened = Store::open(&paths).unwrap();
        assert!(reopened.active_retrieval_publication().unwrap().is_none());
        assert_eq!(reopened.active_index_generation().unwrap(), None);
        let error = reopened
            .validate_query_publication_snapshot(None)
            .unwrap_err();
        assert_eq!(error.code, "publication.source_snapshot_incomplete");
        assert!(generation_path.exists());

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn open_detaches_publication_when_tantivy_inventory_manifest_is_missing() {
        let paths = temp_profile_paths("publication-missing-artifact-inventory");
        let mut store = Store::open(&paths).unwrap();
        let source_id = "qgh://github.com/issue/I_MISSING_ARTIFACT_INVENTORY";
        store
            .upsert_sources_for_run(
                "sync-missing-artifact-inventory",
                &[test_issue(
                    source_id,
                    "owner/repo",
                    "missing-artifact-inventory",
                )],
                &[],
                0,
                &[],
            )
            .unwrap();
        store
            .mark_sync_run_completed("sync-missing-artifact-inventory")
            .unwrap();
        let snapshot = store.capture_retrieval_build_snapshot().unwrap().unwrap();
        let (generation, generation_path) = store
            .reserve_index_generation_for_snapshot(&paths.index_root, &snapshot)
            .unwrap();
        store
            .rebuild_reserved_index_generation(generation, snapshot.sources())
            .unwrap();
        store
            .activate_retrieval_publication(
                snapshot.identity().sync_run_id(),
                generation,
                None,
                snapshot.expected_publication_id(),
            )
            .unwrap();

        let index = tantivy::Index::open_in_dir(&generation_path).unwrap();
        let mut writer = index
            .writer::<tantivy::TantivyDocument>(50_000_000)
            .unwrap();
        writer.commit().unwrap();
        writer.wait_merging_threads().unwrap();
        store
            .conn
            .execute(
                "DELETE FROM schema_migrations WHERE version = ?1",
                params![TANTIVY_COMMIT_INVENTORY_MIGRATION],
            )
            .unwrap();
        drop(store);

        let reopened = Store::open(&paths).unwrap();
        assert!(reopened.active_retrieval_publication().unwrap().is_none());
        assert_eq!(reopened.active_index_generation().unwrap(), None);
        let error = reopened
            .validate_query_publication_snapshot(None)
            .unwrap_err();
        assert_eq!(error.code, "publication.source_snapshot_incomplete");
        assert!(generation_path.exists());

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn open_records_tantivy_inventory_migration_after_valid_artifact_validation() {
        let paths = temp_profile_paths("publication-valid-artifact-migration");
        let mut store = Store::open(&paths).unwrap();
        store
            .upsert_sources_for_run(
                "sync-valid-artifact-migration",
                &[test_issue(
                    "qgh://github.com/issue/I_VALID_ARTIFACT_MIGRATION",
                    "owner/repo",
                    "valid-artifact-migration",
                )],
                &[],
                0,
                &[],
            )
            .unwrap();
        store
            .mark_sync_run_completed("sync-valid-artifact-migration")
            .unwrap();
        let snapshot = store.capture_retrieval_build_snapshot().unwrap().unwrap();
        let (generation, _) = store
            .reserve_index_generation_for_snapshot(&paths.index_root, &snapshot)
            .unwrap();
        store
            .rebuild_reserved_index_generation(generation, snapshot.sources())
            .unwrap();
        let publication_id = store
            .activate_retrieval_publication(
                snapshot.identity().sync_run_id(),
                generation,
                None,
                snapshot.expected_publication_id(),
            )
            .unwrap();
        store
            .conn
            .execute(
                "DELETE FROM schema_migrations WHERE version = ?1",
                params![TANTIVY_COMMIT_INVENTORY_MIGRATION],
            )
            .unwrap();
        drop(store);

        let reopened = Store::open(&paths).unwrap();
        assert_eq!(
            reopened
                .active_retrieval_publication()
                .unwrap()
                .unwrap()
                .publication_id,
            publication_id
        );
        let migration_recorded: bool = reopened
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM schema_migrations WHERE version = ?1)",
                params![TANTIVY_COMMIT_INVENTORY_MIGRATION],
                |row| row.get(0),
            )
            .unwrap();
        assert!(migration_recorded);

        let index =
            tantivy::Index::open_in_dir(paths.index_root.join(format!("generation-{generation}")))
                .unwrap();
        let mut writer = index
            .writer::<tantivy::TantivyDocument>(50_000_000)
            .unwrap();
        writer.commit().unwrap();
        writer.wait_merging_threads().unwrap();
        drop(reopened);

        let reopened_after_corruption = Store::open(&paths).unwrap();
        assert_eq!(
            reopened_after_corruption
                .active_retrieval_publication()
                .unwrap()
                .unwrap()
                .publication_id,
            publication_id
        );
        let error = reopened_after_corruption
            .resolve_active_tantivy_artifact()
            .unwrap_err();
        assert_eq!(error.code, "publication.source_inventory_mismatch");

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn open_records_tantivy_inventory_migration_when_artifact_is_missing() {
        let paths = temp_profile_paths("publication-missing-artifact-migration");
        let mut store = Store::open(&paths).unwrap();
        store
            .upsert_sources_for_run(
                "sync-missing-artifact-migration",
                &[test_issue(
                    "qgh://github.com/issue/I_MISSING_ARTIFACT_MIGRATION",
                    "owner/repo",
                    "missing-artifact-migration",
                )],
                &[],
                0,
                &[],
            )
            .unwrap();
        store
            .mark_sync_run_completed("sync-missing-artifact-migration")
            .unwrap();
        let snapshot = store.capture_retrieval_build_snapshot().unwrap().unwrap();
        let (generation, generation_path) = store
            .reserve_index_generation_for_snapshot(&paths.index_root, &snapshot)
            .unwrap();
        store
            .rebuild_reserved_index_generation(generation, snapshot.sources())
            .unwrap();
        let publication_id = store
            .activate_retrieval_publication(
                snapshot.identity().sync_run_id(),
                generation,
                None,
                snapshot.expected_publication_id(),
            )
            .unwrap();
        store
            .conn
            .execute(
                "DELETE FROM schema_migrations WHERE version = ?1",
                params![TANTIVY_COMMIT_INVENTORY_MIGRATION],
            )
            .unwrap();
        fs::remove_dir_all(generation_path).unwrap();
        drop(store);

        let reopened = Store::open(&paths).unwrap();
        assert_eq!(
            reopened
                .active_retrieval_publication()
                .unwrap()
                .unwrap()
                .publication_id,
            publication_id
        );
        let migration_recorded: bool = reopened
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM schema_migrations WHERE version = ?1)",
                params![TANTIVY_COMMIT_INVENTORY_MIGRATION],
                |row| row.get(0),
            )
            .unwrap();
        assert!(migration_recorded);

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn reopen_reports_runtime_tantivy_corruption_without_detaching_pointer() {
        let paths = temp_profile_paths("publication-runtime-artifact-corruption");
        let mut store = Store::open(&paths).unwrap();
        store
            .upsert_sources_for_run(
                "sync-runtime-artifact-corruption",
                &[test_issue(
                    "qgh://github.com/issue/I_RUNTIME_ARTIFACT_CORRUPTION",
                    "owner/repo",
                    "runtime-artifact-corruption",
                )],
                &[],
                0,
                &[],
            )
            .unwrap();
        store
            .mark_sync_run_completed("sync-runtime-artifact-corruption")
            .unwrap();
        let snapshot = store.capture_retrieval_build_snapshot().unwrap().unwrap();
        let (generation, generation_path) = store
            .reserve_index_generation_for_snapshot(&paths.index_root, &snapshot)
            .unwrap();
        store
            .rebuild_reserved_index_generation(generation, snapshot.sources())
            .unwrap();
        let publication_id = store
            .activate_retrieval_publication(
                snapshot.identity().sync_run_id(),
                generation,
                None,
                snapshot.expected_publication_id(),
            )
            .unwrap();
        let index = tantivy::Index::open_in_dir(&generation_path).unwrap();
        let mut writer = index
            .writer::<tantivy::TantivyDocument>(50_000_000)
            .unwrap();
        writer.commit().unwrap();
        writer.wait_merging_threads().unwrap();
        drop(store);

        let reopened = Store::open(&paths).unwrap();
        assert_eq!(
            reopened
                .active_retrieval_publication()
                .unwrap()
                .unwrap()
                .publication_id,
            publication_id
        );
        let error = reopened.resolve_active_tantivy_artifact().unwrap_err();
        assert_eq!(error.code, "publication.source_inventory_mismatch");

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn open_demotes_embedding_for_detached_pre_epoch_publication_without_deleting_payload() {
        let paths = temp_profile_paths("publication-pre-epoch-embedding-demotion");
        let mut store = Store::open(&paths).unwrap();
        store.enable_vector().unwrap();
        let source_id = "qgh://github.com/issue/I_PRE_EPOCH_EMBEDDING";
        store
            .upsert_sources_for_run(
                "sync-pre-epoch-embedding",
                &[test_issue(source_id, "owner/repo", "pre-epoch-embedding")],
                &[],
                0,
                &[],
            )
            .unwrap();
        let chunk_id = insert_chunk(&mut store, source_id, "pre-epoch embedding chunk");
        let embedding_generation =
            stage_test_generation(&mut store, "manifest-pre-epoch-embedding", &[chunk_id]);
        let (tantivy_generation, _) = store
            .reserve_index_generation(&paths.index_root, 1)
            .unwrap();
        rebuild_reserved_generation(&store, &paths, tantivy_generation);
        store
            .activate_retrieval_publication(
                "sync-pre-epoch-embedding",
                tantivy_generation,
                Some(embedding_generation),
                None,
            )
            .unwrap();
        assert_eq!(
            store
                .embedding_generation_state(embedding_generation)
                .unwrap(),
            "active"
        );
        drop(store);

        let legacy = Connection::open(&paths.db_path).unwrap();
        legacy
            .execute(
                "UPDATE retrieval_publications SET source_snapshot_epoch = NULL WHERE active = 1",
                [],
            )
            .unwrap();
        legacy
            .execute(
                "UPDATE index_generations
                 SET source_snapshot_epoch = NULL, source_inventory_hash = NULL
                 WHERE active = 1",
                [],
            )
            .unwrap();
        drop(legacy);

        let reopened = Store::open(&paths).unwrap();
        assert!(reopened.active_retrieval_publication().unwrap().is_none());
        assert_eq!(
            reopened
                .embedding_generation_state(embedding_generation)
                .unwrap(),
            "ready"
        );
        let retained_chunks: i64 = reopened
            .conn
            .query_row(
                "SELECT count(*) FROM embedding_generation_chunks WHERE generation_id = ?1",
                params![embedding_generation],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retained_chunks, 1);

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn first_publication_absent_to_present_cas_rejects_second_builder() {
        let paths = temp_profile_paths("publication-first-cas");
        let mut store = Store::open(&paths).unwrap();
        store
            .upsert_sources_for_run("sync-first-cas", &[], &[], 0, &[])
            .unwrap();
        store.mark_sync_run_completed("sync-first-cas").unwrap();
        let snapshot = store.capture_retrieval_build_snapshot().unwrap().unwrap();
        assert_eq!(snapshot.expected_publication_id, None);
        let (first_generation, _) = store
            .reserve_index_generation_for_snapshot(&paths.index_root, &snapshot)
            .unwrap();
        let (second_generation, _) = store
            .reserve_index_generation_for_snapshot(&paths.index_root, &snapshot)
            .unwrap();
        store
            .rebuild_reserved_index_generation(first_generation, &snapshot.sources)
            .unwrap();
        store
            .rebuild_reserved_index_generation(second_generation, &snapshot.sources)
            .unwrap();
        let publication = store
            .activate_retrieval_publication(
                &snapshot.identity.sync_run_id,
                first_generation,
                None,
                snapshot.expected_publication_id,
            )
            .unwrap();

        let error = store
            .activate_retrieval_publication(
                &snapshot.identity.sync_run_id,
                second_generation,
                None,
                snapshot.expected_publication_id,
            )
            .unwrap_err();
        assert_eq!(error.code, "publication.cas_conflict");
        assert_eq!(
            store
                .active_retrieval_publication()
                .unwrap()
                .unwrap()
                .publication_id,
            publication
        );

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn later_page_at_new_epoch_blocks_capture_until_sync_completion() {
        let paths = temp_profile_paths("publication-incomplete-page");
        let mut store = Store::open(&paths).unwrap();
        store
            .upsert_sources_for_run(
                "sync-paged",
                &[test_issue(
                    "qgh://github.com/issue/I_PAGE_ONE",
                    "owner/repo",
                    "page-one",
                )],
                &[],
                0,
                &[],
            )
            .unwrap();
        store.mark_sync_run_completed("sync-paged").unwrap();
        assert!(store.capture_retrieval_build_snapshot().unwrap().is_some());
        store
            .upsert_sources_for_run(
                "sync-paged",
                &[test_issue(
                    "qgh://github.com/issue/I_PAGE_TWO",
                    "owner/repo",
                    "page-two",
                )],
                &[],
                0,
                &[],
            )
            .unwrap();

        let error = store.capture_retrieval_build_snapshot().unwrap_err();
        assert_eq!(error.code, "publication.source_snapshot_incomplete");

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn forged_truncated_retrieval_snapshot_is_rejected_before_reservation() {
        let paths = temp_profile_paths("publication-forged-inventory");
        let mut store = Store::open(&paths).unwrap();
        store
            .upsert_sources_for_run(
                "sync-forged-inventory",
                &[
                    test_issue(
                        "qgh://github.com/issue/I_FORGED_INVENTORY_ONE",
                        "owner/repo",
                        "inventory-one",
                    ),
                    test_issue(
                        "qgh://github.com/issue/I_FORGED_INVENTORY_TWO",
                        "owner/repo",
                        "inventory-two",
                    ),
                ],
                &[],
                0,
                &[],
            )
            .unwrap();
        store
            .mark_sync_run_completed("sync-forged-inventory")
            .unwrap();
        let mut forged = store.capture_retrieval_build_snapshot().unwrap().unwrap();
        assert_eq!(forged.sources.len(), 2);
        forged.sources.pop();

        let error = store
            .reserve_index_generation_for_snapshot(&paths.index_root, &forged)
            .unwrap_err();
        assert_eq!(error.code, "publication.source_inventory_mismatch");

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn activation_rejects_same_count_tantivy_artifact_from_different_inventory() {
        let paths = temp_profile_paths("publication-artifact-inventory-mismatch");
        let mut store = Store::open(&paths).unwrap();
        store
            .upsert_sources_for_run(
                "sync-artifact-inventory",
                &[
                    test_issue(
                        "qgh://github.com/issue/I_ARTIFACT_INVENTORY_ONE",
                        "owner/repo",
                        "artifact-one",
                    ),
                    test_issue(
                        "qgh://github.com/issue/I_ARTIFACT_INVENTORY_TWO",
                        "owner/repo",
                        "artifact-two",
                    ),
                ],
                &[],
                0,
                &[],
            )
            .unwrap();
        store
            .mark_sync_run_completed("sync-artifact-inventory")
            .unwrap();
        let snapshot = store.capture_retrieval_build_snapshot().unwrap().unwrap();
        let (generation, _) = store
            .reserve_index_generation_for_snapshot(&paths.index_root, &snapshot)
            .unwrap();
        let mut altered_sources = snapshot.sources().to_vec();
        for (index, source) in altered_sources.iter_mut().enumerate() {
            source.title = format!("Altered artifact title {index}");
            source.body = format!("Altered artifact body {index}");
        }
        store
            .rebuild_reserved_index_generation(generation, &altered_sources)
            .unwrap();

        let error = store
            .activate_retrieval_publication(
                snapshot.identity().sync_run_id(),
                generation,
                None,
                snapshot.expected_publication_id(),
            )
            .unwrap_err();
        assert_eq!(error.code, "publication.source_inventory_mismatch");

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn active_tantivy_resolver_rejects_missing_artifact_without_detaching_pointer() {
        let paths = temp_profile_paths("publication-missing-active-artifact");
        let mut store = Store::open(&paths).unwrap();
        store
            .upsert_sources_for_run(
                "sync-missing-active-artifact",
                &[test_issue(
                    "qgh://github.com/issue/I_MISSING_ACTIVE_ARTIFACT",
                    "owner/repo",
                    "missing-active-artifact",
                )],
                &[],
                0,
                &[],
            )
            .unwrap();
        store
            .mark_sync_run_completed("sync-missing-active-artifact")
            .unwrap();
        let snapshot = store.capture_retrieval_build_snapshot().unwrap().unwrap();
        let (generation, generation_path) = store
            .reserve_index_generation_for_snapshot(&paths.index_root, &snapshot)
            .unwrap();
        store
            .rebuild_reserved_index_generation(generation, snapshot.sources())
            .unwrap();
        let publication_id = store
            .activate_retrieval_publication(
                snapshot.identity().sync_run_id(),
                generation,
                None,
                snapshot.expected_publication_id(),
            )
            .unwrap();
        fs::remove_dir_all(generation_path).unwrap();

        let error = store.resolve_active_tantivy_artifact().unwrap_err();
        assert_eq!(error.code, "publication.tantivy_artifact_not_ready");
        assert_eq!(
            store
                .active_retrieval_publication()
                .unwrap()
                .unwrap()
                .publication_id,
            publication_id
        );

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(unix)]
    #[test]
    fn activation_rejects_symlink_inside_tantivy_generation() {
        use std::os::unix::fs::symlink;

        let paths = temp_profile_paths("publication-activation-contained-symlink");
        let mut store = Store::open(&paths).unwrap();
        store
            .upsert_sources_for_run(
                "sync-activation-contained-symlink",
                &[test_issue(
                    "qgh://github.com/issue/I_ACTIVATION_CONTAINED_SYMLINK",
                    "owner/repo",
                    "activation-contained-symlink",
                )],
                &[],
                0,
                &[],
            )
            .unwrap();
        store
            .mark_sync_run_completed("sync-activation-contained-symlink")
            .unwrap();
        let snapshot = store.capture_retrieval_build_snapshot().unwrap().unwrap();
        let (generation, generation_path) = store
            .reserve_index_generation_for_snapshot(&paths.index_root, &snapshot)
            .unwrap();
        store
            .rebuild_reserved_index_generation(generation, snapshot.sources())
            .unwrap();
        let outside = paths.profile_dir.join("outside-generation");
        fs::write(&outside, b"outside fixture").unwrap();
        symlink(&outside, generation_path.join("unexpected-link")).unwrap();

        let error = store
            .activate_retrieval_publication(
                snapshot.identity().sync_run_id(),
                generation,
                None,
                snapshot.expected_publication_id(),
            )
            .unwrap_err();
        assert_eq!(error.code, "publication.tantivy_artifact_not_ready");
        assert!(store.active_retrieval_publication().unwrap().is_none());
        assert!(outside.exists());

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(unix)]
    #[test]
    fn legacy_index_publish_rejects_symlink_inside_tantivy_generation() {
        use std::os::unix::fs::symlink;

        let paths = temp_profile_paths("legacy-publish-contained-symlink");
        let mut store = Store::open(&paths).unwrap();
        store
            .upsert_sources_for_run(
                "sync-legacy-publish-contained-symlink",
                &[test_issue(
                    "qgh://github.com/issue/I_LEGACY_PUBLISH_CONTAINED_SYMLINK",
                    "owner/repo",
                    "legacy-publish-contained-symlink",
                )],
                &[],
                0,
                &[],
            )
            .unwrap();
        store
            .mark_sync_run_completed("sync-legacy-publish-contained-symlink")
            .unwrap();
        let snapshot = store.capture_retrieval_build_snapshot().unwrap().unwrap();
        let (generation, generation_path) = store
            .reserve_index_generation_for_snapshot(&paths.index_root, &snapshot)
            .unwrap();
        store
            .rebuild_reserved_index_generation(generation, snapshot.sources())
            .unwrap();
        let outside = paths.profile_dir.join("outside-legacy-generation");
        fs::write(&outside, b"outside fixture").unwrap();
        symlink(&outside, generation_path.join("unexpected-link")).unwrap();

        let error = store
            .mark_index_published(generation, &generation_path.to_string_lossy(), 1)
            .unwrap_err();
        assert_eq!(error.code, "publication.tantivy_artifact_not_ready");
        assert_eq!(store.active_index_generation().unwrap(), None);
        assert!(outside.exists());

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn active_tantivy_resolver_rejects_orphan_active_state_for_empty_profile() {
        let paths = temp_profile_paths("publication-resolver-orphan-active-state");
        let mut store = Store::open(&paths).unwrap();
        store
            .upsert_sources_for_run(
                "sync-resolver-orphan-active-state",
                &[test_issue(
                    "qgh://github.com/issue/I_RESOLVER_ORPHAN_ACTIVE_STATE",
                    "owner/repo",
                    "resolver-orphan-active-state",
                )],
                &[],
                0,
                &[],
            )
            .unwrap();
        store
            .mark_sync_run_completed("sync-resolver-orphan-active-state")
            .unwrap();
        let snapshot = store.capture_retrieval_build_snapshot().unwrap().unwrap();
        let (generation, _) = store
            .reserve_index_generation_for_snapshot(&paths.index_root, &snapshot)
            .unwrap();
        store
            .rebuild_reserved_index_generation(generation, snapshot.sources())
            .unwrap();
        store
            .activate_retrieval_publication(
                snapshot.identity().sync_run_id(),
                generation,
                None,
                snapshot.expected_publication_id(),
            )
            .unwrap();
        store
            .conn
            .execute("DELETE FROM retrieval_publication_pointer WHERE id = 1", [])
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE source_entities SET lifecycle_state = 'tombstoned'",
                [],
            )
            .unwrap();

        let error = store.resolve_active_tantivy_artifact().unwrap_err();
        assert_eq!(error.code, "publication.tantivy_artifact_not_ready");

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn active_tantivy_resolver_rejects_multiple_active_publications() {
        let paths = temp_profile_paths("publication-resolver-multiple-active-publications");
        let mut store = Store::open(&paths).unwrap();
        store
            .upsert_sources_for_run(
                "sync-resolver-multiple-active-publications",
                &[test_issue(
                    "qgh://github.com/issue/I_RESOLVER_MULTIPLE_ACTIVE_PUBLICATIONS",
                    "owner/repo",
                    "resolver-multiple-active-publications",
                )],
                &[],
                0,
                &[],
            )
            .unwrap();
        store
            .mark_sync_run_completed("sync-resolver-multiple-active-publications")
            .unwrap();
        let first_snapshot = store.capture_retrieval_build_snapshot().unwrap().unwrap();
        let (first_generation, _) = store
            .reserve_index_generation_for_snapshot(&paths.index_root, &first_snapshot)
            .unwrap();
        store
            .rebuild_reserved_index_generation(first_generation, first_snapshot.sources())
            .unwrap();
        let first_publication = store
            .activate_retrieval_publication(
                first_snapshot.identity().sync_run_id(),
                first_generation,
                None,
                first_snapshot.expected_publication_id(),
            )
            .unwrap();
        let second_snapshot = store.capture_retrieval_build_snapshot().unwrap().unwrap();
        let (second_generation, _) = store
            .reserve_index_generation_for_snapshot(&paths.index_root, &second_snapshot)
            .unwrap();
        store
            .rebuild_reserved_index_generation(second_generation, second_snapshot.sources())
            .unwrap();
        store
            .activate_retrieval_publication(
                second_snapshot.identity().sync_run_id(),
                second_generation,
                None,
                second_snapshot.expected_publication_id(),
            )
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE retrieval_publications SET active = 1 WHERE publication_id = ?1",
                params![first_publication],
            )
            .unwrap();

        let error = store.resolve_active_tantivy_artifact().unwrap_err();
        assert_eq!(error.code, "publication.tantivy_artifact_not_ready");

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn active_tantivy_resolver_does_not_materialize_source_bodies() {
        let paths = temp_profile_paths("publication-resolver-metadata-only");
        let mut store = Store::open(&paths).unwrap();
        let source_id = "qgh://github.com/issue/I_RESOLVER_METADATA_ONLY";
        store
            .upsert_sources_for_run(
                "sync-resolver-metadata-only",
                &[test_issue(
                    source_id,
                    "owner/repo",
                    "resolver-metadata-only",
                )],
                &[],
                0,
                &[],
            )
            .unwrap();
        store
            .mark_sync_run_completed("sync-resolver-metadata-only")
            .unwrap();
        let snapshot = store.capture_retrieval_build_snapshot().unwrap().unwrap();
        let (generation, expected_path) = store
            .reserve_index_generation_for_snapshot(&paths.index_root, &snapshot)
            .unwrap();
        store
            .rebuild_reserved_index_generation(generation, snapshot.sources())
            .unwrap();
        store
            .activate_retrieval_publication(
                snapshot.identity().sync_run_id(),
                generation,
                None,
                snapshot.expected_publication_id(),
            )
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE issue_metadata SET body = X'80' WHERE source_id = ?1",
                params![source_id],
            )
            .unwrap();

        assert_eq!(
            store.resolve_active_tantivy_artifact().unwrap(),
            Some(expected_path)
        );

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn active_tantivy_resolver_rejects_same_count_artifact_inventory_swap() {
        let paths = temp_profile_paths("publication-resolver-same-count-swap");
        let mut store = Store::open(&paths).unwrap();
        store
            .upsert_sources_for_run(
                "sync-resolver-same-count-swap",
                &[test_issue(
                    "qgh://github.com/issue/I_RESOLVER_SAME_COUNT_SWAP",
                    "owner/repo",
                    "resolver-same-count-swap",
                )],
                &[],
                0,
                &[],
            )
            .unwrap();
        store
            .mark_sync_run_completed("sync-resolver-same-count-swap")
            .unwrap();
        let snapshot = store.capture_retrieval_build_snapshot().unwrap().unwrap();
        let (generation, _) = store
            .reserve_index_generation_for_snapshot(&paths.index_root, &snapshot)
            .unwrap();
        store
            .rebuild_reserved_index_generation(generation, snapshot.sources())
            .unwrap();
        store
            .activate_retrieval_publication(
                snapshot.identity().sync_run_id(),
                generation,
                None,
                snapshot.expected_publication_id(),
            )
            .unwrap();
        let mut substituted = snapshot.sources().to_vec();
        substituted[0].title = "same-count-substitution".to_string();
        substituted[0].body = "same-count-substitution".to_string();
        fs::remove_dir_all(paths.index_root.join(format!("generation-{generation}"))).unwrap();
        crate::index::rebuild(&paths.index_root, generation, &substituted).unwrap();

        let error = store.resolve_active_tantivy_artifact().unwrap_err();
        assert_eq!(error.code, "publication.source_inventory_mismatch");

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(unix)]
    #[test]
    fn active_tantivy_resolver_rejects_symlink_inside_generation() {
        use std::os::unix::fs::symlink;

        let paths = temp_profile_paths("publication-resolver-contained-symlink");
        let mut store = Store::open(&paths).unwrap();
        store
            .upsert_sources_for_run(
                "sync-resolver-contained-symlink",
                &[test_issue(
                    "qgh://github.com/issue/I_RESOLVER_CONTAINED_SYMLINK",
                    "owner/repo",
                    "resolver-contained-symlink",
                )],
                &[],
                0,
                &[],
            )
            .unwrap();
        store
            .mark_sync_run_completed("sync-resolver-contained-symlink")
            .unwrap();
        let snapshot = store.capture_retrieval_build_snapshot().unwrap().unwrap();
        let (generation, generation_path) = store
            .reserve_index_generation_for_snapshot(&paths.index_root, &snapshot)
            .unwrap();
        store
            .rebuild_reserved_index_generation(generation, snapshot.sources())
            .unwrap();
        store
            .activate_retrieval_publication(
                snapshot.identity().sync_run_id(),
                generation,
                None,
                snapshot.expected_publication_id(),
            )
            .unwrap();
        let outside = paths.profile_dir.join("outside-artifact");
        fs::write(&outside, "content-free-fixture").unwrap();
        symlink(&outside, generation_path.join("unexpected-link")).unwrap();

        let error = store.resolve_active_tantivy_artifact().unwrap_err();
        assert_eq!(error.code, "publication.tantivy_artifact_not_ready");

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn query_read_transaction_keeps_one_publication_during_concurrent_sync() {
        let paths = temp_profile_paths("publication-query-sync-coherence");
        let mut writer = Store::open(&paths).unwrap();
        let source_id = "qgh://github.com/issue/I_QUERY_SYNC_COHERENCE";
        let first_issue = test_issue(source_id, "owner/repo", "first-snapshot");
        writer
            .upsert_sources_for_run(
                "sync-query-first",
                std::slice::from_ref(&first_issue),
                &[],
                0,
                &[],
            )
            .unwrap();
        writer.mark_sync_run_completed("sync-query-first").unwrap();
        let first_snapshot = writer.capture_retrieval_build_snapshot().unwrap().unwrap();
        let (first_generation, _) = writer
            .reserve_index_generation_for_snapshot(&paths.index_root, &first_snapshot)
            .unwrap();
        writer
            .rebuild_reserved_index_generation(first_generation, &first_snapshot.sources)
            .unwrap();
        let first_publication = writer
            .activate_retrieval_publication(
                &first_snapshot.identity.sync_run_id,
                first_generation,
                None,
                first_snapshot.expected_publication_id,
            )
            .unwrap();

        let reader = Store::open(&paths).unwrap();
        let fence = reader.begin_read_snapshot().unwrap();
        let captured_publication = reader.active_retrieval_publication().unwrap().unwrap();
        reader
            .validate_query_publication_snapshot(Some(&captured_publication))
            .unwrap();
        assert_eq!(captured_publication.publication_id, first_publication);

        let mut second_issue = first_issue;
        second_issue.title = "second snapshot".to_string();
        second_issue.updated_at = "2026-01-03T00:00:00Z".to_string();
        second_issue.indexed_at = "2026-01-03T00:00:01Z".to_string();
        writer
            .upsert_sources_for_run("sync-query-second", &[second_issue], &[], 0, &[])
            .unwrap();
        writer.mark_sync_run_completed("sync-query-second").unwrap();
        let second_snapshot = writer.capture_retrieval_build_snapshot().unwrap().unwrap();
        let (second_generation, _) = writer
            .reserve_index_generation_for_snapshot(&paths.index_root, &second_snapshot)
            .unwrap();
        writer
            .rebuild_reserved_index_generation(second_generation, &second_snapshot.sources)
            .unwrap();
        let second_publication = writer
            .activate_retrieval_publication(
                &second_snapshot.identity.sync_run_id,
                second_generation,
                None,
                second_snapshot.expected_publication_id,
            )
            .unwrap();

        let still_captured = reader.active_retrieval_publication().unwrap().unwrap();
        assert_eq!(still_captured.publication_id, first_publication);
        reader
            .validate_query_publication_snapshot(Some(&still_captured))
            .unwrap();
        reader.end_read_snapshot_and_validate(fence).unwrap();
        assert_eq!(
            reader
                .active_retrieval_publication()
                .unwrap()
                .unwrap()
                .publication_id,
            second_publication
        );

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn mixed_embedding_and_tantivy_identity_fails_activation_and_query_validation() {
        let paths = temp_profile_paths("publication-mixed-identity");
        let mut store = Store::open(&paths).unwrap();
        store.enable_vector().unwrap();
        store
            .upsert_sources_for_run("sync-embedding-a", &[], &[], 0, &[])
            .unwrap();
        store.mark_sync_run_completed("sync-embedding-a").unwrap();
        let embedding_snapshot = store.capture_retrieval_build_snapshot().unwrap().unwrap();
        store
            .upsert_sources_for_run("sync-lexical-b", &[], &[], 0, &[])
            .unwrap();
        store.mark_sync_run_completed("sync-lexical-b").unwrap();
        let embedding_generation_id = store
            .begin_embedding_generation(
                &embedding_snapshot,
                &EmbeddingGenerationSpec {
                    model_manifest_hash: "manifest-mixed".to_string(),
                    runtime_fingerprint_hash: "runtime-mixed".to_string(),
                    chunker_fingerprint: "chunker-mixed".to_string(),
                    context_template_version: crate::context::METADATA_CONTEXT_TEMPLATE_VERSION
                        .to_string(),
                    output_dimension: 2,
                },
            )
            .unwrap();
        store
            .validate_embedding_generation(embedding_generation_id)
            .unwrap();
        let lexical_snapshot = store.capture_retrieval_build_snapshot().unwrap().unwrap();
        let (mixed_generation, _) = store
            .reserve_index_generation_for_snapshot(&paths.index_root, &lexical_snapshot)
            .unwrap();
        store
            .rebuild_reserved_index_generation(mixed_generation, &lexical_snapshot.sources)
            .unwrap();
        let activation_error = store
            .activate_retrieval_publication(
                &lexical_snapshot.identity.sync_run_id,
                mixed_generation,
                Some(embedding_generation_id),
                lexical_snapshot.expected_publication_id,
            )
            .unwrap_err();
        assert_eq!(
            activation_error.code,
            "publication.embedding_snapshot_mismatch"
        );

        let (lexical_generation, _) = store
            .reserve_index_generation_for_snapshot(&paths.index_root, &lexical_snapshot)
            .unwrap();
        store
            .rebuild_reserved_index_generation(lexical_generation, &lexical_snapshot.sources)
            .unwrap();
        store
            .activate_retrieval_publication(
                &lexical_snapshot.identity.sync_run_id,
                lexical_generation,
                None,
                lexical_snapshot.expected_publication_id,
            )
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE embedding_generations SET state = 'active' WHERE id = ?1",
                params![embedding_generation_id],
            )
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE retrieval_publications
                 SET embedding_generation_id = ?1,
                     model_manifest_hash = 'manifest-mixed',
                     chunker_fingerprint = 'chunker-mixed',
                     context_template_version = ?2,
                     output_dimension = 2
                 WHERE active = 1",
                params![
                    embedding_generation_id,
                    crate::context::METADATA_CONTEXT_TEMPLATE_VERSION,
                ],
            )
            .unwrap();
        let publication = store.active_retrieval_publication().unwrap().unwrap();
        let query_error = store
            .validate_query_publication_snapshot(Some(&publication))
            .unwrap_err();
        assert_eq!(query_error.code, "publication.embedding_snapshot_mismatch");

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn unsupported_zero_row_embedding_activation_returns_structured_snapshot_mismatch() {
        let paths = temp_profile_paths("unsupported-zero-row-embedding");
        let mut store = Store::open(&paths).unwrap();
        store.enable_vector().unwrap();
        store
            .upsert_sources_for_run("sync-zero-row", &[], &[], 0, &[])
            .unwrap();
        let identity = store.mark_sync_run_completed("sync-zero-row").unwrap();
        let snapshot = store.capture_retrieval_build_snapshot().unwrap().unwrap();
        let begin_error = store
            .begin_embedding_generation(
                &snapshot,
                &EmbeddingGenerationSpec {
                    model_manifest_hash: "manifest-zero-row".to_string(),
                    runtime_fingerprint_hash: "runtime-zero-row".to_string(),
                    chunker_fingerprint: "chunker-zero-row".to_string(),
                    context_template_version: "qgh.context.legacy".to_string(),
                    output_dimension: 2,
                },
            )
            .unwrap_err();
        assert_eq!(begin_error.code, "embedding.context_template_unsupported");
        store
            .conn
            .execute(
                "INSERT INTO embedding_generations
                    (state, model_manifest_hash, runtime_fingerprint_hash,
                     chunker_fingerprint,
                     context_template_version, output_dimension, source_sync_run_id,
                     source_snapshot_hash, total_chunks, completed_chunks,
                     created_at, updated_at, write_epoch, source_snapshot_epoch)
                 VALUES ('ready', 'manifest-zero-row', 'runtime-zero-row', 'chunker-zero-row',
                         'qgh.context.legacy', 2, ?1, ?2, 0, 0, ?3, ?3, ?4, NULL)",
                params![
                    identity.sync_run_id,
                    source_snapshot_identity_hash(&identity),
                    now_rfc3339(),
                    store.content_write_epoch,
                ],
            )
            .unwrap();
        let embedding_generation_id = store.conn.last_insert_rowid();
        let snapshot = store.capture_retrieval_build_snapshot().unwrap().unwrap();
        let (tantivy_generation, _) = store
            .reserve_index_generation_for_snapshot(&paths.index_root, &snapshot)
            .unwrap();
        store
            .rebuild_reserved_index_generation(tantivy_generation, &snapshot.sources)
            .unwrap();

        let error = store
            .activate_retrieval_publication(
                &identity.sync_run_id,
                tantivy_generation,
                Some(embedding_generation_id),
                snapshot.expected_publication_id,
            )
            .unwrap_err();
        assert_eq!(error.code, "publication.embedding_snapshot_mismatch");

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn embedding_generation_cannot_shrink_authoritative_chunk_inventory() {
        let paths = temp_profile_paths("embedding-forged-inventory");
        let mut store = Store::open(&paths).unwrap();
        store.enable_vector().unwrap();
        let first_source = "qgh://github.com/issue/I_EMBED_INVENTORY_ONE";
        let second_source = "qgh://github.com/issue/I_EMBED_INVENTORY_TWO";
        let first_chunk = insert_test_issue_chunk(&mut store, first_source, "sync-embed-one");
        insert_test_issue_chunk(&mut store, second_source, "sync-embed-two");
        let snapshot = store.capture_retrieval_build_snapshot().unwrap().unwrap();
        let source_version_id = store
            .latest_source_version_id(first_source)
            .unwrap()
            .unwrap();
        let generation_id = store
            .begin_embedding_generation(
                &snapshot,
                &EmbeddingGenerationSpec {
                    model_manifest_hash: "manifest-inventory".to_string(),
                    runtime_fingerprint_hash: "runtime-inventory".to_string(),
                    chunker_fingerprint: crate::chunking::CHUNKER_FINGERPRINT.to_string(),
                    context_template_version: crate::context::METADATA_CONTEXT_TEMPLATE_VERSION
                        .to_string(),
                    output_dimension: 2,
                },
            )
            .unwrap();
        store
            .stage_embedding_generation_batch(
                generation_id,
                &[EmbeddingGenerationChunk {
                    chunk_id: first_chunk,
                    source_version_id,
                    source_version_hash: store
                        .source_version_hash(source_version_id)
                        .unwrap()
                        .unwrap(),
                    context_hash: production_context_hash_for_chunk(
                        &store,
                        "manifest-inventory",
                        crate::chunking::CHUNKER_FINGERPRINT,
                        first_chunk,
                    ),
                    vector: vec![1.0, 2.0],
                }],
            )
            .unwrap();

        let error = store
            .validate_embedding_generation(generation_id)
            .unwrap_err();
        assert_eq!(error.code, "embedding.generation_incomplete");

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
        let snapshot = store.capture_retrieval_build_snapshot().unwrap().unwrap();
        let generation_id = store
            .begin_embedding_generation(
                &snapshot,
                &EmbeddingGenerationSpec {
                    model_manifest_hash: "manifest-bad".to_string(),
                    runtime_fingerprint_hash: "runtime-bad".to_string(),
                    chunker_fingerprint: "chunker-bad".to_string(),
                    context_template_version: crate::context::METADATA_CONTEXT_TEMPLATE_VERSION
                        .to_string(),
                    output_dimension: 2,
                },
            )
            .unwrap();
        store
            .stage_embedding_generation_batch(
                generation_id,
                &[EmbeddingGenerationChunk {
                    chunk_id,
                    source_version_id,
                    source_version_hash: "body-hash-generation-bad".to_string(),
                    context_hash: production_context_hash_for_chunk(
                        &store,
                        "manifest-bad",
                        "chunker-bad",
                        chunk_id,
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
    fn restage_rejects_unowned_mapping_without_touching_foreign_table() {
        let (paths, mut store, chunk_id, generation_id, manifest) =
            ready_generation_fixture("generation-restage-unowned-mapping");
        store
            .conn
            .execute(
                "UPDATE embedding_generations SET state = 'building' WHERE id = ?1",
                params![generation_id],
            )
            .unwrap();
        let staged = staged_test_chunk(&store, chunk_id, &manifest);
        corrupt_mapping_table_with_sentinel(&store, generation_id);

        let error = store
            .stage_embedding_generation_batch(generation_id, &[staged])
            .unwrap_err();
        assert_eq!(error.code, "embedding.generation_corrupt");
        assert_eq!(
            store
                .conn
                .query_row("SELECT marker FROM cleanup_sentinel", [], |row| {
                    row.get::<_, String>(0)
                })
                .unwrap(),
            "preserve"
        );

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn inactive_artifact_cleanup_honors_pending_purge_write_fence() {
        let paths = temp_profile_paths("inactive-cleanup-write-fence");
        let mut writer = Store::open(&paths).unwrap();
        writer.enable_vector().unwrap();
        let source_id = "qgh://github.com/issue/I_INACTIVE_CLEANUP_FENCE";
        insert_test_issue_chunk(&mut writer, source_id, "sync-inactive-cleanup-fence");
        let mut stale_cleanup = Store::open(&paths).unwrap();
        stale_cleanup.enable_vector().unwrap();
        writer
            .queue_purges(&[(
                PurgeTarget::Source {
                    source_id: source_id.to_string(),
                },
                PurgeTrigger::ConfirmedDelete,
            )])
            .unwrap();

        let error = stale_cleanup
            .cleanup_inactive_embedding_artifacts()
            .unwrap_err();
        assert_eq!(error.code, "purge.write_fenced");

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn cleanup_candidate_promoted_after_selection_is_never_deleted() {
        let (paths, mut store, _, generation_id, _) =
            ready_generation_fixture("cleanup-promoted-candidate");
        store.cleanup_promote_generation_after_scan = Some(generation_id);

        assert_eq!(
            store
                .cleanup_embedding_generations("9999-01-01T00:00:00Z", "9999-01-01T00:00:00Z",)
                .unwrap(),
            0
        );
        assert_eq!(
            store.embedding_generation_state(generation_id).unwrap(),
            "active"
        );
        let publication = store.active_retrieval_publication().unwrap().unwrap();
        assert_eq!(publication.embedding_generation_id, Some(generation_id));
        assert!(publication.runtime_fingerprint_hash.is_some());

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn cleanup_preserves_recent_previous_generation() {
        let (paths, mut store, _, generation_id, _) =
            ready_generation_fixture("cleanup-recent-previous");
        publish_test_retrieval(&mut store, &paths, Some(generation_id));
        publish_test_retrieval(&mut store, &paths, None);

        assert_eq!(
            store
                .cleanup_embedding_generations("9999-01-01T00:00:00Z", "0000-01-01T00:00:00Z",)
                .unwrap(),
            0
        );
        assert_eq!(
            store.embedding_generation_state(generation_id).unwrap(),
            "ready"
        );

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn cleanup_removes_inactive_publication_with_generation_atomically() {
        let (paths, mut store, _, generation_id, _) =
            ready_generation_fixture("cleanup-inactive-publication");
        let embedding_publication = publish_test_retrieval(&mut store, &paths, Some(generation_id));
        publish_test_retrieval(&mut store, &paths, None);

        assert_eq!(
            store
                .cleanup_embedding_generations("9999-01-01T00:00:00Z", "9999-01-01T00:00:00Z",)
                .unwrap(),
            1
        );
        assert!(store.embedding_generation_state(generation_id).is_err());
        assert_eq!(
            store
                .conn
                .query_row(
                    "SELECT count(*) FROM retrieval_publications WHERE publication_id = ?1",
                    params![embedding_publication],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0
        );

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn cleanup_protects_pointer_generation_when_flags_drift() {
        let (paths, mut store, _, generation_id, _) =
            ready_generation_fixture("cleanup-pointer-protection");
        let publication_id = publish_test_retrieval(&mut store, &paths, Some(generation_id));
        store
            .conn
            .execute(
                "UPDATE retrieval_publications SET active = 0 WHERE publication_id = ?1",
                params![publication_id],
            )
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE embedding_generations SET state = 'ready' WHERE id = ?1",
                params![generation_id],
            )
            .unwrap();

        assert_eq!(
            store
                .cleanup_embedding_generations("9999-01-01T00:00:00Z", "9999-01-01T00:00:00Z",)
                .unwrap(),
            0
        );
        assert_eq!(
            store.embedding_generation_state(generation_id).unwrap(),
            "ready"
        );
        assert_eq!(
            store
                .active_retrieval_publication()
                .unwrap()
                .unwrap()
                .publication_id,
            publication_id
        );

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn cleanup_rolls_back_all_candidates_when_later_mapping_is_unowned() {
        let (paths, mut store, chunk_id, first_generation, _) =
            ready_generation_fixture("cleanup-atomic-unowned-mapping");
        let second_generation =
            stage_test_generation(&mut store, "manifest-cleanup-second", &[chunk_id]);
        corrupt_mapping_table_with_sentinel(&store, second_generation);

        let error = store
            .cleanup_embedding_generations("9999-01-01T00:00:00Z", "9999-01-01T00:00:00Z")
            .unwrap_err();
        assert_eq!(error.code, "embedding.generation_corrupt");
        for generation_id in [first_generation, second_generation] {
            assert_eq!(
                store.embedding_generation_state(generation_id).unwrap(),
                "ready"
            );
            assert_eq!(
                store
                    .conn
                    .query_row(
                        "SELECT count(*) FROM embedding_generation_chunks WHERE generation_id = ?1",
                        params![generation_id],
                        |row| row.get::<_, i64>(0),
                    )
                    .unwrap(),
                1
            );
            assert_eq!(
                store
                    .conn
                    .query_row(
                        "SELECT count(*) FROM embedding_generation_vector_rows WHERE generation_id = ?1",
                        params![generation_id],
                        |row| row.get::<_, i64>(0),
                    )
                    .unwrap(),
                1
            );
        }
        assert_eq!(
            store
                .conn
                .query_row("SELECT marker FROM cleanup_sentinel", [], |row| {
                    row.get::<_, String>(0)
                })
                .unwrap(),
            "preserve"
        );

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn cleanup_rejects_missing_mapping_without_orphaning_generation_metadata() {
        let (paths, mut store, _, generation_id, _) =
            ready_generation_fixture("cleanup-missing-mapping");
        let vector_rowid: i64 = store
            .conn
            .query_row(
                "SELECT vector_rowid FROM embedding_generation_vector_rows
                 WHERE generation_id = ?1",
                params![generation_id],
                |row| row.get(0),
            )
            .unwrap();
        store
            .conn
            .execute(
                "DELETE FROM embedding_generation_vector_rows WHERE generation_id = ?1",
                params![generation_id],
            )
            .unwrap();

        let error = store
            .cleanup_embedding_generations("9999-01-01T00:00:00Z", "9999-01-01T00:00:00Z")
            .unwrap_err();
        assert_eq!(error.code, "embedding.generation_corrupt");
        assert_eq!(
            store.embedding_generation_state(generation_id).unwrap(),
            "ready"
        );
        assert_eq!(
            store
                .conn
                .query_row(
                    "SELECT count(*) FROM embedding_generation_chunks WHERE generation_id = ?1",
                    params![generation_id],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        let vector_table = generation_vector_table_name(2);
        assert!(store
            .conn
            .query_row(
                &format!("SELECT EXISTS(SELECT 1 FROM {vector_table} WHERE rowid = ?1)"),
                params![vector_rowid],
                |row| row.get::<_, bool>(0),
            )
            .unwrap());

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn cleanup_failure_after_first_delete_rolls_back_every_artifact() {
        let (paths, mut store, chunk_id, first_generation, _) =
            ready_generation_fixture("cleanup-delete-rollback");
        let inactive_publication =
            publish_test_retrieval(&mut store, &paths, Some(first_generation));
        publish_test_retrieval(&mut store, &paths, None);
        let second_generation =
            stage_test_generation(&mut store, "manifest-cleanup-rollback-second", &[chunk_id]);
        let mappings = [first_generation, second_generation]
            .into_iter()
            .map(|generation_id| {
                store
                    .conn
                    .query_row(
                        "SELECT vector_table, vector_rowid
                         FROM embedding_generation_vector_rows WHERE generation_id = ?1",
                        params![generation_id],
                        |row| {
                            Ok((
                                generation_id,
                                row.get::<_, String>(0)?,
                                row.get::<_, i64>(1)?,
                            ))
                        },
                    )
                    .unwrap()
            })
            .collect::<Vec<_>>();
        store.cleanup_fail_after_first_generation_delete = true;

        let error = store
            .cleanup_embedding_generations("9999-01-01T00:00:00Z", "9999-01-01T00:00:00Z")
            .unwrap_err();
        assert_eq!(error.code, "embedding.generation_cleanup_injected_failure");
        assert_eq!(
            store
                .conn
                .query_row(
                    "SELECT count(*) FROM retrieval_publications WHERE publication_id = ?1",
                    params![inactive_publication],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        for (generation_id, vector_table, vector_rowid) in mappings {
            assert_eq!(
                store.embedding_generation_state(generation_id).unwrap(),
                "ready"
            );
            assert_eq!(
                store
                    .conn
                    .query_row(
                        "SELECT count(*) FROM embedding_generation_chunks WHERE generation_id = ?1",
                        params![generation_id],
                        |row| row.get::<_, i64>(0),
                    )
                    .unwrap(),
                1
            );
            assert_eq!(
                store
                    .conn
                    .query_row(
                        "SELECT count(*) FROM embedding_generation_vector_rows
                         WHERE generation_id = ?1",
                        params![generation_id],
                        |row| row.get::<_, i64>(0),
                    )
                    .unwrap(),
                1
            );
            assert!(store
                .conn
                .query_row(
                    &format!("SELECT EXISTS(SELECT 1 FROM {vector_table} WHERE rowid = ?1)"),
                    params![vector_rowid],
                    |row| row.get::<_, bool>(0),
                )
                .unwrap());
        }

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn active_generation_rejects_foreign_mapping_without_state_mutation() {
        let (paths, mut store, _, generation_id, _) =
            ready_generation_fixture("active-generation-foreign-mapping");
        let publication_id = publish_test_retrieval(&mut store, &paths, Some(generation_id));
        store
            .conn
            .execute(
                "UPDATE embedding_generation_vector_rows
                 SET vector_table = 'cleanup_sentinel'
                 WHERE generation_id = ?1",
                params![generation_id],
            )
            .unwrap();

        let error = store
            .validate_embedding_generation(generation_id)
            .unwrap_err();
        assert_eq!(error.code, "embedding.generation_corrupt");
        assert_eq!(
            store.embedding_generation_state(generation_id).unwrap(),
            "active"
        );
        assert_eq!(
            store
                .active_retrieval_publication()
                .unwrap()
                .unwrap()
                .publication_id,
            publication_id
        );

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn generation_vector_search_rejects_foreign_mapping() {
        let (paths, store, _, generation_id, _) =
            ready_generation_fixture("vector-search-foreign-mapping");
        store
            .conn
            .execute(
                "UPDATE embedding_generation_vector_rows
                 SET vector_table = 'cleanup_sentinel'
                 WHERE generation_id = ?1",
                params![generation_id],
            )
            .unwrap();

        let error = store
            .generation_vector_search(
                generation_id,
                &[1.0, 2.0],
                &VectorSearchFilters::default(),
                5,
            )
            .unwrap_err();
        assert_eq!(error.code, "embedding.generation_corrupt");

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn generation_vector_search_rejects_same_dimension_vec0_row_tamper() {
        let (paths, store, _, generation_id, _) =
            ready_generation_fixture("vector-search-row-tamper");
        let (vector_table, vector_rowid): (String, i64) = store
            .conn
            .query_row(
                "SELECT vector_table, vector_rowid
                 FROM embedding_generation_vector_rows
                 WHERE generation_id = ?1",
                params![generation_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        store
            .conn
            .execute(
                &format!("UPDATE {vector_table} SET embedding = ?1 WHERE rowid = ?2"),
                params![encode_embedding_blob(&[9.0, 9.0]), vector_rowid],
            )
            .unwrap();

        let error = store
            .generation_vector_search(
                generation_id,
                &[1.0, 2.0],
                &VectorSearchFilters::default(),
                5,
            )
            .unwrap_err();

        assert_eq!(error.code, "embedding.generation_corrupt");
        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn generation_vector_search_rejects_tamper_moved_outside_candidate_window() {
        let paths = temp_profile_paths("vector-search-moved-out-tamper");
        let mut store = Store::open(&paths).unwrap();
        store.enable_vector().unwrap();
        let first_chunk = insert_test_issue_chunk(
            &mut store,
            "qgh://github.com/issue/I_VECTOR_TAMPER_FIRST",
            "sync-vector-tamper-first",
        );
        let second_chunk = insert_test_issue_chunk(
            &mut store,
            "qgh://github.com/issue/I_VECTOR_TAMPER_SECOND",
            "sync-vector-tamper-second",
        );
        let generation_id = stage_test_generation(
            &mut store,
            "manifest-vector-moved-out-tamper",
            &[first_chunk, second_chunk],
        );
        let (vector_table, vector_rowid): (String, i64) = store
            .conn
            .query_row(
                "SELECT vector_table, vector_rowid
                 FROM embedding_generation_vector_rows
                 WHERE generation_id = ?1 AND chunk_id = ?2",
                params![generation_id, first_chunk],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        store
            .conn
            .execute(
                &format!("UPDATE {vector_table} SET embedding = ?1 WHERE rowid = ?2"),
                params![encode_embedding_blob(&[10_000.0, 10_000.0]), vector_rowid],
            )
            .unwrap();
        let query = [1.0 + first_chunk as f32, 2.0];

        let error = store
            .generation_vector_search(generation_id, &query, &VectorSearchFilters::default(), 1)
            .unwrap_err();

        assert_eq!(error.code, "embedding.generation_corrupt");
        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn generation_vector_search_rejects_joint_authoritative_and_vec0_tamper() {
        let paths = temp_profile_paths("vector-search-joint-row-tamper");
        let mut store = Store::open(&paths).unwrap();
        store.enable_vector().unwrap();
        let first_chunk = insert_test_issue_chunk(
            &mut store,
            "qgh://github.com/issue/I_VECTOR_JOINT_TAMPER_FIRST",
            "sync-vector-joint-tamper-first",
        );
        let second_chunk = insert_test_issue_chunk(
            &mut store,
            "qgh://github.com/issue/I_VECTOR_JOINT_TAMPER_SECOND",
            "sync-vector-joint-tamper-second",
        );
        let generation_id = stage_test_generation(
            &mut store,
            "manifest-vector-joint-tamper",
            &[first_chunk, second_chunk],
        );
        let (vector_table, vector_rowid): (String, i64) = store
            .conn
            .query_row(
                "SELECT vector_table, vector_rowid
                 FROM embedding_generation_vector_rows
                 WHERE generation_id = ?1 AND chunk_id = ?2",
                params![generation_id, first_chunk],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let tampered = encode_embedding_blob(&[10_000.0, 10_000.0]);
        store
            .conn
            .execute(
                "UPDATE embedding_generation_chunks
                 SET vector_blob = ?1
                 WHERE generation_id = ?2 AND chunk_id = ?3",
                params![&tampered, generation_id, first_chunk],
            )
            .unwrap();
        store
            .conn
            .execute(
                &format!("UPDATE {vector_table} SET embedding = ?1 WHERE rowid = ?2"),
                params![tampered, vector_rowid],
            )
            .unwrap();
        let query = [1.0 + first_chunk as f32, 2.0];

        let error = store
            .generation_vector_search(generation_id, &query, &VectorSearchFilters::default(), 1)
            .unwrap_err();

        assert_eq!(error.code, "embedding.generation_corrupt");
        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn activation_rejects_ready_generation_checksum_corruption() {
        let (paths, mut store, _, generation_id, _) =
            ready_generation_fixture("activation-ready-checksum-corruption");
        store
            .conn
            .execute(
                "UPDATE embedding_generation_chunks
                 SET vector_checksum = 'invalid'
                 WHERE generation_id = ?1",
                params![generation_id],
            )
            .unwrap();
        let (snapshot, tantivy_generation) = reserve_test_retrieval(&mut store, &paths);

        let error = store
            .activate_retrieval_publication(
                snapshot.identity().sync_run_id(),
                tantivy_generation,
                Some(generation_id),
                snapshot.expected_publication_id(),
            )
            .unwrap_err();
        assert_eq!(error.code, "embedding.generation_corrupt");
        assert!(store.active_retrieval_publication().unwrap().is_none());
        assert_eq!(
            store.embedding_generation_state(generation_id).unwrap(),
            "ready"
        );

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn activation_rejects_missing_vector_mapping_and_preserves_previous_publication() {
        let (paths, mut store, _, generation_id, _) =
            ready_generation_fixture("activation-missing-vector-mapping");
        let first_publication = publish_test_retrieval(&mut store, &paths, None);
        let (snapshot, second_tantivy) = reserve_test_retrieval(&mut store, &paths);
        store
            .conn
            .execute(
                "DELETE FROM embedding_generation_vector_rows WHERE generation_id = ?1",
                params![generation_id],
            )
            .unwrap();

        let error = store
            .activate_retrieval_publication(
                snapshot.identity().sync_run_id(),
                second_tantivy,
                Some(generation_id),
                snapshot.expected_publication_id(),
            )
            .unwrap_err();
        assert_eq!(error.code, "embedding.generation_corrupt");
        assert_eq!(
            store
                .active_retrieval_publication()
                .unwrap()
                .unwrap()
                .publication_id,
            first_publication
        );
        assert_eq!(
            store.embedding_generation_state(generation_id).unwrap(),
            "ready"
        );

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn ready_generation_rejects_mismatched_vec0_row_without_state_mutation() {
        let (paths, mut store, _, generation_id, _) =
            ready_generation_fixture("ready-generation-mismatched-vec0");
        let vector_rowid: i64 = store
            .conn
            .query_row(
                "SELECT vector_rowid FROM embedding_generation_vector_rows WHERE generation_id = ?1",
                params![generation_id],
                |row| row.get(0),
            )
            .unwrap();
        let vector_table = generation_vector_table_name(2);
        store
            .conn
            .execute(
                &format!("DELETE FROM {vector_table} WHERE rowid = ?1"),
                params![vector_rowid],
            )
            .unwrap();
        store
            .conn
            .execute(
                &format!("INSERT INTO {vector_table}(rowid, embedding) VALUES (?1, ?2)"),
                params![vector_rowid, encode_embedding_blob(&[9.0, 8.0])],
            )
            .unwrap();

        let error = store
            .validate_embedding_generation(generation_id)
            .unwrap_err();
        assert_eq!(error.code, "embedding.generation_corrupt");
        assert_eq!(
            store.embedding_generation_state(generation_id).unwrap(),
            "ready"
        );

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn ready_generation_rejects_missing_mapping_without_state_mutation() {
        let (paths, mut store, _, generation_id, _) =
            ready_generation_fixture("ready-generation-missing-mapping");
        store
            .conn
            .execute(
                "DELETE FROM embedding_generation_vector_rows WHERE generation_id = ?1",
                params![generation_id],
            )
            .unwrap();

        let error = store
            .validate_embedding_generation(generation_id)
            .unwrap_err();
        assert_eq!(error.code, "embedding.generation_corrupt");
        assert_eq!(
            store.embedding_generation_state(generation_id).unwrap(),
            "ready"
        );

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn stale_building_generation_is_removed_only_by_explicit_cleanup() {
        let paths = temp_profile_paths("generation-retention");
        let mut store = Store::open(&paths).unwrap();
        store.enable_vector().unwrap();
        store
            .upsert_sources_for_run("sync-retention", &[], &[], 0, &[])
            .unwrap();
        store.mark_sync_run_completed("sync-retention").unwrap();
        let snapshot = store.capture_retrieval_build_snapshot().unwrap().unwrap();
        let generation_id = store
            .begin_embedding_generation(
                &snapshot,
                &EmbeddingGenerationSpec {
                    model_manifest_hash: "manifest-retention".to_string(),
                    runtime_fingerprint_hash: "runtime-retention".to_string(),
                    chunker_fingerprint: "chunker-retention".to_string(),
                    context_template_version: crate::context::METADATA_CONTEXT_TEMPLATE_VERSION
                        .to_string(),
                    output_dimension: 2,
                },
            )
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

    #[test]
    fn exact_authoritative_noop_preserves_issue_and_comment_version_provenance() {
        let paths = temp_profile_paths("exact-authoritative-version-provenance");
        let mut store = Store::open(&paths).unwrap();
        let issue_id = "qgh://github.com/issue/I_EXACT_VERSION_PROVENANCE";
        let comment_id = "qgh://github.com/issue-comment/IC_EXACT_VERSION_PROVENANCE";
        let issue = test_issue(issue_id, "owner/repo", "exact-version-provenance");
        let comment = test_comment(
            comment_id,
            issue_id,
            "owner/repo",
            "exact-version-provenance",
        );
        store
            .upsert_sources_for_run(
                "sync-exact-version-first",
                std::slice::from_ref(&issue),
                std::slice::from_ref(&comment),
                0,
                &[],
            )
            .unwrap();
        let provenance = |store: &Store, source_id: &str| {
            store
                .conn
                .query_row(
                    "SELECT id, github_updated_at, indexed_at, sync_run_id, lifecycle_state
                     FROM source_versions WHERE source_id = ?1",
                    params![source_id],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                        ))
                    },
                )
                .unwrap()
        };
        let issue_before = provenance(&store, issue_id);
        let comment_before = provenance(&store, comment_id);
        let epoch_before = read_source_snapshot_epoch(&store.conn).unwrap();
        let mut repeated_issue = issue.clone();
        repeated_issue.indexed_at = "2026-01-03T00:00:01Z".to_string();
        let mut repeated_comment = comment.clone();
        repeated_comment.indexed_at = "2026-01-03T00:00:02Z".to_string();

        store
            .upsert_sources_for_run(
                "sync-exact-version-second",
                &[repeated_issue],
                &[repeated_comment],
                0,
                &[],
            )
            .unwrap();

        assert_eq!(provenance(&store, issue_id), issue_before);
        assert_eq!(provenance(&store, comment_id), comment_before);
        assert_eq!(
            read_source_snapshot_epoch(&store.conn).unwrap(),
            epoch_before
        );
        for source_id in [issue_id, comment_id] {
            let version_count: i64 = store
                .conn
                .query_row(
                    "SELECT count(*) FROM source_versions WHERE source_id = ?1",
                    params![source_id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(version_count, 1);
        }

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(not(feature = "vector-search"))]
    #[test]
    fn bm25_store_treats_null_and_mixed_chunk_fingerprints_as_stale() {
        let paths = temp_profile_paths("bm25-stale-chunk-fingerprints");
        let mut store = Store::open(&paths).unwrap();
        let source_id = "qgh://github.com/issue/I_BM25_STALE_CHUNK_FINGERPRINT";
        store
            .upsert_sources_for_run(
                "sync-bm25-stale-chunk-fingerprint",
                &[test_issue(source_id, "owner/repo", "bm25-stale-chunk")],
                &[],
                0,
                &[],
            )
            .unwrap();
        let source_version_id = store.latest_source_version_id(source_id).unwrap().unwrap();
        store
            .conn
            .execute_batch(&format!(
                "CREATE TABLE chunks (
                    id INTEGER PRIMARY KEY, source_version_id INTEGER NOT NULL,
                    chunker_fingerprint TEXT
                 );
                 CREATE TABLE embedding_fingerprints (id INTEGER PRIMARY KEY);
                 CREATE TABLE chunk_embeddings (chunk_id INTEGER PRIMARY KEY);
                 INSERT INTO chunks VALUES (1, {source_version_id}, NULL);
                 INSERT INTO chunks VALUES (
                    2, {source_version_id}, '{}'
                 );",
                crate::chunking::CHUNKER_FINGERPRINT
            ))
            .unwrap();

        assert!(!store
            .source_version_chunks_match_fingerprint(
                source_version_id,
                crate::chunking::CHUNKER_FINGERPRINT
            )
            .unwrap());
        store
            .conn
            .execute(
                "UPDATE chunks SET chunker_fingerprint = ?1",
                params![crate::chunking::CHUNKER_FINGERPRINT],
            )
            .unwrap();
        assert!(store
            .source_version_chunks_match_fingerprint(
                source_version_id,
                crate::chunking::CHUNKER_FINGERPRINT
            )
            .unwrap());
        store
            .conn
            .execute(
                "UPDATE chunks SET chunker_fingerprint = 'legacy-mixed' WHERE id = 2",
                [],
            )
            .unwrap();
        assert!(!store
            .source_version_chunks_match_fingerprint(
                source_version_id,
                crate::chunking::CHUNKER_FINGERPRINT
            )
            .unwrap());

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[test]
    fn authoritative_source_changes_still_update_provenance_and_reactivate() {
        let paths = temp_profile_paths("authoritative-source-change-provenance");
        let mut store = Store::open(&paths).unwrap();
        let issue_id = "qgh://github.com/issue/I_CHANGED_VERSION_PROVENANCE";
        let comment_id = "qgh://github.com/issue-comment/IC_CHANGED_VERSION_PROVENANCE";
        let mut issue = test_issue(issue_id, "owner/repo", "changed-version-provenance");
        let mut comment = test_comment(
            comment_id,
            issue_id,
            "owner/repo",
            "changed-version-provenance",
        );
        store
            .upsert_sources_for_run(
                "sync-changed-version-first",
                std::slice::from_ref(&issue),
                std::slice::from_ref(&comment),
                0,
                &[],
            )
            .unwrap();

        let epoch_before_timestamp = read_source_snapshot_epoch(&store.conn).unwrap();
        issue.updated_at = "2026-01-03T00:00:00Z".to_string();
        issue.indexed_at = "2026-01-03T00:00:01Z".to_string();
        comment.updated_at = "2026-01-03T00:00:00Z".to_string();
        comment.indexed_at = "2026-01-03T00:00:02Z".to_string();
        store
            .upsert_sources_for_run(
                "sync-changed-version-timestamp",
                std::slice::from_ref(&issue),
                std::slice::from_ref(&comment),
                0,
                &[],
            )
            .unwrap();
        assert_eq!(
            read_source_snapshot_epoch(&store.conn).unwrap(),
            epoch_before_timestamp + 1
        );
        for (source_id, indexed_at) in [
            (issue_id, "2026-01-03T00:00:01Z"),
            (comment_id, "2026-01-03T00:00:02Z"),
        ] {
            let provenance: (String, String, String) = store
                .conn
                .query_row(
                    "SELECT github_updated_at, indexed_at, sync_run_id
                     FROM source_versions WHERE source_id = ?1",
                    params![source_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap();
            assert_eq!(
                provenance,
                (
                    "2026-01-03T00:00:00Z".to_string(),
                    indexed_at.to_string(),
                    "sync-changed-version-timestamp".to_string()
                )
            );
        }

        let epoch_before_metadata = read_source_snapshot_epoch(&store.conn).unwrap();
        issue.title = "Changed metadata title".to_string();
        comment.parent_issue_title = issue.title.clone();
        store
            .upsert_sources_for_run(
                "sync-changed-version-metadata",
                std::slice::from_ref(&issue),
                std::slice::from_ref(&comment),
                0,
                &[],
            )
            .unwrap();
        assert_eq!(
            read_source_snapshot_epoch(&store.conn).unwrap(),
            epoch_before_metadata + 1
        );
        assert_eq!(
            store.get_issue(issue_id).unwrap().unwrap().title,
            "Changed metadata title"
        );
        assert_eq!(
            store
                .get_comment(comment_id)
                .unwrap()
                .unwrap()
                .parent_issue
                .title,
            "Changed metadata title"
        );

        store.tombstone_source(issue_id, "deleted").unwrap();
        let epoch_before_reactivation = read_source_snapshot_epoch(&store.conn).unwrap();
        store
            .upsert_sources_for_run(
                "sync-changed-version-reactivation",
                std::slice::from_ref(&issue),
                &[],
                0,
                &[],
            )
            .unwrap();
        assert_eq!(
            read_source_snapshot_epoch(&store.conn).unwrap(),
            epoch_before_reactivation + 1
        );
        assert!(store.get_issue(issue_id).unwrap().is_some());
        assert!(store.get_tombstone(issue_id).unwrap().is_none());

        let epoch_before_edit = read_source_snapshot_epoch(&store.conn).unwrap();
        comment.body = "Edited authoritative comment body".to_string();
        comment.body_hash = "edited-authoritative-comment-hash".to_string();
        comment.updated_at = "2026-01-04T00:00:00Z".to_string();
        comment.indexed_at = "2026-01-04T00:00:01Z".to_string();
        store
            .upsert_sources_for_run("sync-changed-version-edit", &[], &[comment], 0, &[])
            .unwrap();
        assert_eq!(
            read_source_snapshot_epoch(&store.conn).unwrap(),
            epoch_before_edit + 1
        );
        let comment_version_count: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM source_versions WHERE source_id = ?1",
                params![comment_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(comment_version_count, 2);

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    #[cfg(feature = "vector-search")]
    #[test]
    fn parent_issue_title_only_upsert_invalidates_comment_context_without_new_body_version() {
        let paths = temp_profile_paths("parent-title-context-invalidation");
        let mut store = Store::open(&paths).unwrap();
        store.enable_vector().unwrap();
        let issue_id = "qgh://github.com/issue/I_PARENT_CONTEXT";
        let comment_id = "qgh://github.com/issue-comment/IC_PARENT_CONTEXT";
        let issue = test_issue(issue_id, "owner/repo", "original");
        let mut comment = test_comment(comment_id, issue_id, "owner/repo", "comment");
        comment.parent_issue_title = issue.title.clone();
        store
            .upsert_sources_for_run(
                "sync-parent-context-original",
                std::slice::from_ref(&issue),
                std::slice::from_ref(&comment),
                0,
                &[],
            )
            .unwrap();
        let comment_version_id = store.latest_source_version_id(comment_id).unwrap().unwrap();
        let chunk_id = store
            .replace_chunks_for_source_version(
                comment_id,
                comment_version_id,
                &[test_chunk("unchanged comment chunk")],
            )
            .unwrap()[0]
            .chunk_id;
        store
            .mark_sync_run_completed("sync-parent-context-original")
            .unwrap();
        let snapshot = store.capture_retrieval_build_snapshot().unwrap().unwrap();
        let before = store
            .active_contextual_embedding_chunks()
            .unwrap()
            .remove(0);
        let model_manifest_hash = "manifest-parent-context";
        let generation_id = store
            .begin_embedding_generation(
                &snapshot,
                &EmbeddingGenerationSpec {
                    model_manifest_hash: model_manifest_hash.to_string(),
                    runtime_fingerprint_hash: format!("runtime-{model_manifest_hash}"),
                    chunker_fingerprint: before.chunk.chunker_fingerprint.clone(),
                    context_template_version: crate::context::METADATA_CONTEXT_TEMPLATE_VERSION
                        .to_string(),
                    output_dimension: 2,
                },
            )
            .unwrap();
        store
            .stage_embedding_generation_batch(
                generation_id,
                &[EmbeddingGenerationChunk {
                    chunk_id,
                    source_version_id: comment_version_id,
                    source_version_hash: comment.body_hash.clone(),
                    context_hash: before
                        .prepared_input
                        .context_hash(model_manifest_hash, &before.chunk.chunker_fingerprint),
                    vector: vec![1.0, 2.0],
                }],
            )
            .unwrap();
        let dirty_comment_tasks_before: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM index_tasks WHERE source_id = ?1 AND completed_at IS NULL",
                params![comment_id],
                |row| row.get(0),
            )
            .unwrap();

        let mut renamed_issue = issue.clone();
        renamed_issue.title = "Renamed parent title".to_string();
        renamed_issue.updated_at = "2026-01-03T00:00:00Z".to_string();
        renamed_issue.indexed_at = "2026-01-03T00:00:01Z".to_string();
        store
            .upsert_sources_for_run("sync-parent-context-renamed", &[renamed_issue], &[], 0, &[])
            .unwrap();

        let stored_comment = store.get_comment(comment_id).unwrap().unwrap();
        assert_eq!(stored_comment.parent_issue.title, "Renamed parent title");
        assert_eq!(stored_comment.body, comment.body);
        assert_eq!(
            store.latest_source_version_id(comment_id).unwrap(),
            Some(comment_version_id)
        );
        let dirty_comment_tasks_after: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM index_tasks WHERE source_id = ?1 AND completed_at IS NULL",
                params![comment_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(dirty_comment_tasks_after, dirty_comment_tasks_before + 1);
        let error = store
            .validate_embedding_generation(generation_id)
            .unwrap_err();
        assert_eq!(error.code, "publication.source_snapshot_changed");

        let _ = fs::remove_dir_all(paths.profile_dir);
    }

    fn test_issue(source_id: &str, repo: &str, private_marker: &str) -> IssueRecord {
        IssueRecord {
            source_id: source_id.to_string(),
            host: "github.com".to_string(),
            repo: repo.to_string(),
            node_id: source_id.rsplit('/').next().unwrap().to_string(),
            github_id: 404,
            number: 47,
            title: format!("Title {private_marker}"),
            body: format!("Body {private_marker}"),
            state: "open".to_string(),
            labels: vec![format!("label-{private_marker}")],
            milestone: Some(format!("milestone-{private_marker}")),
            assignees: vec![format!("assignee-{private_marker}")],
            author: Some(format!("author-{private_marker}")),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-02T00:00:00Z".to_string(),
            closed_at: None,
            canonical_url: format!("https://github.com/{repo}/issues/47"),
            body_hash: format!("body-hash-{private_marker}"),
            indexed_at: "2026-01-02T00:00:01Z".to_string(),
        }
    }

    fn test_comment(
        source_id: &str,
        parent_issue_source_id: &str,
        repo: &str,
        private_marker: &str,
    ) -> CommentRecord {
        CommentRecord {
            source_id: source_id.to_string(),
            host: "github.com".to_string(),
            repo: repo.to_string(),
            node_id: source_id.rsplit('/').next().unwrap().to_string(),
            github_id: 405,
            body: format!("Comment {private_marker}"),
            author: Some(format!("comment-author-{private_marker}")),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-02T00:00:00Z".to_string(),
            canonical_url: format!("https://github.com/{repo}/issues/47#issuecomment-405"),
            body_hash: format!("comment-body-hash-{private_marker}"),
            indexed_at: "2026-01-02T00:00:01Z".to_string(),
            parent_issue_source_id: parent_issue_source_id.to_string(),
            parent_issue_number: 47,
            parent_issue_title: format!("Parent {private_marker}"),
            parent_issue_canonical_url: format!("https://github.com/{repo}/issues/47"),
        }
    }

    fn test_chunk(private_marker: &str) -> MarkdownChunk {
        MarkdownChunk {
            chunk_index: 0,
            byte_start: 0,
            byte_end: private_marker.len(),
            token_start: 0,
            token_end: 1,
            token_count: 1,
            body: private_marker.to_string(),
            chunker_version: crate::chunking::CHUNKER_VERSION.to_string(),
            chunker_fingerprint: crate::chunking::CHUNKER_FINGERPRINT.to_string(),
            heading_path: Vec::new(),
        }
    }

    #[cfg(feature = "vector-search")]
    fn insert_chunk(store: &mut Store, source_id: &str, body: &str) -> i64 {
        let source_version_id = store.latest_source_version_id(source_id).unwrap().unwrap();
        store
            .replace_chunks_for_source_version(source_id, source_version_id, &[test_chunk(body)])
            .unwrap()[0]
            .chunk_id
    }

    #[cfg(feature = "vector-search")]
    fn production_context_hash_for_chunk(
        store: &Store,
        model_manifest_hash: &str,
        chunker_fingerprint: &str,
        chunk_id: i64,
    ) -> String {
        store
            .active_contextual_embedding_chunks()
            .unwrap()
            .into_iter()
            .find(|chunk| chunk.chunk.chunk_id == chunk_id)
            .unwrap()
            .prepared_input
            .context_hash(model_manifest_hash, chunker_fingerprint)
    }

    #[cfg(feature = "vector-search")]
    fn stage_test_generation(store: &mut Store, manifest: &str, chunk_ids: &[i64]) -> i64 {
        seal_latest_test_sync(store);
        let snapshot = store.capture_retrieval_build_snapshot().unwrap().unwrap();
        let spec = EmbeddingGenerationSpec {
            model_manifest_hash: manifest.to_string(),
            runtime_fingerprint_hash: format!("runtime-{manifest}"),
            chunker_fingerprint: crate::chunking::CHUNKER_FINGERPRINT.to_string(),
            context_template_version: crate::context::METADATA_CONTEXT_TEMPLATE_VERSION.to_string(),
            output_dimension: 2,
        };
        let generation_id = store.begin_embedding_generation(&snapshot, &spec).unwrap();
        let chunks = chunk_ids
            .iter()
            .map(|chunk_id| {
                let source_version_id: i64 = store
                    .conn
                    .query_row(
                        "SELECT source_version_id FROM chunks WHERE id = ?1",
                        params![chunk_id],
                        |row| row.get(0),
                    )
                    .unwrap();
                let source_version_hash = store
                    .source_version_hash(source_version_id)
                    .unwrap()
                    .unwrap();
                EmbeddingGenerationChunk {
                    chunk_id: *chunk_id,
                    source_version_id,
                    source_version_hash,
                    context_hash: production_context_hash_for_chunk(
                        store,
                        manifest,
                        crate::chunking::CHUNKER_FINGERPRINT,
                        *chunk_id,
                    ),
                    vector: vec![1.0 + *chunk_id as f32, 2.0],
                }
            })
            .collect::<Vec<_>>();
        store
            .stage_embedding_generation_batch(generation_id, &chunks)
            .unwrap();
        store.validate_embedding_generation(generation_id).unwrap();
        generation_id
    }

    #[cfg(feature = "vector-search")]
    fn ready_generation_fixture(name: &str) -> (ProfilePaths, Store, i64, i64, String) {
        let paths = temp_profile_paths(name);
        let mut store = Store::open(&paths).unwrap();
        store.enable_vector().unwrap();
        let source_id = format!("qgh://github.com/issue/I_{name}");
        let chunk_id = insert_test_issue_chunk(&mut store, &source_id, &format!("sync-{name}"));
        let manifest = format!("manifest-{name}");
        let generation_id = stage_test_generation(&mut store, &manifest, &[chunk_id]);
        (paths, store, chunk_id, generation_id, manifest)
    }

    #[cfg(feature = "vector-search")]
    fn publish_test_retrieval(
        store: &mut Store,
        paths: &ProfilePaths,
        embedding_generation_id: Option<i64>,
    ) -> i64 {
        let (snapshot, tantivy_generation) = reserve_test_retrieval(store, paths);
        store
            .activate_retrieval_publication(
                snapshot.identity().sync_run_id(),
                tantivy_generation,
                embedding_generation_id,
                snapshot.expected_publication_id(),
            )
            .unwrap()
    }

    #[cfg(feature = "vector-search")]
    fn reserve_test_retrieval(
        store: &mut Store,
        paths: &ProfilePaths,
    ) -> (RetrievalBuildSnapshot, i64) {
        let snapshot = store.capture_retrieval_build_snapshot().unwrap().unwrap();
        let (tantivy_generation, _) = store
            .reserve_index_generation_for_snapshot(&paths.index_root, &snapshot)
            .unwrap();
        rebuild_reserved_generation(store, paths, tantivy_generation);
        (snapshot, tantivy_generation)
    }

    #[cfg(feature = "vector-search")]
    fn staged_test_chunk(store: &Store, chunk_id: i64, manifest: &str) -> EmbeddingGenerationChunk {
        let source_version_id: i64 = store
            .conn
            .query_row(
                "SELECT source_version_id FROM chunks WHERE id = ?1",
                params![chunk_id],
                |row| row.get(0),
            )
            .unwrap();
        EmbeddingGenerationChunk {
            chunk_id,
            source_version_id,
            source_version_hash: store
                .source_version_hash(source_version_id)
                .unwrap()
                .unwrap(),
            context_hash: production_context_hash_for_chunk(
                store,
                manifest,
                crate::chunking::CHUNKER_FINGERPRINT,
                chunk_id,
            ),
            vector: vec![1.0, 2.0],
        }
    }

    #[cfg(feature = "vector-search")]
    fn corrupt_mapping_table_with_sentinel(store: &Store, generation_id: i64) {
        let mapping_id: i64 = store
            .conn
            .query_row(
                "SELECT id FROM embedding_generation_vector_rows WHERE generation_id = ?1",
                params![generation_id],
                |row| row.get(0),
            )
            .unwrap();
        store
            .conn
            .execute_batch(
                "CREATE TABLE cleanup_sentinel(marker TEXT NOT NULL);
                 INSERT INTO cleanup_sentinel(rowid, marker) VALUES (1, 'preserve');",
            )
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE cleanup_sentinel SET rowid = ?1 WHERE rowid = 1",
                params![mapping_id],
            )
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE embedding_generation_vector_rows
                 SET vector_table = 'cleanup_sentinel'
                 WHERE generation_id = ?1",
                params![generation_id],
            )
            .unwrap();
    }

    fn seal_latest_test_sync(store: &mut Store) -> String {
        let sync_run_id = store
            .conn
            .query_row(
                "SELECT id FROM sync_runs ORDER BY rowid DESC LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        store.mark_sync_run_completed(&sync_run_id).unwrap();
        sync_run_id
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
        let chunk_id = store
            .replace_chunks_for_source_version(source_id, source_version_id, &chunks)
            .unwrap()[0]
            .chunk_id;
        store.mark_sync_run_completed(sync_run_id).unwrap();
        chunk_id
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

    fn rebuild_reserved_generation(
        store: &Store,
        _paths: &ProfilePaths,
        generation: i64,
    ) -> PathBuf {
        let expected_source_count: i64 = store
            .conn
            .query_row(
                "SELECT source_count FROM index_generations WHERE generation = ?1",
                params![generation],
                |row| row.get(0),
            )
            .unwrap();
        let sources = store.active_index_sources().unwrap();
        assert_eq!(sources.len(), expected_source_count as usize);
        store
            .rebuild_reserved_index_generation(generation, &sources)
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
