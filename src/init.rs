//! Owns first-run profile and repository-policy orchestration.
//!
//! Keep policy preflight -> profile bootstrap -> policy CAS publication in order:
//! conflicts must fail before durable profile mutation, and final publication must
//! revalidate the repository policy observed during preflight.

use crate::cli::{InitArgs, InitRepoArgs, InitTokenSourceArg};
use crate::config::{
    bootstrap_profile_repo, current_git_worktree_root, git_remote_defaults_for_root, load_profile,
    load_profile_optional, parse_repo, suggest_init_profile_id, GitRemote, Profile,
    ProfileBootstrapInput, ProfileBootstrapTarget, TokenSource,
};
use crate::error::QghError;
use crate::paths::ProfilePaths;
use crate::repo_policy_mutation::RepoPolicyMutationPlan;
use serde_json::{json, Value};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub(crate) struct InitOutcome {
    pub(crate) data: Value,
    pub(crate) warnings: Vec<Value>,
    pub(crate) meta: Value,
}

pub(crate) fn run(profile_arg: Option<&str>, args: &InitArgs) -> Result<InitOutcome, QghError> {
    if let Some(crate::cli::InitTarget::Repo(repo_args)) = &args.target {
        return init_repo_policy(profile_arg, repo_args.clone());
    }
    let Some(root) = current_git_worktree_root() else {
        return Err(QghError::validation(
            "config.no_git_worktree",
            "qgh init must be run inside a git worktree.",
        )
        .with_hint("Run qgh init from a git worktree."));
    };
    let remote = optional_git_remote_defaults(&root, args)?;
    let preset = init_preset(profile_arg, args, &root, remote.as_ref())?;
    if args.yes {
        return finish_init_preset(&root, preset);
    }
    write_init_preset_preview(&preset)?;
    if prompt_use_defaults()? {
        finish_init_preset(&root, preset)
    } else {
        init_custom_interactive(&root, remote.as_ref(), profile_arg, args)
    }
}

fn init_repo_policy(
    profile_arg: Option<&str>,
    args: InitRepoArgs,
) -> Result<InitOutcome, QghError> {
    let Some(root) = current_git_worktree_root() else {
        return Err(QghError::validation(
            "config.no_git_worktree",
            "qgh init must be run inside a git worktree.",
        )
        .with_hint("Run qgh init from a git worktree or pass --repo after initializing git."));
    };

    let (repo, repo_source) = match args.repo.as_deref() {
        Some(repo) => {
            parse_repo(repo).map_err(|_| invalid_repo_input())?;
            (repo.to_string(), "cli")
        }
        None => (repo_from_origin_remote(&root)?, "git_remote"),
    };

    let explicit_profile = explicit_profile_for_init(profile_arg);
    let (profile_validation, warnings, meta_profile_id, meta_profile_source) =
        match explicit_profile {
            Some((profile_id, profile_source)) => {
                let profile = load_profile(&profile_id)?;
                if !profile.allows_repo(&repo) {
                    return Err(QghError::validation(
                        "validation.invalid_repo",
                        format!(
                            "Repo `{repo}` is outside profile `{}` allowlist.",
                            profile.id
                        ),
                    )
                    .with_details(json!({
                        "profile_id": profile.id,
                        "repo": repo
                    }))
                    .with_hint(
                        "Use a repo from the profile allowlist or update the profile config.",
                    ));
                }
                (
                    json!({
                        "status": "validated",
                        "profile_id": profile.id,
                        "profile_source": profile_source,
                        "allowlist_match": true
                    }),
                    Vec::new(),
                    Some(profile_id),
                    Some(profile_source),
                )
            }
            None => (
                json!({
                    "status": "not_checked",
                    "profile_id": Value::Null,
                    "profile_source": Value::Null,
                    "allowlist_match": Value::Null
                }),
                vec![json!({
                    "code": "config.profile_not_checked",
                    "severity": "warn",
                    "message": "Profile allowlist was not checked because no profile was explicit."
                })],
                None,
                None,
            ),
        };

    let path = root.join(".qgh.toml");
    let policy_plan = RepoPolicyMutationPlan::prepare(&path, &repo, true, args.force, false)?;
    let policy_text = repo_policy_toml(&repo);
    let policy_action = policy_plan.commit(policy_text.as_bytes())?;
    let overwritten = policy_action == "overwritten";

    Ok(InitOutcome {
        data: json!({
            "path": path.to_string_lossy(),
            "repo": repo,
            "repo_source": repo_source,
            "overwritten": overwritten,
            "profile_validation": profile_validation
        }),
        warnings,
        meta: json!({
            "profile_id": meta_profile_id,
            "profile_source": meta_profile_source,
            "repo": repo,
            "repo_source": repo_source,
            "repo_policy_path": Value::Null
        }),
    })
}

