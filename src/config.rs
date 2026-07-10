use crate::embedding::{
    default_prepared_model_store, is_builtin_preset_id, parse_hf_model_reference,
    FastembedProviderOptions, PoolingKind, PreparedModelStore, QuantizationKind,
    DEFAULT_HF_MODEL_ID, DEFAULT_QUERY_PREFIX,
};
use crate::error::QghError;
use crate::freshness::{parse_duration_seconds, DEFAULT_QUERY_MAX_AGE_SECONDS};
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
    pub embedding: Option<EmbeddingConfig>,
    pub reconcile_after_seconds: Option<i64>,
    pub freshness: FreshnessSettings,
    pub bootstrap: BootstrapSettings,
    pub sync_max_age_seconds: Option<i64>,
    pub comments_mode: CommentsMode,
    pub comment_parent_resolution_budget: usize,
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

    pub fn freshness_settings(&self, repo_policy: Option<&RepoPolicy>) -> FreshnessSettings {
        let mut settings = self.freshness;
        if let Some(policy_freshness) = repo_policy.map(|policy| policy.query.freshness) {
            if let Some(query_max_age_seconds) = policy_freshness.query_max_age_seconds {
                settings.query_max_age_seconds = query_max_age_seconds;
            }
            if let Some(query_stale_behavior) = policy_freshness.query_stale_behavior {
                settings.query_stale_behavior = query_stale_behavior;
            }
            if let Some(active_issue_max_age_seconds) =
                policy_freshness.active_issue_max_age_seconds
            {
                settings.active_issue_max_age_seconds = Some(active_issue_max_age_seconds);
            }
        }
        settings
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
    pub freshness: RepoPolicyFreshness,
}

#[derive(Debug, Clone, Copy)]
pub struct FreshnessSettings {
    pub query_max_age_seconds: i64,
    pub query_stale_behavior: StaleBehavior,
    pub active_issue_max_age_seconds: Option<i64>,
}

/// Default recent-bootstrap lookback: 12 months. The lookback fixes the
/// bootstrap floor recorded in coverage metadata; it is not a corpus boundary.
pub const DEFAULT_BOOTSTRAP_LOOKBACK_SECONDS: i64 = 12 * 30 * 24 * 60 * 60;

