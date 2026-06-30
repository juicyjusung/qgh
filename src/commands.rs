use crate::cli::{InitArgs, InitRepoArgs, InitTokenSourceArg, QueryArgs, ReconcileMode};
use crate::config::{
    bootstrap_profile_repo, current_git_worktree_root, discover_repo_policy,
    git_remote_defaults_for_root, load_profile, load_repo_policy_at, parse_repo, resolve_token,
    GitRemote, Profile, ProfileBootstrapInput, RepoPolicy, TokenSource,
};
use crate::error::QghError;
use crate::github;
use crate::index;
use crate::model::{ReconciliationCandidate, StoredComment, StoredIssue, StoredSource};
use crate::paths::ProfilePaths;
use crate::resolution::ResolvedRepoScope;
use crate::store::Store;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

const GET_BATCH_SIZE_CAP: usize = 20;

pub struct InitCommandOutcome {
    pub data: Value,
    pub warnings: Vec<Value>,
    pub meta: Value,
}

pub async fn sync(
    profile_id: &str,
    reconcile: Option<ReconcileMode>,
    repo_scope: Option<&ResolvedRepoScope>,
    show_progress: bool,
) -> Result<Value, QghError> {
    let progress = StderrSyncProgress::new(show_progress);
    progress.line(format_args!(
        "qgh sync: loading profile profile={profile_id}"
    ));
    let profile = load_profile(profile_id)?;
    let token = resolve_token(&profile)?;
    let mut store = Store::open(&profile.paths)?;
    let cursors = store.sync_cursors()?;
    let fetch_profile = profile_scoped_to_repo(&profile, repo_scope)?;
    progress.line(format_args!(
        "qgh sync: fetching GitHub issues/comments repos={}",
        fetch_profile.repos.len()
    ));
    let fetched =
        match github::fetch_issues(&fetch_profile, &token, &cursors, Some(&progress)).await? {
            github::FetchOutcome::Fetched(fetched) => fetched,
            github::FetchOutcome::Backoff(backoff) => {
                progress.line(format_args!(
                    "qgh sync: backoff reason={} scope={} retry_after_seconds={}",
                    backoff.reason, backoff.scope, backoff.retry_after_seconds
                ));
                let backoff = store.record_backoff_state(
                    &backoff.reason,
                    &backoff.scope,
                    backoff.retry_after_seconds,
                    backoff.reset_at.as_deref(),
                )?;
                let status = store.status()?;
                return Ok(json!({
                    "profile_id": profile.id,
                    "sync_state": "backoff",
                    "backoff": backoff,
                    "sync": {
                        "last_successful_sync": status.last_sync_at,
                        "scheduler": {
                            "max_in_flight_requests": profile.max_in_flight_requests,
                            "hard_cap": 16
                        }
                    },
                    "sources": {
                        "issue_count": status.issue_count,
                        "comment_count": status.comment_count,
                        "tombstone_count": status.tombstone_count
                    },
                    "index": {
                        "active_generation": status.active_generation,
                        "dirty_task_count": status.dirty_task_count
                    }
                }));
            }
        };
    progress.line(format_args!(
        "qgh sync: fetched issues={} comments={} skipped_pull_requests={}",
        fetched.issues.len(),
        fetched.comments.len(),
        fetched.skipped_pull_requests
    ));
    progress.line(format_args!("qgh sync: writing SQLite store"));
    let summary = store.upsert_sources(
        &fetched.issues,
        &fetched.comments,
        fetched.skipped_pull_requests,
        &fetched.cursor_updates,
    )?;
    progress.line(format_args!(
        "qgh sync: stored upserted_issues={} upserted_comments={} cursor_updates={}",
        summary.upserted_issues,
        summary.upserted_comments,
        summary.cursor_updates.len()
    ));
    let reconciliation = if reconcile == Some(ReconcileMode::Full) {
        let candidates = store.active_reconciliation_candidates()?;
        let candidates = reconciliation_candidates_scoped_to_repo(candidates, repo_scope);
        let estimated_api_cost_class = estimate_api_cost_class(candidates.len());
        progress.line(format_args!(
            "qgh sync: reconciling sources={} mode=full",
            candidates.len()
        ));
        let result =
            github::reconcile_sources(&fetch_profile, &token, &candidates, Some(&progress)).await?;
        let mut tombstoned_sources = 0;
        for unavailable in result.unavailable_sources {
            store.tombstone_source(&unavailable.source_id, &unavailable.reason)?;
            tombstoned_sources += 1;
        }
        store.record_reconciliation_run(
            "full",
            result.checked_sources,
            tombstoned_sources,
            estimated_api_cost_class,
        )?;
        json!({
            "mode": "full",
            "checked_sources": result.checked_sources,
            "tombstoned_sources": tombstoned_sources,
            "estimated_api_cost_class": estimated_api_cost_class
        })
    } else {
        json!({
            "mode": "none"
        })
    };
    store.clear_backoff_state()?;
    let sources = store.active_index_sources()?;
    progress.line(format_args!(
        "qgh sync: rebuilding BM25 index sources={}",
        sources.len()
    ));
    let (generation, reserved_generation_path) =
        store.reserve_index_generation(&profile.paths.index_root, sources.len())?;
    let generation_path = index::rebuild(&profile.paths.index_root, generation, &sources)?;
    debug_assert_eq!(generation_path, reserved_generation_path);
    store.mark_index_published(
        generation,
        &generation_path.to_string_lossy(),
        sources.len(),
    )?;
    progress.line(format_args!(
        "qgh sync: published BM25 index generation={} sources={}",
        generation,
        sources.len()
    ));
    let status = store.status()?;
    let watermarks = summary
        .cursor_updates
        .iter()
        .map(|cursor| (cursor.endpoint.clone(), json!(cursor.watermark)))
        .collect::<serde_json::Map<_, _>>();
    progress.line(format_args!(
        "qgh sync: complete sync_run_id={}",
        summary.sync_run_id
    ));
    Ok(json!({
        "profile_id": profile.id,
        "sync_state": "ok",
        "sync_run_id": summary.sync_run_id,
        "scheduler": {
            "max_in_flight_requests": profile.max_in_flight_requests,
            "hard_cap": 16
        },
        "issues": {
            "fetched": summary.fetched_issues,
            "upserted": summary.upserted_issues,
            "skipped_pull_requests": summary.skipped_pull_requests
        },
        "comments": {
            "fetched": summary.fetched_comments,
            "upserted": summary.upserted_comments
        },
        "cursors": {
            "updated": summary.cursor_updates.len(),
            "not_modified_endpoints": summary.not_modified_endpoints,
            "watermarks": watermarks
        },
        "index": {
            "active_generation": generation,
            "dirty_task_count": status.dirty_task_count
        },
        "reconciliation": reconciliation
    }))
}

