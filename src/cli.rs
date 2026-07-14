use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "qgh",
    version,
    about = "Local GitHub Issues retrieval with human output by default; use --json for qgh.v1 envelopes",
    after_help = "WORKFLOW:\n  qgh init                         Configure a repository and profile\n  qgh sync                         Refresh the explicit GitHub scope\n  qgh schedule run <profile>...    Run one bounded multi-profile freshness pass\n  qgh query \"<terms>\"              Find source candidates\n  qgh get <source_id>              Open the full source before you cite it\n  qgh status                       Inspect local readiness without network access\n\nUse query -> get -> cite. Add --json to a command for stable qgh.v1 agent output."
)]
pub struct Cli {
    #[arg(
        long,
        global = true,
        help = "Select a configured profile; repo context is used when omitted"
    )]
    pub profile: Option<String>,

    #[command(subcommand)]
    pub command: Command,
}

impl Cli {
    pub fn wants_json(&self) -> bool {
        match &self.command {
            Command::Sync(args) => args.wants_json(),
            Command::Embed(args) => args.json,
            Command::Model(args) => args.wants_json(),
            Command::Init(args) => args.wants_json(),
            Command::Status(args) => args.json,
            Command::Schedule(args) => args.wants_json(),
            Command::Doctor { json } => *json,
            Command::Query(args) | Command::Search(args) => args.json,
            Command::Get { json, .. } => *json,
            Command::Mcp => false,
        }
    }

    pub fn wants_quiet(&self) -> bool {
        match &self.command {
            Command::Sync(args) => args.quiet(),
            Command::Embed(args) => args.quiet,
            Command::Schedule(args) => args.quiet(),
            _ => false,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum Command {
    #[command(
        about = "Sync GitHub Issues/comments and refresh local search",
        long_about = "Sync GitHub Issues/comments, refresh BM25, and update configured incremental embeddings. This command contacts GitHub. Confirmed deletions or permission loss, and repos explicitly removed from a profile allowlist, may purge qgh-managed local data; transient failures do not. Use --backfill for one explicit historical pass."
    )]
    Sync(SyncArgs),
    #[command(
        about = "Rebuild all local vector embeddings",
        long_about = "Run an advanced full rebuild of local vector embeddings. Normal sync updates embeddings incrementally; use this command only to repair or intentionally recompute every vector."
    )]
    Embed(EmbedArgs),
    #[command(
        about = "Install a verified local embedding or reranker model",
        long_about = "Install into qgh's global local model store. Model weights are shared across profiles, so --profile is not valid for this command."
    )]
    Model(ModelArgs),
    #[command(about = "Create or update qgh profile and repository configuration")]
    Init(InitArgs),
    #[command(about = "Search the local snapshot for source candidates")]
    Query(QueryArgs),
    #[command(about = "Alias for query: search the local snapshot for source candidates")]
    Search(QueryArgs),
    #[command(about = "Open authoritative local sources before citing them")]
    Get {
        #[arg(required = true, num_args = 1.., help = "One to 20 qgh source_id values")]
        source_ids: Vec<String>,
        #[arg(
            long,
            help = "Use the profile_id emitted in query get_args for a stable round trip"
        )]
        profile_id: Option<String>,
        #[arg(
            long,
            help = "Opt in to a lifecycle check that contacts GitHub and purges confirmed unavailable local content"
        )]
        verify_lifecycle: bool,
        #[arg(long, help = "Emit a qgh.v1 JSON envelope instead of a human summary")]
        json: bool,
    },
    #[command(about = "Inspect local search readiness without network access")]
    Status(StatusArgs),
    #[command(about = "Run bounded explicit-profile sync passes or manage user scheduling")]
    Schedule(ScheduleArgs),
    #[command(
        about = "Probe GitHub connectivity and local model health",
        long_about = "Run explicit diagnostics. This command contacts GitHub and loads the configured local model runtime; use status for a local-only readiness check."
    )]
    Doctor {
        #[arg(long, help = "Emit a qgh.v1 JSON envelope instead of a human summary")]
        json: bool,
    },
    #[command(about = "Serve the read-only query/get/status MCP tools over stdio")]
    Mcp,
}

#[derive(Debug, Clone, Args)]
pub struct ModelArgs {
    #[command(subcommand)]
    pub command: ModelCommand,
}

impl ModelArgs {
    pub fn wants_json(&self) -> bool {
        match &self.command {
            ModelCommand::Install(args) => args.json,
        }
    }
}

