use crate::config::{Profile, RepoRef};
use crate::error::QghError;
use crate::model::{CommentRecord, CursorUpdate, IssueRecord, StoredCursor};
use crate::time::now_rfc3339;
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use reqwest::header::{HeaderMap, ETAG, IF_NONE_MATCH, LINK};
use reqwest::StatusCode;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

const SOURCE_ID_ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'/')
    .add(b':')
    .add(b';')
    .add(b'=')
    .add(b'@')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'|');

pub struct FetchResult {
    pub issues: Vec<IssueRecord>,
    pub comments: Vec<CommentRecord>,
    pub skipped_pull_requests: usize,
    pub cursor_updates: Vec<CursorUpdate>,
}

pub async fn fetch_issues(
    profile: &Profile,
    token: &str,
    cursors: &[StoredCursor],
) -> Result<FetchResult, QghError> {
    let client = reqwest::Client::new();
    let cursor_map = cursor_map(cursors);
    let mut issues = Vec::new();
    let mut comments = Vec::new();
    let mut cursor_updates = Vec::new();
    let mut skipped_pull_requests = 0;

    for repo in &profile.repos {
        let endpoint = issue_endpoint(repo);
        let stored_cursor = cursor_map.get(&endpoint);
        let mut max_watermark = stored_cursor.and_then(|cursor| cursor.cursor.clone());
        let mut next_url = Some(issue_url(profile, repo, stored_cursor));
        let mut endpoint_not_modified = false;
        let mut response_etag = stored_cursor.and_then(|cursor| cursor.etag.clone());
        while let Some(url) = next_url.take() {
            let mut request = client
                .get(&url)
                .bearer_auth(token)
                .header("accept", "application/vnd.github+json");
            if let Some(etag) = stored_cursor.and_then(|cursor| cursor.etag.as_ref()) {
                request = request.header(IF_NONE_MATCH, etag);
            }
            let response = request
                .send()
                .await
                .map_err(|error| QghError::github(error.to_string()))?;
            let status = response.status();
            let headers = response.headers().clone();
            if status == StatusCode::NOT_MODIFIED {
                endpoint_not_modified = true;
                break;
            }
            if !status.is_success() {
                return Err(QghError::github(format!(
                    "GitHub issues request failed with HTTP {status}."
                )));
            }
            response_etag = header_string(&headers, ETAG).or(response_etag);
            let page: Vec<ApiIssue> = response
                .json()
                .await
                .map_err(|error| QghError::github(format!("Invalid GitHub issue JSON: {error}")))?;
            let indexed_at = now_rfc3339();
            for item in page {
                if item.pull_request.is_some() {
                    skipped_pull_requests += 1;
                    continue;
                }
                let issue = item.into_record(profile, repo, &indexed_at);
                max_watermark = max_timestamp(max_watermark, &issue.updated_at);
                comments.extend(
                    fetch_issue_comments(
                        &client,
                        profile,
                        token,
                        &cursor_map,
                        &mut cursor_updates,
                        repo,
                        &issue,
                    )
                    .await?,
                );
                issues.push(issue);
            }
            next_url = next_link(&headers);
        }
        cursor_updates.push(CursorUpdate {
            endpoint,
            cursor: max_watermark,
            etag: response_etag,
            not_modified: endpoint_not_modified,
        });
    }

    Ok(FetchResult {
        issues,
        comments,
        skipped_pull_requests,
        cursor_updates,
    })
}

