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

#[derive(Debug, Clone, Serialize)]
pub struct SourceVersionView {
    pub body_hash: String,
    pub github_updated_at: String,
    pub indexed_at: String,
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

#[derive(Debug, Clone)]
pub struct SyncSummary {
    pub sync_run_id: String,
    pub fetched: usize,
    pub upserted: usize,
    pub skipped_pull_requests: usize,
}

#[derive(Debug, Clone)]
pub struct StatusSnapshot {
    pub issue_count: i64,
    pub tombstone_count: i64,
    pub active_generation: i64,
    pub dirty_task_count: i64,
    pub last_sync_at: Option<String>,
}