#[derive(Debug, Clone, Subcommand)]
pub enum ModelCommand {
    #[command(
        about = "Download and verify one supported local model",
        long_about = "Download and verify one supported local model into qgh's global model store. This explicit acquisition contacts Hugging Face; repository content is never sent."
    )]
    Install(ModelInstallArgs),
}

#[derive(Debug, Clone, Args)]
pub struct ModelInstallArgs {
    #[arg(value_enum, help = "Supported embedding or reranker preset to install")]
    pub model: ModelPresetArg,
    #[arg(long, help = "Emit a qgh.v1 JSON envelope instead of a human summary")]
    pub json: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ModelPresetArg {
    #[value(name = "qwen3-embedding-0.6b")]
    Qwen3Embedding06b,
    #[value(name = "qwen3-reranker-0.6b")]
    Qwen3Reranker06b,
}

impl ModelPresetArg {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Qwen3Embedding06b => "qwen3-embedding-0.6b",
            Self::Qwen3Reranker06b => "qwen3-reranker-0.6b",
        }
    }
}

#[derive(Debug, Clone, Args)]
pub struct EmbedArgs {
    #[arg(
        long,
        help = "Run the advanced full rebuild and recompute every stored vector for the active fingerprint"
    )]
    pub force: bool,
    #[arg(
        long,
        help = "Hide progress on stderr and keep the final human summary plain"
    )]
    pub quiet: bool,
    #[arg(long, help = "Emit a qgh.v1 JSON envelope instead of a human summary")]
    pub json: bool,
}

#[derive(Debug, Clone, Args)]
pub struct SyncArgs {
    #[arg(
        long,
        help = "After sync, contact GitHub to verify lifecycle state (full or recent); confirmed unavailable sources may be purged locally"
    )]
    pub reconcile: Option<ReconcileMode>,
    #[arg(long, help = "Window for --reconcile recent (e.g. 7d); default 7d")]
    pub window: Option<String>,
    #[arg(
        long,
        help = "Sync one explicit owner/repo regardless of current worktree"
    )]
    pub repo: Option<String>,
    #[arg(
        long,
        help = "Sync every repo in the selected profile instead of the effective repo scope"
    )]
    pub all: bool,
    #[arg(
        long,
        help = "Run one budgeted historical pass instead of live sync; repeat until coverage is complete"
    )]
    pub backfill: bool,
    #[arg(long, help = "Max issue pages to fetch this backfill run")]
    pub max_requests: Option<usize>,
    #[arg(
        long,
        help = "Max wall-clock duration for this backfill run (e.g. 90s)"
    )]
    pub max_duration: Option<String>,
    #[arg(long, help = "Only sync when the local snapshot is older than max-age")]
    pub if_stale: bool,
    #[arg(
        long,
        help = "Snapshot staleness threshold (e.g. 30m); overrides [sync].max_age"
    )]
    pub max_age: Option<String>,
    #[arg(
        long,
        help = "Hide progress on stderr and keep the final human summary plain"
    )]
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
    #[command(
        about = "Refresh one issue and its comments",
        long_about = "Refresh one issue and its comments by fetching the complete comment list, then apply only confirmed lifecycle changes. This command contacts GitHub and may purge confirmed unavailable local content."
    )]
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
    #[arg(help = "Positive GitHub issue number to refresh")]
    pub number: i64,
    #[arg(long, help = "Target owner/repo; required when scope is ambiguous")]
    pub repo: Option<String>,
    #[arg(
        long,
        help = "Hide progress on stderr and keep the final human summary plain"
    )]
    pub quiet: bool,
    #[arg(long, help = "Emit a qgh.v1 JSON envelope instead of a human summary")]
    pub json: bool,
}

#[derive(Debug, Clone, Args)]
pub struct InitArgs {
    #[command(subcommand)]
    pub target: Option<InitTarget>,
    #[arg(
        long,
        help = "Explicit owner/repo; inferred from git origin when omitted"
    )]
    pub repo: Option<String>,
    #[arg(
        short = 'y',
        long,
        help = "Accept inferred defaults non-interactively; required values must be available"
    )]
    pub yes: bool,
    #[arg(long, help = "GitHub host, e.g. github.com or a GHES hostname")]
    pub host: Option<String>,
    #[arg(long, help = "GitHub REST API base URL for the selected host")]
    pub api_base_url: Option<String>,
    #[arg(long, help = "GitHub web base URL used for canonical source links")]
    pub web_base_url: Option<String>,
    #[arg(
        long,
        help = "Store a github_cli or env token source reference; never a literal token"
    )]
    pub token_source: Option<InitTokenSourceArg>,
    #[arg(
        long,
        help = "Token environment variable name when --token-source env is selected"
    )]
    pub token_env: Option<String>,
    #[arg(
        long,
        help = "Overwrite an existing .qgh.toml repository policy after explicit confirmation"
    )]
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
    #[command(
        about = "Create repository policy only",
        long_about = "Create repository policy only at the current git worktree root. This does not create a profile or store credentials."
    )]
    Repo(InitRepoArgs),
}