struct StderrSyncProgress {
    enabled: bool,
}

impl StderrSyncProgress {
    fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    fn line(&self, args: fmt::Arguments<'_>) {
        if self.enabled {
            eprintln!("{args}");
        }
    }
}

impl github::ProgressReporter for StderrSyncProgress {
    fn report(&self, event: github::ProgressEvent) {
        match event {
            github::ProgressEvent::RepoStarted { repo } => {
                self.line(format_args!("qgh sync: fetching repo={repo}"));
            }
            github::ProgressEvent::IssuePageFetched { repo, item_count } => {
                self.line(format_args!(
                    "qgh sync: received issue page repo={repo} items={item_count}"
                ));
            }
            github::ProgressEvent::RepoProgress {
                repo,
                issues,
                comments,
                skipped_pull_requests,
            } => {
                self.line(format_args!(
                    "qgh sync: processed repo={repo} issues={issues} comments={comments} skipped_pull_requests={skipped_pull_requests}"
                ));
            }
            github::ProgressEvent::IssueEndpointNotModified { repo } => {
                self.line(format_args!("qgh sync: issues unchanged repo={repo}"));
            }
            github::ProgressEvent::CommentPageFetched {
                repo,
                issue_number,
                item_count,
            } => {
                self.line(format_args!(
                    "qgh sync: received comment page repo={repo} issue=#{issue_number} items={item_count}"
                ));
            }
            github::ProgressEvent::Backoff {
                reason,
                scope,
                retry_after_seconds,
            } => {
                self.line(format_args!(
                    "qgh sync: GitHub backoff reason={reason} scope={scope} retry_after_seconds={retry_after_seconds}"
                ));
            }
            github::ProgressEvent::ReconciliationProgress { checked, total } => {
                self.line(format_args!(
                    "qgh sync: reconciled checked_sources={checked}/{total}"
                ));
            }
        }
    }
}

fn profile_scoped_to_repo(
    profile: &Profile,
    repo_scope: Option<&ResolvedRepoScope>,
) -> Result<Profile, QghError> {
    let Some(repo_scope) = repo_scope else {
        return Ok(profile.clone());
    };
    let Some(repo) = profile
        .repos
        .iter()
        .find(|repo| repo.full_name() == repo_scope.repo)
        .cloned()
    else {
        return Err(QghError::validation(
            "validation.invalid_repo",
            format!(
                "Repo `{}` is outside profile `{}` allowlist.",
                repo_scope.repo, profile.id
            ),
        )
        .with_details(json!({
            "profile_id": profile.id,
            "repo": repo_scope.repo
        }))
        .with_hint("Use a repo from the profile allowlist or update the profile config."));
    };
    let mut scoped = profile.clone();
    scoped.repos = vec![repo];
    Ok(scoped)
}

fn reconciliation_candidates_scoped_to_repo(
    candidates: Vec<ReconciliationCandidate>,
    repo_scope: Option<&ResolvedRepoScope>,
) -> Vec<ReconciliationCandidate> {
    let Some(repo_scope) = repo_scope else {
        return candidates;
    };
    candidates
        .into_iter()
        .filter(|candidate| candidate.repo == repo_scope.repo)
        .collect()
}

pub fn init(profile_arg: Option<&str>, args: &InitArgs) -> Result<InitCommandOutcome, QghError> {
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

pub fn init_repo_policy(
    profile_arg: Option<&str>,
    args: InitRepoArgs,
) -> Result<InitCommandOutcome, QghError> {
    let Some(root) = current_git_worktree_root() else {
        return Err(QghError::validation(
            "config.no_git_worktree",
            "qgh init must be run inside a git worktree.",
        )
        .with_hint("Run qgh init from a git worktree or pass --repo after initializing git."));
    };

    let (repo, repo_source) = match args.repo.as_deref() {
        Some(repo) => {
            parse_repo(repo).map_err(|message| {
                QghError::validation(
                    "validation.invalid_repo",
                    format!("Repo `{repo}` {message}"),
                )
                .with_details(json!({ "repo": repo }))
                .with_hint("Use explicit owner/repo format.")
            })?;
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
                    "message": "Profile allowlist was not checked because no profile was explicit."
                })],
                None,
                None,
            ),
        };

    let path = root.join(".qgh.toml");
    let overwritten = path.exists();
    if overwritten && !args.force {
        return Err(QghError::validation(
            "config.repo_policy_exists",
            "Repo policy already exists.",
        )
        .with_details(json!({ "path": path.to_string_lossy() }))
        .with_hint("Use --force to overwrite the existing .qgh.toml."));
    }

    fs::write(&path, repo_policy_toml(&repo)).map_err(|error| {
        QghError::storage(format!(
            "Failed to write repo policy at {}: {error}",
            path.display()
        ))
    })?;
    load_repo_policy_at(&path)?;

    let meta_repo_source = if repo_source == "cli" {
        Some("cli")
    } else {
        None
    };
    Ok(InitCommandOutcome {
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
            "repo_source": meta_repo_source,
            "repo_policy_path": Value::Null
        }),
    })
}