async fn fetch_issue_comments(
    client: &reqwest::Client,
    profile: &Profile,
    token: &str,
    cursor_map: &BTreeMap<String, StoredCursor>,
    cursor_updates: &mut Vec<CursorUpdate>,
    repo: &RepoRef,
    issue: &IssueRecord,
) -> Result<Vec<CommentRecord>, QghError> {
    let mut comments = Vec::new();
    let endpoint = comment_endpoint(repo, issue.number);
    let stored_cursor = cursor_map.get(&endpoint);
    let mut max_watermark = stored_cursor.and_then(|cursor| cursor.cursor.clone());
    let mut response_etag = stored_cursor.and_then(|cursor| cursor.etag.clone());
    let mut endpoint_not_modified = false;
    let mut next_url = Some(comment_url(profile, repo, issue.number, stored_cursor));
    while let Some(url) = next_url.take() {
        let mut request = client
            .get(&url)
            .bearer_auth(token)
            .header("accept", "application/vnd.github+json");
        if let Some(etag) = stored_cursor.and_then(|cursor| cursor.etag.as_ref()) {
            request = request.header(IF_NONE_MATCH, etag);
        }
        let response = request
            .send()
            .await
            .map_err(|error| QghError::github(error.to_string()))?;
        let status = response.status();
        let headers = response.headers().clone();
        if status == StatusCode::NOT_MODIFIED {
            endpoint_not_modified = true;
            break;
        }
        if !status.is_success() {
            return Err(QghError::github(format!(
                "GitHub issue comments request failed with HTTP {status}."
            )));
        }
        response_etag = header_string(&headers, ETAG).or(response_etag);
        let page: Vec<ApiComment> = response
            .json()
            .await
            .map_err(|error| QghError::github(format!("Invalid GitHub comment JSON: {error}")))?;
        let indexed_at = now_rfc3339();
        for comment in page {
            max_watermark = max_timestamp(max_watermark, &comment.updated_at);
            comments.push(comment.into_record(profile, repo, issue, &indexed_at));
        }
        next_url = next_link(&headers);
    }
    cursor_updates.push(CursorUpdate {
        endpoint,
        cursor: max_watermark,
        etag: response_etag,
        not_modified: endpoint_not_modified,
    });
    Ok(comments)
}

#[derive(Debug, Deserialize)]
struct ApiIssue {
    id: i64,
    node_id: String,
    number: i64,
    title: String,
    body: Option<String>,
    state: String,
    labels: Vec<ApiLabel>,
    milestone: Option<ApiMilestone>,
    assignees: Vec<ApiUser>,
    user: Option<ApiUser>,
    created_at: String,
    updated_at: String,
    closed_at: Option<String>,
    html_url: String,
    #[serde(default)]
    pull_request: Option<serde_json::Value>,
}