#[derive(Debug, Clone, Args)]
pub struct InitRepoArgs {
    #[arg(
        long,
        help = "Explicit owner/repo; inferred from git origin when omitted"
    )]
    pub repo: Option<String>,
    #[arg(long, help = "Overwrite an existing .qgh.toml repository policy")]
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
    #[arg(help = "Search terms, issue URL, comment URL, or exact identifier")]
    pub query: String,
    #[arg(
        long,
        help = "Rerank the top fused candidates with a configured local model"
    )]
    pub rerank: bool,
    #[arg(long, help = "Maximum number of source candidates to return")]
    pub limit: Option<usize>,
    #[arg(long, help = "Restrict results to owner/repo")]
    pub repo: Option<String>,
    #[arg(long, help = "Require a label; repeat to require multiple labels")]
    pub label: Vec<String>,
    #[arg(long, help = "Restrict results to open or closed issues")]
    pub state: Option<String>,
    #[arg(long, help = "Restrict results to a GitHub author login")]
    pub author: Option<String>,
    #[arg(
        long,
        help = "Resolve an exact issue number within the effective repo scope"
    )]
    pub issue: Option<i64>,
    #[arg(long, hide = true)]
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

#[derive(Debug, Clone, Args)]
pub struct ScheduleArgs {
    #[command(subcommand)]
    pub command: ScheduleCommand,
}

impl ScheduleArgs {
    pub fn wants_json(&self) -> bool {
        match &self.command {
            ScheduleCommand::Run(args) => args.json,
            ScheduleCommand::Start(args) => args.json,
            ScheduleCommand::Status(args) => args.json,
            ScheduleCommand::Stop(args) => args.json,
        }
    }

    pub fn quiet(&self) -> bool {
        matches!(&self.command, ScheduleCommand::Run(args) if args.quiet)
    }
}

#[derive(Debug, Clone, Subcommand)]
pub enum ScheduleCommand {
    #[command(
        about = "Run one bounded foreground freshness pass",
        long_about = "Plan an explicit profile list locally, then run bounded sequential freshness syncs. This command may contact configured GitHub hosts. It never performs hidden bootstrap, backfill, reconciliation, or model work."
    )]
    Run(ScheduleRunArgs),
    #[command(
        about = "Install or update the user schedule",
        long_about = "Install one user-scoped macOS LaunchAgent or Linux systemd timer for explicit github_cli profiles. This lifecycle operation does not contact GitHub and never installs a cron fallback or stores a token."
    )]
    Start(ScheduleStartArgs),
    #[command(
        about = "Inspect the local user schedule without network access",
        long_about = "Read only local registration and artifact state. This command does not contact GitHub or the user lifecycle manager."
    )]
    Status(ScheduleStatusArgs),
    #[command(
        about = "Stop and remove the user schedule",
        long_about = "Disable and remove qgh's user-scoped LaunchAgent or systemd timer. This command does not contact GitHub and does not install a fallback scheduler."
    )]
    Stop(ScheduleStopArgs),
}

#[derive(Debug, Clone, Args)]
pub struct ScheduleRunArgs {
    #[arg(required = true, num_args = 1.., help = "Explicit profile ids to coordinate")]
    pub profile_ids: Vec<String>,
    #[arg(
        long,
        help = "Hide progress on stderr and keep the final human summary plain"
    )]
    pub quiet: bool,
    #[arg(long, help = "Emit a qgh.v1 JSON envelope instead of a human summary")]
    pub json: bool,
}

#[derive(Debug, Clone, Args)]
pub struct ScheduleStartArgs {
    #[arg(required = true, num_args = 1.., help = "Explicit profile ids to schedule")]
    pub profile_ids: Vec<String>,
    #[arg(long, default_value = "1h", help = "Fixed schedule interval")]
    pub interval: String,
    #[arg(long, help = "Emit a qgh.v1 JSON envelope instead of a human summary")]
    pub json: bool,
}

#[derive(Debug, Clone, Args)]
pub struct ScheduleStatusArgs {
    #[arg(long, help = "Emit a qgh.v1 JSON envelope instead of a human summary")]
    pub json: bool,
}

#[derive(Debug, Clone, Args)]
pub struct ScheduleStopArgs {
    #[arg(long, help = "Emit a qgh.v1 JSON envelope instead of a human summary")]
    pub json: bool,
}
