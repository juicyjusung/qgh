use crate::cli::Cli;
use crate::commands;
use crate::error::QghError;
use crate::mcp;
use crate::output::{
    print_error, print_human_success, print_human_warnings, print_success, SuccessOutputKind,
};
use crate::resolution::{
    repo_scope_from_cli_arg, repo_scope_from_worktree, resolve_context, resolve_explicit_context,
    ResolvedCommandContext, ResolvedRepoScope,
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
            if outcome.json_mode {
                print_success(outcome.data, outcome.warnings, outcome.meta);
            } else {
                print_human_success(
                    outcome.output_kind,
                    &outcome.data,
                    &outcome.warnings,
                    &outcome.meta,
                );
                if !matches!(outcome.output_kind, SuccessOutputKind::Init) {
                    print_human_warnings(&outcome.warnings);
                }
            }
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
    output_kind: SuccessOutputKind,
    json_mode: bool,
    data: Value,
    warnings: Vec<Value>,
    meta: Value,
}

async fn run(cli: Cli) -> Result<CommandOutcome, QghError> {
    let json_mode = cli.wants_json();
    if let crate::cli::Command::Init(args) = &cli.command {
        let outcome = commands::init(cli.profile.as_deref(), args)?;
        return Ok(CommandOutcome {
            output_kind: SuccessOutputKind::Init,
            json_mode,
            data: outcome.data,
            warnings: outcome.warnings,
            meta: outcome.meta,
        });
    }

    let context = resolve_command_context(&cli)?;
    let profile_id = context.profile_id.clone();
    let command = cli.command;
    let output_kind = success_output_kind(&command);
    let is_doctor = matches!(command, crate::cli::Command::Doctor { .. });

    let (mut data, warnings) = match command {
        crate::cli::Command::Sync(args) => {
            let show_progress = !args.wants_json() && !args.quiet();
            let data = match &args.target {
                Some(crate::cli::SyncTarget::Issue(issue_args)) => {
                    commands::sync_issue(
                        &profile_id,
                        issue_args.number,
                        context.repo_scope.as_ref(),
                        show_progress,
                    )
                    .await?
                }
                None => {
                    commands::sync(
                        &profile_id,
                        args.reconcile,
                        args.if_stale,
                        args.max_age.as_deref(),
                        context.repo_scope.as_ref(),
                        show_progress,
                    )
                    .await?
                }
            };
            (data, Vec::new())
        }
        crate::cli::Command::Query(args) | crate::cli::Command::Search(args) => {
            let outcome = commands::query(&profile_id, args, context.repo_scope.as_ref())?;
            (outcome.data, outcome.warnings)
        }
        crate::cli::Command::Get {
            source_ids,
            verify_lifecycle,
            ..
        } => {
            let data = commands::get_cli(
                &profile_id,
                &source_ids,
                context.repo_scope.as_ref(),
                verify_lifecycle,
            )
            .await?;
            (data, Vec::new())
        }
        crate::cli::Command::Status(args) => {
            let outcome = commands::status(&profile_id, &args)?;
            (outcome.data, outcome.warnings)
        }
        crate::cli::Command::Doctor { .. } => {
            let data = commands::doctor(&profile_id).await?;
            (data, Vec::new())
        }
        crate::cli::Command::Init(_) => unreachable!("init is handled before normal resolution"),
        crate::cli::Command::Mcp => unreachable!("MCP is handled before normal CLI output"),
    };

    if matches!(output_kind, SuccessOutputKind::Status) {
        data["resolution"] = context.resolution_json();
    }
    if is_doctor {
        enrich_doctor_data(&mut data, &context);
    }

    Ok(CommandOutcome {
        output_kind,
        json_mode,
        data,
        warnings,
        meta: context.meta_json(),
    })
}

fn success_output_kind(command: &crate::cli::Command) -> SuccessOutputKind {
    match command {
        crate::cli::Command::Sync(_) => SuccessOutputKind::Sync,
        crate::cli::Command::Query(_) | crate::cli::Command::Search(_) => SuccessOutputKind::Query,
        crate::cli::Command::Get { .. } => SuccessOutputKind::Get,
        crate::cli::Command::Status(_) => SuccessOutputKind::Status,
        crate::cli::Command::Doctor { .. } => SuccessOutputKind::Doctor,
        crate::cli::Command::Init(_) => SuccessOutputKind::Init,
        crate::cli::Command::Mcp => unreachable!("MCP is handled before normal CLI output"),
    }
}

fn resolve_command_context(cli: &Cli) -> Result<ResolvedCommandContext, QghError> {
    if let crate::cli::Command::Get {
        profile_id: Some(profile_id),
        ..
    } = &cli.command
    {
        return resolve_get_args_context(cli.profile.as_deref(), profile_id);
    }
    let repo_scope = effective_repo_scope_for_command(&cli.command)?;
    resolve_context(cli.profile.as_deref(), repo_scope)
}

fn resolve_get_args_context(
    cli_profile_arg: Option<&str>,
    get_args_profile_id: &str,
) -> Result<ResolvedCommandContext, QghError> {
    if let Some(cli_profile_id) = cli_profile_arg {
        reject_cli_profile_mismatch(cli_profile_id, get_args_profile_id, "--profile")?;
        return resolve_explicit_context(get_args_profile_id, "get_args", None);
    }
    if let Ok(env_profile_id) = std::env::var("QGH_PROFILE") {
        reject_cli_profile_mismatch(&env_profile_id, get_args_profile_id, "QGH_PROFILE")?;
    }
    resolve_explicit_context(get_args_profile_id, "get_args", None)
}

fn reject_cli_profile_mismatch(
    boundary_profile_id: &str,
    get_args_profile_id: &str,
    boundary_source: &str,
) -> Result<(), QghError> {
    if get_args_profile_id != boundary_profile_id {
        return Err(QghError::validation(
            "validation.cli",
            format!("get --profile-id cannot differ from {boundary_source}."),
        )
        .with_details(json!({
            "boundary_profile_id": boundary_profile_id,
            "get_args_profile_id": get_args_profile_id
        }))
        .with_hint("Use the profile_id emitted by the query result without a conflicting profile override."));
    }
    Ok(())
}

fn effective_repo_scope_for_command(
    command: &crate::cli::Command,
) -> Result<Option<ResolvedRepoScope>, QghError> {
    match command {
        crate::cli::Command::Query(args) | crate::cli::Command::Search(args) => {
            if let Some(repo) = &args.repo {
                return repo_scope_from_cli_arg(repo).map(Some);
            }
            repo_scope_from_worktree()
        }
        crate::cli::Command::Get {
            profile_id: Some(_),
            ..
        } => Ok(None),
        crate::cli::Command::Get {
            profile_id: None, ..
        }
        | crate::cli::Command::Status(_)
        | crate::cli::Command::Doctor { .. } => repo_scope_from_worktree(),
        crate::cli::Command::Sync(args) => {
            if let Some(crate::cli::SyncTarget::Issue(issue_args)) = &args.target {
                if let Some(repo) = &issue_args.repo {
                    return repo_scope_from_cli_arg(repo).map(Some);
                }
                return repo_scope_from_worktree();
            }
            if args.all {
                Ok(None)
            } else {
                repo_scope_from_worktree()
            }
        }
        crate::cli::Command::Init(_) => unreachable!("init is handled before normal resolution"),
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