fn init_custom_interactive(
    root: &Path,
    remote: Option<&GitRemote>,
    profile_arg: Option<&str>,
    args: &InitArgs,
) -> Result<InitOutcome, QghError> {
    let (repo, repo_source) = match args.repo.as_deref() {
        Some(repo) => {
            parse_repo(repo).map_err(|_| invalid_repo_input())?;
            (repo.to_string(), "cli")
        }
        None => (
            remote
                .map(|remote| remote.repo.clone())
                .ok_or_else(|| missing_init_value("--repo"))?,
            "git_remote",
        ),
    };
    let host_default = args
        .host
        .clone()
        .or_else(|| remote.map(|remote| remote.host.clone()))
        .ok_or_else(|| missing_init_value("--host"))?
        .to_ascii_lowercase();
    let explicit_profile = explicit_profile_for_init(profile_arg);
    let (profile_id, profile_source) = match explicit_profile {
        Some((profile_id, profile_source)) => (profile_id, profile_source),
        None => {
            let profile_default = suggest_init_profile_id(&repo, &host_default)?;
            (prompt_line("profile id", &profile_default)?, "cli")
        }
    };
    let host = prompt_line("host", &host_default)?.to_ascii_lowercase();
    let existing_profile = load_profile_optional(&profile_id)?;
    let existing_profile = existing_profile
        .as_ref()
        .filter(|profile| profile.host.eq_ignore_ascii_case(&host));
    let matching_remote = remote.filter(|remote| remote.host.eq_ignore_ascii_case(&host));
    let api_default = args
        .api_base_url
        .clone()
        .or_else(|| existing_profile.map(|profile| profile.api_base_url.clone()))
        .or_else(|| matching_remote.map(|remote| remote.api_base_url.clone()))
        .unwrap_or_else(|| default_api_base_url(&host));
    let api_base_url = prompt_line("api base url", &api_default)?;
    let api_base_url_explicit =
        args.api_base_url.is_some() || !same_init_endpoint(&api_base_url, &api_default);
    let web_default = args
        .web_base_url
        .clone()
        .or_else(|| existing_profile.map(|profile| profile.web_base_url.clone()))
        .or_else(|| matching_remote.map(|remote| remote.web_base_url.clone()))
        .unwrap_or_else(|| default_web_base_url(&host));
    let web_base_url = prompt_line("web base url", &web_default)?;
    let web_base_url_explicit =
        args.web_base_url.is_some() || !same_init_endpoint(&web_base_url, &web_default);
    let (token_source, token_source_explicit) =
        init_token_source_for_profile(args, existing_profile, true)?;
    let write_repo_policy = prompt_bool("create .qgh.toml", true)?;
    finish_profile_init(
        root,
        ProfileInitPlan {
            profile_target: ProfileBootstrapTarget::Exact(profile_id),
            profile_source,
            repo,
            repo_source,
            host,
            api_base_url,
            web_base_url,
            api_base_url_explicit,
            web_base_url_explicit,
            token_source_explicit,
            token_source,
            write_repo_policy,
            force_repo_policy: args.force,
        },
    )
}