impl ApiIssue {
    fn into_record(self, profile: &Profile, repo: &RepoRef, indexed_at: &str) -> IssueRecord {
        let body = self.body.unwrap_or_default();
        let body_hash = hex_sha256(&body);
        let encoded_node_id = utf8_percent_encode(&self.node_id, SOURCE_ID_ENCODE_SET).to_string();
        IssueRecord {
            source_id: format!("qgh://{}/issue/{}", profile.host, encoded_node_id),
            host: profile.host.clone(),
            repo: repo.full_name(),
            node_id: self.node_id,
            github_id: self.id,
            number: self.number,
            title: self.title,
            body,
            state: self.state,
            labels: self.labels.into_iter().map(|label| label.name).collect(),
            milestone: self.milestone.map(|milestone| milestone.title),
            assignees: self.assignees.into_iter().map(|user| user.login).collect(),
            author: self.user.map(|user| user.login),
            created_at: self.created_at,
            updated_at: self.updated_at,
            closed_at: self.closed_at,
            canonical_url: self.html_url,
            body_hash,
            indexed_at: indexed_at.to_string(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct ApiLabel {
    name: String,
}

#[derive(Debug, Deserialize)]
struct ApiMilestone {
    title: String,
}

#[derive(Debug, Deserialize)]
struct ApiUser {
    login: String,
}

#[derive(Debug, Deserialize)]
struct ApiComment {
    id: i64,
    node_id: String,
    body: Option<String>,
    html_url: String,
    created_at: String,
    updated_at: String,
    user: Option<ApiUser>,
}

impl ApiComment {
    fn into_record(
        self,
        profile: &Profile,
        repo: &RepoRef,
        issue: &IssueRecord,
        indexed_at: &str,
    ) -> CommentRecord {
        let body = self.body.unwrap_or_default();
        let body_hash = hex_sha256(&body);
        let encoded_node_id = utf8_percent_encode(&self.node_id, SOURCE_ID_ENCODE_SET).to_string();
        CommentRecord {
            source_id: format!("qgh://{}/issue-comment/{}", profile.host, encoded_node_id),
            host: profile.host.clone(),
            repo: repo.full_name(),
            node_id: self.node_id,
            github_id: self.id,
            body,
            author: self.user.map(|user| user.login),
            created_at: self.created_at,
            updated_at: self.updated_at,
            canonical_url: self.html_url,
            body_hash,
            indexed_at: indexed_at.to_string(),
            parent_issue_source_id: issue.source_id.clone(),
            parent_issue_number: issue.number,
            parent_issue_title: issue.title.clone(),
            parent_issue_canonical_url: issue.canonical_url.clone(),
        }
    }
}

fn next_link(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(LINK)?.to_str().ok()?;
    for part in value.split(',') {
        let trimmed = part.trim();
        if !trimmed.contains("rel=\"next\"") {
            continue;
        }
        let start = trimmed.find('<')? + 1;
        let end = trimmed[start..].find('>')? + start;
        return Some(trimmed[start..end].to_string());
    }
    None
}

fn cursor_map(cursors: &[StoredCursor]) -> BTreeMap<String, StoredCursor> {
    cursors
        .iter()
        .cloned()
        .map(|cursor| (cursor.endpoint.clone(), cursor))
        .collect()
}

fn issue_endpoint(repo: &RepoRef) -> String {
    format!("issues:{}", repo.full_name())
}

fn comment_endpoint(repo: &RepoRef, issue_number: i64) -> String {
    format!("comments:{}#{issue_number}", repo.full_name())
}

fn issue_url(profile: &Profile, repo: &RepoRef, cursor: Option<&StoredCursor>) -> String {
    let mut url = format!(
        "{}/repos/{}/{}/issues?state=all&sort=updated&direction=asc&per_page=100",
        profile.api_base_url, repo.owner, repo.name
    );
    if let Some(since) = cursor
        .and_then(|cursor| cursor.cursor.as_deref())
        .map(overlapped_since)
    {
        url.push_str("&since=");
        url.push_str(&utf8_percent_encode(&since, SOURCE_ID_ENCODE_SET).to_string());
    }
    url
}

fn comment_url(
    profile: &Profile,
    repo: &RepoRef,
    issue_number: i64,
    cursor: Option<&StoredCursor>,
) -> String {
    let mut url = format!(
        "{}/repos/{}/{}/issues/{issue_number}/comments?per_page=100",
        profile.api_base_url, repo.owner, repo.name
    );
    if let Some(since) = cursor
        .and_then(|cursor| cursor.cursor.as_deref())
        .map(overlapped_since)
    {
        url.push_str("&since=");
        url.push_str(&utf8_percent_encode(&since, SOURCE_ID_ENCODE_SET).to_string());
    }
    url
}

fn overlapped_since(timestamp: &str) -> String {
    DateTime::parse_from_rfc3339(timestamp)
        .map(|parsed| {
            (parsed.with_timezone(&Utc) - Duration::seconds(60))
                .to_rfc3339_opts(SecondsFormat::Secs, true)
        })
        .unwrap_or_else(|_| timestamp.to_string())
}

fn max_timestamp(current: Option<String>, candidate: &str) -> Option<String> {
    match current {
        Some(current) if current.as_str() >= candidate => Some(current),
        _ => Some(candidate.to_string()),
    }
}

fn header_string(headers: &HeaderMap, name: reqwest::header::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_string())
}

fn hex_sha256(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}
