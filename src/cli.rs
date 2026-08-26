use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ColorMode {
    /// Detect terminal capabilities automatically
    Auto,
    /// Always use colors
    Always,
    /// Never use colors
    Never,
}

#[derive(Debug, Clone, Subcommand)]
pub enum DaemonCommand {
    /// Start the daemon (Beta; foreground if --foreground, otherwise detached)
    Start {
        /// Run in foreground (for service managers)
        #[arg(long)]
        foreground: bool,
    },
    /// Stop a running Beta daemon
    Stop,
    /// Show Beta daemon status
    Status,
    /// Install the Beta daemon as a system service (LaunchAgent on macOS, systemd on Linux, Task Scheduler on Windows)
    Install,
    /// Uninstall the Beta daemon system service
    Uninstall,
}

#[derive(Debug, Clone, Subcommand)]
pub enum ProviderCommand {
    /// Add a custom API provider (e.g. OpenRouter) for launching Codex with a third-party model
    #[command(
        after_help = "The API key is read from a hidden prompt (or stdin with --api-key-stdin), never from the command line.\n\nExample:\n  codex-switch provider add openrouter \\\n    --base-url https://openrouter.ai/api/v1 \\\n    --model openai/gpt-5.3-codex"
    )]
    Add {
        /// Provider alias (codex-switch name)
        alias: String,
        /// API base URL, e.g. https://openrouter.ai/api/v1
        #[arg(long)]
        base_url: String,
        /// Default model id (for OpenRouter, the full slug incl. provider prefix)
        #[arg(long)]
        model: String,
        /// Human-readable provider name (defaults to the alias)
        #[arg(long)]
        name: Option<String>,
        /// Environment variable Codex reads the key from (defaults to a codex-switch-owned name)
        #[arg(long)]
        env_key: Option<String>,
        /// Codex wire protocol (current Codex only supports "responses")
        #[arg(long, default_value = "responses")]
        wire_api: String,
        /// Reasoning effort for thinking models, saved as
        /// `model_reasoning_effort=<VALUE>` (common: none, minimal, low, medium,
        /// high, xhigh, max; Codex also accepts ultra). Passed to Codex verbatim
        #[arg(long, value_name = "EFFORT")]
        reasoning: Option<String>,
        /// Disable Codex's built-in web_search server tool for models that reject
        /// it (saved as `web_search=disabled`)
        #[arg(long)]
        no_web_search: bool,
        /// Extra `codex -c KEY=VALUE` override to apply at launch (repeatable);
        /// passed through verbatim, so any value Codex accepts works
        #[arg(long = "set", value_name = "KEY=VALUE")]
        set: Vec<String>,
        /// Read the API key from stdin instead of an interactive hidden prompt
        #[arg(long)]
        api_key_stdin: bool,
    },
    /// List saved custom providers
    List,
    /// Show one provider's details (API key redacted)
    Show {
        /// Provider alias
        alias: String,
    },
    /// Remove a custom provider and its stored key
    Remove {
        /// Provider alias
        alias: String,
        /// Skip confirmation prompt
        #[arg(long, short)]
        yes: bool,
    },
}

#[derive(Parser)]
#[command(
    name = "codex-switch",
    version = concat!(env!("CARGO_PKG_VERSION"), "\n", env!("CARGO_PKG_REPOSITORY")),
    about = "Codex account switcher -- multi-profile manager with usage dashboard\nhttps://github.com/xjoker/codex-switch",
    long_about = None,
    after_help = "Examples:\n  codex-switch list\n  codex-switch use\n  codex-switch rename old-alias new-alias\n  codex-switch import ./auth-backups\n  codex-switch self-update --check\n\nRun `codex-switch <command> --help` for command-specific options."
)]
pub struct Cli {
    /// Output as compact JSON (supported by list, use, reset-card, rename, delete, login, import, self-update, daemon status)
    #[arg(long, global = true)]
    pub json: bool,

    /// Output as pretty-printed JSON
    #[arg(long, global = true)]
    pub json_pretty: bool,

    /// Proxy URL (overrides CS_PROXY / HTTP_PROXY / HTTPS_PROXY / ALL_PROXY env vars)
    ///
    /// Supported formats:
    ///   http://[user:pass@]host:port
    ///   https://[user:pass@]host:port
    ///   socks4://host:port
    ///   socks5://[user:pass@]host:port      (local DNS)
    ///   socks5h://[user:pass@]host:port     (remote DNS)
    #[arg(long, global = true, env = "CS_PROXY")]
    pub proxy: Option<String>,