struct InitPreset {
    profile_target: ProfileBootstrapTarget,
    profile_source: &'static str,
    repo: String,
    repo_source: &'static str,
    host: String,
    api_base_url: String,
    web_base_url: String,
    api_base_url_explicit: bool,
    web_base_url_explicit: bool,
    token_source_explicit: bool,
    token_source: TokenSource,
    write_repo_policy: bool,
    force_repo_policy: bool,
    repo_policy_path: PathBuf,
}

fn init_preset(
    profile_arg: Option<&str>,
    args: &InitArgs,
    root: &Path,
    remote: Option<&GitRemote>,
) -> Result<InitPreset, QghError> {
    let (repo, repo_source) = match args.repo.as_deref() {
        Some(repo) => {
            parse_repo(repo).map_err(|_| invalid_repo_input())?;
            (repo.to_string(), "cli")
        }
        None => (
            remote
                .map(|remote| remote.repo.clone())
                .ok_or_else(|| missing_init_value("--repo"))?,
            "git_remote",
        ),
    };
    let host = args
        .host
        .clone()
        .or_else(|| remote.map(|remote| remote.host.clone()))
        .ok_or_else(|| missing_init_value("--host"))?
        .to_ascii_lowercase();
    let (profile_target, profile_source) = match explicit_profile_for_init(profile_arg) {
        Some((profile_id, profile_source)) => {
            (ProfileBootstrapTarget::Exact(profile_id), profile_source)
        }
        None if args.yes => (ProfileBootstrapTarget::Auto, "cli"),
        None => (
            ProfileBootstrapTarget::Exact(suggest_init_profile_id(&repo, &host)?),
            "cli",
        ),
    };
    let matching_remote = remote.filter(|remote| remote.host.eq_ignore_ascii_case(&host));
    let api_base_url = args
        .api_base_url
        .clone()
        .or_else(|| matching_remote.map(|remote| remote.api_base_url.clone()))
        .unwrap_or_else(|| default_api_base_url(&host));
    let web_base_url = args
        .web_base_url
        .clone()
        .or_else(|| matching_remote.map(|remote| remote.web_base_url.clone()))
        .unwrap_or_else(|| default_web_base_url(&host));
    let existing_profile = match &profile_target {
        ProfileBootstrapTarget::Exact(profile_id) => load_profile_optional(profile_id)?,
        ProfileBootstrapTarget::Auto => None,
    };
    let (token_source, token_source_explicit) =
        init_token_source_for_profile(args, existing_profile.as_ref(), false)?;
    Ok(InitPreset {
        profile_target,
        profile_source,
        repo,
        repo_source,
        host,
        api_base_url,
        web_base_url,
        api_base_url_explicit: args.api_base_url.is_some(),
        web_base_url_explicit: args.web_base_url.is_some(),
        token_source_explicit,
        token_source,
        write_repo_policy: true,
        force_repo_policy: args.force,
        repo_policy_path: root.join(".qgh.toml"),
    })
}

fn finish_init_preset(root: &Path, preset: InitPreset) -> Result<InitOutcome, QghError> {
    finish_profile_init(
        root,
        ProfileInitPlan {
            profile_target: preset.profile_target,
            profile_source: preset.profile_source,
            repo: preset.repo,
            repo_source: preset.repo_source,
            host: preset.host,
            api_base_url: preset.api_base_url,
            web_base_url: preset.web_base_url,
            api_base_url_explicit: preset.api_base_url_explicit,
            web_base_url_explicit: preset.web_base_url_explicit,
            token_source_explicit: preset.token_source_explicit,
            token_source: preset.token_source,
            write_repo_policy: preset.write_repo_policy,
            force_repo_policy: preset.force_repo_policy,
        },
    )
}

