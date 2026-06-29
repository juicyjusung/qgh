use crate::error::QghError;
use crate::paths::{config_file_path, ensure_private_dir, set_private_file, ProfilePaths};
use serde::{Deserialize, Serialize};
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
    pub query: RepoPolicyQuery,
}

#[derive(Debug, Clone)]
pub struct RepoPolicyDefaults {
    pub state: Option<String>,
    pub labels: Vec<String>,
    pub source_types: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RepoPolicyQuery {
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum TokenSource {
    GithubCli,
    Env { env: String },
    CredentialStore { service: String, account: String },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    schema_version: String,
    profiles: BTreeMap<String, RawProfile>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
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

pub struct ProfileBootstrapInput {
    pub profile_id: String,
    pub host: String,
    pub api_base_url: String,
    pub web_base_url: String,
    pub repo: String,
    pub token_source: TokenSource,
}

pub struct ProfileBootstrapOutcome {
    pub config_path: PathBuf,
    pub profile_action: &'static str,
    pub repo_allowlist_action: &'static str,
    pub token_source_kind: &'static str,
}

pub fn bootstrap_profile_repo(
    input: ProfileBootstrapInput,
) -> Result<ProfileBootstrapOutcome, QghError> {
    validate_profile_id(&input.profile_id)?;
    parse_repo(&input.repo).map_err(|message| {
        QghError::validation(
            "validation.invalid_repo",
            format!("Repo `{}` {message}", input.repo),
        )
    })?;
    validate_remote_host(&input.host)?;
    validate_base_url("api_base_url", &input.api_base_url)?;
    validate_base_url("web_base_url", &input.web_base_url)?;
    validate_token_source(&input.token_source)?;

    let config_path = config_file_path()?;
    let mut config = load_config_file_optional()?.unwrap_or_else(|| ConfigFile {
        schema_version: "qgh.config.v1".to_string(),
        profiles: BTreeMap::new(),
    });
    let profile_action;
    let repo_allowlist_action;
    match config.profiles.get_mut(&input.profile_id) {
        Some(profile) => {
            if profile.host != input.host
                || profile.api_base_url.trim_end_matches('/') != input.api_base_url
                || profile.web_base_url.trim_end_matches('/') != input.web_base_url
            {
                return Err(QghError::config(format!(
                    "Profile `{}` already exists with different host or base URLs.",
                    input.profile_id
                )));
            }
            profile_action = "updated";
            if profile.repos.iter().any(|repo| repo == &input.repo) {
                repo_allowlist_action = "already_present";
            } else {
                profile.repos.push(input.repo.clone());
                repo_allowlist_action = "added";
            }
        }
        None => {
            config.profiles.insert(
                input.profile_id.clone(),
                RawProfile {
                    host: input.host,
                    api_base_url: input.api_base_url.trim_end_matches('/').to_string(),
                    web_base_url: input.web_base_url.trim_end_matches('/').to_string(),
                    repos: vec![input.repo.clone()],
                    reconcile_after_days: None,
                    max_in_flight_requests: None,
                    token_source: input.token_source.clone(),
                },
            );
            profile_action = "created";
            repo_allowlist_action = "added";
        }
    }

    if config.schema_version != "qgh.config.v1" {
        return Err(QghError::config("Unsupported config schema_version."));
    }
    let Some(parent) = config_path.parent() else {
        return Err(QghError::config("Config path has no parent directory."));
    };
    ensure_private_dir(parent)?;
    let text = toml::to_string_pretty(&config)
        .map_err(|error| QghError::config(format!("Failed to serialize config: {error}")))?;
    fs::write(&config_path, text)?;
    set_private_file(&config_path)?;
    load_profile(&input.profile_id)?;

    Ok(ProfileBootstrapOutcome {
        config_path,
        profile_action,
        repo_allowlist_action,
        token_source_kind: token_source_kind(&input.token_source),
    })
}

pub struct GitRemote {
    pub host: String,
    pub api_base_url: String,
    pub web_base_url: String,
    pub repo: String,
}

pub fn single_matching_profile_id(
    repo_scope: Option<&str>,
    host_scope: Option<&str>,
) -> Result<String, QghError> {
    let Some(repo_scope) = repo_scope else {
        return Err(QghError::no_matching_profile(None));
    };
    let config = load_config_file()?;
    let mut matches = Vec::new();
    for (profile_id, raw) in &config.profiles {
        validate_profile_id(profile_id)?;
        if host_scope.is_some_and(|host| raw.host != host) {
            continue;
        }
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

pub(crate) fn origin_remote_from_current_worktree() -> Result<Option<GitRemote>, QghError> {
    let Some(root) = current_git_worktree_root() else {
        return Ok(None);
    };
    if root.join(".qgh.toml").exists() {
        return Ok(None);
    }
    match origin_remote(&root) {
        Ok(remote) => parse_github_remote(&remote).map(Some),
        Err(error) if error.code == "config.git_remote_unavailable" => Ok(None),
        Err(error) => Err(error),
    }
}

pub(crate) fn git_remote_defaults_for_root(root: &Path) -> Result<GitRemote, QghError> {
    let remote = origin_remote(root)?;
    parse_github_remote(&remote)
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

fn load_config_file_optional() -> Result<Option<ConfigFile>, QghError> {
    let config_file = config_file_path()?;
    if !config_file.exists() {
        return Ok(None);
    }
    load_config_file().map(Some)
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

pub(crate) fn current_git_worktree_root() -> Option<PathBuf> {
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

fn origin_remote(root: &Path) -> Result<String, QghError> {
    let output = Command::new("git")
        .args(["config", "--get", "remote.origin.url"])
        .current_dir(root)
        .output()
        .map_err(|error| {
            QghError::validation(
                "config.git_remote_unavailable",
                format!("Failed to read git origin remote: {error}"),
            )
            .with_hint("Pass --repo owner/repo or configure a GitHub origin remote.")
        })?;
    if !output.status.success() {
        return Err(QghError::validation(
            "config.git_remote_unavailable",
            "Git origin remote is not configured.",
        )
        .with_hint("Pass --repo owner/repo or configure a GitHub origin remote."));
    }
    let remote = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if remote.is_empty() {
        return Err(QghError::validation(
            "config.git_remote_unavailable",
            "Git origin remote is empty.",
        )
        .with_hint("Pass --repo owner/repo or configure a GitHub origin remote."));
    }
    Ok(remote)
}

fn parse_github_remote(remote: &str) -> Result<GitRemote, QghError> {
    let remote = remote.trim().trim_end_matches('/');
    let (host, repo) = if let Some(rest) = remote.strip_prefix("https://") {
        let Some((host, path)) = rest.split_once('/') else {
            return Err(unsupported_git_remote(remote));
        };
        (host, path)
    } else if let Some(rest) = remote.strip_prefix("git@") {
        let Some((host, path)) = rest.split_once(':') else {
            return Err(unsupported_git_remote(remote));
        };
        (host, path)
    } else if let Some(rest) = remote.strip_prefix("ssh://git@") {
        let Some((host, path)) = rest.split_once('/') else {
            return Err(unsupported_git_remote(remote));
        };
        (host, path)
    } else {
        return Err(unsupported_git_remote(remote));
    };
    let repo = repo.trim_end_matches(".git");
    let parsed = parse_repo(repo).map_err(|_| unsupported_git_remote(remote))?;
    let repo = parsed.full_name();
    let web_base_url = format!("https://{host}");
    let api_base_url = if host == "github.com" {
        "https://api.github.com".to_string()
    } else {
        format!("https://{host}/api/v3")
    };
    Ok(GitRemote {
        host: host.to_string(),
        api_base_url,
        web_base_url,
        repo,
    })
}

fn unsupported_git_remote(remote: &str) -> QghError {
    QghError::validation(
        "config.unsupported_git_remote",
        "Git origin remote is not a supported GitHub repository remote.",
    )
    .with_details(serde_json::json!({ "remote": remote }))
    .with_hint("Pass --repo owner/repo or use a GitHub origin remote.")
}

pub(crate) fn load_repo_policy_at(path: &Path) -> Result<RepoPolicy, QghError> {
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
    let query = parse_repo_policy_query(policy.query)?;
    Ok(RepoPolicy {
        path: path.to_path_buf(),
        repo,
        defaults,
        query,
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

fn parse_repo_policy_query(raw: RawRepoPolicyQuery) -> Result<RepoPolicyQuery, QghError> {
    if raw.limit.is_some_and(|limit| limit == 0) {
        return Err(QghError::invalid_repo_policy(
            "Repo policy query.limit must be greater than zero.",
        ));
    }
    Ok(RepoPolicyQuery { limit: raw.limit })
}

fn reject_local_path_like(field: &str, value: &str) -> Result<(), QghError> {
    if value.starts_with('/') || value.starts_with('~') || looks_like_windows_absolute_path(value) {
        return Err(QghError::invalid_repo_policy(format!(
            "Repo policy field `{field}` must not contain a user-local absolute path."
        )));
    }
    Ok(())
}

fn validate_remote_host(host: &str) -> Result<(), QghError> {
    if host.is_empty() || host.contains('/') || host.contains('*') {
        return Err(QghError::validation(
            "validation.invalid_host",
            "Host must be a plain GitHub host name.",
        ));
    }
    Ok(())
}

fn validate_base_url(field: &str, value: &str) -> Result<(), QghError> {
    if !(value.starts_with("https://") || value.starts_with("http://")) {
        return Err(QghError::validation(
            "validation.invalid_url",
            format!("{field} must be an absolute HTTP(S) URL."),
        ));
    }
    Ok(())
}

fn validate_token_source(token_source: &TokenSource) -> Result<(), QghError> {
    match token_source {
        TokenSource::GithubCli => Ok(()),
        TokenSource::Env { env } => {
            if env.is_empty() || env.contains('=') || env.contains('/') {
                return Err(QghError::validation(
                    "validation.invalid_token_source",
                    "Token env var name must be a non-empty environment variable name.",
                ));
            }
            Ok(())
        }
        TokenSource::CredentialStore { .. } => Ok(()),
    }
}

fn token_source_kind(token_source: &TokenSource) -> &'static str {
    match token_source {
        TokenSource::GithubCli => "github_cli",
        TokenSource::Env { .. } => "env",
        TokenSource::CredentialStore { .. } => "credential_store",
    }
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

pub(crate) fn parse_repo(value: &str) -> Result<RepoRef, String> {
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
