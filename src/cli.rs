use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "qgh", version, about = "Local GitHub Issues retrieval")]
pub struct Cli {
    #[arg(long, global = true)]
    pub profile: Option<String>,

    #[command(subcommand)]
    pub command: Command,
}

impl Cli {
    pub fn wants_json(&self) -> bool {
        match &self.command {
            Command::Sync { json }
            | Command::Query { json, .. }
            | Command::Search { json, .. }
            | Command::Get { json, .. }
            | Command::Status { json }
            | Command::Doctor { json } => *json,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum Command {
    Sync {
        #[arg(long)]
        json: bool,
    },
    Query {
        query: String,
        #[arg(long, default_value_t = 10)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    Search {
        query: String,
        #[arg(long, default_value_t = 10)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    Get {
        source_id: String,
        #[arg(long)]
        json: bool,
    },
    Status {
        #[arg(long)]
        json: bool,
    },
    Doctor {
        #[arg(long)]
        json: bool,
    },
}
