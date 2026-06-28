use crate::config::{Profile, RepoRef};
use crate::error::QghError;
use crate::model::{CommentRecord, IssueRecord};
use crate::time::now_rfc3339;
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use reqwest::header::{HeaderMap, LINK};
use serde::Deserialize;
use sha2::{Digest, Sha256};

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
}

pub async fn fetch_issues(profile: &Profile, token: &str) -> Result<FetchResult, QghError> {
    let client = reqwest::Client::new();
    let mut issues = Vec::new();
    let mut comments = Vec::new();
    let mut skipped_pull_requests = 0;

    for repo in &profile.repos {
        let mut next_url = Some(format!(
            "{}/repos/{}/{}/issues?state=all&sort=updated&direction=asc&per_page=100",
            profile.api_base_url, repo.owner, repo.name
        ));
        while let Some(url) = next_url.take() {
            let response = client
                .get(&url)
                .bearer_auth(token)
                .header("accept", "application/vnd.github+json")
                .send()
                .await
                .map_err(|error| QghError::github(error.to_string()))?;
            let status = response.status();
            let headers = response.headers().clone();
            if !status.is_success() {
                return Err(QghError::github(format!(
                    "GitHub issues request failed with HTTP {status}."
                )));
            }
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
                comments.extend(fetch_issue_comments(&client, profile, token, repo, &issue).await?);
                issues.push(issue);
            }
            next_url = next_link(&headers);
        }
    }

    Ok(FetchResult {
        issues,
        comments,
        skipped_pull_requests,
    })
}

async fn fetch_issue_comments(
    client: &reqwest::Client,
    profile: &Profile,
    token: &str,
    repo: &RepoRef,
    issue: &IssueRecord,
) -> Result<Vec<CommentRecord>, QghError> {
    let mut comments = Vec::new();
    let mut next_url = Some(format!(
        "{}/repos/{}/{}/issues/{}/comments?per_page=100",
        profile.api_base_url, repo.owner, repo.name, issue.number
    ));
    while let Some(url) = next_url.take() {
        let response = client
            .get(&url)
            .bearer_auth(token)
            .header("accept", "application/vnd.github+json")
            .send()
            .await
            .map_err(|error| QghError::github(error.to_string()))?;
        let status = response.status();
        let headers = response.headers().clone();
        if !status.is_success() {
            return Err(QghError::github(format!(
                "GitHub issue comments request failed with HTTP {status}."
            )));
        }
        let page: Vec<ApiComment> = response
            .json()
            .await
            .map_err(|error| QghError::github(format!("Invalid GitHub comment JSON: {error}")))?;
        let indexed_at = now_rfc3339();
        comments.extend(
            page.into_iter()
                .map(|comment| comment.into_record(profile, repo, issue, &indexed_at)),
        );
        next_url = next_link(&headers);
    }
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

fn hex_sha256(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}