fn write_init_preset_preview(preset: &InitPreset) -> Result<(), QghError> {
    let ProfileBootstrapTarget::Exact(profile_id) = &preset.profile_target else {
        return Err(QghError::config(
            "Automatic profile selection cannot be previewed before the config lock is acquired.",
        ));
    };
    let paths = ProfilePaths::resolve(profile_id)?;
    let mut stderr = io::stderr();
    writeln!(stderr, "Detected qgh init defaults:")?;
    writeln!(stderr, "  repo: {}", preset.repo)?;
    writeln!(stderr, "  host: {}", preset.host)?;
    writeln!(stderr, "  profile id: {profile_id}")?;
    writeln!(
        stderr,
        "  token source: {}",
        token_source_display(&preset.token_source)
    )?;
    writeln!(stderr, "  config path: {}", paths.config_file.display())?;
    writeln!(stderr, "  repo policy: create")?;
    writeln!(
        stderr,
        "  repo policy path: {}",
        preset.repo_policy_path.display()
    )?;
    writeln!(stderr, "  db path: {}", paths.db_path.display())?;
    Ok(())
}

struct ProfileInitPlan {
    profile_target: ProfileBootstrapTarget,
    profile_source: &'static str,
    repo: String,
    repo_source: &'static str,
    host: String,
    api_base_url: String,
    web_base_url: String,
    api_base_url_explicit: bool,
    web_base_url_explicit: bool,
    token_source_explicit: bool,
    token_source: TokenSource,
    write_repo_policy: bool,
    force_repo_policy: bool,
}

fn finish_profile_init(root: &Path, plan: ProfileInitPlan) -> Result<InitOutcome, QghError> {
    let policy_path = root.join(".qgh.toml");
    let repo_policy_plan = RepoPolicyMutationPlan::prepare(
        &policy_path,
        &plan.repo,
        plan.write_repo_policy,
        plan.force_repo_policy,
        true,
    )?;

    let bootstrap = bootstrap_profile_repo(ProfileBootstrapInput {
        target: plan.profile_target,
        host: plan.host,
        api_base_url: plan.api_base_url,
        web_base_url: plan.web_base_url,
        api_base_url_explicit: plan.api_base_url_explicit,
        web_base_url_explicit: plan.web_base_url_explicit,
        token_source_explicit: plan.token_source_explicit,
        repo: plan.repo.clone(),
        token_source: plan.token_source,
    })?;

    let policy_text = repo_policy_toml(&plan.repo);
    let repo_policy_action = repo_policy_plan.commit(policy_text.as_bytes())?;

    let profile_id = bootstrap.profile_id.clone();
    let repo = plan.repo;
    let repo_policy_path = if plan.write_repo_policy {
        Value::String(policy_path.to_string_lossy().to_string())
    } else {
        Value::Null
    };
    let mut warnings = Vec::new();
    if !bootstrap.duplicate_profile_ids.is_empty() {
        warnings.push(json!({
            "code": "config.duplicate_repo_allowlist",
            "severity": "warn",
            "message": format!(
                "Repo `{}` is also allowlisted in profile(s): {}. Profile auto-resolution will be ambiguous.",
                repo,
                bootstrap.duplicate_profile_ids.join(", ")
            )
        }));
    }
    let mut next_steps = Vec::new();
    if let Some(model) = bootstrap.default_model_install.as_deref() {
        next_steps.push(format!("qgh model install {model}"));
    }
    next_steps.push("qgh sync".to_string());
    next_steps.push("qgh query <terms>".to_string());
    Ok(InitOutcome {
        data: json!({
            "profile_config_path": bootstrap.config_path.to_string_lossy(),
            "profile_id": profile_id.clone(),
            "profile_action": bootstrap.profile_action,
            "repo": repo.clone(),
            "repo_allowlist_action": bootstrap.repo_allowlist_action,
            "repo_policy_action": repo_policy_action,
            "repo_policy_path": repo_policy_path.clone(),
            "token_source": {
                "kind": bootstrap.token_source_kind
            },
            "next_steps": next_steps
        }),
        warnings,
        meta: json!({
            "profile_id": profile_id,
            "profile_source": plan.profile_source,
            "repo": repo,
            "repo_source": plan.repo_source,
            "repo_policy_path": Value::Null
        }),
    })
}

