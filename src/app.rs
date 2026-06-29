use crate::cli::Cli;
use crate::commands;
use crate::config::{discover_repo_policy, single_matching_profile_id};
use crate::error::QghError;
use crate::mcp;
use crate::output::{print_error, print_success};
use clap::error::ErrorKind;
use clap::Parser;
use serde_json::{json, Value};
use std::path::PathBuf;

pub async fn run_from_env() -> i32 {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            print!("{error}");
            return 0;
        }
        Err(error) => {
            let qgh_error = QghError::validation("validation.cli", error.to_string());
            print_error(&qgh_error, std::env::args().any(|arg| arg == "--json"));
            return qgh_error.exit_code;
        }
    };
    if matches!(cli.command, crate::cli::Command::Mcp) {
        return run_mcp(cli).await;
    }
    let wants_json = cli.wants_json();
    match run(cli).await {
        Ok(outcome) => {
            print_success(outcome.data, outcome.meta);
            0
        }
        Err(error) => {
            let exit_code = error.exit_code;
            print_error(&error, wants_json);
            exit_code
        }
    }
}

async fn run_mcp(cli: Cli) -> i32 {
    let Some(profile_id) = cli.profile else {
        let error = QghError::missing_profile();
        print_error(&error, false);
        return error.exit_code;
    };
    match mcp::run_stdio(&profile_id).await {
        Ok(()) => 0,
        Err(error) => {
            let exit_code = error.exit_code;
            print_error(&error, false);
            exit_code
        }
    }
}

struct CommandOutcome {
    data: Value,
    meta: Value,
}

#[derive(Debug, Clone)]
struct ResolvedCommandContext {
    profile_id: String,
    profile_source: &'static str,
    repo_scope: Option<ResolvedRepoScope>,
    allowlist_match_count: Option<usize>,
}

#[derive(Debug, Clone)]
struct ResolvedRepoScope {
    repo: String,
    source: &'static str,
    repo_policy_path: Option<PathBuf>,
}

async fn run(cli: Cli) -> Result<CommandOutcome, QghError> {
    let context = resolve_command_context(&cli)?;
    let profile_id = context.profile_id.clone();
    let command = cli.command;
    let is_status = matches!(command, crate::cli::Command::Status { .. });
    let is_doctor = matches!(command, crate::cli::Command::Doctor { .. });

    let mut data = match command {
        crate::cli::Command::Sync(args) => commands::sync(&profile_id, args.reconcile).await,
        crate::cli::Command::Query(args) | crate::cli::Command::Search(args) => {
            commands::query(&profile_id, args)
        }
        crate::cli::Command::Get { source_id, .. } => commands::get(&profile_id, &source_id).await,
        crate::cli::Command::Status { .. } => commands::status(&profile_id),
        crate::cli::Command::Doctor { .. } => commands::doctor(&profile_id).await,
        crate::cli::Command::Mcp => unreachable!("MCP is handled before normal CLI output"),
    }?;

    if is_status {
        data["resolution"] = context.resolution_json();
    }
    if is_doctor {
        enrich_doctor_data(&mut data, &context);
    }

    Ok(CommandOutcome {
        data,
        meta: context.meta_json(),
    })
}

fn resolve_command_context(cli: &Cli) -> Result<ResolvedCommandContext, QghError> {
    let repo_scope = effective_repo_scope_for_command(&cli.command)?;
    if let Some(profile_id) = &cli.profile {
        return Ok(ResolvedCommandContext {
            profile_id: profile_id.clone(),
            profile_source: "cli",
            repo_scope,
            allowlist_match_count: None,
        });
    }
    if let Ok(profile_id) = std::env::var("QGH_PROFILE") {
        return Ok(ResolvedCommandContext {
            profile_id,
            profile_source: "env",
            repo_scope,
            allowlist_match_count: None,
        });
    }
    let profile_id =
        single_matching_profile_id(repo_scope.as_ref().map(|scope| scope.repo.as_str()))?;
    Ok(ResolvedCommandContext {
        profile_id,
        profile_source: "single_match",
        repo_scope,
        allowlist_match_count: Some(1),
    })
}

fn effective_repo_scope_for_command(
    command: &crate::cli::Command,
) -> Result<Option<ResolvedRepoScope>, QghError> {
    match command {
        crate::cli::Command::Query(args) | crate::cli::Command::Search(args) => {
            if let Some(repo) = &args.repo {
                validate_repo_scope(repo)?;
                return Ok(Some(ResolvedRepoScope {
                    repo: repo.clone(),
                    source: "cli",
                    repo_policy_path: None,
                }));
            }
            repo_scope_from_policy()
        }
        crate::cli::Command::Get { .. }
        | crate::cli::Command::Status { .. }
        | crate::cli::Command::Doctor { .. } => repo_scope_from_policy(),
        crate::cli::Command::Sync(_) => Ok(None),
        crate::cli::Command::Mcp => unreachable!("MCP is handled before normal CLI output"),
    }
}

fn repo_scope_from_policy() -> Result<Option<ResolvedRepoScope>, QghError> {
    Ok(discover_repo_policy()?.map(|policy| ResolvedRepoScope {
        repo: policy.repo.full_name(),
        source: "repo_policy",
        repo_policy_path: Some(policy.path),
    }))
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
    fn meta_json(&self) -> Value {
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

    fn resolution_json(&self) -> Value {
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

fn enrich_doctor_data(data: &mut Value, context: &ResolvedCommandContext) {
    if let Some(checks) = data.get_mut("checks").and_then(Value::as_array_mut) {
        checks.push(json!({
            "name": "repo_policy",
            "ok": true,
            "path": context.repo_scope
                .as_ref()
                .and_then(|scope| scope.repo_policy_path.as_ref())
                .map(|path| path.to_string_lossy().to_string()),
            "repo": context.repo_scope.as_ref().map(|scope| scope.repo.clone())
        }));
        checks.push(json!({
            "name": "profile_resolution",
            "ok": true,
            "profile_id": context.profile_id,
            "profile_source": context.profile_source,
            "allowlist_match_count": context.allowlist_match_count
        }));
    }
    data["resolution"] = context.resolution_json();
}
