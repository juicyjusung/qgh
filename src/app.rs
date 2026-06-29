use crate::cli::Cli;
use crate::commands;
use crate::error::QghError;
use crate::mcp;
use crate::output::{print_error, print_success};
use crate::resolution::{
    repo_scope_from_cli_arg, repo_scope_from_policy, resolve_context, ResolvedCommandContext,
    ResolvedRepoScope,
};
use clap::error::ErrorKind;
use clap::Parser;
use serde_json::{json, Value};

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
    match mcp::run_stdio(cli.profile).await {
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
    resolve_context(cli.profile.as_deref(), repo_scope)
}

fn effective_repo_scope_for_command(
    command: &crate::cli::Command,
) -> Result<Option<ResolvedRepoScope>, QghError> {
    match command {
        crate::cli::Command::Query(args) | crate::cli::Command::Search(args) => {
            if let Some(repo) = &args.repo {
                return repo_scope_from_cli_arg(repo).map(Some);
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