fn init_custom_interactive(
    root: &std::path::Path,
    remote: Option<&GitRemote>,
    profile_arg: Option<&str>,
    args: &InitArgs,
) -> Result<InitCommandOutcome, QghError> {
    let repo = match args.repo.as_deref() {
        Some(repo) => {
            parse_repo(repo).map_err(|message| {
                QghError::validation(
                    "validation.invalid_repo",
                    format!("Repo `{repo}` {message}"),
                )
            })?;
            repo.to_string()
        }
        None => remote
            .map(|remote| remote.repo.clone())
            .ok_or_else(|| missing_init_value("--repo"))?,
    };
    let profile_default = profile_arg.unwrap_or("work");
    let profile_id = prompt_line("profile id", profile_default)?;
    let host_default = args
        .host
        .clone()
        .or_else(|| remote.map(|remote| remote.host.clone()))
        .ok_or_else(|| missing_init_value("--host"))?;
    let host = prompt_line("host", &host_default)?;
    let api_default = args
        .api_base_url
        .clone()
        .or_else(|| remote.map(|remote| remote.api_base_url.clone()))
        .unwrap_or_else(|| default_api_base_url(&host));
    let api_base_url = prompt_line(
        "api base url",
        args.api_base_url.as_deref().unwrap_or(&api_default),
    )?;
    let web_default = args
        .web_base_url
        .clone()
        .or_else(|| remote.map(|remote| remote.web_base_url.clone()))
        .unwrap_or_else(|| default_web_base_url(&host));
    let web_base_url = prompt_line(
        "web base url",
        args.web_base_url.as_deref().unwrap_or(&web_default),
    )?;
    let token_source_name = match args.token_source {
        Some(InitTokenSourceArg::GithubCli) => "github_cli".to_string(),
        Some(InitTokenSourceArg::Env) => "env".to_string(),
        None => prompt_line("token source (github_cli/env)", "github_cli")?,
    };
    let token_source = match token_source_name.as_str() {
        "github_cli" => TokenSource::GithubCli,
        "env" => {
            let env = match args.token_env.as_deref() {
                Some(env) => env.to_string(),
                None => prompt_line("token env var", "GITHUB_TOKEN")?,
            };
            TokenSource::Env { env }
        }
        _ => {
            return Err(QghError::validation(
                "validation.invalid_token_source",
                "Token source must be `github_cli` or `env`.",
            ));
        }
    };
    let write_repo_policy = prompt_bool("create .qgh.toml", true)?;
    finish_profile_init(
        &root,
        ProfileInitPlan {
            profile_id,
            repo,
            host,
            api_base_url,
            web_base_url,
            token_source,
            write_repo_policy,
            force_repo_policy: args.force,
        },
    )
}

struct InitPreset {
    profile_id: String,
    repo: String,
    host: String,
    api_base_url: String,
    web_base_url: String,
    token_source: TokenSource,
    write_repo_policy: bool,
    force_repo_policy: bool,
    config_path: PathBuf,
    repo_policy_path: PathBuf,
    db_path: PathBuf,
}

fn init_preset(
    profile_arg: Option<&str>,
    args: &InitArgs,
    root: &std::path::Path,
    remote: Option<&GitRemote>,
) -> Result<InitPreset, QghError> {
    let repo = match args.repo.as_deref() {
        Some(repo) => {
            parse_repo(repo).map_err(|message| {
                QghError::validation(
                    "validation.invalid_repo",
                    format!("Repo `{repo}` {message}"),
                )
            })?;
            repo.to_string()
        }
        None => remote
            .map(|remote| remote.repo.clone())
            .ok_or_else(|| missing_init_value("--repo"))?,
    };
    let profile_id = profile_arg.unwrap_or("work").to_string();
    let host = args
        .host
        .clone()
        .or_else(|| remote.map(|remote| remote.host.clone()))
        .ok_or_else(|| missing_init_value("--host"))?;
    let api_base_url = args
        .api_base_url
        .clone()
        .or_else(|| remote.map(|remote| remote.api_base_url.clone()))
        .unwrap_or_else(|| default_api_base_url(&host));
    let web_base_url = args
        .web_base_url
        .clone()
        .or_else(|| remote.map(|remote| remote.web_base_url.clone()))
        .unwrap_or_else(|| default_web_base_url(&host));
    let token_source = init_token_source_or_default(args)?;
    let paths = ProfilePaths::resolve(&profile_id)?;
    Ok(InitPreset {
        profile_id,
        repo,
        host,
        api_base_url,
        web_base_url,
        token_source,
        write_repo_policy: true,
        force_repo_policy: args.force,
        config_path: paths.config_file,
        repo_policy_path: root.join(".qgh.toml"),
        db_path: paths.db_path,
    })
}

fn finish_init_preset(
    root: &std::path::Path,
    preset: InitPreset,
) -> Result<InitCommandOutcome, QghError> {
    finish_profile_init(
        root,
        ProfileInitPlan {
            profile_id: preset.profile_id,
            repo: preset.repo,
            host: preset.host,
            api_base_url: preset.api_base_url,
            web_base_url: preset.web_base_url,
            token_source: preset.token_source,
            write_repo_policy: preset.write_repo_policy,
            force_repo_policy: preset.force_repo_policy,
        },
    )
}