fn prompt_line(label: &str, default: &str) -> Result<String, QghError> {
    let mut stderr = io::stderr();
    write!(stderr, "{label} [{default}]: ")?;
    stderr.flush()?;
    let mut line = String::new();
    let bytes = io::stdin().read_line(&mut line)?;
    if bytes == 0 {
        writeln!(stderr, "\nqgh init canceled; no files changed.")?;
        return Err(init_cancelled());
    }
    let value = line.trim();
    if value.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(value.to_string())
    }
}

fn prompt_use_defaults() -> Result<bool, QghError> {
    let answer = prompt_line("Use these defaults?", "Y/n")?;
    if answer == "Y/n" {
        return Ok(true);
    }
    match answer.to_ascii_lowercase().as_str() {
        "y" | "yes" => Ok(true),
        "n" | "no" => Ok(false),
        _ => Err(QghError::validation(
            "validation.invalid_init_answer",
            "Use these defaults? expects yes or no.",
        )),
    }
}

fn prompt_bool(label: &str, default: bool) -> Result<bool, QghError> {
    let default_text = if default { "Y/n" } else { "y/N" };
    let answer = prompt_line(label, default_text)?;
    if answer == default_text {
        return Ok(default);
    }
    match answer.to_ascii_lowercase().as_str() {
        "y" | "yes" => Ok(true),
        "n" | "no" => Ok(false),
        _ => Err(QghError::validation(
            "validation.invalid_init_answer",
            format!("{label} expects yes or no."),
        )),
    }
}

fn init_cancelled() -> QghError {
    QghError::validation(
        "validation.init_cancelled",
        "qgh init canceled before writing files.",
    )
    .with_hint("Run qgh init again, or use qgh init -y for non-interactive setup.")
}

fn explicit_profile_for_init(profile_arg: Option<&str>) -> Option<(String, &'static str)> {
    if let Some(profile_id) = profile_arg {
        return Some((profile_id.to_string(), "cli"));
    }
    std::env::var("QGH_PROFILE")
        .ok()
        .map(|profile_id| (profile_id, "env"))
}

fn init_token_source_for_profile(
    args: &InitArgs,
    existing_profile: Option<&Profile>,
    prompt_for_new_profile: bool,
) -> Result<(TokenSource, bool), QghError> {
    let Some(profile) = existing_profile else {
        let token_source = if prompt_for_new_profile {
            prompt_init_token_source(args)?
        } else {
            init_token_source_or_default(args)?
        };
        return Ok((token_source, args.token_source.is_some()));
    };

    let explicit = args.token_source.is_some() || args.token_env.is_some();
    let requested = match args.token_source {
        None if args.token_env.is_some() => return Err(missing_init_value("--token-source")),
        None => profile.token_source.clone(),
        Some(InitTokenSourceArg::GithubCli) => {
            if args.token_env.is_some() {
                return Err(QghError::validation(
                    "validation.invalid_token_source",
                    "--token-env can only be used with --token-source env.",
                ));
            }
            TokenSource::GithubCli
        }
        Some(InitTokenSourceArg::Env) => match args.token_env.as_deref() {
            Some(env) => TokenSource::Env {
                env: env.to_string(),
            },
            None => match &profile.token_source {
                TokenSource::Env { env } => TokenSource::Env { env: env.clone() },
                _ => {
                    return Err(QghError::config(format!(
                        "Profile `{}` already exists with a different token source.",
                        profile.id
                    )));
                }
            },
        },
    };
    if requested != profile.token_source {
        return Err(QghError::config(format!(
            "Profile `{}` already exists with a different token source.",
            profile.id
        )));
    }
    Ok((profile.token_source.clone(), explicit))
}

