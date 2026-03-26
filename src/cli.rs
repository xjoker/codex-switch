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

#[derive(Parser)]
#[command(
    name = "codex-switch",
    version = concat!(env!("CARGO_PKG_VERSION"), "\n", env!("CARGO_PKG_REPOSITORY")),
    about = "Codex account switcher — multi-profile manager with usage dashboard\nhttps://github.com/xjoker/codex-switch",
    long_about = None,
    after_help = "Examples:\n  codex-switch list\n  codex-switch use\n  codex-switch import ./auth-backups\n  codex-switch self-update --check\n\nRun `codex-switch <command> --help` for command-specific options."
)]
pub struct Cli {
    /// Output as compact JSON (supported by list, use, delete, login, import, self-update)
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
    /// Switch to a profile; omit alias to auto-select the best available account
    Use {
        /// Profile alias (omit to auto-select by remaining quota)
        alias: Option<String>,
        /// Force switch even if Codex processes are running
        #[arg(long)]
        force: bool,
    },
    /// List all profiles with account info, usage, and availability
    List {
        /// Force refresh, bypass cache
        #[arg(long, short)]
        force: bool,
    },
    /// Delete a profile
    Delete {
        /// Profile alias
        alias: String,
    },
    /// Log in via browser or --device code flow; re-authorizes if alias already exists
    Login {
        /// Profile alias — if it already exists, re-authorizes it; otherwise creates a new profile
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
        after_help = "Examples:\n  codex-switch self-update --check\n  codex-switch self-update\n  codex-switch self-update --version 0.0.3\n\nUpdate checks are manual only. The app never checks automatically on startup.\nDowngrades are not supported; `--version` only accepts the current version or a newer release."
    )]
    SelfUpdate {
        /// Check whether a newer version is available without installing it
        #[arg(long)]
        check: bool,
        /// Install a specific newer version instead of the latest release
        #[arg(long)]
        version: Option<String>,
    },
    /// Launch the interactive TUI
    Tui,
    /// Open the ~/.codex-switch directory in the system file manager
    Open,
}