fn write_init_preset_preview(preset: &InitPreset) -> Result<(), QghError> {
    let mut stderr = io::stderr();
    writeln!(stderr, "Detected qgh init defaults:")?;
    writeln!(stderr, "  repo: {}", preset.repo)?;
    writeln!(stderr, "  host: {}", preset.host)?;
    writeln!(stderr, "  profile id: {}", preset.profile_id)?;
    writeln!(
        stderr,
        "  token source: {}",
        token_source_display(&preset.token_source)
    )?;
    writeln!(stderr, "  config path: {}", preset.config_path.display())?;
    writeln!(stderr, "  repo policy: create")?;
    writeln!(
        stderr,
        "  repo policy path: {}",
        preset.repo_policy_path.display()
    )?;
    writeln!(stderr, "  db path: {}", preset.db_path.display())?;
    Ok(())
}

struct ProfileInitPlan {
    profile_id: String,
    repo: String,
    host: String,
    api_base_url: String,
    web_base_url: String,
    token_source: TokenSource,
    write_repo_policy: bool,
    force_repo_policy: bool,
}

fn finish_profile_init(
    root: &std::path::Path,
    plan: ProfileInitPlan,
) -> Result<InitCommandOutcome, QghError> {
    let policy_path = root.join(".qgh.toml");
    let repo_policy_action = plan_repo_policy_action(
        &policy_path,
        &plan.repo,
        plan.write_repo_policy,
        plan.force_repo_policy,
    )?;

    let bootstrap = bootstrap_profile_repo(ProfileBootstrapInput {
        profile_id: plan.profile_id.clone(),
        host: plan.host,
        api_base_url: plan.api_base_url,
        web_base_url: plan.web_base_url,
        repo: plan.repo.clone(),
        token_source: plan.token_source,
    })?;

    apply_repo_policy_action(&policy_path, &plan.repo, repo_policy_action)?;

    let profile_id = plan.profile_id;
    let repo = plan.repo;
    let repo_policy_path = if plan.write_repo_policy {
        Value::String(policy_path.to_string_lossy().to_string())
    } else {
        Value::Null
    };
    Ok(InitCommandOutcome {
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
            "next_steps": ["qgh sync", "qgh query <terms>"]
        }),
        warnings: Vec::new(),
        meta: json!({
            "profile_id": profile_id,
            "profile_source": "cli",
            "repo": repo,
            "repo_source": "cli",
            "repo_policy_path": repo_policy_path
        }),
    })
}

fn plan_repo_policy_action(
    policy_path: &std::path::Path,
    requested_repo: &str,
    write_repo_policy: bool,
    force_repo_policy: bool,
) -> Result<&'static str, QghError> {
    if !write_repo_policy {
        return Ok("skipped");
    }
    if !policy_path.exists() {
        return Ok("created");
    }
    if force_repo_policy {
        return Ok("overwritten");
    }
    let existing_policy = load_repo_policy_at(policy_path)?;
    let existing_repo = existing_policy.repo.full_name();
    if existing_repo == requested_repo {
        return Ok("already_exists");
    }
    Err(QghError::validation(
        "config.repo_policy_exists",
        "Repo policy already exists for a different repo.",
    )
    .with_details(json!({
        "path": policy_path.to_string_lossy(),
        "existing_repo": existing_repo,
        "requested_repo": requested_repo
    }))
    .with_hint("Use --force to overwrite the existing .qgh.toml."))
}