fn prompt_init_token_source(args: &InitArgs) -> Result<TokenSource, QghError> {
    let token_source_name = match args.token_source {
        Some(InitTokenSourceArg::GithubCli) => "github_cli".to_string(),
        Some(InitTokenSourceArg::Env) => "env".to_string(),
        None => prompt_line("token source (github_cli/env)", "github_cli")?,
    };
    match token_source_name.as_str() {
        "github_cli" => {
            if args.token_env.is_some() {
                return Err(QghError::validation(
                    "validation.invalid_token_source",
                    "--token-env can only be used with --token-source env.",
                ));
            }
            Ok(TokenSource::GithubCli)
        }
        "env" => {
            let env = match args.token_env.as_deref() {
                Some(env) => env.to_string(),
                None => prompt_line("token env var", "GITHUB_TOKEN")?,
            };
            Ok(TokenSource::Env { env })
        }
        _ => Err(QghError::validation(
            "validation.invalid_token_source",
            "Token source must be `github_cli` or `env`.",
        )),
    }
}

fn init_token_source_or_default(args: &InitArgs) -> Result<TokenSource, QghError> {
    match args.token_source {
        Some(InitTokenSourceArg::GithubCli) => {
            if args.token_env.is_some() {
                return Err(QghError::validation(
                    "validation.invalid_token_source",
                    "--token-env can only be used with --token-source env.",
                ));
            }
            Ok(TokenSource::GithubCli)
        }
        Some(InitTokenSourceArg::Env) => {
            let env = match args.token_env.clone() {
                Some(env) => env,
                None if args.yes => return Err(missing_init_value("--token-env")),
                None => prompt_line("token env var", "GITHUB_TOKEN")?,
            };
            Ok(TokenSource::Env { env })
        }
        None => {
            if args.token_env.is_some() {
                return Err(missing_init_value("--token-source"));
            }
            Ok(TokenSource::GithubCli)
        }
    }
}

fn token_source_display(token_source: &TokenSource) -> String {
    match token_source {
        TokenSource::GithubCli => "github_cli".to_string(),
        TokenSource::Env { env } => format!("env ({env})"),
        TokenSource::Unsupported => "unsupported".to_string(),
    }
}

fn optional_git_remote_defaults(
    root: &Path,
    args: &InitArgs,
) -> Result<Option<GitRemote>, QghError> {
    match git_remote_defaults_for_root(root) {
        Ok(remote) => Ok(Some(remote)),
        Err(_error) if args.repo.is_some() && args.host.is_some() => Ok(None),
        Err(error) => Err(error),
    }
}

fn default_api_base_url(host: &str) -> String {
    if host == "github.com" {
        "https://api.github.com".to_string()
    } else {
        format!("https://{host}/api/v3")
    }
}

fn default_web_base_url(host: &str) -> String {
    format!("https://{host}")
}

fn same_init_endpoint(left: &str, right: &str) -> bool {
    left.trim_end_matches('/') == right.trim_end_matches('/')
}

fn missing_init_value(flag: &str) -> QghError {
    QghError::validation(
        "validation.missing_init_value",
        format!("{flag} is required for non-interactive qgh init."),
    )
    .with_hint("Provide all required init flags with --yes.")
}

fn repo_from_origin_remote(root: &Path) -> Result<String, QghError> {
    Ok(git_remote_defaults_for_root(root)?.repo)
}

fn repo_policy_toml(repo: &str) -> String {
    format!(
        r#"schema_version = "qgh.repo.v1"

[repo]
github = "{repo}"

[defaults]
scope = "repo"
state = "all"
source_types = ["issue", "issue_comment"]
labels = []

[query]
limit = 10
"#
    )
}

fn invalid_repo_input() -> QghError {
    QghError::validation(
        "validation.invalid_repo",
        "Repo must use explicit owner/repo format.",
    )
    .with_hint("Use explicit owner/repo format.")
}
