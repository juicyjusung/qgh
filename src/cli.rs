use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "qgh",
    version,
    about = "Local GitHub Issues retrieval with human output by default; use --json for qgh.v1 envelopes"
)]
pub struct Cli {
    #[arg(long, global = true)]
    pub profile: Option<String>,

    #[command(subcommand)]
    pub command: Command,
}

impl Cli {
    pub fn wants_json(&self) -> bool {
        match &self.command {
            Command::Sync(args) => args.wants_json(),
            Command::Init(args) => args.wants_json(),
            Command::Status(args) => args.json,
            Command::Doctor { json } => *json,
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
        #[arg(required = true, num_args = 1.., help = "One to 20 qgh source_id values")]
        source_ids: Vec<String>,
        #[arg(long)]
        profile_id: Option<String>,
        #[arg(
            long,
            help = "Opt in to a GitHub lifecycle check for active/transferred/unavailable state"
        )]
        verify_lifecycle: bool,
        #[arg(long, help = "Emit a qgh.v1 JSON envelope instead of a human summary")]
        json: bool,
    },
    Status(StatusArgs),
    Doctor {
        #[arg(long, help = "Emit a qgh.v1 JSON envelope instead of a human summary")]
        json: bool,
    },
    Mcp,
}

#[derive(Debug, Clone, Args)]
pub struct SyncArgs {
    #[arg(long)]
    pub reconcile: Option<ReconcileMode>,
    #[arg(
        long,
        help = "Window for --reconcile recent (e.g. 7d); default [profile].reconcile_after"
    )]
    pub window: Option<String>,
    #[arg(long)]
    pub all: bool,
    #[arg(long, help = "Only sync when the local snapshot is older than max-age")]
    pub if_stale: bool,
    #[arg(
        long,
        help = "Snapshot staleness threshold (e.g. 30m); overrides [sync].max_age"
    )]
    pub max_age: Option<String>,
    #[arg(long)]
    pub quiet: bool,
    #[arg(long, help = "Emit a qgh.v1 JSON envelope instead of a human summary")]
    pub json: bool,
    #[command(subcommand)]
    pub target: Option<SyncTarget>,
}

impl SyncArgs {
    pub fn wants_json(&self) -> bool {
        self.json
            || self
                .target
                .as_ref()
                .is_some_and(|target| target.wants_json())
    }

    pub fn quiet(&self) -> bool {
        self.quiet || self.target.as_ref().is_some_and(|target| target.quiet())
    }
}

#[derive(Debug, Clone, Subcommand)]
pub enum SyncTarget {
    Issue(SyncIssueArgs),
}

impl SyncTarget {
    fn wants_json(&self) -> bool {
        match self {
            SyncTarget::Issue(args) => args.json,
        }
    }

    fn quiet(&self) -> bool {
        match self {
            SyncTarget::Issue(args) => args.quiet,
        }
    }
}

#[derive(Debug, Clone, Args)]
pub struct SyncIssueArgs {
    pub number: i64,
    #[arg(long)]
    pub repo: Option<String>,
    #[arg(long)]
    pub quiet: bool,
    #[arg(long, help = "Emit a qgh.v1 JSON envelope instead of a human summary")]
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
    #[arg(long, help = "Emit a qgh.v1 JSON envelope instead of a human summary")]
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
        self.json || self.repo_args().json
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
    #[arg(long, help = "Emit a qgh.v1 JSON envelope instead of a human summary")]
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
    Recent,
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
    #[arg(
        long,
        value_name = "DURATION",
        help = "Override query snapshot max age for this run, e.g. 90s, 30m, 7d, 12mo"
    )]
    pub max_age: Option<String>,
    #[arg(long, help = "Fail this run if the local snapshot is stale")]
    pub require_fresh: bool,
    #[arg(long, help = "Emit a qgh.v1 JSON envelope instead of a human summary")]
    pub json: bool,
}

#[derive(Debug, Clone, Args)]
pub struct StatusArgs {
    #[arg(
        long,
        value_name = "DURATION",
        help = "Override status snapshot max age for this run, e.g. 90s, 30m, 7d, 12mo"
    )]
    pub max_age: Option<String>,
    #[arg(long, help = "Fail this run if the local snapshot is stale")]
    pub require_fresh: bool,
    #[arg(long, help = "Emit a qgh.v1 JSON envelope instead of a human summary")]
    pub json: bool,
}
