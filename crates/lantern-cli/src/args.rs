//! Command-line grammar, independent of command execution.
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;

/// Top-level Lantern arguments.
#[derive(Debug, Parser)]
#[command(name = "lantern", version, about = "Manage local development servers")]
pub struct Cli {
    /// Emit machine-readable JSON.
    #[arg(long, global = true)]
    pub json: bool,
    /// Daemon URL; overrides LANTERN_DAEMON_URL.
    #[arg(long, global = true, env = "LANTERN_DAEMON_URL")]
    pub host: Option<String>,
    /// Disable terminal colors.
    #[arg(long, global = true)]
    pub no_color: bool,
    /// Start the daemon if it is unavailable.
    #[arg(long, global = true)]
    pub start_daemon: bool,
    /// Increase diagnostic verbosity.
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,
    /// Operation to perform; defaults to ls.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Complete command tree.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// List instances and unmanaged listeners.
    #[command(alias = "list")]
    Ls(LsArgs),
    /// Show global or instance status.
    #[command(alias = "st")]
    Status { target: Option<String> },
    /// Start existing instances.
    #[command(aliases = ["start", "up"])]
    On(OnArgs),
    /// Stop instances.
    #[command(aliases = ["stop", "down"])]
    Off(OffArgs),
    /// Restart existing instances.
    Restart(TargetsArgs),
    /// Create or reuse an instance for a branch.
    Run(RunArgs),
    /// Re-run checkout preparation.
    Setup(SetupArgs),
    /// Read or follow instance logs.
    #[command(alias = "log")]
    Logs(LogsArgs),
    /// Open an instance, editor or terminal.
    Open(OpenArgs),
    /// Manage branch checkouts.
    #[command(alias = "branch")]
    Wt {
        #[command(subcommand)]
        command: WorktreeCommand,
    },
    /// Route an externally started server.
    Adopt {
        target: String,
        #[arg(long)]
        port: Option<u16>,
    },
    /// Stop routing an externally started server.
    Unadopt { target: String },
    /// Scan workspace paths for projects.
    Scan { paths: Vec<PathBuf> },
    /// Register a project directory.
    Add {
        path: Option<PathBuf>,
        #[arg(long)]
        write_manifest: bool,
    },
    /// Manage registered projects.
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
    },
    /// Inspect and validate manifests.
    Manifest {
        #[command(subcommand)]
        command: ManifestCommand,
    },
    /// Print an offline JSON schema.
    Schema {
        #[arg(value_enum, default_value = "manifest")]
        name: SchemaName,
    },
    /// Manage shared services.
    #[command(alias = "svc", alias = "mailpit")]
    Services {
        #[command(subcommand)]
        command: Option<ServiceCommand>,
    },
    /// Inspect system configuration.
    #[command(alias = "health")]
    Doctor {
        #[arg(long)]
        fix: bool,
    },
    /// Initialize system integration.
    Init {
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Remove system integration.
    Uninstall {
        #[arg(long)]
        purge_data: bool,
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Import a v1 installation.
    MigrateV1 {
        #[arg(long)]
        from: Option<PathBuf>,
        #[arg(long)]
        dry_run: bool,
        #[arg(short = 'y', long)]
        yes: bool,
        #[arg(long)]
        force: bool,
    },
    /// Run or control the daemon.
    Daemon {
        #[command(subcommand)]
        command: Option<DaemonCommand>,
    },
    /// Run the GUI or send it an activation request.
    Gui(GuiArgs),
    /// MCP setup and URL.
    Mcp {
        #[command(subcommand)]
        command: McpCommand,
    },
    /// Inspect and change daemon settings.
    Config {
        #[command(subcommand)]
        command: Option<ConfigCommand>,
    },
    /// Show or refresh the daemon's shell environment.
    Env {
        #[command(subcommand)]
        command: EnvCommand,
    },
    /// Stream daemon events as NDJSON.
    Events {
        #[arg(long)]
        logs: Option<String>,
    },
    /// Generate shell completions.
    Completions { shell: Shell },
}

/// Instance-list filters.
#[derive(Debug, Default, Args)]
pub struct LsArgs {
    pub project: Option<String>,
    #[arg(short, long)]
    pub running: bool,
    #[arg(short, long)]
    pub all: bool,
    #[arg(long)]
    pub no_listeners: bool,
}
/// Shared operation wait options.
#[derive(Debug, Clone, Args)]
pub struct WaitArgs {
    #[arg(long, conflicts_with = "no_wait")]
    pub wait: bool,
    #[arg(long, conflicts_with = "wait")]
    pub no_wait: bool,
    #[arg(long, default_value_t = 120, value_parser = clap::value_parser!(u64).range(1..=600))]
    pub timeout: u64,
}
/// Arguments for starting targets.
#[derive(Debug, Args)]
pub struct OnArgs {
    #[arg(required = true)]
    pub targets: Vec<String>,
    #[arg(long)]
    pub open: bool,
    #[command(flatten)]
    pub wait: WaitArgs,
}
/// Arguments for stopping targets or all instances.
#[derive(Debug, Args)]
pub struct OffArgs {
    #[arg(required_unless_present = "all", conflicts_with = "all")]
    pub targets: Vec<String>,
    #[arg(long, conflicts_with = "targets")]
    pub all: bool,
    #[arg(short = 'y', long)]
    pub yes: bool,
    #[command(flatten)]
    pub wait: WaitArgs,
}
/// A set of targets and wait options.
#[derive(Debug, Args)]
pub struct TargetsArgs {
    #[arg(required = true)]
    pub targets: Vec<String>,
    #[command(flatten)]
    pub wait: WaitArgs,
}
/// Branch selection flags.
#[derive(Debug, Clone, Args)]
pub struct BranchOptions {
    #[arg(long, conflicts_with_all = ["detach", "pr"])]
    pub new: bool,
    #[arg(long, requires = "new")]
    pub base: Option<String>,
    #[arg(long, conflicts_with = "pr")]
    pub detach: bool,
    #[arg(long)]
    pub pr: Option<u32>,
}
/// Run a branch instance.
#[derive(Debug, Args)]
pub struct RunArgs {
    pub target: String,
    #[command(flatten)]
    pub branch: BranchOptions,
    #[arg(long)]
    pub no_start: bool,
    #[arg(long)]
    pub open: bool,
    #[command(flatten)]
    pub wait: WaitArgs,
}
/// Prepare an instance.
#[derive(Debug, Args)]
pub struct SetupArgs {
    pub target: String,
    #[arg(long)]
    pub force: bool,
    #[command(flatten)]
    pub wait: WaitArgs,
}
/// Log stream filter.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum LogStream {
    Stdout,
    Stderr,
}
/// Log options.
#[derive(Debug, Args)]
pub struct LogsArgs {
    pub target: Option<String>,
    #[arg(short, long)]
    pub follow: bool,
    #[arg(short = 'n', default_value_t = 200, value_parser = clap::value_parser!(u32).range(0..=5000))]
    pub lines: u32,
    #[arg(long, value_enum)]
    pub stream: Option<LogStream>,
    #[arg(short = 't', long)]
    pub timestamps: bool,
}
/// Options for opening an instance.
#[derive(Debug, Args)]
pub struct OpenArgs {
    pub target: Option<String>,
    #[arg(long, conflicts_with = "terminal")]
    pub editor: bool,
    #[arg(long)]
    pub terminal: bool,
    #[arg(long)]
    pub print: bool,
    #[arg(long)]
    pub no_start: bool,
}
/// Worktree subcommands.
#[derive(Debug, Subcommand)]
pub enum WorktreeCommand {
    /// List all checkouts.
    Ls { project: Option<String> },
    /// List branches.
    Branches { project: Option<String> },
    /// Create or reuse a checkout without starting it.
    Add {
        project: String,
        branch: String,
        #[command(flatten)]
        options: BranchOptions,
        #[arg(long, requires = "pr")]
        update: bool,
        #[command(flatten)]
        wait: WaitArgs,
    },
    /// Pin a checkout.
    Pin { target: String },
    /// Unpin a checkout.
    Unpin { target: String },
    /// Change a checkout's URL slug.
    Relabel { target: String, slug: String },
    /// Suggest or remove stale checkouts.
    Gc {
        project: Option<String>,
        #[arg(long)]
        apply: bool,
        #[arg(long)]
        fetch: bool,
    },
    /// Remove or forget a checkout.
    Rm {
        target: String,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        delete_branch: bool,
        #[arg(short = 'y', long)]
        yes: bool,
    },
}
/// Project commands.
#[derive(Debug, Subcommand)]
pub enum ProjectCommand {
    /// Unregister a project.
    Rm {
        id: String,
        #[arg(long)]
        keep_running: bool,
        #[arg(long)]
        remove_worktrees: bool,
    },
    /// Enable a project.
    Enable { id: String },
    /// Disable a project.
    Disable { id: String },
    /// Show a project.
    Show { id: String },
}
/// Manifest commands.
#[derive(Debug, Subcommand)]
pub enum ManifestCommand {
    /// Parse and validate locally.
    Check { path: Option<PathBuf> },
    /// Show a resolved manifest.
    Show { target: Option<String> },
}
/// Built-in schema names.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum SchemaName {
    Manifest,
    Profile,
    Config,
}
/// Shared-service commands.
#[derive(Debug, Subcommand)]
pub enum ServiceCommand {
    /// Start a service.
    Start { name: String },
    /// Stop a service.
    Stop { name: String },
    /// Restart a service.
    Restart { name: String },
    /// Open a service UI.
    Open { name: String },
}
/// Daemon process management commands.
#[derive(Debug, Clone, Subcommand)]
pub enum DaemonCommand {
    /// Run the daemon in the foreground.
    Run,
    /// Start the user unit.
    Start,
    /// Stop the user unit.
    Stop,
    /// Restart the user unit.
    Restart,
    /// Show user-unit status.
    Status,
    /// Show daemon journal output.
    Logs {
        #[arg(short, long)]
        follow: bool,
    },
    /// Enable the user unit.
    Enable,
    /// Disable the user unit.
    Disable,
}
/// GUI invocation forwarded to the binary.
#[derive(Debug, Clone, Default, Args)]
pub struct GuiArgs {
    #[arg(long)]
    pub tray: bool,
    #[arg(long)]
    pub quit: bool,
    #[arg(long)]
    pub foreground: bool,
    pub target: Option<String>,
}
/// MCP commands.
#[derive(Debug, Subcommand)]
pub enum McpCommand {
    /// Print the MCP endpoint.
    Url,
    /// Install an MCP configuration.
    Install {
        target: McpTarget,
        #[arg(long, value_enum, default_value = "user")]
        scope: McpScope,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        print: bool,
    },
}
/// Supported MCP clients.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum McpTarget {
    ClaudeCode,
    Codex,
    Cursor,
    Json,
}
/// Scope of an MCP installation.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum McpScope {
    User,
    Project,
}
/// Settings commands.
#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Read a setting.
    Get { key: String },
    /// Update a setting.
    Set { key: String, value: String },
    /// Print the configuration path.
    Path,
    /// Reload configuration.
    Reload,
}
/// Shell environment commands.
#[derive(Debug, Subcommand)]
pub enum EnvCommand {
    /// Show the cached shell environment.
    Show,
    /// Refresh the shell environment.
    Refresh,
}