    /// Color output mode
    #[arg(long, global = true, default_value = "auto", env = "CS_COLOR")]
    pub color: ColorMode,

    /// Enable debug logging (shows HTTP requests, API responses, cache status)
    #[arg(long, global = true)]
    pub debug: bool,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Switch to a profile; omit alias to auto-select using the unified scoring algorithm
    Use {
        /// Profile alias (omit to auto-select)
        alias: Option<String>,
        /// When the pool is exhausted, automatically consume the earliest-expiring
        /// reset card to revive an account (only applies when alias is omitted;
        /// ignored when an alias is given)
        #[arg(long)]
        consume_card: bool,
    },
    /// List all profiles with account info, usage, and availability
    List {
        /// Force refresh, bypass cache
        #[arg(long, short)]
        force: bool,
    },
    /// Consume the earliest-expiring Codex reset card for a profile
    ResetCard {
        /// Profile alias
        alias: String,
        /// Skip confirmation prompt
        #[arg(long, short)]
        yes: bool,
    },
    /// Rename a profile
    Rename {
        /// Current profile alias
        old: String,
        /// New profile alias
        new: String,
    },
    /// Delete a profile (archived for recovery)
    Delete {
        /// Profile alias
        alias: String,
        /// Skip confirmation prompt
        #[arg(long, short)]
        yes: bool,
    },
    /// Log in via browser or --device code flow; re-authorizes if alias already exists
    Login {
        /// Profile alias -- if it already exists, re-authorizes it; otherwise creates a new profile
        alias: Option<String>,

        /// Use device code flow (for headless servers without a browser)
        #[arg(long)]
        device: bool,
    },
    /// Import an auth.json file, or recursively scan a directory for JSON files to validate and import
    Import {
        /// Path to an auth.json file or a directory containing JSON files
        path: String,
        /// Optional profile alias (single-file import only; directories auto-assign aliases)
        alias: Option<String>,
    },
    /// Manually check GitHub Releases (`--check`) or update this binary
    #[command(
        after_help = "Examples:\n  codex-switch self-update --check\n  codex-switch self-update\n  codex-switch self-update --dev\n  codex-switch self-update --stable\n\nOnly the TUI checks automatically at startup. Other commands never check automatically.\nWithout flags, updates within the current channel (stable or dev).\n`--dev` switches to the dev channel. `--stable` switches back to stable."
    )]
    SelfUpdate {
        /// Check whether a newer version is available without installing it
        #[arg(long)]
        check: bool,
        /// Install a specific newer version instead of the latest release
        #[arg(long, conflicts_with_all = ["dev", "stable"])]
        version: Option<String>,
        /// Switch to the dev channel (latest dev build)
        #[arg(long, conflicts_with = "stable")]
        dev: bool,
        /// Switch back to the stable channel (from dev)
        #[arg(long, conflicts_with = "dev")]
        stable: bool,
    },
    /// Send a minimal request to activate the quota window countdown for one or all profiles
    ///
    /// Fresh accounts show no reset timer until their first real request.
    /// This command triggers that timer without running a real task.
    #[command(
        after_help = "Examples:\n  codex-switch warmup          # warmup all profiles\n  codex-switch warmup myalias  # warmup a specific profile"
    )]
    Warmup {
        /// Profile alias to warm up (omit to warm up all profiles)
        alias: Option<String>,
    },
    /// Launch codex CLI with the best (or specified) profile's auth
    Launch {
        /// Profile alias (omit to auto-select best available)
        alias: Option<String>,
        /// When the pool is exhausted, automatically consume the earliest-expiring
        /// reset card to revive an account (only applies when alias is omitted;
        /// ignored when an alias is given)
        #[arg(long)]
        consume_card: bool,
        /// All remaining arguments passed through to codex
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Launch the interactive TUI
    Tui,
    /// Open the ~/.codex-switch directory in the system file manager
    Open,
    /// Manage custom API providers (OpenRouter, etc.) for launching Codex with a third-party model
    #[command(subcommand)]
    Provider(ProviderCommand),
    /// Background daemon (Beta) for automatic account switching
    #[command(subcommand)]
    Daemon(DaemonCommand),
}
