use crate::cli::Cli;
use crate::commands;
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
    let Some(profile_id) = cli.profile.clone() else {
        return Err(QghError::missing_profile());
    };

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
