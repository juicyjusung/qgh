use crate::config::{
    discover_repo_policy, load_profile, origin_remote_from_current_worktree,
    single_matching_profile_id,
};
use crate::error::QghError;
use serde_json::{json, Value};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub(crate) struct ResolvedCommandContext {
    pub(crate) profile_id: String,
    pub(crate) profile_source: &'static str,
    pub(crate) repo_scope: Option<ResolvedRepoScope>,
    pub(crate) allowlist_match_count: Option<usize>,
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedRepoScope {
    pub(crate) repo: String,
    pub(crate) source: &'static str,
    pub(crate) host: Option<String>,
    pub(crate) repo_policy_path: Option<PathBuf>,
}

pub(crate) fn resolve_context(
    profile_arg: Option<&str>,
    repo_scope: Option<ResolvedRepoScope>,
) -> Result<ResolvedCommandContext, QghError> {
    if let Some(profile_id) = profile_arg {
        return resolve_explicit_context(profile_id, "cli", repo_scope);
    }
    if let Ok(profile_id) = std::env::var("QGH_PROFILE") {
        return resolve_explicit_context(&profile_id, "env", repo_scope);
    }
    let profile_id = single_matching_profile_id(
        repo_scope.as_ref().map(|scope| scope.repo.as_str()),
        repo_scope.as_ref().and_then(|scope| scope.host.as_deref()),
    )?;
    Ok(ResolvedCommandContext {
        profile_id,
        profile_source: "single_match",
        repo_scope,
        allowlist_match_count: Some(1),
    })
}

pub(crate) fn resolve_explicit_context(
    profile_id: &str,
    profile_source: &'static str,
    repo_scope: Option<ResolvedRepoScope>,
) -> Result<ResolvedCommandContext, QghError> {
    if let Some(scope) = &repo_scope {
        validate_profile_allows_scope(profile_id, scope)?;
    }
    Ok(ResolvedCommandContext {
        profile_id: profile_id.to_string(),
        profile_source,
        repo_scope,
        allowlist_match_count: None,
    })
}

fn validate_profile_allows_scope(
    profile_id: &str,
    repo_scope: &ResolvedRepoScope,
) -> Result<(), QghError> {
    let profile = load_profile(profile_id)?;
    if profile.allows_repo(&repo_scope.repo)
        && repo_scope
            .host
            .as_deref()
            .is_none_or(|host| profile.host == host)
    {
        return Ok(());
    }
    let details = json!({
        "profile_id": profile.id,
        "repo": repo_scope.repo,
        "host": repo_scope.host.as_ref(),
        "repo_policy_path": repo_scope
            .repo_policy_path
            .as_ref()
            .map(|path| path.to_string_lossy().to_string())
    });
    if repo_scope.source == "repo_policy" {
        return Err(QghError::invalid_repo_policy(format!(
            "Repo policy repo `{}` is outside profile `{}` allowlist.",
            repo_scope.repo, profile.id
        ))
        .with_details(details)
        .with_hint("Update `.qgh.toml` or the profile repo allowlist."));
    }
    Err(QghError::validation(
        "validation.invalid_repo",
        format!(
            "Repo `{}` is outside profile `{}` allowlist.",
            repo_scope.repo, profile.id
        ),
    )
    .with_details(details)
    .with_hint("Use a repo from the profile allowlist or update the profile config."))
}

pub(crate) fn repo_scope_from_policy() -> Result<Option<ResolvedRepoScope>, QghError> {
    Ok(discover_repo_policy()?.map(|policy| ResolvedRepoScope {
        repo: policy.repo.full_name(),
        source: "repo_policy",
        host: None,
        repo_policy_path: Some(policy.path),
    }))
}

pub(crate) fn repo_scope_from_worktree() -> Result<Option<ResolvedRepoScope>, QghError> {
    if let Some(scope) = repo_scope_from_policy()? {
        return Ok(Some(scope));
    }
    Ok(
        origin_remote_from_current_worktree()?.map(|remote| ResolvedRepoScope {
            repo: remote.repo,
            source: "git_remote",
            host: Some(remote.host),
            repo_policy_path: None,
        }),
    )
}

pub(crate) fn repo_scope_from_cli_arg(repo: &str) -> Result<ResolvedRepoScope, QghError> {
    repo_scope_from_explicit_arg(repo, "cli")
}

pub(crate) fn repo_scope_from_command_arg(repo: &str) -> Result<ResolvedRepoScope, QghError> {
    repo_scope_from_explicit_arg(repo, "command")
}

fn repo_scope_from_explicit_arg(
    repo: &str,
    source: &'static str,
) -> Result<ResolvedRepoScope, QghError> {
    validate_repo_scope(repo)?;
    Ok(ResolvedRepoScope {
        repo: repo.to_string(),
        source,
        host: None,
        repo_policy_path: None,
    })
}

fn validate_repo_scope(repo: &str) -> Result<(), QghError> {
    let Some((owner, name)) = repo.split_once('/') else {
        return Err(QghError::validation(
            "validation.invalid_repo",
            "Repo filter must use owner/repo format.",
        ));
    };
    if owner.is_empty() || name.is_empty() || name.contains('/') || repo.contains('*') {
        return Err(QghError::validation(
            "validation.invalid_repo",
            "Repo filter must use explicit owner/repo format.",
        ));
    }
    Ok(())
}

impl ResolvedCommandContext {
    pub(crate) fn meta_json(&self) -> Value {
        json!({
            "profile_id": self.profile_id,
            "profile_source": self.profile_source,
            "repo": self.repo_scope.as_ref().map(|scope| scope.repo.clone()),
            "repo_source": self.repo_scope.as_ref().map(|scope| scope.source),
            "repo_policy_path": self.repo_scope
                .as_ref()
                .and_then(|scope| scope.repo_policy_path.as_ref())
                .map(|path| path.to_string_lossy().to_string())
        })
    }

    pub(crate) fn resolution_json(&self) -> Value {
        json!({
            "profile_id": self.profile_id,
            "profile_source": self.profile_source,
            "effective_repo_scope": self.repo_scope.as_ref().map(|scope| scope.repo.clone()),
            "repo_source": self.repo_scope.as_ref().map(|scope| scope.source),
            "repo_policy_path": self.repo_scope
                .as_ref()
                .and_then(|scope| scope.repo_policy_path.as_ref())
                .map(|path| path.to_string_lossy().to_string()),
            "allowlist_match_count": self.allowlist_match_count
        })
    }
}
