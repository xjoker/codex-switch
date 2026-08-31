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
        after_help = "The API key is read from a hidden prompt (or stdin with --api-key-stdin), never from the command line.\n--model is repeatable; the first is the default. --reasoning / --no-web-search attach to the most recent --model.\n--fetch-models GETs {base_url}/models and saves chat slugs (embedding/reranker omitted). Use it instead of, or together with, --model. Catalogs larger than 48 models (OpenRouter-sized) are not imported wholesale: pass --model to pick, or use TUI `f`.\n\nExample:\n  codex-switch provider add openrouter \\\n    --base-url https://openrouter.ai/api/v1 \\\n    --fetch-models \\\n    --model openai/gpt-5.3-codex \\\n    --model deepseek/deepseek-r1-0528 --reasoning high\n  printf '%s' \"$KEY\" | codex-switch provider add zai --base-url https://api.example/v1 --fetch-models --api-key-stdin"
    )]
    Add {
        /// Provider alias (the only user-facing name)
        alias: String,
        /// API base URL, e.g. https://openrouter.ai/api/v1
        #[arg(long)]
        base_url: String,
        /// Model id (repeatable; first is the default). For OpenRouter, the full slug
        #[arg(long, action = clap::ArgAction::Append)]
        model: Vec<String>,
        /// Environment variable Codex reads the key from (defaults to a codex-switch-owned name)
        #[arg(long)]
        env_key: Option<String>,
        /// Codex wire protocol (current Codex only supports "responses")
        #[arg(long, default_value = "responses")]
        wire_api: String,
        /// Reasoning effort attached to the most recent --model (repeatable)
        #[arg(long, value_name = "EFFORT", action = clap::ArgAction::Append)]
        reasoning: Vec<String>,
        /// Disable web_search on the most recent --model (repeatable)
        #[arg(long, action = clap::ArgAction::Count)]
        no_web_search: u8,
        /// Extra `codex -c KEY=VALUE` override to apply at launch (repeatable);
        /// passed through verbatim, so any value Codex accepts works
        #[arg(long = "set", value_name = "KEY=VALUE")]
        set: Vec<String>,
        /// Catalog metadata fallback after the gateway `/models` call: HTTP(S)
        /// URL, local JSON file, or `none` to skip. Default is the public
        /// OpenRouter list (no login).
        #[arg(long = "metadata-fallback", value_name = "URL|PATH|none")]
        metadata_fallback: Option<String>,
        /// GET `{base_url}/models` and save chat slugs (embedding/reranker
        /// omitted). Required unless `--model` is given. Catalogs larger than
        /// 48 models must be picked with `--model` (or the TUI picker).
        #[arg(long)]
        fetch_models: bool,
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
    /// Rename a custom provider
    Rename {
        /// Current provider alias
        old: String,
        /// New provider alias
        new: String,
    },
    /// Remove a custom provider and its stored key
    Remove {
        /// Provider alias
        alias: String,
        /// Skip confirmation prompt
        #[arg(long, short)]
        yes: bool,
    },
    /// Replace saved models with chat slugs from the provider's GET /models.
    /// Large catalogs must be picked with `--model`.
    FetchModels {
        /// Provider alias
        alias: String,
        /// Chat slug to keep (repeatable). Required when the gateway lists more
        /// than 48 chat models.
        #[arg(long, action = clap::ArgAction::Append)]
        model: Vec<String>,
    },
    /// Probe whether saved models speak Codex's Responses API (no `input`, so a
    /// supporting endpoint 400s at validation without generating tokens).
    Probe {
        /// Provider alias
        alias: String,
        /// Probe this saved model only (default: every saved model)
        #[arg(long)]
        model: Option<String>,
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
    /// Output as compact JSON (supported by list, use, launch, reset-card, rename, delete, login, import, self-update, daemon status, provider add/list/show/rename/remove/fetch-models/probe)
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

    /// Enable debug logging (shows HTTP status, retries, and cache status)
    #[arg(long, global = true)]
    pub debug: bool,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
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
    /// Launch Codex CLI with the best (or specified) profile's auth
    #[command(
        after_help = "Codex argv is everything after `--`, a known Codex subcommand (`exec`, `resume`, …), or a flag that is not a launch/codex-switch option (`-s`, `--sandbox`, …). Tokens on both sides of `--` are kept (so `launch work exec -- --json` still runs `exec`).\nUse `--` when the Codex argv starts with a prompt, or with a flag that also exists on codex-switch (`--json`, `--color`, `--model`).\n\nExamples:\n  codex-switch launch work -- exec --json \"review this\"\n  codex-switch launch work exec -- --json \"review this\"\n  codex-switch launch exec --json \"do the thing\"\n  codex-switch launch openrouter -- -s workspace-write -a never\n\n`--model` before `--` selects a saved provider model, or is forwarded as Codex `--model` for a ChatGPT profile. `--model` after `--` is Codex's own flag.\n`--json launch` prints one JSON envelope after Codex exits (Codex stdout/stderr are captured into that envelope, not mixed onto stdout)."
    )]
    Launch {
        /// Profile alias (omit to auto-select best available)
        alias: Option<String>,
        /// When the pool is exhausted, automatically consume the earliest-expiring
        /// reset card to revive an account (only applies when alias is omitted;
        /// ignored when an alias is given)
        #[arg(long)]
        consume_card: bool,
        /// For a custom provider, select a saved model (default_model otherwise).
        /// For a ChatGPT profile, forwarded to Codex as `--model`.
        #[arg(long)]
        model: Option<String>,
        /// Codex argv; prefer `--` before this so flags are not parsed by codex-switch
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

/// Split `codex-switch launch …` so Codex argv is never parsed as a
/// codex-switch alias or global flag.
///
/// Clap treats a bare `--` as "stop parsing flags" but still fills the next
/// positional, so `launch -- work` would otherwise become alias `work`. A
/// known Codex subcommand (`exec`, `resume`, …) or a non-launch flag (`-s`)
/// in the alias slot is treated the same way, so `launch exec --json` is not
/// `Profile 'exec' not found`.
pub(crate) fn extract_launch_passthrough(argv: &[String]) -> (Vec<String>, Option<Vec<String>>) {
    let Some(launch_at) = first_subcommand(argv).filter(|&i| argv[i] == "launch") else {
        return (argv.to_vec(), None);
    };
    let mut i = launch_at + 1;
    while i < argv.len() {
        let arg = argv[i].as_str();
        if arg == "--" {
            return (argv[..i].to_vec(), Some(argv[i + 1..].to_vec()));
        }
        if let Some(skip) = skip_launch_or_global_flag(argv, i) {
            i += skip;
            continue;
        }
        if arg.starts_with('-') || is_codex_subcommand(arg) {
            return (argv[..i].to_vec(), Some(argv[i..].to_vec()));
        }
        if let Some(rel) = argv[i + 1..].iter().position(|next| next == "--") {
            let dash = i + 1 + rel;
            return (argv[..dash].to_vec(), Some(argv[dash + 1..].to_vec()));
        }
        return (argv.to_vec(), None);
    }
    (argv.to_vec(), None)
}

/// Concatenate clap's trailing launch args with argv taken from after `--`
/// (or from a Codex subcommand / foreign flag). `launch work exec -- --json`
/// must keep `exec`.
pub(crate) fn merge_launch_args(
    clap_args: Vec<String>,
    passthrough: Option<Vec<String>>,
) -> Vec<String> {
    match passthrough {
        Some(right) => {
            let mut args = clap_args;
            args.extend(right);
            args
        }
        None => clap_args,
    }
}

fn skip_launch_or_global_flag(argv: &[String], i: usize) -> Option<usize> {
    let arg = argv[i].as_str();
    if arg == "-h" || arg == "-V" {
        return Some(1);
    }
    let rest = arg.strip_prefix("--")?;
    if rest.is_empty() {
        return None;
    }
    let (name, has_eq) = match rest.split_once('=') {
        Some((name, _)) => (name, true),
        None => (rest, false),
    };
    match name {
        "consume-card" | "json" | "json-pretty" | "debug" | "help" | "version" => Some(1),
        "model" | "proxy" | "color" => {
            if has_eq || i + 1 >= argv.len() || argv[i + 1] == "--" || argv[i + 1].starts_with('-')
            {
                Some(1)
            } else {
                Some(2)
            }
        }
        _ => None,
    }
}

pub(crate) fn is_codex_subcommand(name: &str) -> bool {
    matches!(
        name,
        "agents"
            | "exec"
            | "review"
            | "login"
            | "logout"
            | "mcp"
            | "plugin"
            | "mcp-server"
            | "app-server"
            | "remote-control"
            | "completion"
            | "update"
            | "doctor"
            | "sandbox"
            | "debug"
            | "apply"
            | "resume"
            | "queue"
            | "archive"
            | "delete"
            | "migrate-rollouts"
            | "unarchive"
            | "fork"
            | "cloud"
            | "exec-server"
            | "features"
            | "help"
    )
}

fn first_subcommand(argv: &[String]) -> Option<usize> {
    let mut i = 1;
    while i < argv.len() {
        let arg = argv[i].as_str();
        if arg == "--" {
            return None;
        }
        if let Some(rest) = arg.strip_prefix("--") {
            if rest.is_empty() {
                return None;
            }
            i += if rest.contains('=') || !global_value_flag(rest) {
                1
            } else {
                2
            };
            continue;
        }
        if arg.starts_with('-') {
            i += 1;
            continue;
        }
        return Some(i);
    }
    None
}

fn global_value_flag(name: &str) -> bool {
    matches!(name, "proxy" | "color")
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn provider_add_allows_no_web_search_on_each_model() {
        let cli = Cli::try_parse_from([
            "codex-switch",
            "provider",
            "add",
            "or",
            "--base-url",
            "https://openrouter.ai/api/v1",
            "--model",
            "a",
            "--no-web-search",
            "--model",
            "b",
            "--no-web-search",
            "--api-key-stdin",
        ])
        .expect("repeatable --no-web-search must parse");
        match cli.command {
            Commands::Provider(ProviderCommand::Add {
                no_web_search,
                model,
                ..
            }) => {
                assert_eq!(model, ["a", "b"]);
                assert_eq!(
                    no_web_search, 2,
                    "each --no-web-search is kept so models_from_cli_args can attach it"
                );
            }
            _ => panic!("expected provider add"),
        }
    }

    #[test]
    fn provider_add_allows_fetch_models_without_model() {
        let cli = Cli::try_parse_from([
            "codex-switch",
            "provider",
            "add",
            "zai",
            "--base-url",
            "https://example.test/v1",
            "--fetch-models",
            "--api-key-stdin",
        ])
        .expect("--fetch-models must satisfy the model list");
        match cli.command {
            Commands::Provider(ProviderCommand::Add {
                fetch_models,
                model,
                ..
            }) => {
                assert!(fetch_models);
                assert!(model.is_empty());
            }
            _ => panic!("expected provider add"),
        }
    }

    #[test]
    fn provider_fetch_models_subcommand_parses() {
        let cli = Cli::try_parse_from(["codex-switch", "provider", "fetch-models", "zai"])
            .expect("fetch-models subcommand");
        match cli.command {
            Commands::Provider(ProviderCommand::FetchModels { alias, model }) => {
                assert_eq!(alias, "zai");
                assert!(model.is_empty());
            }
            _ => panic!("expected provider fetch-models"),
        }
    }

    #[test]
    fn provider_fetch_models_accepts_repeatable_model() {
        let cli = Cli::try_parse_from([
            "codex-switch",
            "provider",
            "fetch-models",
            "or",
            "--model",
            "openai/gpt-4.1-nano",
            "--model",
            "deepseek/deepseek-r1-0528",
        ])
        .expect("fetch-models --model");
        match cli.command {
            Commands::Provider(ProviderCommand::FetchModels { alias, model }) => {
                assert_eq!(alias, "or");
                assert_eq!(model, ["openai/gpt-4.1-nano", "deepseek/deepseek-r1-0528"]);
            }
            _ => panic!("expected provider fetch-models"),
        }
    }

    #[test]
    fn provider_probe_subcommand_parses() {
        let cli = Cli::try_parse_from([
            "codex-switch",
            "provider",
            "probe",
            "AI-KR",
            "--model",
            "deepseek-v4-flash",
        ])
        .expect("probe subcommand");
        match cli.command {
            Commands::Provider(ProviderCommand::Probe { alias, model }) => {
                assert_eq!(alias, "AI-KR");
                assert_eq!(model.as_deref(), Some("deepseek-v4-flash"));
            }
            _ => panic!("expected provider probe"),
        }
    }

    fn parse_launch(raw_args: &[&str]) -> (bool, Option<String>, Option<String>, Vec<String>) {
        let raw = argv(raw_args);
        let (left, right) = extract_launch_passthrough(&raw);
        let cli = Cli::try_parse_from(&left).unwrap_or_else(|e| panic!("{e}"));
        match cli.command {
            Commands::Launch {
                alias, model, args, ..
            } => (cli.json, alias, model, merge_launch_args(args, right)),
            other => panic!("expected launch, got {other:?}"),
        }
    }

    #[test]
    fn launch_passthrough_after_double_dash_keeps_codex_exec_json_and_color() {
        let (json, alias, model, args) = parse_launch(&[
            "codex-switch",
            "launch",
            "work",
            "--",
            "exec",
            "--json",
            "--color",
            "never",
            "do the thing",
        ]);
        assert!(!json, "Codex --json after -- must not turn on cs --json");
        assert_eq!(alias.as_deref(), Some("work"));
        assert_eq!(model, None);
        assert_eq!(args, ["exec", "--json", "--color", "never", "do the thing"]);
    }

    #[test]
    fn launch_without_double_dash_must_not_steal_codex_exec_json() {
        let (json, alias, model, args) = parse_launch(&[
            "codex-switch",
            "launch",
            "work",
            "exec",
            "--json",
            "do the thing",
        ]);
        assert!(
            !json,
            "cs --json is global and currently steals Codex exec --json; this test names the contract"
        );
        assert_eq!(alias.as_deref(), Some("work"));
        assert_eq!(model, None);
        assert_eq!(args, ["exec", "--json", "do the thing"]);
    }

    #[test]
    fn launch_cs_json_before_double_dash_still_applies_to_codex_switch() {
        let (json, alias, _, args) = parse_launch(&[
            "codex-switch",
            "--json",
            "launch",
            "work",
            "--",
            "exec",
            "--json",
        ]);
        assert!(json);
        assert_eq!(alias.as_deref(), Some("work"));
        assert_eq!(args, ["exec", "--json"]);
    }

    #[test]
    fn launch_model_after_double_dash_is_codex_model_not_cs_model() {
        let (_, alias, model, args) = parse_launch(&[
            "codex-switch",
            "launch",
            "openrouter",
            "--",
            "--model",
            "openai/gpt-5.3-codex",
        ]);
        assert_eq!(alias.as_deref(), Some("openrouter"));
        assert_eq!(model, None);
        assert_eq!(args, ["--model", "openai/gpt-5.3-codex"]);
    }

    #[test]
    fn launch_cs_model_before_double_dash_is_kept_separate_from_passthrough() {
        let (_, alias, model, args) = parse_launch(&[
            "codex-switch",
            "launch",
            "work",
            "--model",
            "gpt-5.4",
            "--",
            "exec",
            "hi",
        ]);
        assert_eq!(alias.as_deref(), Some("work"));
        assert_eq!(model.as_deref(), Some("gpt-5.4"));
        assert_eq!(args, ["exec", "hi"]);
    }

    #[test]
    fn launch_double_dash_then_alias_shaped_prompt_is_passthrough_not_alias() {
        let raw = argv(&["codex-switch", "launch", "--", "work"]);
        let (left, right) = extract_launch_passthrough(&raw);
        let cli = Cli::try_parse_from(&left).unwrap_or_else(|e| panic!("{e}"));
        match cli.command {
            Commands::Launch { alias, args, .. } => {
                assert_eq!(alias, None);
                assert!(args.is_empty(), "clap argv stops before --");
            }
            other => panic!("expected launch, got {other:?}"),
        }
        assert_eq!(right, Some(argv(&["work"])));
    }

    #[test]
    fn extract_launch_passthrough_keeps_exec_json_after_separator() {
        let raw = argv(&[
            "codex-switch",
            "--json",
            "launch",
            "work",
            "--",
            "exec",
            "--json",
            "do the thing",
        ]);
        let (left, right) = extract_launch_passthrough(&raw);
        let cli = Cli::try_parse_from(&left).unwrap_or_else(|e| panic!("{e}"));
        assert!(cli.json);
        match cli.command {
            Commands::Launch {
                alias, model, args, ..
            } => {
                assert_eq!(alias.as_deref(), Some("work"));
                assert_eq!(model, None);
                assert!(args.is_empty());
            }
            other => panic!("expected launch, got {other:?}"),
        }
        assert_eq!(right, Some(argv(&["exec", "--json", "do the thing"])));
    }

    #[test]
    fn extract_launch_passthrough_ignores_provider_alias_named_launch() {
        let raw = argv(&[
            "codex-switch",
            "provider",
            "add",
            "launch",
            "--base-url",
            "https://example.test",
            "--model",
            "m",
        ]);
        let (left, right) = extract_launch_passthrough(&raw);
        assert_eq!(left, raw);
        assert_eq!(right, None);
    }

    #[test]
    fn extract_launch_passthrough_preserves_codex_double_dash() {
        let raw = argv(&[
            "codex-switch",
            "launch",
            "work",
            "--",
            "exec",
            "--",
            "--looks-like-flag",
        ]);
        let (_, right) = extract_launch_passthrough(&raw);
        assert_eq!(right, Some(argv(&["exec", "--", "--looks-like-flag"])));
    }

    #[test]
    fn launch_cs_json_flag_on_the_subcommand_is_for_codex_switch() {
        let (json, alias, _, args) = parse_launch(&["codex-switch", "launch", "work", "--json"]);
        assert!(json);
        assert_eq!(alias.as_deref(), Some("work"));
        assert_eq!(args, [] as [&str; 0]);
    }

    #[test]
    fn launch_without_double_dash_must_not_steal_codex_exec_color() {
        let cli = Cli::try_parse_from([
            "codex-switch",
            "launch",
            "work",
            "exec",
            "--color",
            "never",
            "do",
        ])
        .unwrap_or_else(|e| panic!("{e}"));
        match cli.command {
            Commands::Launch { alias, args, .. } => {
                assert_eq!(alias.as_deref(), Some("work"));
                assert_eq!(
                    cli.color,
                    ColorMode::Auto,
                    "Codex exec --color must not change cs --color"
                );
                assert_eq!(args, ["exec", "--color", "never", "do"]);
            }
            other => panic!("expected launch, got {other:?}"),
        }
    }

    #[test]
    fn launch_passthrough_keeps_sandbox_and_cd_flags() {
        let (_, alias, _, args) = parse_launch(&[
            "codex-switch",
            "launch",
            "work",
            "--",
            "-s",
            "workspace-write",
            "-C",
            "/tmp/proj",
            "-a",
            "never",
            "ship it",
        ]);
        assert_eq!(alias.as_deref(), Some("work"));
        assert_eq!(
            args,
            [
                "-s",
                "workspace-write",
                "-C",
                "/tmp/proj",
                "-a",
                "never",
                "ship it"
            ]
        );
    }

    #[test]
    fn launch_merges_args_on_both_sides_of_double_dash() {
        let (json, alias, _, args) = parse_launch(&[
            "codex-switch",
            "launch",
            "work",
            "exec",
            "--",
            "--json",
            "hi",
        ]);
        assert!(!json);
        assert_eq!(alias.as_deref(), Some("work"));
        assert_eq!(args, ["exec", "--json", "hi"]);
    }

    #[test]
    fn launch_exec_without_double_dash_is_codex_not_an_alias() {
        let (json, alias, _, args) =
            parse_launch(&["codex-switch", "launch", "exec", "--json", "do the thing"]);
        assert!(
            !json,
            "Codex exec --json must not turn on cs --json when exec is the first token"
        );
        assert_eq!(alias, None);
        assert_eq!(args, ["exec", "--json", "do the thing"]);
    }

    #[test]
    fn launch_sandbox_flag_without_double_dash_is_codex_not_a_parse_error() {
        let (_, alias, _, args) = parse_launch(&[
            "codex-switch",
            "launch",
            "work",
            "--",
            "-s",
            "workspace-write",
        ]);
        assert_eq!(alias.as_deref(), Some("work"));
        assert_eq!(args, ["-s", "workspace-write"]);

        let (_, alias, _, args) = parse_launch(&[
            "codex-switch",
            "launch",
            "-s",
            "workspace-write",
            "-a",
            "never",
        ]);
        assert_eq!(alias, None);
        assert_eq!(args, ["-s", "workspace-write", "-a", "never"]);
    }

    #[test]
    fn launch_resume_without_alias_is_codex_subcommand() {
        let (_, alias, _, args) = parse_launch(&["codex-switch", "launch", "resume", "--last"]);
        assert_eq!(alias, None);
        assert_eq!(args, ["resume", "--last"]);
    }

    #[test]
    fn merge_launch_args_keeps_left_tokens_then_right() {
        assert_eq!(
            merge_launch_args(argv(&["exec"]), Some(argv(&["--json", "hi"]))),
            argv(&["exec", "--json", "hi"])
        );
        assert_eq!(merge_launch_args(argv(&["exec"]), None), argv(&["exec"]));
    }
}
