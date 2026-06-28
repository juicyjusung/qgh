use crate::error::QghError;
use crate::paths::ProfilePaths;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::process::Command;

#[derive(Debug, Clone)]
pub struct Profile {
    pub id: String,
    pub host: String,
    pub api_base_url: String,
    pub web_base_url: String,
    pub repos: Vec<RepoRef>,
    pub reconcile_after_days: Option<i64>,
    pub token_source: TokenSource,
    pub paths: ProfilePaths,
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
    token_source: TokenSource,
}

pub fn load_profile(profile_id: &str) -> Result<Profile, QghError> {
    validate_profile_id(profile_id)?;
    let paths = ProfilePaths::resolve(profile_id)?;
    let text = fs::read_to_string(&paths.config_file).map_err(|error| {
        QghError::config(format!(
            "Failed to read config at {}: {error}",
            paths.config_file.display()
        ))
    })?;
    let config: ConfigFile = toml::from_str(&text)
        .map_err(|error| QghError::config(format!("Invalid config TOML: {error}")))?;
    if config.schema_version != "qgh.config.v1" {
        return Err(QghError::config("Unsupported config schema_version."));
    }
    let Some(raw) = config.profiles.get(profile_id) else {
        return Err(QghError::config(format!(
            "Profile `{profile_id}` is not defined."
        )));
    };
    if raw.repos.is_empty() {
        return Err(QghError::config("Profile repos must not be empty."));
    }
    if raw.reconcile_after_days.is_some_and(|days| days < 0) {
        return Err(QghError::config(
            "Profile reconcile_after_days must not be negative.",
        ));
    }
    let repos = raw
        .repos
        .iter()
        .map(|repo| parse_repo(repo))
        .collect::<Result<Vec<_>, _>>()?;

    Ok(Profile {
        id: profile_id.to_string(),
        host: raw.host.clone(),
        api_base_url: raw.api_base_url.trim_end_matches('/').to_string(),
        web_base_url: raw.web_base_url.trim_end_matches('/').to_string(),
        repos,
        reconcile_after_days: raw.reconcile_after_days,
        token_source: raw.token_source.clone(),
        paths,
    })
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

fn parse_repo(value: &str) -> Result<RepoRef, QghError> {
    let Some((owner, name)) = value.split_once('/') else {
        return Err(QghError::config(format!(
            "Repo `{value}` must use owner/repo format."
        )));
    };
    if owner.is_empty()
        || name.is_empty()
        || owner.contains('/')
        || name.contains('/')
        || value.contains('*')
    {
        return Err(QghError::config(format!(
            "Repo `{value}` must be an explicit owner/repo allowlist entry."
        )));
    }
    Ok(RepoRef {
        owner: owner.to_string(),
        name: name.to_string(),
    })
}
