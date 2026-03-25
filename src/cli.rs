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
    version,
    about = "Codex account switcher — multi-profile manager with usage dashboard",
    long_about = None
)]
pub struct Cli {
    /// Output result as JSON (suitable for scripts/pipes)
    #[arg(long, global = true)]
    pub json: bool,

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

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Switch to a profile; omit alias to auto-select the best available account
    Use {
        /// Profile alias (omit to auto-select by remaining quota)
        alias: Option<String>,
    },
    /// List all saved profiles (auto-saves live auth.json if not yet tracked)
    List,
    /// Delete a profile
    Delete {
        /// Profile alias
        alias: String,
    },
    /// Show all profiles with account info and usage
    Status,
    /// Log in to a new account, or re-authorize an existing profile if alias is given
    Login {
        /// Profile alias — if it already exists, re-authorizes it; otherwise creates a new profile
        alias: Option<String>,
    },
    /// Import an auth.json file and save it as a profile
    Import {
        /// Path to auth.json file
        file: String,
        /// Profile alias
        alias: String,
    },
    /// Launch the interactive TUI
    Tui,
    /// Open the ~/.codex-switch directory in the system file manager
    Open,
}