#[derive(Debug, Clone, Copy)]
pub struct BootstrapSettings {
    pub lookback_seconds: i64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RepoPolicyFreshness {
    pub query_max_age_seconds: Option<i64>,
    pub query_stale_behavior: Option<StaleBehavior>,
    pub active_issue_max_age_seconds: Option<i64>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StaleBehavior {
    Warn,
    Fail,
}

/// How fresh issue comments are fetched during sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CommentsMode {
    /// One `/issues/{n}/comments` request per issue (default).
    #[default]
    PerIssue,
    /// One repo-level `/issues/comments?since` listing, cheaper for large repos.
    RepoListing,
}

/// Default remote parent-classification budget for repo-level comment listing.
pub const DEFAULT_PARENT_RESOLUTION_BUDGET: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum EmbeddingProviderKind {
    Local,
}

#[cfg_attr(not(feature = "fastembed-provider"), allow(dead_code))]
#[derive(Debug, Clone)]
pub struct EmbeddingConfig {
    pub provider: EmbeddingProviderKind,
    pub manifest_path: Option<PathBuf>,
    pub model: Option<String>,
    pub model_path: Option<PathBuf>,
    pub file: Option<String>,
    pub pooling: Option<PoolingKind>,
    pub query_prefix: Option<String>,
    pub quantization: Option<QuantizationKind>,
    pub token_source: Option<EmbeddingTokenSource>,
}

impl EmbeddingConfig {
    pub fn fastembed_options(&self) -> FastembedProviderOptions {
        let token_source_env = match &self.token_source {
            Some(EmbeddingTokenSource::Env { env }) => Some(env.clone()),
            Some(EmbeddingTokenSource::Unsupported) | None => None,
        };
        FastembedProviderOptions {
            manifest_path: self.manifest_path.clone(),
            model: self.model.clone(),
            model_path: self.model_path.clone(),
            file: self.file.clone(),
            pooling: self.pooling,
            query_prefix: self.query_prefix.clone(),
            quantization: self.quantization,
            token_source_env,
            cache_dir: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum EmbeddingTokenSource {
    Env {
        env: String,
    },
    #[serde(other)]
    Unsupported,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum TokenSource {
    GithubCli,
    Env {
        env: String,
    },
    #[serde(other)]
    Unsupported,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    schema_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    embedding: Option<RawEmbeddingConfig>,
    profiles: BTreeMap<String, RawProfile>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawEmbeddingConfig {
    provider: EmbeddingProviderKind,
    #[serde(default)]
    manifest_path: Option<PathBuf>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    model_path: Option<PathBuf>,
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    pooling: Option<PoolingKind>,
    #[serde(default)]
    query_prefix: Option<String>,
    #[serde(default)]
    quantization: Option<QuantizationKind>,
    #[serde(default)]
    token_source: Option<EmbeddingTokenSource>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawProfile {
    host: String,
    api_base_url: String,
    web_base_url: String,
    repos: Vec<String>,
    #[serde(default)]
    query_max_age: Option<String>,
    #[serde(default)]
    query_stale_behavior: Option<StaleBehavior>,
    #[serde(default)]
    active_issue_max_age: Option<String>,
    #[serde(default)]
    reconcile_after: Option<String>,
    #[serde(default)]
    reconcile_after_days: Option<i64>,
    #[serde(default)]
    max_in_flight_requests: Option<usize>,
    #[serde(default)]
    bootstrap_lookback: Option<String>,
    #[serde(default)]
    sync_max_age: Option<String>,
    #[serde(default)]
    comments_mode: Option<CommentsMode>,
    #[serde(default)]
    comment_parent_resolution_budget: Option<usize>,
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
    #[serde(default)]
    max_age: Option<String>,
    #[serde(default)]
    stale_behavior: Option<StaleBehavior>,
    #[serde(default)]
    active_issue_max_age: Option<String>,
}

pub fn load_profile(profile_id: &str) -> Result<Profile, QghError> {
    validate_profile_id(profile_id)?;
    let config = load_config_file()?;
    let Some(raw) = config.profiles.get(profile_id) else {
        return Err(QghError::config(format!(
            "Profile `{profile_id}` is not defined."
        )));
    };
    profile_from_raw(
        profile_id,
        raw,
        config.embedding.as_ref().map(embedding_config_from_raw),
    )
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
    pub duplicate_profile_ids: Vec<String>,
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
        embedding: None,
        profiles: BTreeMap::new(),
    });
    let duplicate_profile_ids =
        profiles_allowlisting_repo(&config.profiles, &input.repo, Some(&input.profile_id));
    let profile_action;
    let repo_allowlist_action;
    let effective_token_source_kind;
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
            effective_token_source_kind = token_source_kind(&profile.token_source);
        }
        None => {
            effective_token_source_kind = token_source_kind(&input.token_source);
            config.profiles.insert(
                input.profile_id.clone(),
                RawProfile {
                    host: input.host,
                    api_base_url: input.api_base_url.trim_end_matches('/').to_string(),
                    web_base_url: input.web_base_url.trim_end_matches('/').to_string(),
                    repos: vec![input.repo.clone()],
                    query_max_age: None,
                    query_stale_behavior: None,
                    active_issue_max_age: None,
                    reconcile_after: None,
                    reconcile_after_days: None,
                    max_in_flight_requests: None,
                    bootstrap_lookback: None,
                    sync_max_age: None,
                    comments_mode: None,
                    comment_parent_resolution_budget: None,
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
        token_source_kind: effective_token_source_kind,
        duplicate_profile_ids,
    })
}

fn profiles_allowlisting_repo(
    profiles: &BTreeMap<String, RawProfile>,
    repo: &str,
    exclude_profile_id: Option<&str>,
) -> Vec<String> {
    profiles
        .iter()
        .filter(|(profile_id, raw)| {
            exclude_profile_id != Some(profile_id.as_str())
                && raw.repos.iter().any(|allowed| allowed == repo)
        })
        .map(|(profile_id, _)| profile_id.clone())
        .collect()
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

pub fn suggest_init_profile_id(repo: &str, host: &str) -> Result<String, QghError> {
    let profiles = load_config_file_optional()?
        .map(|config| config.profiles)
        .unwrap_or_default();
    Ok(suggest_profile_id_from(&profiles, repo, host))
}

fn suggest_profile_id_from(
    profiles: &BTreeMap<String, RawProfile>,
    repo: &str,
    host: &str,
) -> String {
    if let Some((profile_id, _)) = profiles
        .iter()
        .find(|(_, raw)| raw.host == host && raw.repos.iter().any(|allowed| allowed == repo))
    {
        return profile_id.clone();
    }
    if let Some((profile_id, _)) = profiles.iter().find(|(_, raw)| raw.host == host) {
        return profile_id.clone();
    }
    let candidate = derive_profile_id_from_host(host);
    match profiles.get(&candidate) {
        Some(existing) if existing.host != host => sanitize_profile_id(host),
        _ => candidate,
    }
}

fn derive_profile_id_from_host(host: &str) -> String {
    if host.eq_ignore_ascii_case("github.com") {
        return "github".to_string();
    }
    sanitize_profile_id(host.split('.').next().unwrap_or(host))
}

fn sanitize_profile_id(value: &str) -> String {
    let mut id: String = value
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .skip_while(|c| !c.is_ascii_lowercase() && !c.is_ascii_digit())
        .collect();
    id.truncate(64);
    if id.is_empty() {
        "github".to_string()
    } else {
        id
    }
}

/// Best-effort origin remote for profile disambiguation: unlike
/// `origin_remote_from_current_worktree`, it runs even when `.qgh.toml`
/// exists and swallows parse errors instead of failing resolution.
pub(crate) fn origin_remote_best_effort() -> Option<GitRemote> {
    let root = current_git_worktree_root()?;
    let remote = origin_remote(&root).ok()?;
    parse_github_remote(&remote).ok()
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
    let config: ConfigFile = toml::from_str(&text).map_err(|error| {
        QghError::config(redacted_toml_error(
            "Invalid config TOML",
            &text,
            error.span(),
            redacted_toml_reason(error.message()),
        ))
    })?;
    if config.schema_version != "qgh.config.v1" {
        return Err(QghError::config("Unsupported config schema_version."));
    }
    if let Some(embedding) = &config.embedding {
        parse_embedding_config(embedding)?;
    }
    validate_config_token_sources(&config)?;
    Ok(config)
}

fn load_config_file_optional() -> Result<Option<ConfigFile>, QghError> {
    let config_file = config_file_path()?;
    if !config_file.exists() {
        return Ok(None);
    }
    load_config_file().map(Some)
}

fn profile_from_raw(
    profile_id: &str,
    raw: &RawProfile,
    embedding: Option<EmbeddingConfig>,
) -> Result<Profile, QghError> {
    let paths = ProfilePaths::resolve(profile_id)?;
    if raw.repos.is_empty() {
        return Err(QghError::config("Profile repos must not be empty."));
    }
    let freshness = parse_profile_freshness(raw)?;
    let bootstrap = parse_profile_bootstrap(raw)?;
    let sync_max_age_seconds = raw
        .sync_max_age
        .as_deref()
        .map(|value| parse_duration_seconds("sync_max_age", value))
        .transpose()?;
    let reconcile_after_seconds = parse_reconcile_after(raw)?;
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
        embedding,
        reconcile_after_seconds,
        freshness,
        bootstrap,
        sync_max_age_seconds,
        comments_mode: raw.comments_mode.unwrap_or_default(),
        comment_parent_resolution_budget: raw
            .comment_parent_resolution_budget
            .unwrap_or(DEFAULT_PARENT_RESOLUTION_BUDGET),
        max_in_flight_requests,
        token_source: raw.token_source.clone(),
        paths,
    })
}

fn embedding_config_from_raw(raw: &RawEmbeddingConfig) -> EmbeddingConfig {
    EmbeddingConfig {
        provider: raw.provider,
        manifest_path: raw.manifest_path.clone(),
        model: raw.model.clone(),
        model_path: raw.model_path.clone(),
        file: raw.file.clone(),
        pooling: raw.pooling,
        query_prefix: raw.query_prefix.clone(),
        quantization: raw.quantization,
        token_source: raw.token_source.clone(),
    }
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
        (strip_url_userinfo(host), path)
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
    if host.is_empty() {
        return Err(unsupported_git_remote(remote));
    }
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

fn strip_url_userinfo(host: &str) -> &str {
    host.rsplit_once('@')
        .map(|(_, host_without_userinfo)| host_without_userinfo)
        .unwrap_or(host)
}

fn unsupported_git_remote(remote: &str) -> QghError {
    QghError::validation(
        "config.unsupported_git_remote",
        "Git origin remote is not a supported GitHub repository remote.",
    )
    .with_details(serde_json::json!({ "remote": sanitized_remote_for_error(remote) }))
    .with_hint("Pass --repo owner/repo or use a GitHub origin remote.")
}

fn sanitized_remote_for_error(remote: &str) -> String {
    let remote = remote.trim();
    if let Some((scheme, rest)) = remote.split_once("://") {
        if let Some((_, after_userinfo)) = rest.split_once('@') {
            return format!("{scheme}://<redacted>@{after_userinfo}");
        }
    }
    if let Some((_, after_userinfo)) = remote.rsplit_once('@') {
        return format!("<redacted>@{after_userinfo}");
    }
    remote.to_string()
}

pub(crate) fn load_repo_policy_at(path: &Path) -> Result<RepoPolicy, QghError> {
    let text = fs::read_to_string(path).map_err(|error| {
        QghError::invalid_repo_policy(format!(
            "Failed to read repo policy at {}: {error}",
            path.display()
        ))
    })?;
    let policy: RepoPolicyFile = toml::from_str(&text).map_err(|error| {
        QghError::invalid_repo_policy(redacted_toml_error(
            "Invalid repo policy TOML",
            &text,
            error.span(),
            redacted_toml_reason(error.message()),
        ))
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

fn redacted_toml_error(
    context: &str,
    source: &str,
    span: Option<std::ops::Range<usize>>,
    reason: &str,
) -> String {
    let Some(start) = span
        .map(|span| span.start)
        .filter(|start| *start <= source.len())
    else {
        return format!("{context}: {reason}.");
    };
    let prefix = &source[..start];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let column = prefix
        .rsplit_once('\n')
        .map(|(_, line_prefix)| line_prefix.chars().count() + 1)
        .unwrap_or_else(|| prefix.chars().count() + 1);
    format!("{context} at line {line}, column {column}: {reason}.")
}

fn redacted_toml_reason(message: &str) -> &'static str {
    for (needle, reason) in [
        ("unknown field", "unknown field"),
        ("unknown variant", "unknown variant"),
        ("invalid type", "type mismatch"),
        ("missing field", "missing required field"),
        ("duplicate field", "duplicate field"),
    ] {
        if message.contains(needle) {
            return reason;
        }
    }
    "syntax or schema violation"
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
    let freshness = RepoPolicyFreshness {
        query_max_age_seconds: raw
            .max_age
            .as_deref()
            .map(|value| parse_duration_seconds("query.max_age", value))
            .transpose()
            .map_err(|error| QghError::invalid_repo_policy(error.message))?,
        query_stale_behavior: raw.stale_behavior,
        active_issue_max_age_seconds: raw
            .active_issue_max_age
            .as_deref()
            .map(|value| parse_duration_seconds("query.active_issue_max_age", value))
            .transpose()
            .map_err(|error| QghError::invalid_repo_policy(error.message))?,
    };
    Ok(RepoPolicyQuery {
        limit: raw.limit,
        freshness,
    })
}

fn parse_profile_freshness(raw: &RawProfile) -> Result<FreshnessSettings, QghError> {
    Ok(FreshnessSettings {
        query_max_age_seconds: raw
            .query_max_age
            .as_deref()
            .map(|value| parse_duration_seconds("query_max_age", value))
            .transpose()?
            .unwrap_or(DEFAULT_QUERY_MAX_AGE_SECONDS),
        query_stale_behavior: raw.query_stale_behavior.unwrap_or(StaleBehavior::Warn),
        active_issue_max_age_seconds: raw
            .active_issue_max_age
            .as_deref()
            .map(|value| parse_duration_seconds("active_issue_max_age", value))
            .transpose()?,
    })
}

fn parse_profile_bootstrap(raw: &RawProfile) -> Result<BootstrapSettings, QghError> {
    let lookback_seconds = raw
        .bootstrap_lookback
        .as_deref()
        .map(|value| parse_duration_seconds("bootstrap_lookback", value))
        .transpose()?
        .unwrap_or(DEFAULT_BOOTSTRAP_LOOKBACK_SECONDS);
    Ok(BootstrapSettings { lookback_seconds })
}

fn parse_reconcile_after(raw: &RawProfile) -> Result<Option<i64>, QghError> {
    match (raw.reconcile_after.as_deref(), raw.reconcile_after_days) {
        (Some(_), Some(_)) => Err(QghError::config(
            "Use only reconcile_after duration; reconcile_after_days is a deprecated alias.",
        )),
        (Some(value), None) => parse_duration_seconds("reconcile_after", value).map(Some),
        (None, Some(days)) => {
            if days < 0 {
                return Err(QghError::config(
                    "Profile reconcile_after_days must not be negative.",
                ));
            }
            days.checked_mul(24 * 60 * 60).map(Some).ok_or_else(|| {
                QghError::config("Profile reconcile_after_days duration is too large.")
            })
        }
        (None, None) => Ok(None),
    }
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
    if host.is_empty() || host.contains('/') || host.contains('*') || host.contains('@') {
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
        TokenSource::Unsupported => Err(QghError::validation(
            "validation.invalid_token_source",
            "Token source must be `github_cli` or `env`.",
        )),
    }
}

fn token_source_kind(token_source: &TokenSource) -> &'static str {
    match token_source {
        TokenSource::GithubCli => "github_cli",
        TokenSource::Env { .. } => "env",
        TokenSource::Unsupported => "unsupported",
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
                .args(github_cli_token_args(&profile.host))
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
        TokenSource::Unsupported => Err(QghError::validation(
            "validation.invalid_token_source",
            "Token source must be `github_cli` or `env`.",
        )),
    }
}

fn github_cli_token_args(host: &str) -> [&str; 4] {
    ["auth", "token", "--hostname", host]
}

fn validate_config_token_sources(config: &ConfigFile) -> Result<(), QghError> {
    for raw in config.profiles.values() {
        validate_token_source(&raw.token_source)?;
    }
    Ok(())
}

fn parse_embedding_config(raw: &RawEmbeddingConfig) -> Result<(), QghError> {
    let EmbeddingProviderKind::Local = raw.provider;
    if let Some(manifest_path) = &raw.manifest_path {
        if manifest_path.as_os_str().is_empty() {
            return Err(QghError::config(
                "Embedding manifest_path must not be empty.",
            ));
        }
        if raw.model.is_some()
            || raw.model_path.is_some()
            || raw.file.is_some()
            || raw.pooling.is_some()
            || raw.query_prefix.is_some()
            || raw.quantization.is_some()
            || raw.token_source.is_some()
        {
            return Err(QghError::config(
                "Embedding manifest_path cannot be combined with legacy model, model_path, file, pooling, query_prefix, quantization, or token_source fields.",
            ));
        }
        let options = FastembedProviderOptions {
            manifest_path: Some(manifest_path.clone()),
            model: None,
            model_path: None,
            file: None,
            pooling: None,
            query_prefix: None,
            quantization: None,
            token_source_env: None,
            cache_dir: None,
        };
        let prepared = default_prepared_model_store()
            .and_then(|store| store.inspect_prepared_alias_contract(&options));
        let source_store = PreparedModelStore::new(PathBuf::new());
        match fs::symlink_metadata(manifest_path) {
            Ok(_) => {
                let source = source_store
                    .inspect_manifest_contract(manifest_path)
                    .map_err(embedding_config_error)?;
                if let Ok(prepared) = &prepared {
                    if prepared.manifest_hash() != source.manifest_hash() {
                        return Err(QghError::validation(
                            "embedding.prepared_alias_mismatch",
                            "Prepared model alias does not match the configured manifest.",
                        ));
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && prepared.is_ok() => {}
            Err(_) => {
                source_store
                    .inspect_manifest_contract(manifest_path)
                    .map_err(embedding_config_error)?;
            }
        }
        return Ok(());
    }
    if raw.model.is_some() && raw.model_path.is_some() {
        return Err(QghError::config(
            "Embedding config must use only one of `model` or `model_path`.",
        ));
    }
    if raw.model_path.is_some() && raw.quantization.is_none() {
        return Err(QghError::config(
            "Legacy embedding model_path requires explicit quantization = \"none\" or \"static\".",
        ));
    }
    if let Some(model) = raw.model.as_deref() {
        if !is_builtin_preset_id(model) {
            validate_hf_model_reference(model)?;
        }
    } else if raw.model_path.is_none() {
        validate_hf_model_reference(&format!("hf:{DEFAULT_HF_MODEL_ID}"))?;
    }
    if let Some(file) = &raw.file {
        validate_embedding_repo_file(file)?;
    }
    if let Some(query_prefix) = &raw.query_prefix {
        if query_prefix != DEFAULT_QUERY_PREFIX {
            return Err(QghError::config(
                "Embedding query_prefix must be the lowercase `query: ` prefix.",
            ));
        }
    }
    if raw.model_path.is_some() && raw.token_source.is_some() {
        return Err(QghError::config(
            "Embedding token_source is only valid with `model = \"hf:<org>/<repo>\"`.",
        ));
    }
    if let Some(token_source) = &raw.token_source {
        validate_embedding_token_source(token_source)?;
    }
    Ok(())
}

fn embedding_config_error(error: crate::embedding::EmbeddingProviderError) -> QghError {
    QghError::validation(error.code(), error.message()).with_details(error.details().clone())
}

fn validate_hf_model_reference(model: &str) -> Result<(), QghError> {
    let Some(reference) = parse_hf_model_reference(model) else {
        return Err(QghError::config(
            "Embedding model must use `hf:<org>/<repo>[@revision]`.",
        ));
    };
    let repo = reference.model_id;
    let Some((owner, name)) = repo.split_once('/') else {
        return Err(QghError::config(
            "Embedding model must use `hf:<org>/<repo>[@revision]`.",
        ));
    };
    if owner.is_empty()
        || name.is_empty()
        || owner.contains("..")
        || name.contains("..")
        || repo.contains('\\')
        || repo.contains('*')
        || reference.revision.contains("..")
        || reference.revision.contains('\\')
        || reference.revision.contains('*')
    {
        return Err(QghError::config(
            "Embedding model must use `hf:<org>/<repo>[@revision]`.",
        ));
    }
    Ok(())
}

fn validate_embedding_repo_file(file: &str) -> Result<(), QghError> {
    if file.is_empty()
        || file.starts_with('/')
        || file.starts_with('~')
        || file.split('/').any(|part| part == ".." || part.is_empty())
        || looks_like_windows_absolute_path(file)
    {
        return Err(QghError::config(
            "Embedding file must be a relative path inside the model repository.",
        ));
    }
    Ok(())
}

fn validate_embedding_token_source(token_source: &EmbeddingTokenSource) -> Result<(), QghError> {
    match token_source {
        EmbeddingTokenSource::Env { env } => {
            if env.is_empty() || env.contains('=') || env.contains('/') {
                return Err(QghError::validation(
                    "validation.invalid_token_source",
                    "Embedding token env var name must be a non-empty environment variable name.",
                ));
            }
            Ok(())
        }
        EmbeddingTokenSource::Unsupported => Err(QghError::validation(
            "validation.invalid_token_source",
            "Embedding token_source must be `env`.",
        )),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_manifest_path_is_exclusive_with_legacy_model_configuration() {
        let base = RawEmbeddingConfig {
            provider: EmbeddingProviderKind::Local,
            manifest_path: Some(PathBuf::from("/tmp/model/manifest.json")),
            model: None,
            model_path: None,
            file: None,
            pooling: None,
            query_prefix: None,
            quantization: None,
            token_source: None,
        };
        let mut with_model = base.clone();
        with_model.model = Some("arctic-m-v2-fp32".to_string());
        assert!(parse_embedding_config(&with_model)
            .unwrap_err()
            .message
            .contains("manifest_path"));

        let mut with_manual_behavior = base;
        with_manual_behavior.pooling = Some(PoolingKind::Cls);
        assert!(parse_embedding_config(&with_manual_behavior)
            .unwrap_err()
            .message
            .contains("manifest_path"));
    }

    #[test]
    fn explicit_manifest_config_validates_contract_without_requiring_artifacts() {
        let root = std::env::temp_dir().join(format!(
            "qgh-config-manifest-contract-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let manifest_path = root.join("manifest.json");
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&explicit_manifest_fixture("model.onnx")).unwrap(),
        )
        .unwrap();
        let config = explicit_manifest_config(manifest_path.clone());

        parse_embedding_config(&config).unwrap();

        fs::write(root.join("model.onnx"), b"x").unwrap();
        parse_embedding_config(&config).unwrap();

        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&explicit_manifest_fixture("../model.onnx")).unwrap(),
        )
        .unwrap();
        let error = parse_embedding_config(&config).unwrap_err();
        assert_eq!(error.code, "embedding.artifact_path_invalid");
    }

    fn explicit_manifest_config(manifest_path: PathBuf) -> RawEmbeddingConfig {
        RawEmbeddingConfig {
            provider: EmbeddingProviderKind::Local,
            manifest_path: Some(manifest_path),
            model: None,
            model_path: None,
            file: None,
            pooling: None,
            query_prefix: None,
            quantization: None,
            token_source: None,
        }
    }

    fn explicit_manifest_fixture(model_path: &str) -> serde_json::Value {
        let artifact = |role: &str, relative_path: &str, byte_size: u64| {
            serde_json::json!({
                "role": role,
                "relative_path": relative_path,
                "sha256": "0".repeat(64),
                "byte_size": byte_size
            })
        };
        serde_json::json!({
            "schema_version": "qgh.model_manifest.v1",
            "preset_id": null,
            "provider": "fastembed",
            "model_source": {"type": "local", "declared_id": "fixture"},
            "artifacts": [
                artifact("onnx_model", model_path, 8),
                artifact("tokenizer", "tokenizer.json", 8),
                artifact("config", "config.json", 8),
                artifact("special_tokens_map", "special_tokens_map.json", 8),
                artifact("tokenizer_config", "tokenizer_config.json", 8)
            ],
            "tokenizer": "hf_tokenizer_json",
            "query_prefix": "",
            "document_prefix": "",
            "pooling": "cls",
            "normalization": "l2",
            "native_dimension": 4,
            "output_dimension": 4,
            "max_length": 32,
            "quantization": "none",
            "context_template_version": "qgh.context.v1"
        })
    }

    fn raw_profile(host: &str, repos: &[&str]) -> RawProfile {
        RawProfile {
            host: host.to_string(),
            api_base_url: format!("https://{host}/api/v3"),
            web_base_url: format!("https://{host}"),
            repos: repos.iter().map(|repo| repo.to_string()).collect(),
            query_max_age: None,
            query_stale_behavior: None,
            active_issue_max_age: None,
            reconcile_after: None,
            reconcile_after_days: None,
            max_in_flight_requests: None,
            bootstrap_lookback: None,
            sync_max_age: None,
            comments_mode: None,
            comment_parent_resolution_budget: None,
            token_source: TokenSource::GithubCli,
        }
    }

    fn profiles(entries: &[(&str, RawProfile)]) -> BTreeMap<String, RawProfile> {
        entries
            .iter()
            .map(|(id, raw)| (id.to_string(), raw.clone()))
            .collect()
    }

    #[test]
    fn github_cli_token_args_pass_profile_host() {
        assert_eq!(
            github_cli_token_args("oss.navercorp.com"),
            ["auth", "token", "--hostname", "oss.navercorp.com"]
        );
    }

    #[test]
    fn suggest_profile_id_prefers_profile_already_allowlisting_repo() {
        let profiles = profiles(&[
            ("dogfood", raw_profile("github.com", &["owner/other"])),
            ("github", raw_profile("github.com", &["owner/repo"])),
        ]);
        assert_eq!(
            suggest_profile_id_from(&profiles, "owner/repo", "github.com"),
            "github"
        );
    }

    #[test]
    fn suggest_profile_id_falls_back_to_host_match() {
        let profiles = profiles(&[
            ("work", raw_profile("oss.example.com", &["owner/other"])),
            ("github", raw_profile("github.com", &["owner/other"])),
        ]);
        assert_eq!(
            suggest_profile_id_from(&profiles, "owner/repo", "oss.example.com"),
            "work"
        );
    }

    #[test]
    fn suggest_profile_id_derives_from_host_when_no_profile_matches() {
        let profiles = profiles(&[("work", raw_profile("oss.example.com", &["owner/other"]))]);
        assert_eq!(
            suggest_profile_id_from(&profiles, "owner/repo", "github.com"),
            "github"
        );
        assert_eq!(
            suggest_profile_id_from(&BTreeMap::new(), "owner/repo", "oss.navercorp.com"),
            "oss"
        );
    }

    #[test]
    fn suggest_profile_id_avoids_host_conflicting_default() {
        let profiles = profiles(&[("oss", raw_profile("oss.other.com", &["owner/other"]))]);
        assert_eq!(
            suggest_profile_id_from(&profiles, "owner/repo", "oss.navercorp.com"),
            "oss.navercorp.com"
        );
    }

    #[test]
    fn profiles_allowlisting_repo_excludes_target_profile() {
        let profiles = profiles(&[
            ("github", raw_profile("github.com", &["owner/repo"])),
            ("test", raw_profile("github.com", &["owner/repo"])),
            ("work", raw_profile("oss.example.com", &["owner/other"])),
        ]);
        assert_eq!(
            profiles_allowlisting_repo(&profiles, "owner/repo", Some("github")),
            vec!["test".to_string()]
        );
        assert!(profiles_allowlisting_repo(&profiles, "owner/none", None).is_empty());
    }
}
