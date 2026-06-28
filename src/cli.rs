use clap::{Args, Parser, Subcommand};

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
            Command::Sync { json } | Command::Status { json } | Command::Doctor { json } => *json,
            Command::Query(args) | Command::Search(args) => args.json,
            Command::Get { json, .. } => *json,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum Command {
    Sync {
        #[arg(long)]
        json: bool,
    },
    Query(QueryArgs),
    Search(QueryArgs),
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

#[derive(Debug, Clone, Args)]
pub struct QueryArgs {
    pub query: String,
    #[arg(long, default_value_t = 10)]
    pub limit: usize,
    #[arg(long)]
    pub repo: Option<String>,
    #[arg(long)]
    pub label: Vec<String>,
    #[arg(long)]
    pub state: Option<String>,
    #[arg(long)]
    pub author: Option<String>,
    #[arg(long)]
    pub issue: Option<i64>,
    #[arg(long)]
    pub wiki: Option<String>,
    #[arg(long)]
    pub json: bool,
}
