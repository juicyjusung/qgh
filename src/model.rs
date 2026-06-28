use serde::Serialize;

#[derive(Debug, Clone)]
pub struct IssueRecord {
    pub source_id: String,
    pub host: String,
    pub repo: String,
    pub node_id: String,
    pub github_id: i64,
    pub number: i64,
    pub title: String,
    pub body: String,
    pub state: String,
    pub labels: Vec<String>,
    pub milestone: Option<String>,
    pub assignees: Vec<String>,
    pub author: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
    pub canonical_url: String,
    pub body_hash: String,
    pub indexed_at: String,
}

#[derive(Debug, Clone)]
pub struct CommentRecord {
    pub source_id: String,
    pub host: String,
    pub repo: String,
    pub node_id: String,
    pub github_id: i64,
    pub body: String,
    pub author: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub canonical_url: String,
    pub body_hash: String,
    pub indexed_at: String,
    pub parent_issue_source_id: String,
    pub parent_issue_number: i64,
    pub parent_issue_title: String,
    pub parent_issue_canonical_url: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceVersionView {
    pub body_hash: String,
    pub github_updated_at: String,
    pub indexed_at: String,
    pub sync_run_id: String,
    pub lifecycle_state: String,
}

#[derive(Debug, Clone)]
pub struct StoredIssue {
    pub source_id: String,
    pub repo: String,
    pub number: i64,
    pub title: String,
    pub body: String,
    pub state: String,
    pub labels: Vec<String>,
    pub author: Option<String>,
    pub canonical_url: String,
    pub source_version: SourceVersionView,
}

#[derive(Debug, Clone, Serialize)]
pub struct ParentIssueView {
    pub source_id: String,
    pub repo: String,
    pub number: i64,
    pub title: String,
    pub canonical_url: String,
}

#[derive(Debug, Clone)]
pub struct StoredComment {
    pub source_id: String,
    pub repo: String,
    pub issue_number: i64,
    pub body: String,
    pub author: Option<String>,
    pub canonical_url: String,
    pub parent_issue: ParentIssueView,
    pub source_version: SourceVersionView,
}

#[derive(Debug, Clone)]
pub enum StoredSource {
    Issue(StoredIssue),
    Comment(StoredComment),
}

#[derive(Debug, Clone)]
pub struct ReconciliationCandidate {
    pub source_id: String,
    pub entity_type: String,
    pub repo: String,
    pub issue_number: i64,
    pub github_id: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TombstoneView {
    pub source_id: String,
    pub reason: String,
    pub observed_at: String,
}

#[derive(Debug, Clone)]
pub struct ReconciliationRunView {
    pub completed_at: String,
    pub checked_source_count: i64,
    pub tombstoned_count: i64,
    pub estimated_api_cost_class: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct BackoffView {
    pub reason: String,
    pub scope: String,
    pub retry_after_seconds: i64,
    pub reset_at: Option<String>,
    pub observed_at: String,
    pub last_successful_sync: Option<String>,
}

#[derive(Debug, Clone)]
pub struct IndexSource {
    pub source_id: String,
    pub entity_type: String,
    pub repo: String,
    pub issue_number: i64,
    pub state: String,
    pub labels: Vec<String>,
    pub author: Option<String>,
    pub title: String,
    pub body: String,
    pub parent_issue_title: String,
    pub github_updated_at: String,
    pub indexed_at: String,
}

#[derive(Debug, Clone)]
pub struct SyncSummary {
    pub sync_run_id: String,
    pub fetched_issues: usize,
    pub upserted_issues: usize,
    pub fetched_comments: usize,
    pub upserted_comments: usize,
    pub skipped_pull_requests: usize,
    pub cursor_updates: Vec<CursorView>,
    pub not_modified_endpoints: usize,
}

#[derive(Debug, Clone)]
pub struct StatusSnapshot {
    pub issue_count: i64,
    pub comment_count: i64,
    pub tombstone_count: i64,
    pub active_generation: i64,
    pub dirty_task_count: i64,
    pub last_sync_at: Option<String>,
    pub last_reconciliation: Option<ReconciliationRunView>,
    pub backoff: Option<BackoffView>,
    pub cursors: Vec<CursorView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CursorView {
    pub endpoint: String,
    pub watermark: Option<String>,
    pub has_etag: bool,
}

#[derive(Debug, Clone)]
pub struct StoredCursor {
    pub endpoint: String,
    pub cursor: Option<String>,
    pub etag: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CursorUpdate {
    pub endpoint: String,
    pub cursor: Option<String>,
    pub etag: Option<String>,
    pub not_modified: bool,
}
