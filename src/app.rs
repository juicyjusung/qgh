use crate::cli::Cli;
use crate::commands;
use crate::config::{discover_repo_policy, single_matching_profile_id};
use crate::error::QghError;
use crate::mcp;
use crate::output::{print_error, print_success};
use clap::error::ErrorKind;
use clap::Parser;

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
        Ok(data) => {
            print_success(data);
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

async fn run(cli: Cli) -> Result<serde_json::Value, QghError> {
    let profile_id = resolve_profile_id(&cli)?;

    match cli.command {
        crate::cli::Command::Sync(args) => commands::sync(&profile_id, args.reconcile).await,
        crate::cli::Command::Query(args) | crate::cli::Command::Search(args) => {
            commands::query(&profile_id, args)
        }
        crate::cli::Command::Get { source_id, .. } => commands::get(&profile_id, &source_id).await,
        crate::cli::Command::Status { .. } => commands::status(&profile_id),
        crate::cli::Command::Doctor { .. } => commands::doctor(&profile_id).await,
        crate::cli::Command::Mcp => unreachable!("MCP is handled before normal CLI output"),
    }
}

fn resolve_profile_id(cli: &Cli) -> Result<String, QghError> {
    if let Some(profile_id) = &cli.profile {
        return Ok(profile_id.clone());
    }
    if let Ok(profile_id) = std::env::var("QGH_PROFILE") {
        return Ok(profile_id);
    }
    let repo_scope = effective_repo_scope_for_command(&cli.command)?;
    single_matching_profile_id(repo_scope.as_deref())
}

fn effective_repo_scope_for_command(
    command: &crate::cli::Command,
) -> Result<Option<String>, QghError> {
    match command {
        crate::cli::Command::Query(args) | crate::cli::Command::Search(args) => {
            if let Some(repo) = &args.repo {
                validate_repo_scope(repo)?;
                return Ok(Some(repo.clone()));
            }
            repo_scope_from_policy()
        }
        crate::cli::Command::Get { .. } | crate::cli::Command::Status { .. } => {
            repo_scope_from_policy()
        }
        crate::cli::Command::Sync(_) | crate::cli::Command::Doctor { .. } => Ok(None),
        crate::cli::Command::Mcp => unreachable!("MCP is handled before normal CLI output"),
    }
}

fn repo_scope_from_policy() -> Result<Option<String>, QghError> {
    Ok(discover_repo_policy()?.map(|policy| policy.repo.full_name()))
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
