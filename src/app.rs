use crate::cli::Cli;
use crate::commands;
use crate::error::QghError;
use crate::output::{print_error, print_success};
use clap::Parser;

pub async fn run_from_env() -> i32 {
    let cli = Cli::parse();
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

async fn run(cli: Cli) -> Result<serde_json::Value, QghError> {
    let Some(profile_id) = cli.profile.clone() else {
        return Err(QghError::missing_profile());
    };

    match cli.command {
        crate::cli::Command::Sync { .. } => commands::sync(&profile_id).await,
        crate::cli::Command::Query { query, limit, .. }
        | crate::cli::Command::Search { query, limit, .. } => {
            commands::query(&profile_id, &query, limit)
        }
        crate::cli::Command::Get { source_id, .. } => commands::get(&profile_id, &source_id),
        crate::cli::Command::Status { .. } => commands::status(&profile_id),
        crate::cli::Command::Doctor { .. } => commands::doctor(&profile_id),
    }
}
