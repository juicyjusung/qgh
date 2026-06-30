use clap::{Args, Parser, Subcommand, ValueEnum};

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
            Command::Sync(args) => args.json,
            Command::Init(args) => args.wants_json(),
            Command::Status { json } | Command::Doctor { json } => *json,
            Command::Query(args) | Command::Search(args) => args.json,
            Command::Get { json, .. } => *json,
            Command::Mcp => false,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum Command {
    Sync(SyncArgs),
    Init(InitArgs),
    Query(QueryArgs),
    Search(QueryArgs),
    Get {
        source_id: String,
        #[arg(long)]
        profile_id: Option<String>,
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
    Mcp,
}

#[derive(Debug, Clone, Args)]
pub struct SyncArgs {
    #[arg(long)]
    pub reconcile: Option<ReconcileMode>,
    #[arg(long)]
    pub all: bool,
    #[arg(long)]
    pub quiet: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Args)]
pub struct InitArgs {
    #[command(subcommand)]
    pub target: Option<InitTarget>,
    #[arg(long)]
    pub repo: Option<String>,
    #[arg(short = 'y', long)]
    pub yes: bool,
    #[arg(long)]
    pub host: Option<String>,
    #[arg(long)]
    pub api_base_url: Option<String>,
    #[arg(long)]
    pub web_base_url: Option<String>,
    #[arg(long)]
    pub token_source: Option<InitTokenSourceArg>,
    #[arg(long)]
    pub token_env: Option<String>,
    #[arg(long)]
    pub force: bool,
    #[arg(long)]
    pub json: bool,
}

impl InitArgs {
    pub fn repo_args(&self) -> InitRepoArgs {
        match &self.target {
            Some(InitTarget::Repo(args)) => args.clone(),
            None => InitRepoArgs {
                repo: self.repo.clone(),
                force: self.force,
                json: self.json,
            },
        }
    }

    fn wants_json(&self) -> bool {
        self.repo_args().json
    }
}

#[derive(Debug, Clone, Subcommand)]
pub enum InitTarget {
    Repo(InitRepoArgs),
}

#[derive(Debug, Clone, Args)]
pub struct InitRepoArgs {
    #[arg(long)]
    pub repo: Option<String>,
    #[arg(long)]
    pub force: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "snake_case")]
pub enum InitTokenSourceArg {
    GithubCli,
    Env,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ReconcileMode {
    Full,
}

#[derive(Debug, Clone, Args)]
pub struct QueryArgs {
    pub query: String,
    #[arg(long)]
    pub limit: Option<usize>,
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
