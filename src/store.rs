use crate::error::QghError;
use crate::model::{
    CommentRecord, IndexSource, IssueRecord, ParentIssueView, SourceVersionView, StatusSnapshot,
    StoredComment, StoredIssue, StoredSource, SyncSummary,
};
use crate::paths::ProfilePaths;
use crate::time::{now_rfc3339, now_run_id_suffix};
use rusqlite::{params, Connection, OptionalExtension};
use std::fs;

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(paths: &ProfilePaths) -> Result<Self, QghError> {
        fs::create_dir_all(&paths.profile_dir)?;
        let conn = Connection::open(&paths.db_path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    pub fn upsert_sources(
        &mut self,
        issues: &[IssueRecord],
        comments: &[CommentRecord],
        skipped_pull_requests: usize,
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

        tx.commit()?;
        Ok(SyncSummary {
            sync_run_id,
            fetched_issues: issues.len(),
            upserted_issues: issues.len(),
            fetched_comments: comments.len(),
            upserted_comments: comments.len(),
            skipped_pull_requests,
        })
    }

    pub fn active_issues(&self) -> Result<Vec<StoredIssue>, QghError> {
        let mut stmt = self.conn.prepare(
            "SELECT im.source_id, im.repo, im.issue_number, im.title, im.body, im.state,
                    im.labels_json, im.author, im.canonical_url,
                    sv.body_hash, sv.github_updated_at, sv.indexed_at
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
                        sv.body_hash, sv.github_updated_at, sv.indexed_at
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
                        cm.parent_issue_canonical_url, sv.body_hash, sv.github_updated_at, sv.indexed_at
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

    pub fn mark_index_published(
        &self,
        generation: i64,
        path: &str,
        source_count: usize,
    ) -> Result<(), QghError> {
        let now = now_rfc3339();
        self.conn
            .execute("UPDATE index_generations SET active = 0", [])?;
        self.conn.execute(
            "INSERT INTO index_generations (generation, path, source_count, created_at, active)
             VALUES (?1, ?2, ?3, ?4, 1)",
            params![generation, path, source_count as i64, now],
        )?;
        self.conn.execute(
            "UPDATE index_tasks SET completed_at = ?1 WHERE completed_at IS NULL",
            params![now],
        )?;
        Ok(())
    }

    pub fn next_index_generation(&self) -> Result<i64, QghError> {
        let current: Option<i64> = self
            .conn
            .query_row("SELECT max(generation) FROM index_generations", [], |row| {
                row.get(0)
            })
            .optional()?
            .flatten();
        Ok(current.unwrap_or(0) + 1)
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
        Ok(StatusSnapshot {
            issue_count,
            comment_count,
            tombstone_count,
            active_generation: active_generation.unwrap_or(0),
            dirty_task_count,
            last_sync_at,
        })
    }

    fn migrate(&self) -> Result<(), QghError> {
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

            CREATE TABLE IF NOT EXISTS tombstones (
                source_id TEXT PRIMARY KEY,
                reason TEXT NOT NULL,
                observed_at TEXT NOT NULL
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
        "INSERT OR IGNORE INTO source_versions
            (source_id, body_hash, github_updated_at, indexed_at, sync_run_id, lifecycle_state)
         VALUES (?1, ?2, ?3, ?4, ?5, 'active')",
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
        },
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
