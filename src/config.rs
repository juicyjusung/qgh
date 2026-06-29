use crate::error::QghError;
use crate::paths::{config_file_path, ProfilePaths};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone)]
pub struct Profile {
    pub id: String,
    pub host: String,
    pub api_base_url: String,
    pub web_base_url: String,
    pub repos: Vec<RepoRef>,
    pub reconcile_after_days: Option<i64>,
    pub max_in_flight_requests: usize,
    pub token_source: TokenSource,
    pub paths: ProfilePaths,
}

impl Profile {
    pub fn allows_repo(&self, repo: &str) -> bool {
        self.repos
            .iter()
            .any(|allowed_repo| allowed_repo.full_name() == repo)
    }
}

#[derive(Debug, Clone)]
pub struct RepoRef {
    pub owner: String,
    pub name: String,
}

impl RepoRef {
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

#[derive(Debug, Clone)]
pub struct RepoPolicy {
    pub path: PathBuf,
    pub repo: RepoRef,
    pub defaults: RepoPolicyDefaults,
}

#[derive(Debug, Clone)]
pub struct RepoPolicyDefaults {
    pub state: Option<String>,
    pub labels: Vec<String>,
    pub source_types: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum TokenSource {
    GithubCli,
    Env { env: String },
    CredentialStore { service: String, account: String },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    schema_version: String,
    profiles: BTreeMap<String, RawProfile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProfile {
    host: String,
    api_base_url: String,
    web_base_url: String,
    repos: Vec<String>,
    #[serde(default)]
    reconcile_after_days: Option<i64>,
    #[serde(default)]
    max_in_flight_requests: Option<usize>,
    token_source: TokenSource,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RepoPolicyFile {
    schema_version: String,
    repo: RawRepoPolicyRepo,
    #[serde(default)]
    defaults: RawRepoPolicyDefaults,
    #[serde(default)]
    query: RawRepoPolicyQuery,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRepoPolicyRepo {
    github: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRepoPolicyDefaults {
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    source_types: Vec<String>,
    #[serde(default)]
    labels: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRepoPolicyQuery {
    #[serde(default)]
    limit: Option<usize>,
}

pub fn load_profile(profile_id: &str) -> Result<Profile, QghError> {
    validate_profile_id(profile_id)?;
    let config = load_config_file()?;
    let Some(raw) = config.profiles.get(profile_id) else {
        return Err(QghError::config(format!(
            "Profile `{profile_id}` is not defined."
        )));
    };
    profile_from_raw(profile_id, raw)
}

pub fn single_matching_profile_id(repo_scope: Option<&str>) -> Result<String, QghError> {
    let Some(repo_scope) = repo_scope else {
        return Err(QghError::no_matching_profile(None));
    };
    let config = load_config_file()?;
    let mut matches = Vec::new();
    for (profile_id, raw) in &config.profiles {
        validate_profile_id(profile_id)?;
        if raw
            .repos
            .iter()
            .map(|repo| parse_repo(repo))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|message| QghError::config(format!("Profile `{profile_id}` {message}")))?
            .iter()
            .any(|repo| repo.full_name() == repo_scope)
        {
            matches.push(profile_id.clone());
        }
    }
    match matches.len() {
        0 => Err(QghError::no_matching_profile(Some(repo_scope))),
        1 => Ok(matches.remove(0)),
        _ => Err(QghError::ambiguous_profile(repo_scope, matches)),
    }
}

fn load_config_file() -> Result<ConfigFile, QghError> {
    let config_file = config_file_path()?;
    let text = fs::read_to_string(&config_file).map_err(|error| {
        QghError::config(format!(
            "Failed to read config at {}: {error}",
            config_file.display()
        ))
    })?;
    let config: ConfigFile = toml::from_str(&text)
        .map_err(|error| QghError::config(format!("Invalid config TOML: {error}")))?;
    if config.schema_version != "qgh.config.v1" {
        return Err(QghError::config("Unsupported config schema_version."));
    }
    Ok(config)
}

fn profile_from_raw(profile_id: &str, raw: &RawProfile) -> Result<Profile, QghError> {
    let paths = ProfilePaths::resolve(profile_id)?;
    if raw.repos.is_empty() {
        return Err(QghError::config("Profile repos must not be empty."));
    }
    if raw.reconcile_after_days.is_some_and(|days| days < 0) {
        return Err(QghError::config(
            "Profile reconcile_after_days must not be negative.",
        ));
    }
    let max_in_flight_requests = raw.max_in_flight_requests.unwrap_or(4);
    if !(1..=16).contains(&max_in_flight_requests) {
        return Err(QghError::config(
            "Profile max_in_flight_requests must be between 1 and 16.",
        ));
    }
    let repos = raw
        .repos
        .iter()
        .map(|repo| {
            parse_repo(repo).map_err(|message| QghError::config(format!("Repo `{repo}` {message}")))
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(Profile {
        id: profile_id.to_string(),
        host: raw.host.clone(),
        api_base_url: raw.api_base_url.trim_end_matches('/').to_string(),
        web_base_url: raw.web_base_url.trim_end_matches('/').to_string(),
        repos,
        reconcile_after_days: raw.reconcile_after_days,
        max_in_flight_requests,
        token_source: raw.token_source.clone(),
        paths,
    })
}

pub fn discover_repo_policy() -> Result<Option<RepoPolicy>, QghError> {
    let Some(root) = current_git_worktree_root() else {
        return Ok(None);
    };
    let path = root.join(".qgh.toml");
    if !path.exists() {
        return Ok(None);
    }
    load_repo_policy_at(&path).map(Some)
}

fn current_git_worktree_root() -> Option<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if root.is_empty() {
        None
    } else {
        Some(PathBuf::from(root))
    }
}

fn load_repo_policy_at(path: &Path) -> Result<RepoPolicy, QghError> {
    let text = fs::read_to_string(path).map_err(|error| {
        QghError::invalid_repo_policy(format!(
            "Failed to read repo policy at {}: {error}",
            path.display()
        ))
    })?;
    let policy: RepoPolicyFile = toml::from_str(&text).map_err(|error| {
        QghError::invalid_repo_policy(format!("Invalid repo policy TOML: {error}"))
    })?;
    if policy.schema_version != "qgh.repo.v1" {
        return Err(QghError::invalid_repo_policy(
            "Unsupported repo policy schema_version.",
        ));
    }
    reject_local_path_like("repo.github", &policy.repo.github)?;
    let repo = parse_repo(&policy.repo.github).map_err(QghError::invalid_repo_policy)?;
    let defaults = parse_repo_policy_defaults(policy.defaults)?;
    parse_repo_policy_query(policy.query)?;
    Ok(RepoPolicy {
        path: path.to_path_buf(),
        repo,
        defaults,
    })
}

fn parse_repo_policy_defaults(raw: RawRepoPolicyDefaults) -> Result<RepoPolicyDefaults, QghError> {
    if let Some(scope) = &raw.scope {
        reject_local_path_like("defaults.scope", scope)?;
        if scope != "repo" {
            return Err(QghError::invalid_repo_policy(
                "Repo policy defaults.scope must be `repo`.",
            ));
        }
    }

    let state = match raw.state {
        Some(state) => {
            reject_local_path_like("defaults.state", &state)?;
            match state.as_str() {
                "all" => None,
                "open" | "closed" => Some(state),
                _ => {
                    return Err(QghError::invalid_repo_policy(
                        "Repo policy defaults.state must be `all`, `open`, or `closed`.",
                    ));
                }
            }
        }
        None => None,
    };

    let source_types = if raw.source_types.is_empty() {
        vec!["issue".to_string(), "issue_comment".to_string()]
    } else {
        raw.source_types
    };
    for source_type in &source_types {
        reject_local_path_like("defaults.source_types", source_type)?;
        if !matches!(source_type.as_str(), "issue" | "issue_comment") {
            return Err(QghError::invalid_repo_policy(
                "Repo policy defaults.source_types may contain only `issue` or `issue_comment`.",
            ));
        }
    }
    for label in &raw.labels {
        reject_local_path_like("defaults.labels", label)?;
    }

    Ok(RepoPolicyDefaults {
        state,
        labels: raw.labels,
        source_types,
    })
}

fn parse_repo_policy_query(raw: RawRepoPolicyQuery) -> Result<(), QghError> {
    if raw.limit.is_some_and(|limit| limit == 0) {
        return Err(QghError::invalid_repo_policy(
            "Repo policy query.limit must be greater than zero.",
        ));
    }
    Ok(())
}

fn reject_local_path_like(field: &str, value: &str) -> Result<(), QghError> {
    if value.starts_with('/') || value.starts_with('~') || looks_like_windows_absolute_path(value) {
        return Err(QghError::invalid_repo_policy(format!(
            "Repo policy field `{field}` must not contain a user-local absolute path."
        )));
    }
    Ok(())
}

fn looks_like_windows_absolute_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 3
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/')
        && bytes[0].is_ascii_alphabetic()
}

pub fn resolve_token(profile: &Profile) -> Result<String, QghError> {
    match &profile.token_source {
        TokenSource::Env { env } => std::env::var(env).map_err(|_| {
            QghError::auth(format!(
                "Configured token environment variable `{env}` is not set."
            ))
        }),
        TokenSource::GithubCli => {
            let output = Command::new("gh")
                .args(["auth", "token"])
                .output()
                .map_err(|error| {
                    QghError::auth(format!("Failed to run `gh auth token`: {error}"))
                })?;
            if !output.status.success() {
                return Err(QghError::auth("`gh auth token` did not return a token."));
            }
            let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if token.is_empty() {
                return Err(QghError::auth("`gh auth token` returned an empty token."));
            }
            Ok(token)
        }
        TokenSource::CredentialStore { service, account } => Err(QghError::auth(format!(
            "credential_store token resolution is not implemented in this tracer for service `{service}` and account `{account}`."
        ))),
    }
}

fn validate_profile_id(profile_id: &str) -> Result<(), QghError> {
    let mut chars = profile_id.chars();
    let Some(first) = chars.next() else {
        return Err(QghError::config("Profile id must not be empty."));
    };
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return Err(QghError::config("Invalid profile id."));
    }
    if profile_id.len() > 64
        || !chars
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
    {
        return Err(QghError::config("Invalid profile id."));
    }
    Ok(())
}

fn parse_repo(value: &str) -> Result<RepoRef, String> {
    let Some((owner, name)) = value.split_once('/') else {
        return Err("must use owner/repo format.".to_string());
    };
    if owner.is_empty()
        || name.is_empty()
        || owner.contains('/')
        || name.contains('/')
        || value.contains('*')
    {
        return Err("must be an explicit owner/repo allowlist entry.".to_string());
    }
    Ok(RepoRef {
        owner: owner.to_string(),
        name: name.to_string(),
    })
}