fn apply_repo_policy_action(
    policy_path: &std::path::Path,
    repo: &str,
    action: &'static str,
) -> Result<(), QghError> {
    if !matches!(action, "created" | "overwritten") {
        return Ok(());
    }
    fs::write(policy_path, repo_policy_toml(repo)).map_err(|error| {
        QghError::storage(format!(
            "Failed to write repo policy at {}: {error}",
            policy_path.display()
        ))
    })?;
    load_repo_policy_at(policy_path)?;
    Ok(())
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

pub fn query(
    profile_id: &str,
    args: QueryArgs,
    repo_scope: Option<&ResolvedRepoScope>,
) -> Result<Value, QghError> {
    let profile = load_profile(profile_id)?;
    let repo_policy = discover_repo_policy()?;
    let filters = QueryFilters::from_args(&args, &profile, repo_policy.as_ref(), repo_scope)?;
    let limit = effective_limit(&args, repo_policy.as_ref());
    let store = Store::open(&profile.paths)?;
    if let Some(results) = exact_results(&store, &args.query, &filters, &profile.id)? {
        return Ok(json!({
            "profile_id": profile.id,
            "result_filtering": {
                "unresolvable_hits": 0
            },
            "results": results
        }));
    }
    let active_index_path = active_index_path(&store, &profile.paths.index_active)?;
    let hits = index::search(&active_index_path, &args.query, limit)?;
    let mut results = Vec::new();
    let mut unresolvable_hits = 0;
    for hit in hits {
        let Some(source) = store.get_source(&hit.source_id)? else {
            unresolvable_hits += 1;
            continue;
        };
        if !filters.matches(&source) {
            continue;
        }
        results.push(source_result(source, Ranking::Bm25(hit.score), &profile.id));
    }
    Ok(json!({
        "profile_id": profile.id,
        "result_filtering": {
            "unresolvable_hits": unresolvable_hits
        },
        "results": results
    }))
}

fn explicit_profile_for_init(profile_arg: Option<&str>) -> Option<(String, &'static str)> {
    if let Some(profile_id) = profile_arg {
        return Some((profile_id.to_string(), "cli"));
    }
    std::env::var("QGH_PROFILE")
        .ok()
        .map(|profile_id| (profile_id, "env"))
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
    root: &std::path::Path,
    args: &InitArgs,
) -> Result<Option<GitRemote>, QghError> {
    match git_remote_defaults_for_root(root) {
        Ok(remote) => Ok(Some(remote)),
        Err(error) if args.repo.is_some() && args.host.is_some() => Ok(None),
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

fn missing_init_value(flag: &str) -> QghError {
    QghError::validation(
        "validation.missing_init_value",
        format!("{flag} is required for non-interactive qgh init."),
    )
    .with_hint("Provide all required init flags with --yes.")
}

fn repo_from_origin_remote(root: &std::path::Path) -> Result<String, QghError> {
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

#[derive(Debug)]
struct QueryFilters {
    repo: Option<String>,
    labels: Vec<String>,
    state: Option<String>,
    author: Option<String>,
    issue: Option<i64>,
    source_types: Vec<String>,
}

impl QueryFilters {
    fn from_args(
        args: &QueryArgs,
        profile: &Profile,
        repo_policy: Option<&RepoPolicy>,
        repo_scope: Option<&ResolvedRepoScope>,
    ) -> Result<Self, QghError> {
        if args.wiki.is_some() {
            return Err(QghError::validation(
                "validation.unsupported_filter",
                "Wiki filters are post-MVP and unsupported.",
            ));
        }
        let repo = effective_repo(args, profile, repo_policy, repo_scope)?;
        let state = effective_state(args, repo_policy)?;
        let labels = effective_labels(args, repo_policy);
        let source_types = effective_source_types(repo_policy);
        Ok(Self {
            repo,
            labels,
            state,
            author: args.author.clone(),
            issue: args.issue,
            source_types,
        })
    }

    fn matches(&self, source: &StoredSource) -> bool {
        match source {
            StoredSource::Issue(issue) => {
                self.source_type_matches("issue")
                    && self.repo_matches(&issue.repo)
                    && self.issue_matches(issue.number)
                    && self.author_matches(issue.author.as_deref())
                    && self.state_matches(Some(&issue.state))
                    && self.labels.iter().all(|label| issue.labels.contains(label))
            }
            StoredSource::Comment(comment) => {
                self.source_type_matches("issue_comment")
                    && self.repo_matches(&comment.repo)
                    && self.issue_matches(comment.issue_number)
                    && self.author_matches(comment.author.as_deref())
                    && self.state.is_none()
                    && self.labels.is_empty()
            }
        }
    }

    fn repo_matches(&self, repo: &str) -> bool {
        self.repo.as_deref().is_none_or(|expected| expected == repo)
    }

    fn issue_matches(&self, issue_number: i64) -> bool {
        self.issue.is_none_or(|expected| expected == issue_number)
    }

    fn author_matches(&self, author: Option<&str>) -> bool {
        self.author
            .as_deref()
            .is_none_or(|expected| author == Some(expected))
    }

    fn state_matches(&self, state: Option<&String>) -> bool {
        self.state
            .as_ref()
            .is_none_or(|expected| state == Some(expected))
    }

    fn source_type_matches(&self, source_type: &str) -> bool {
        self.source_types
            .iter()
            .any(|allowed| allowed == source_type)
    }
}

fn effective_repo(
    args: &QueryArgs,
    profile: &Profile,
    repo_policy: Option<&RepoPolicy>,
    repo_scope: Option<&ResolvedRepoScope>,
) -> Result<Option<String>, QghError> {
    if let Some(repo) = &args.repo {
        validate_repo(repo)?;
        if !profile.allows_repo(repo) {
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
            .with_hint("Use a repo from the profile allowlist or update the profile config."));
        }
        return Ok(Some(repo.clone()));
    }

    let Some(scope) = repo_scope else {
        return Ok(None);
    };
    let repo = scope.repo.clone();
    if !profile.allows_repo(&repo) {
        if let Some(repo_policy) = repo_policy {
            return Err(QghError::invalid_repo_policy(format!(
                "Repo policy repo `{repo}` is outside profile `{}` allowlist.",
                profile.id
            ))
            .with_details(json!({
                "profile_id": profile.id,
                "repo": repo,
                "repo_policy_path": repo_policy.path
            }))
            .with_hint("Update `.qgh.toml` or the profile repo allowlist."));
        }
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
        .with_hint("Use a repo from the profile allowlist or update the profile config."));
    }
    Ok(Some(repo))
}

fn effective_state(
    args: &QueryArgs,
    repo_policy: Option<&RepoPolicy>,
) -> Result<Option<String>, QghError> {
    if let Some(state) = &args.state {
        if !matches!(state.as_str(), "open" | "closed") {
            return Err(QghError::validation(
                "validation.invalid_state",
                "State filter must be `open` or `closed`.",
            ));
        }
        return Ok(Some(state.clone()));
    }
    Ok(repo_policy.and_then(|policy| policy.defaults.state.clone()))
}

fn effective_labels(args: &QueryArgs, repo_policy: Option<&RepoPolicy>) -> Vec<String> {
    if !args.label.is_empty() {
        return args.label.clone();
    }
    repo_policy
        .map(|policy| policy.defaults.labels.clone())
        .unwrap_or_default()
}

fn effective_source_types(repo_policy: Option<&RepoPolicy>) -> Vec<String> {
    repo_policy
        .map(|policy| policy.defaults.source_types.clone())
        .unwrap_or_else(|| vec!["issue".to_string(), "issue_comment".to_string()])
}

fn effective_limit(args: &QueryArgs, repo_policy: Option<&RepoPolicy>) -> usize {
    args.limit
        .or_else(|| repo_policy.and_then(|policy| policy.query.limit))
        .unwrap_or(10)
}

fn exact_results(
    store: &Store,
    query_text: &str,
    filters: &QueryFilters,
    profile_id: &str,
) -> Result<Option<Vec<Value>>, QghError> {
    if let Some(source) = exact_url_result(store, query_text)? {
        return Ok(Some(if filters.matches(&source) {
            vec![source_result(source, Ranking::Exact, profile_id)]
        } else {
            Vec::new()
        }));
    }
    let issue_number = filters.issue.or_else(|| parse_issue_number(query_text));
    let Some(issue_number) = issue_number else {
        return Ok(None);
    };
    let matches = if let Some(repo) = &filters.repo {
        store
            .find_issue_by_repo_number(repo, issue_number)?
            .into_iter()
            .collect::<Vec<_>>()
    } else {
        store.find_issues_by_number(issue_number)?
    };
    if matches.len() > 1 {
        return Err(QghError::validation(
            "validation.ambiguous_locator",
            "Issue number matches multiple repos; add --repo.",
        ));
    }
    Ok(Some(
        matches
            .into_iter()
            .map(StoredSource::Issue)
            .filter(|source| filters.matches(source))
            .map(|source| source_result(source, Ranking::Exact, profile_id))
            .collect(),
    ))
}

fn exact_url_result(store: &Store, query_text: &str) -> Result<Option<StoredSource>, QghError> {
    if !query_text.starts_with("https://github.com/") {
        return Ok(None);
    }
    if query_text.contains("#issuecomment-") {
        return store
            .find_comment_by_canonical_url(query_text)
            .map(|comment| comment.map(StoredSource::Comment));
    }
    store
        .find_issue_by_canonical_url(query_text)
        .map(|issue| issue.map(StoredSource::Issue))
}

fn parse_issue_number(query_text: &str) -> Option<i64> {
    query_text
        .strip_prefix('#')
        .unwrap_or(query_text)
        .parse::<i64>()
        .ok()
}

fn validate_repo(repo: &str) -> Result<(), QghError> {
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

fn enforce_source_scope(
    source_id: &str,
    source: &StoredSource,
    repo_scope: Option<&ResolvedRepoScope>,
) -> Result<(), QghError> {
    let Some(repo_scope) = repo_scope else {
        return Ok(());
    };
    let source_repo = match source {
        StoredSource::Issue(issue) => &issue.repo,
        StoredSource::Comment(comment) => &comment.repo,
    };
    if source_repo == &repo_scope.repo {
        return Ok(());
    }
    Err(QghError::source_outside_effective_scope(
        source_id,
        source_repo,
        &repo_scope.repo,
    ))
}

pub async fn get(
    profile_id: &str,
    source_id: &str,
    repo_scope: Option<&ResolvedRepoScope>,
) -> Result<Value, QghError> {
    let profile = load_profile(profile_id)?;
    let mut store = Store::open(&profile.paths)?;
    let source = get_source_with_lifecycle(&profile, &mut store, source_id, repo_scope).await?;
    Ok(json!({
        "profile_id": profile.id,
        "source": source
    }))
}

pub async fn get_cli(
    profile_id: &str,
    source_ids: &[String],
    repo_scope: Option<&ResolvedRepoScope>,
) -> Result<Value, QghError> {
    if source_ids.len() == 1 {
        return get(profile_id, &source_ids[0], repo_scope).await;
    }
    if source_ids.len() > GET_BATCH_SIZE_CAP {
        return Err(QghError::validation(
            "validation.batch_size",
            format!("get accepts at most {GET_BATCH_SIZE_CAP} source_id values per batch."),
        )
        .with_details(json!({
            "requested": source_ids.len(),
            "batch_size_cap": GET_BATCH_SIZE_CAP
        }))
        .with_hint("Split the source_id list into smaller qgh get batches."));
    }

    let profile = load_profile(profile_id)?;
    let mut store = Store::open(&profile.paths)?;
    let mut items = Vec::with_capacity(source_ids.len());
    let mut returned = 0;
    let mut failed = 0;
    for (input_index, source_id) in source_ids.iter().enumerate() {
        match get_source_with_lifecycle(&profile, &mut store, source_id, repo_scope).await {
            Ok(source) => {
                returned += 1;
                items.push(json!({
                    "input_index": input_index,
                    "source_id": source_id,
                    "ok": true,
                    "source": source
                }));
            }
            Err(error) if is_get_item_error(&error) => {
                failed += 1;
                items.push(json!({
                    "input_index": input_index,
                    "source_id": source_id,
                    "ok": false,
                    "error": error
                }));
            }
            Err(error) => return Err(error),
        }
    }

    Ok(json!({
        "profile_id": profile.id,
        "summary": {
            "requested": source_ids.len(),
            "returned": returned,
            "failed": failed,
            "batch_size_cap": GET_BATCH_SIZE_CAP
        },
        "lifecycle_check_policy": {
            "mode": "sequential",
            "max_in_flight_requests": 1,
            "profile_max_in_flight_requests": profile.max_in_flight_requests,
            "hard_cap": 16
        },
        "items": items
    }))
}

async fn get_source_with_lifecycle(
    profile: &Profile,
    store: &mut Store,
    source_id: &str,
    repo_scope: Option<&ResolvedRepoScope>,
) -> Result<Value, QghError> {
    if let Some(tombstone) = store.get_tombstone(source_id)? {
        return Err(QghError::source_tombstoned(
            &tombstone.source_id,
            &tombstone.reason,
            &tombstone.observed_at,
        ));
    }
    let Some(source) = store.get_source(source_id)? else {
        return Err(QghError::source_not_found(source_id));
    };
    enforce_source_scope(source_id, &source, repo_scope)?;
    let mut source_json = match source {
        StoredSource::Issue(issue) => issue_source(issue),
        StoredSource::Comment(comment) => comment_source(comment),
    };
    source_json["lifecycle_check"] = match resolve_token(&profile) {
        Ok(token) => {
            if let Some(candidate) = store.get_reconciliation_candidate(source_id)? {
                match github::check_source_lifecycle(&profile, &token, &candidate).await {
                    Ok(github::LifecycleCheck::Active) => json!({ "status": "active" }),
                    Ok(github::LifecycleCheck::Unavailable { reason }) => {
                        let tombstone = store.tombstone_source(source_id, &reason)?;
                        return Err(QghError::source_tombstoned(
                            &tombstone.source_id,
                            &tombstone.reason,
                            &tombstone.observed_at,
                        ));
                    }
                    Err(error) => json!({
                        "status": "not_checked",
                        "error_code": error.code
                    }),
                }
            } else {
                json!({ "status": "not_checked", "reason": "missing_candidate" })
            }
        }
        Err(error) => json!({
            "status": "not_checked",
            "error_code": error.code
        }),
    };
    Ok(source_json)
}

fn is_get_item_error(error: &QghError) -> bool {
    matches!(
        error.code.as_str(),
        "source.not_found" | "source.tombstoned" | "source.outside_effective_scope"
    )
}

pub fn get_local(
    profile_id: &str,
    source_id: &str,
    repo_scope: Option<&ResolvedRepoScope>,
) -> Result<Value, QghError> {
    let profile = load_profile(profile_id)?;
    let store = Store::open(&profile.paths)?;
    if let Some(tombstone) = store.get_tombstone(source_id)? {
        return Err(QghError::source_tombstoned(
            &tombstone.source_id,
            &tombstone.reason,
            &tombstone.observed_at,
        ));
    }
    let Some(source) = store.get_source(source_id)? else {
        return Err(QghError::source_not_found(source_id));
    };
    enforce_source_scope(source_id, &source, repo_scope)?;
    let mut source_json = match source {
        StoredSource::Issue(issue) => issue_source(issue),
        StoredSource::Comment(comment) => comment_source(comment),
    };
    source_json["lifecycle_check"] = json!({
        "status": "not_checked",
        "reason": "mcp_read_only"
    });
    Ok(json!({
        "profile_id": profile.id,
        "source": source_json
    }))
}

pub fn status(profile_id: &str) -> Result<Value, QghError> {
    let profile = load_profile(profile_id)?;
    let store = Store::open(&profile.paths)?;
    let status = store.status()?;
    let active_index_path = active_index_path(&store, &profile.paths.index_active)?;
    let source_count = (status.issue_count + status.comment_count) as usize;
    let age_days = status
        .last_reconciliation
        .as_ref()
        .and_then(|run| age_days(&run.completed_at));
    let stale = profile
        .reconcile_after_days
        .is_some_and(|days| age_days.is_none_or(|age| age > days));
    let stale_warning = if stale {
        json!("reconciliation.stale")
    } else {
        Value::Null
    };
    let last_reconciliation = status.last_reconciliation.as_ref();
    let cursors = status
        .cursors
        .iter()
        .map(|cursor| {
            (
                cursor.endpoint.clone(),
                json!({
                    "watermark": cursor.watermark,
                    "has_etag": cursor.has_etag
                }),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    Ok(json!({
        "profile_id": profile.id,
        "github": {
            "host": profile.host,
            "api_base_url": profile.api_base_url,
            "web_base_url": profile.web_base_url
        },
        "paths": {
            "config": profile.paths.config_file,
            "profile_data": profile.paths.profile_dir,
            "database": profile.paths.db_path,
            "tantivy_index": active_index_path,
            "cache": profile.paths.cache_dir,
            "logs": profile.paths.log_dir
        },
        "sources": {
            "issue_count": status.issue_count,
            "comment_count": status.comment_count,
            "tombstone_count": status.tombstone_count
        },
        "database": {
            "schema_version": "qgh.db.v1"
        },
        "index": {
            "active_generation": status.active_generation,
            "dirty_task_count": status.dirty_task_count
        },
        "sync": {
            "last_sync_at": status.last_sync_at,
            "cursors": cursors,
            "backoff": status.backoff,
            "scheduler": {
                "max_in_flight_requests": profile.max_in_flight_requests,
                "hard_cap": 16
            }
        },
        "reconciliation": {
            "last_full_at": last_reconciliation.map(|run| run.completed_at.clone()),
            "age_days": age_days,
            "stale": stale,
            "stale_warning": stale_warning,
            "estimated_api_cost_class": estimate_api_cost_class(source_count),
            "last_checked_source_count": last_reconciliation.map(|run| run.checked_source_count),
            "last_tombstoned_count": last_reconciliation.map(|run| run.tombstoned_count),
            "last_estimated_api_cost_class": last_reconciliation.map(|run| run.estimated_api_cost_class.clone())
        },
        "privacy": {
            "classification": "sensitive_derivative_data",
            "default_network_egress": "configured_github_host_only",
            "hosted_provider_egress": "disabled",
            "local_paths_may_contain_private_content": true,
            "single_user_permissions": "0600_files_0700_dirs_where_supported"
        }
    }))
}

pub async fn doctor(profile_id: &str) -> Result<Value, QghError> {
    let profile = load_profile(profile_id)?;
    let store = Store::open(&profile.paths)?;
    let status = store.status()?;
    let permissions_ok = private_paths_ok(&profile.paths);
    let sqlite_ok = status.active_generation >= 0;
    let active_index_path = active_index_path(&store, &profile.paths.index_active)?;
    let tantivy_ok = !active_index_path.exists()
        || index::search(&active_index_path, "__qgh_doctor_probe__", 1).is_ok();
    let (github_ok, rate_limit_ok, rate_limit_headers) = match resolve_token(&profile) {
        Ok(token) => doctor_github_probe(&profile, &token).await,
        Err(_) => (false, false, json!({})),
    };
    Ok(json!({
        "profile_id": profile.id,
        "checks": [
            {
                "name": "config",
                "ok": true
            },
            {
                "name": "file_permissions",
                "ok": permissions_ok
            },
            {
                "name": "sqlite",
                "ok": sqlite_ok
            },
            {
                "name": "tantivy",
                "ok": tantivy_ok
            },
            {
                "name": "github_auth_reachability",
                "ok": github_ok
            },
            {
                "name": "rate_limit_headers",
                "ok": rate_limit_ok,
                "headers": rate_limit_headers
            }
        ],
        "mcp": {
            "doctor_exposed": false,
            "tools": ["query", "get", "status"]
        }
    }))
}

fn active_index_path(store: &Store, fallback: &std::path::Path) -> Result<PathBuf, QghError> {
    Ok(store
        .active_index_path()?
        .map(PathBuf::from)
        .unwrap_or_else(|| fallback.to_path_buf()))
}

enum Ranking {
    Bm25(f32),
    Exact,
}

fn source_result(source: StoredSource, ranking: Ranking, profile_id: &str) -> Value {
    match source {
        StoredSource::Issue(issue) => issue_result(issue, ranking, profile_id),
        StoredSource::Comment(comment) => comment_result(comment, ranking, profile_id),
    }
}

fn issue_result(issue: StoredIssue, ranking: Ranking, profile_id: &str) -> Value {
    let source_id = issue.source_id;
    json!({
        "source_id": source_id,
        "entity_type": "issue",
        "repo": issue.repo,
        "issue_number": issue.number,
        "title": issue.title,
        "canonical_url": issue.canonical_url,
        "snippet": snippet(&issue.body),
        "get_args": {
            "source_id": source_id,
            "profile_id": profile_id
        },
        "parent_issue": Value::Null,
        "source_version": issue.source_version,
        "ranking": ranking_json(ranking)
    })
}

fn comment_result(comment: StoredComment, ranking: Ranking, profile_id: &str) -> Value {
    let source_id = comment.source_id;
    json!({
        "source_id": source_id,
        "entity_type": "issue_comment",
        "repo": comment.repo,
        "issue_number": comment.issue_number,
        "author": comment.author,
        "canonical_url": comment.canonical_url,
        "parent_issue": comment.parent_issue,
        "snippet": snippet(&comment.body),
        "get_args": {
            "source_id": source_id,
            "profile_id": profile_id
        },
        "source_version": comment.source_version,
        "ranking": ranking_json(ranking)
    })
}

fn ranking_json(ranking: Ranking) -> Value {
    match ranking {
        Ranking::Bm25(score) => json!({
            "kind": "bm25",
            "lexical_score": score
        }),
        Ranking::Exact => json!({
            "kind": "exact",
            "lexical_score": Value::Null
        }),
    }
}

fn estimate_api_cost_class(source_count: usize) -> &'static str {
    match source_count {
        0 => "none",
        1..=100 => "low",
        101..=1000 => "medium",
        _ => "high",
    }
}

fn age_days(timestamp: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(timestamp).ok().map(|parsed| {
        Utc::now()
            .signed_duration_since(parsed.with_timezone(&Utc))
            .num_days()
            .max(0)
    })
}

async fn doctor_github_probe(profile: &crate::config::Profile, token: &str) -> (bool, bool, Value) {
    let url = format!("{}/rate_limit", profile.api_base_url);
    let response = reqwest::Client::new()
        .get(url)
        .bearer_auth(token)
        .header("accept", "application/vnd.github+json")
        .header("user-agent", github::user_agent())
        .header("x-github-api-version", github::GITHUB_API_VERSION)
        .send()
        .await;
    let Ok(response) = response else {
        return (false, false, json!({}));
    };
    let headers = response.headers();
    let remaining = headers
        .get("x-ratelimit-remaining")
        .and_then(|value| value.to_str().ok())
        .map(ToString::to_string);
    let reset = headers
        .get("x-ratelimit-reset")
        .and_then(|value| value.to_str().ok())
        .map(ToString::to_string);
    let rate_limit_ok = remaining.is_some();
    (
        response.status().is_success(),
        rate_limit_ok,
        json!({
            "x-ratelimit-remaining": remaining,
            "x-ratelimit-reset": reset
        }),
    )
}

fn private_paths_ok(paths: &crate::paths::ProfilePaths) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let dirs = [
            &paths.profile_dir,
            &paths.cache_dir,
            &paths.log_dir,
            &paths.index_active,
        ];
        for dir in dirs.into_iter().filter(|path| path.exists()) {
            let Ok(metadata) = std::fs::metadata(dir) else {
                return false;
            };
            if metadata.permissions().mode() & 0o077 != 0 {
                return false;
            }
        }
        if paths.db_path.exists() {
            let Ok(metadata) = std::fs::metadata(&paths.db_path) else {
                return false;
            };
            if metadata.permissions().mode() & 0o077 != 0 {
                return false;
            }
        }
    }
    true
}

fn issue_source(issue: StoredIssue) -> Value {
    json!({
        "source_id": issue.source_id,
        "entity_type": "issue",
        "repo": issue.repo,
        "issue_number": issue.number,
        "title": issue.title,
        "body": issue.body,
        "canonical_url": issue.canonical_url,
        "source_version": issue.source_version
    })
}

fn comment_source(comment: StoredComment) -> Value {
    json!({
        "source_id": comment.source_id,
        "entity_type": "issue_comment",
        "repo": comment.repo,
        "issue_number": comment.issue_number,
        "author": comment.author,
        "body": comment.body,
        "canonical_url": comment.canonical_url,
        "parent_issue": comment.parent_issue,
        "source_version": comment.source_version
    })
}

fn snippet(body: &str) -> String {
    const MAX: usize = 180;
    if body.len() <= MAX {
        return body.to_string();
    }
    let mut end = MAX;
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &body[..end])
}
