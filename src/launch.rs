use crate::output::{print_json, user_println};
use crate::provider::{self, ProviderProfile, ReasoningLaunch};
use crate::signals::ShutdownListener;
use crate::{auth, config, profile};
use anyhow::{Context, Result};

/// How the window Codex needs to read the staged `auth.json` ended.
#[derive(Debug, PartialEq, Eq)]
enum LaunchWait {
    Elapsed,
    Interrupted,
}

/// Waits out that window, returning early if the user interrupts.
///
/// `interrupt` is deliberately a parameter rather than something this function
/// builds: it has to be registered before staging starts. Tokio discards a
/// signal that arrives with nothing registered for it, so a listener created
/// here would leave the whole staging window under the default terminate
/// action — Ctrl+C during the swap would kill the process outright, with the
/// staged profile left live and the user's own credentials stranded in a
/// `.bak` file whose name nothing ever printed.
async fn wait_for_codex_to_read_auth(
    interrupt: &mut ShutdownListener,
    delay: std::time::Duration,
) -> LaunchWait {
    tokio::select! {
        _ = tokio::time::sleep(delay) => LaunchWait::Elapsed,
        _ = interrupt.recv() => LaunchWait::Interrupted,
    }
}

/// Launch Codex for one alias from the TUI. Returns Codex's exit code instead of
/// terminating the codex-switch process on failure.
pub(crate) async fn launch_for_tui(
    alias: &str,
    model: Option<&str>,
    reasoning: ReasoningLaunch,
    extra_args: Vec<String>,
) -> Result<i32> {
    launch_interactive(Some(alias), extra_args, false, false, model, reasoning).await
}

pub(crate) async fn launch_cmd(
    alias: Option<&str>,
    args: Vec<String>,
    json: bool,
    consume_card: bool,
    model: Option<&str>,
) -> Result<()> {
    finish_launch_cli(
        launch_interactive(
            alias,
            args,
            json,
            consume_card,
            model,
            ReasoningLaunch::Saved,
        )
        .await?,
    )
}

fn finish_launch_cli(exit_code: i32) -> Result<()> {
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}

async fn launch_interactive(
    alias: Option<&str>,
    args: Vec<String>,
    json: bool,
    consume_card: bool,
    model: Option<&str>,
    reasoning: ReasoningLaunch,
) -> Result<i32> {
    use std::io::IsTerminal;

    // A custom API provider profile takes a separate, simpler path: it has no
    // OAuth auth.json to stage, so it never touches ~/.codex/auth.json. It is
    // translated into `codex -c …` overrides with the key injected via the
    // environment. Auto-select (no alias) stays ChatGPT-only.
    if let Some(alias) = alias
        && provider::exists(alias)
    {
        let profile = provider::load(alias)?;
        return launch_provider(profile, model, reasoning, args, json).await;
    }

    let forwarded = chatgpt_codex_argv(model, args);

    let mut revival_hint = None;
    let target_alias = match alias {
        Some(alias) => {
            let profiles = profile::list_profiles()?;
            if !profiles.iter().any(|profile| profile == alias) {
                anyhow::bail!("Profile '{}' not found", alias);
            }
            alias.to_string()
        }
        None => {
            let card_policy = if consume_card {
                crate::commands::profile::CardPolicy::PreApproved
            } else if !json && std::io::stdin().is_terminal() {
                crate::commands::profile::CardPolicy::Prompt
            } else {
                crate::commands::profile::CardPolicy::Deny
            };
            let outcome = crate::commands::profile::select_best_profile(json, card_policy).await?;
            revival_hint = outcome.revival_hint;
            outcome.alias
        }
    };
    if let Some(hint) = &revival_hint
        && !json
    {
        user_println(&crate::commands::profile::revival_hint_message(hint));
    }

    ensure_codex_available()?;

    let codex_auth = auth::codex_auth_path()?;
    // Unique per-invocation backup name (PID + timestamp): prevents two
    // concurrent `launch` commands from clobbering each other's backup.
    let backup = codex_auth.with_extension(format!(
        "json.bak.{}.{}",
        std::process::id(),
        auth::now_unix_secs()
    ));

    // Registered before the first byte of the user's auth.json moves, so a
    // Ctrl+C anywhere from here to the restore is recorded rather than
    // discarded. See `wait_for_codex_to_read_auth`.
    let mut interrupt = ShutdownListener::interrupt_only()
        .context("registering the interrupt handler that guards the staged auth.json")?;

    // The dedicated launch lease covers only stage -> process start -> short
    // read window -> restore. It does not hold the auth write lock or wait for
    // the interactive child to exit.
    let launch_lease = tokio::task::spawn_blocking(profile::lock_launch_session)
        .await
        .context("launch lease task panicked")?
        .context("acquiring launch session lease")?;
    // All codex-switch writers acquire this lease before mutating live auth,
    // so the existence snapshot cannot race a concurrent switch.
    let had_original = codex_auth.exists();

    // Swap auth.json → start codex → wait for it to read auth → restore.
    // Codex CLI reads auth.json only at startup, so we only need to hold
    // the swapped state for a few seconds, not the entire session.
    let stage_result = {
        let codex_auth2 = codex_auth.clone();
        let backup2 = backup.clone();
        let target_alias2 = target_alias.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let _lock = profile::lock_live_auth().context("acquiring auth lock")?;

            if had_original {
                backup_launch_auth(&codex_auth2, &backup2)?;
            }

            profile::stage_profile_auth(&target_alias2)?;
            Ok(())
        })
        .await
        .context("lock task panicked")?
    };
    if let Err(stage_err) = stage_result {
        if backup.exists() || !had_original {
            let codex_auth2 = codex_auth.clone();
            let backup2 = backup.clone();
            let alias2 = target_alias.clone();
            tokio::task::spawn_blocking(move || {
                restore_launch_auth(&codex_auth2, &backup2, had_original, &alias2)
            })
            .await
            .context("restore task panicked after launch staging failure")??;
        }
        drop(launch_lease);
        return Err(stage_err).context("staging launch auth");
    }
    // The auth lock is released here; the launch lease keeps other live-auth
    // writers out until the staged file is restored.

    if !json {
        user_println(&format!("Launching codex with profile '{target_alias}'..."));
    }

    let child_result = spawn_codex(&forwarded, None, json, None);

    let mut child = match child_result {
        Ok(child) => child,
        Err(spawn_err) => {
            let codex_auth2 = codex_auth.clone();
            let backup2 = backup.clone();
            let alias2 = target_alias.clone();
            tokio::task::spawn_blocking(move || {
                restore_launch_auth(&codex_auth2, &backup2, had_original, &alias2)
            })
            .await
            .context("restore task panicked after Codex spawn failure")??;
            drop(launch_lease);
            return Err(spawn_err).context("Failed to start codex");
        }
    };
    let pipes = take_codex_pipes(&mut child, json);

    // Give codex time to read auth.json, then restore immediately.
    // Configurable via [launch] restore_delay_secs (default: 3).
    let delay = std::time::Duration::from_secs(config::get().launch.restore_delay_secs);
    // An interrupt anywhere since staging began — including one that landed
    // while the swap itself was running — lands here, and the restore below
    // still runs.
    if wait_for_codex_to_read_auth(&mut interrupt, delay).await == LaunchWait::Interrupted && !json
    {
        user_println("Interrupted; restoring original auth.json...");
    }

    {
        let codex_auth2 = codex_auth.clone();
        let backup2 = backup.clone();
        let alias2 = target_alias.clone();
        tokio::task::spawn_blocking(move || {
            restore_launch_auth(&codex_auth2, &backup2, had_original, &alias2)
        })
        .await
        .context("lock task panicked")??;
    }
    drop(launch_lease);

    // Wait for codex to exit
    let status = child.wait().context("waiting for codex")?;
    let captured = join_codex_pipes(pipes);

    let exit_code = child_exit_code(&status);

    if json {
        let mut payload = serde_json::json!({
            "ok": status.success(),
            "alias": target_alias,
            "action": "launched",
            "exit_code": exit_code,
            "codex_stdout": captured.stdout,
            "codex_stderr": captured.stderr,
            "codex_stdout_truncated": captured.stdout_truncated,
            "codex_stderr_truncated": captured.stderr_truncated,
        });
        if let Some(model) = display_model(model, &forwarded) {
            payload["model"] = serde_json::Value::String(model);
        }
        if let Some(hint) = &revival_hint {
            payload["hint"] =
                serde_json::Value::String(crate::commands::profile::revival_hint_message(hint));
        }
        print_json(&payload);
    } else {
        user_println("codex exited");
    }

    Ok(exit_code)
}

/// Codex argv for a ChatGPT `launch`: optional `--model` is spliced after a
/// Codex subcommand in `passthrough` (Codex 0.149 ignores flags in front of
/// `exec`). Interactive launch has no subcommand, so `--model` stays in front.
pub(crate) fn chatgpt_codex_argv(model: Option<&str>, passthrough: Vec<String>) -> Vec<String> {
    let mut extra = Vec::new();
    if let Some(model) = model.filter(|model| !model.is_empty()) {
        extra.push("--model".to_string());
        extra.push(model.to_string());
    }
    splice_after_subcommand(extra, passthrough)
}

/// Codex argv for a provider `launch`. Codex 0.149 applies `-c` on the
/// subcommand (`codex exec -c …`), not in front of it (`codex -c … exec` is
/// ignored and the child talks to api.openai.com). Interactive launch has no
/// subcommand, so overrides stay in front. If the caller already passed
/// `--model` / `-m`, drop our per-model `-c` pairs (`model`,
/// `model_reasoning_effort`, `web_search`) so they do not fight the one-shot
/// model; provider definition overrides stay.
pub(crate) fn provider_codex_argv(overrides: Vec<String>, passthrough: Vec<String>) -> Vec<String> {
    let mut overrides = overrides;
    if passthrough_sets_model(&passthrough) {
        strip_c_pair(&mut overrides, is_per_model_override);
    }
    splice_after_subcommand(overrides, passthrough)
}

fn splice_after_subcommand(overrides: Vec<String>, passthrough: Vec<String>) -> Vec<String> {
    let mut cmd_at = None;
    for (i, arg) in passthrough.iter().enumerate() {
        if arg == "--" {
            break;
        }
        if crate::cli::is_codex_subcommand(arg) {
            cmd_at = Some(i);
            break;
        }
    }
    let Some(idx) = cmd_at else {
        let mut argv = overrides;
        argv.extend(passthrough);
        return argv;
    };
    // Codex 0.149 ignores options in front of the subcommand, so flags that
    // the user put before `exec` move after it along with our `-c` overrides.
    let mut argv = Vec::with_capacity(overrides.len() + passthrough.len());
    argv.push(passthrough[idx].clone());
    argv.extend(overrides);
    argv.extend(
        passthrough
            .into_iter()
            .enumerate()
            .filter_map(|(i, arg)| (i != idx).then_some(arg)),
    );
    argv
}

fn passthrough_sets_model(args: &[String]) -> bool {
    passthrough_model_value(args).is_some()
}

fn passthrough_model_value(args: &[String]) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "--" {
            return None;
        }
        if arg == "--model" || arg == "-m" {
            return args.get(i + 1).cloned();
        }
        if let Some(value) = arg.strip_prefix("--model=") {
            return Some(value.to_string());
        }
        i += 1;
    }
    None
}

fn display_model(cs_model: Option<&str>, passthrough: &[String]) -> Option<String> {
    passthrough_model_value(passthrough).or_else(|| {
        cs_model
            .filter(|model| !model.is_empty())
            .map(str::to_string)
    })
}

fn is_per_model_override(value: &str) -> bool {
    value
        .split_once('=')
        .is_some_and(|(key, _)| matches!(key, "model" | "model_reasoning_effort" | "web_search"))
}

fn strip_c_pair(argv: &mut Vec<String>, value_matches: impl Fn(&str) -> bool) {
    let mut i = 0;
    while i + 1 < argv.len() {
        if argv[i] == "-c" && value_matches(&argv[i + 1]) {
            argv.drain(i..=i + 1);
            continue;
        }
        i += 1;
    }
}

fn spawn_codex(
    args: &[String],
    extra_env: Option<(String, String)>,
    json: bool,
    isolated_codex_home: Option<&std::path::Path>,
) -> std::io::Result<std::process::Child> {
    let mut cmd = std::process::Command::new("codex");
    cmd.args(args);
    if json {
        // `--json launch` is non-interactive: inherited stdin is often a pipe
        // (not a TTY), and Codex exec then waits to append it as extra input.
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
    } else {
        cmd.stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit());
    }
    if let Some((name, value)) = extra_env {
        cmd.env(name, value);
    }
    if let Some(home) = isolated_codex_home {
        cmd.env("CODEX_HOME", home);
    }
    cmd.spawn()
}

struct CodexPipes {
    stdout: Option<std::thread::JoinHandle<CapturedBytes>>,
    stderr: Option<std::thread::JoinHandle<CapturedBytes>>,
}

struct CapturedCodexIo {
    stdout: String,
    stderr: String,
    stdout_truncated: bool,
    stderr_truncated: bool,
}

struct CapturedBytes {
    bytes: Vec<u8>,
    truncated: bool,
}

const CODEX_CAPTURE_LIMIT: usize = 1024 * 1024;

fn read_bounded(mut pipe: impl std::io::Read) -> CapturedBytes {
    let mut bytes = Vec::with_capacity(CODEX_CAPTURE_LIMIT.min(64 * 1024));
    let mut chunk = [0_u8; 16 * 1024];
    let mut truncated = false;
    while let Ok(read) = pipe.read(&mut chunk) {
        if read == 0 {
            break;
        }
        let remaining = CODEX_CAPTURE_LIMIT.saturating_sub(bytes.len());
        let kept = remaining.min(read);
        bytes.extend_from_slice(&chunk[..kept]);
        truncated |= kept < read;
    }
    CapturedBytes { bytes, truncated }
}

fn take_codex_pipes(child: &mut std::process::Child, json: bool) -> CodexPipes {
    if !json {
        return CodexPipes {
            stdout: None,
            stderr: None,
        };
    }
    CodexPipes {
        stdout: child
            .stdout
            .take()
            .map(|mut pipe| std::thread::spawn(move || read_bounded(&mut pipe))),
        stderr: child
            .stderr
            .take()
            .map(|mut pipe| std::thread::spawn(move || read_bounded(&mut pipe))),
    }
}

fn join_codex_pipes(pipes: CodexPipes) -> CapturedCodexIo {
    fn into_string(handle: Option<std::thread::JoinHandle<CapturedBytes>>) -> (String, bool) {
        let captured = handle.and_then(|h| h.join().ok()).unwrap_or(CapturedBytes {
            bytes: Vec::new(),
            truncated: false,
        });
        let mut text = String::from_utf8_lossy(&captured.bytes).into_owned();
        let mut truncated = captured.truncated;
        if text.len() > CODEX_CAPTURE_LIMIT {
            let mut end = CODEX_CAPTURE_LIMIT;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
            truncated = true;
        }
        (text, truncated)
    }
    let (stdout, stdout_truncated) = into_string(pipes.stdout);
    let (stderr, stderr_truncated) = into_string(pipes.stderr);
    CapturedCodexIo {
        stdout,
        stderr,
        stdout_truncated,
        stderr_truncated,
    }
}

/// Verify the `codex` binary is on PATH. Do not run it: `codex --version`
/// writes PATH-alias helpers into `$CODEX_HOME/tmp`.
fn ensure_codex_available() -> Result<()> {
    if command_is_on_path("codex") {
        return Ok(());
    }
    anyhow::bail!("codex not found in PATH. Install: npm install -g @openai/codex")
}

fn command_is_on_path(name: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    let candidates = if cfg!(windows) {
        vec![
            format!("{name}.exe"),
            format!("{name}.cmd"),
            format!("{name}.bat"),
            name.to_string(),
        ]
    } else {
        vec![name.to_string()]
    };
    for dir in std::env::split_paths(&paths) {
        for file in &candidates {
            if dir.join(file).is_file() {
                return true;
            }
        }
    }
    false
}

/// Codex's exit code, mapping a Unix signal death to `128 + signal`.
fn child_exit_code(status: &std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        status.code().unwrap_or_else(|| {
            use std::os::unix::process::ExitStatusExt;
            status.signal().map(|s| 128 + s).unwrap_or(1)
        })
    }
    #[cfg(not(unix))]
    {
        status.code().unwrap_or(1)
    }
}

/// Launch Codex against a custom API provider profile.
///
/// Unlike the ChatGPT path this stages nothing: the provider is applied as
/// `codex -c …` overrides and the API key is injected into the child process
/// environment under the profile's `env_key`. The saved model catalog is
/// passed to Codex without gateway discovery on the launch path. Each launch
/// gets its own Codex home under the provider directory so concurrent models
/// do not share sqlite or rewrite
/// the user's `config.toml` model keys. Prompts and skills are linked to the
/// user's `$CODEX_HOME`. Model and endpoint come from `-c`. `auth.json` is not
/// swapped.
async fn launch_provider(
    profile: ProviderProfile,
    model: Option<&str>,
    reasoning: ReasoningLaunch,
    args: Vec<String>,
    json: bool,
) -> Result<i32> {
    ensure_codex_available()?;

    let selected = profile.resolve_model(model)?.clone();
    let shown_model = passthrough_model_value(&args).unwrap_or_else(|| selected.id.clone());
    if profile.responses_support_for(&shown_model) == Some(false) {
        anyhow::bail!(
            "Model '{}' on provider '{}' has no Codex Responses channel. The saved explicit probe marked it unsupported; probe again with `codex-switch provider probe {} --model {}` after the endpoint changes.",
            shown_model,
            profile.alias,
            profile.alias,
            shown_model,
        );
    }
    let (env_name, env_value) = profile.launch_env();
    let mut session = provider::ProviderCodexHome::begin(&profile.alias)?;
    let overrides =
        profile.codex_config_args_from_saved_catalog_at(model, reasoning.clone(), &session.path)?;
    let codex_args = provider_codex_argv(overrides, args.clone());

    if !json {
        let reasoning_note = match &reasoning {
            ReasoningLaunch::Saved => String::new(),
            ReasoningLaunch::Skip => " reasoning=(skip)".to_string(),
            ReasoningLaunch::Effort(effort) => format!(" reasoning={effort}"),
        };
        user_println(&format!(
            "Launching Codex with provider '{}' ({}{})...",
            profile.alias, shown_model, reasoning_note
        ));
    }

    let mut child = match spawn_codex(
        &codex_args,
        Some((env_name, env_value)),
        json,
        Some(&session.path),
    ) {
        Ok(child) => child,
        Err(err) => {
            session
                .restore()
                .context("merging Codex config after spawn failure")?;
            return Err(err).context("Failed to start Codex");
        }
    };
    let pipes = take_codex_pipes(&mut child, json);

    let wait = child.wait();
    session
        .restore()
        .context("merging Codex config after provider launch")?;
    let status = wait.context("waiting for Codex")?;
    let captured = join_codex_pipes(pipes);
    let exit_code = child_exit_code(&status);

    if json {
        print_json(&serde_json::json!({
            "ok": status.success(),
            "alias": profile.alias,
            "action": "launched",
            "provider": profile.provider_id,
            "model": shown_model,
            "exit_code": exit_code,
            "codex_stdout": captured.stdout,
            "codex_stderr": captured.stderr,
            "codex_stdout_truncated": captured.stdout_truncated,
            "codex_stderr_truncated": captured.stderr_truncated,
        }));
    } else {
        user_println("codex exited");
    }

    Ok(exit_code)
}

/// Snapshot the live auth.json into `backup` before it is overwritten by the
/// staged profile.
///
/// Written via `atomic_write_private` (temp file + rename) rather than
/// `std::fs::copy`, and for the same reason `restore_launch_auth` avoids it:
/// this file holds a one-time-use `refresh_token`, so a truncated copy left
/// behind by a mid-write crash is unrecoverable without a fresh login.
fn backup_launch_auth(codex_auth: &std::path::Path, backup: &std::path::Path) -> Result<()> {
    let original = std::fs::read(codex_auth)
        .with_context(|| format!("reading {} for backup", codex_auth.display()))?;
    auth::atomic_write_private(backup, &original)
        .with_context(|| format!("backing up {}", codex_auth.display()))
}

/// Roll the staged profile back out of the live auth.json, keeping anything
/// Codex refreshed while it was staged.
///
/// `alias` is the profile that was staged, i.e. the owner of whatever Codex may
/// have rewritten in place.
fn restore_launch_auth(
    codex_auth: &std::path::Path,
    backup: &std::path::Path,
    had_original: bool,
    alias: &str,
) -> Result<()> {
    let _lock = profile::lock_live_auth().context("acquiring auth lock for restore")?;
    match preserve_refreshed_launch_auth(codex_auth, alias) {
        Ok(true) => user_println(&format!(
            "Codex refreshed the credentials of profile '{alias}'; saved them before restoring."
        )),
        Ok(false) => {}
        // An error here means the live file holds credentials newer than the
        // profile's that could not be stored: either they belong to another
        // account, or the write failed. Rolling back would overwrite — or with
        // no original, delete — the only copy the auth server still accepts,
        // and rotation makes that irreversible. Leaving the live file in place
        // is the recoverable outcome: `codex-switch use` fixes a wrong account,
        // nothing fixes a destroyed token.
        Err(err) => {
            return Err(err).with_context(|| {
                let recovery = if had_original {
                    format!(
                        "The pre-launch auth.json is kept at {}, so nothing is lost: save the \
                         live credentials with `codex-switch import {}`, then restore that \
                         backup by hand.",
                        backup.display(),
                        codex_auth.display()
                    )
                } else {
                    format!(
                        "There was no pre-launch auth.json, so deleting this file would lose \
                         these credentials outright: save them with `codex-switch import {}`.",
                        codex_auth.display()
                    )
                };
                format!(
                    "refusing to roll back {}: it holds newer credentials that could not be \
                     saved into profile '{alias}'. {recovery}",
                    codex_auth.display()
                )
            });
        }
    }
    if had_original {
        let saved = std::fs::read(backup)
            .with_context(|| format!("reading launch auth backup {}", backup.display()))?;
        auth::atomic_write_private(codex_auth, &saved).with_context(|| {
            format!(
                "restoring launch auth backup {} -> {}",
                backup.display(),
                codex_auth.display()
            )
        })?;
        std::fs::remove_file(backup)
            .with_context(|| format!("removing launch auth backup {}", backup.display()))?;
    } else if codex_auth.exists() {
        std::fs::remove_file(codex_auth)
            .with_context(|| format!("removing staged launch auth {}", codex_auth.display()))?;
    }
    Ok(())
}

/// Fold credentials Codex refreshed in place back into the staged profile.
///
/// Codex CLI refreshes on startup when the staged `last_refresh` is old enough,
/// and OpenAI rotates `refresh_token` on every use: the moment Codex refreshes,
/// the copy still stored in the profile is revoked. Restoring the backup over
/// that write would leave the profile holding a dead token — unrecoverable
/// without a full re-login, and undetectable until the profile is next used.
///
/// Returns whether the profile was updated. Nothing is written unless the live
/// file proves it is both newer than the profile and the same account, so a
/// stale or foreign live copy can never overwrite good credentials.
///
/// Caller MUST hold the lock from `lock_live_auth()`.
fn preserve_refreshed_launch_auth(codex_auth: &std::path::Path, alias: &str) -> Result<bool> {
    if !codex_auth.exists() {
        return Ok(false);
    }
    let profile_path = profile::profile_auth_path(alias)?;
    if !profile_path.exists() {
        return Ok(false);
    }
    let saved = auth::read_auth(&profile_path)
        .with_context(|| format!("reading profile '{alias}' auth.json"))?;
    let live = auth::read_auth(codex_auth).with_context(|| {
        format!(
            "reading live auth.json {} before launch restore",
            codex_auth.display()
        )
    })?;
    if !live_is_newer(&saved, &live) {
        return Ok(false);
    }
    ensure_same_account(alias, &saved, &live)?;
    // Managed Codex policy may change while the launched process is running.
    // Re-evaluate at the final credential-write boundary, after identity
    // checks but before the rotated token reaches the profile store.
    auth::validate_managed_auth_value(&live)?;
    auth::write_auth(&profile_path, &live)
        .with_context(|| format!("saving refreshed credentials into profile '{alias}'"))?;
    Ok(true)
}

/// `last_refresh` is the same RFC3339 stamp Codex and codex-switch both write,
/// so a strictly later value is the evidence that Codex rotated the tokens.
/// A profile without a stamp loses to any live file that has one, because the
/// staged copy came from that profile and therefore had no stamp either.
fn live_is_newer(saved: &serde_json::Value, live: &serde_json::Value) -> bool {
    let Some(live_ts) = last_refresh(live) else {
        return false;
    };
    match last_refresh(saved) {
        Some(saved_ts) => live_ts > saved_ts,
        None => true,
    }
}

fn last_refresh(val: &serde_json::Value) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    chrono::DateTime::parse_from_rfc3339(val.get("last_refresh")?.as_str()?).ok()
}

/// Same rule as `profile::update_profile_from_live`: the email must be present
/// on both sides and equal, and account ids must agree when both are known.
fn ensure_same_account(
    alias: &str,
    saved: &serde_json::Value,
    live: &serde_json::Value,
) -> Result<()> {
    let saved = profile::extract_identity(saved);
    let live = profile::extract_identity(live);
    let email_matches = matches!(
        (&saved.email, &live.email),
        (Some(saved), Some(live)) if saved == live
    );
    let account_matches = match (&saved.account_id, &live.account_id) {
        (Some(saved), Some(live)) => saved == live,
        _ => true,
    };
    if email_matches && account_matches {
        return Ok(());
    }
    anyhow::bail!(
        "live auth.json was refreshed into a different account than profile '{alias}'; \
         leaving the profile untouched"
    )
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::sync::MutexGuard;

    use super::{
        chatgpt_codex_argv, ensure_codex_available, passthrough_model_value, provider_codex_argv,
        restore_launch_auth,
    };
    // Only the permission assertions call this, and those are unix-only, so an
    // unconditional import is dead on Windows and fails `-D warnings` there.
    #[cfg(unix)]
    use super::backup_launch_auth;

    #[test]
    fn chatgpt_argv_puts_model_after_a_codex_subcommand() {
        assert_eq!(
            chatgpt_codex_argv(
                Some("gpt-5.4"),
                vec!["exec".into(), "--json".into(), "hi".into()]
            ),
            ["exec", "--model", "gpt-5.4", "--json", "hi"]
        );
    }

    #[test]
    fn ensure_codex_available_looks_up_path_without_running_codex() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let fake = if cfg!(windows) {
            dir.path().join("codex.exe")
        } else {
            dir.path().join("codex")
        };
        std::fs::write(&fake, b"").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let previous = std::env::var_os("PATH");
        unsafe {
            std::env::set_var("PATH", dir.path());
        }
        struct Restore(Option<std::ffi::OsString>);
        impl Drop for Restore {
            fn drop(&mut self) {
                unsafe {
                    match &self.0 {
                        Some(value) => std::env::set_var("PATH", value),
                        None => std::env::remove_var("PATH"),
                    }
                }
            }
        }
        let _restore = Restore(previous);
        ensure_codex_available().expect("a PATH file named codex is enough; do not run it");
    }

    #[test]
    fn ensure_codex_available_fails_when_codex_is_not_on_path() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let empty = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("PATH");
        unsafe {
            std::env::set_var("PATH", empty.path());
        }
        struct Restore(Option<std::ffi::OsString>);
        impl Drop for Restore {
            fn drop(&mut self) {
                unsafe {
                    match &self.0 {
                        Some(value) => std::env::set_var("PATH", value),
                        None => std::env::remove_var("PATH"),
                    }
                }
            }
        }
        let _restore = Restore(previous);
        let err = ensure_codex_available().unwrap_err().to_string();
        assert!(
            err.contains("codex not found in PATH"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn chatgpt_argv_without_model_is_passthrough_only() {
        assert_eq!(
            chatgpt_codex_argv(None, vec!["resume".into(), "--last".into()]),
            ["resume", "--last"]
        );
    }

    #[test]
    fn passthrough_model_value_reads_long_short_and_equals_forms() {
        assert_eq!(
            passthrough_model_value(&["exec".into(), "--model".into(), "one-shot".into()]),
            Some("one-shot".into())
        );
        assert_eq!(
            passthrough_model_value(&["-m".into(), "one-shot".into(), "exec".into()]),
            Some("one-shot".into())
        );
        assert_eq!(
            passthrough_model_value(&["--model=one-shot".into(), "exec".into()]),
            Some("one-shot".into())
        );
        assert_eq!(
            passthrough_model_value(&["--".into(), "--model".into(), "not-a-flag".into()]),
            None
        );
    }

    #[test]
    fn provider_argv_puts_overrides_after_exec() {
        let argv = provider_codex_argv(
            vec![
                "-c".into(),
                r#"model="saved""#.into(),
                "-c".into(),
                r#"model_provider="p""#.into(),
            ],
            vec!["exec".into(), "--json".into(), "do".into()],
        );
        assert_eq!(
            argv,
            [
                "exec",
                "-c",
                r#"model="saved""#,
                "-c",
                r#"model_provider="p""#,
                "--json",
                "do"
            ]
        );
    }

    #[test]
    fn provider_argv_keeps_overrides_in_front_without_subcommand() {
        let argv = provider_codex_argv(
            vec!["-c".into(), r#"model_provider="p""#.into()],
            vec!["-s".into(), "read-only".into()],
        );
        assert_eq!(argv, ["-c", r#"model_provider="p""#, "-s", "read-only"]);
    }

    #[test]
    fn provider_argv_drops_per_model_overrides_when_passthrough_sets_model() {
        let argv = provider_codex_argv(
            vec![
                "-c".into(),
                r#"model_provider="p""#.into(),
                "-c".into(),
                r#"model="saved""#.into(),
                "-c".into(),
                "model_reasoning_effort=high".into(),
                "-c".into(),
                "web_search=disabled".into(),
                "-c".into(),
                r#"model_providers.p.base_url="https://example.test""#.into(),
            ],
            vec!["exec".into(), "--model".into(), "one-shot".into()],
        );
        assert_eq!(
            argv,
            [
                "exec",
                "-c",
                r#"model_provider="p""#,
                "-c",
                r#"model_providers.p.base_url="https://example.test""#,
                "--model",
                "one-shot",
            ]
        );
    }

    #[test]
    fn provider_argv_moves_flags_that_precede_exec_after_the_subcommand() {
        let argv = provider_codex_argv(
            vec![
                "-c".into(),
                r#"model_provider="p""#.into(),
                "-c".into(),
                r#"model="saved""#.into(),
            ],
            vec!["-m".into(), "one-shot".into(), "exec".into(), "hi".into()],
        );
        assert_eq!(
            argv,
            [
                "exec",
                "-c",
                r#"model_provider="p""#,
                "-m",
                "one-shot",
                "hi",
            ]
        );
    }

    #[test]
    fn provider_argv_keeps_c_model_when_double_dash_makes_model_a_prompt() {
        let argv = provider_codex_argv(
            vec!["-c".into(), r#"model="saved""#.into()],
            vec!["--".into(), "--model".into(), "not-a-flag".into()],
        );
        assert_eq!(
            argv,
            ["-c", r#"model="saved""#, "--", "--model", "not-a-flag"]
        );
    }

    /// Staging moves the user's live `auth.json` aside and puts a profile's
    /// credentials in its place; the restore that undoes it only runs once the
    /// wait below returns. A Ctrl+C pressed *during* staging therefore lands
    /// before the wait starts polling — and if the listener is created inside
    /// the wait, tokio has nothing registered at broadcast time, discards the
    /// signal's record of itself, and the default terminate action kills the
    /// process with the staged profile still live and the original stranded in
    /// a `.bak` file the user never sees named.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_interrupt_arriving_during_staging_still_triggers_the_restore() {
        use super::{LaunchWait, wait_for_codex_to_read_auth};
        use crate::signals::{RAISE_LOCK, ShutdownListener};
        use std::time::Duration;
        use tokio::signal::unix::{SignalKind, signal};

        let _raise = RAISE_LOCK.lock().await;
        // Registered where `launch_cmd` registers it: before the first byte of
        // the user's auth.json is touched.
        let mut interrupt = ShutdownListener::interrupt_only().expect("interrupt listener");

        // Turns "tokio finished broadcasting" into an awaitable event so the
        // assertion never depends on sleeping long enough.
        let mut witness = signal(SignalKind::interrupt()).expect("witness listener");

        // SAFETY: raising SIGINT at our own process, with both listeners above
        // already registered, so the default terminate action cannot fire.
        // This stands in for the staging window: the signal lands well before
        // anything polls for it.
        assert_eq!(unsafe { libc::raise(libc::SIGINT) }, 0);
        witness.recv().await;

        // A delay long enough that returning `Elapsed` is impossible.
        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_codex_to_read_auth(&mut interrupt, Duration::from_secs(600)),
        )
        .await
        .expect("the wait must observe an interrupt that predates it");
        assert_eq!(
            outcome,
            LaunchWait::Interrupted,
            "the restore has to run, so the wait must report the interrupt"
        );
    }

    /// The ordinary path: nothing interrupts, so the wait just times out and
    /// the restore runs on schedule.
    #[tokio::test]
    async fn an_uninterrupted_wait_reports_the_elapsed_delay() {
        use super::{LaunchWait, wait_for_codex_to_read_auth};
        use crate::signals::{RAISE_LOCK, ShutdownListener};
        use std::time::Duration;

        // Not raising anything, but a sibling test does, and a raise is
        // process-wide: without the lock this listener can catch it.
        let _raise = RAISE_LOCK.lock().await;
        let mut interrupt = ShutdownListener::interrupt_only().expect("interrupt listener");
        assert_eq!(
            wait_for_codex_to_read_auth(&mut interrupt, Duration::from_millis(10)).await,
            LaunchWait::Elapsed
        );
    }

    struct TestAppHome {
        _lock: MutexGuard<'static, ()>,
        home: tempfile::TempDir,
        previous: Option<OsString>,
    }

    impl TestAppHome {
        fn new() -> Self {
            let lock = crate::profile::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let home = tempfile::tempdir().unwrap();
            let previous = std::env::var_os("CODEX_SWITCH_HOME");
            unsafe {
                std::env::set_var("CODEX_SWITCH_HOME", home.path());
            }
            Self {
                _lock: lock,
                home,
                previous,
            }
        }

        fn path(&self) -> &std::path::Path {
            self.home.path()
        }
    }

    impl Drop for TestAppHome {
        fn drop(&mut self) {
            unsafe {
                match &self.previous {
                    Some(value) => std::env::set_var("CODEX_SWITCH_HOME", value),
                    None => std::env::remove_var("CODEX_SWITCH_HOME"),
                }
            }
        }
    }

    #[test]
    fn restore_launch_auth_restores_original_and_removes_backup() {
        let home = TestAppHome::new();
        let codex_auth = home.path().join("codex/auth.json");
        let backup = home.path().join("auth.backup");
        std::fs::create_dir_all(codex_auth.parent().unwrap()).unwrap();
        std::fs::write(&codex_auth, b"staged profile").unwrap();
        std::fs::write(&backup, b"original auth").unwrap();

        restore_launch_auth(&codex_auth, &backup, true, "work").unwrap();

        assert_eq!(std::fs::read(&codex_auth).unwrap(), b"original auth");
        assert!(!backup.exists());
        assert!(home.path().join("auth.lock").exists());
    }

    #[test]
    fn restore_launch_auth_removes_staged_auth_without_original() {
        let home = TestAppHome::new();
        let codex_auth = home.path().join("codex/auth.json");
        let backup = home.path().join("auth.backup");
        std::fs::create_dir_all(codex_auth.parent().unwrap()).unwrap();
        std::fs::write(&codex_auth, b"staged profile").unwrap();

        restore_launch_auth(&codex_auth, &backup, false, "work").unwrap();

        assert!(!codex_auth.exists());
        assert!(!backup.exists());
    }

    #[test]
    fn restore_launch_auth_without_original_or_staged_file_is_noop() {
        let home = TestAppHome::new();
        let codex_auth = home.path().join("codex/auth.json");
        let backup = home.path().join("auth.backup");

        restore_launch_auth(&codex_auth, &backup, false, "work").unwrap();

        assert!(!codex_auth.exists());
        assert!(!backup.exists());
    }

    // ── Atomic write contract ───────────────────────────────────────
    //
    // Both the backup and the restore write the live auth.json, which holds a
    // one-time-use refresh_token: a crash mid-write must never leave a
    // truncated file, and the file must never be group/world readable. These
    // are the two observable differences between `atomic_write_private` and
    // `std::fs::copy` (which preserves source permissions and copies bytes
    // in place rather than via a temp file + rename), so we assert on them
    // rather than trying to simulate a crash directly.

    #[cfg(unix)]
    fn mode(path: &std::path::Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    #[test]
    fn backup_launch_auth_writes_backup_with_private_permissions() {
        let home = TestAppHome::new();
        let codex_auth = home.path().join("codex/auth.json");
        let backup = home.path().join("auth.backup");
        std::fs::create_dir_all(codex_auth.parent().unwrap()).unwrap();
        // Default `fs::write` permissions (governed by umask) are not 0600,
        // so this only passes if the backup path went through the private
        // atomic writer rather than a permission-preserving copy.
        std::fs::write(&codex_auth, b"live credentials").unwrap();

        backup_launch_auth(&codex_auth, &backup).unwrap();

        assert_eq!(std::fs::read(&backup).unwrap(), b"live credentials");
        assert_eq!(mode(&backup), 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn restore_launch_auth_writes_target_with_private_permissions() {
        let home = TestAppHome::new();
        let codex_auth = home.path().join("codex/auth.json");
        let backup = home.path().join("auth.backup");
        std::fs::create_dir_all(codex_auth.parent().unwrap()).unwrap();
        std::fs::write(&codex_auth, b"staged profile").unwrap();
        std::fs::write(&backup, b"original auth").unwrap();

        restore_launch_auth(&codex_auth, &backup, true, "work").unwrap();

        assert_eq!(std::fs::read(&codex_auth).unwrap(), b"original auth");
        assert_eq!(mode(&codex_auth), 0o600);
    }

    #[test]
    fn restore_launch_auth_leaves_no_stray_files_when_target_already_existed() {
        let home = TestAppHome::new();
        let codex_dir = home.path().join("codex");
        let codex_auth = codex_dir.join("auth.json");
        let backup = home.path().join("auth.backup");
        std::fs::create_dir_all(&codex_dir).unwrap();
        std::fs::write(&codex_auth, b"staged profile").unwrap();
        std::fs::write(&backup, b"original auth").unwrap();

        restore_launch_auth(&codex_auth, &backup, true, "work").unwrap();

        let entries: Vec<_> = std::fs::read_dir(&codex_dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(
            entries,
            vec![std::ffi::OsString::from("auth.json")],
            "no leftover temp file should remain next to the restored auth.json"
        );
    }

    // ── Codex-side refresh during the launch window ───────────────
    //
    // Codex CLI refreshes a staged auth.json whose `last_refresh` is old
    // enough, and OpenAI revokes the old refresh_token the moment it is used.
    // The restore must therefore fold a newer live copy back into the profile
    // instead of rolling the backup over it.

    fn jwt(payload: &serde_json::Value) -> String {
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        format!(
            "x.{}.y",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(payload).unwrap())
        )
    }

    /// `account` seeds both the email and the account id, so two calls with the
    /// same `account` describe the same ChatGPT account.
    fn auth_value(account: &str, refresh_token: &str, last_refresh: &str) -> serde_json::Value {
        let email = format!("{account}@example.com");
        let account_id = format!("acct-{account}");
        let claims = serde_json::json!({
            "email": email,
            "https://api.openai.com/auth": {
                "chatgpt_plan_type": "plus",
                "chatgpt_account_id": account_id,
                "chatgpt_user_id": format!("user_{account_id}"),
            }
        });
        serde_json::json!({
            "tokens": {
                "id_token": jwt(&claims),
                "access_token": format!("access-{refresh_token}"),
                "refresh_token": refresh_token,
                "account_id": account_id,
            },
            "last_refresh": last_refresh,
        })
    }

    /// Profile "work" plus a staged live file holding the same credentials,
    /// mirroring the state `stage_profile_auth` leaves behind.
    fn staged_launch(
        home: &TestAppHome,
        profile_value: &serde_json::Value,
    ) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
        let profile_path = crate::profile::profile_auth_path("work").unwrap();
        std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        crate::auth::write_auth(&profile_path, profile_value).unwrap();

        let codex_auth = home.path().join("codex/auth.json");
        std::fs::create_dir_all(codex_auth.parent().unwrap()).unwrap();
        crate::auth::write_auth(&codex_auth, profile_value).unwrap();

        let backup = home.path().join("auth.backup");
        crate::auth::write_auth(
            &backup,
            &auth_value("other", "other-refresh", "2026-07-01T00:00:00Z"),
        )
        .unwrap();

        (profile_path, codex_auth, backup)
    }

    fn read_json(path: &std::path::Path) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn restore_saves_credentials_codex_refreshed_during_launch() {
        let home = TestAppHome::new();
        let staged = auth_value("a", "refresh-old", "2026-07-01T00:00:00Z");
        let (profile_path, codex_auth, backup) = staged_launch(&home, &staged);

        // Codex rotated the token in place while it was staged.
        let refreshed = auth_value("a", "refresh-new", "2026-07-20T10:00:00Z");
        crate::auth::write_auth(&codex_auth, &refreshed).unwrap();

        restore_launch_auth(&codex_auth, &backup, true, "work").unwrap();

        assert_eq!(
            read_json(&profile_path),
            refreshed,
            "the rotated refresh_token must survive the restore"
        );
        assert_eq!(
            read_json(&codex_auth)["tokens"]["refresh_token"],
            "other-refresh",
            "the original live credentials must still be restored"
        );
        assert!(!backup.exists());
    }

    #[test]
    fn restore_rechecks_managed_workspace_policy_before_saving_refreshed_credentials() {
        let home = TestAppHome::new();
        let staged = auth_value("allowed", "refresh-old", "2026-07-01T00:00:00Z");
        let (profile_path, codex_auth, backup) = staged_launch(&home, &staged);
        let refreshed = auth_value("allowed", "refresh-new", "2026-07-20T10:00:00Z");
        crate::auth::write_auth(&codex_auth, &refreshed).unwrap();

        let codex_home = home.path().join("codex");
        std::fs::write(
            codex_home.join("config.toml"),
            "forced_login_method = \"chatgpt\"\nforced_chatgpt_workspace_id = \"acct-blocked\"\n",
        )
        .unwrap();
        let previous_codex_home = std::env::var_os("CODEX_HOME");
        unsafe {
            std::env::set_var("CODEX_HOME", &codex_home);
        }
        let result = restore_launch_auth(&codex_auth, &backup, true, "work");
        unsafe {
            match previous_codex_home {
                Some(value) => std::env::set_var("CODEX_HOME", value),
                None => std::env::remove_var("CODEX_HOME"),
            }
        }

        let err = result.expect_err("policy changes during launch must fail closed");
        assert!(format!("{err:#}").contains("not allowed"));
        assert_eq!(read_json(&profile_path), staged);
        assert_eq!(
            read_json(&codex_auth),
            refreshed,
            "the only rotated credential copy must remain recoverable"
        );
        assert!(backup.exists());
    }

    #[test]
    fn restore_saves_refreshed_credentials_when_there_was_no_original() {
        let home = TestAppHome::new();
        let staged = auth_value("a", "refresh-old", "2026-07-01T00:00:00Z");
        let (profile_path, codex_auth, backup) = staged_launch(&home, &staged);
        std::fs::remove_file(&backup).unwrap();

        let refreshed = auth_value("a", "refresh-new", "2026-07-20T10:00:00Z");
        crate::auth::write_auth(&codex_auth, &refreshed).unwrap();

        restore_launch_auth(&codex_auth, &backup, false, "work").unwrap();

        assert_eq!(read_json(&profile_path), refreshed);
        assert!(!codex_auth.exists());
    }

    #[test]
    fn restore_leaves_profile_untouched_when_codex_did_not_refresh() {
        let home = TestAppHome::new();
        let staged = auth_value("a", "refresh-old", "2026-07-01T00:00:00Z");
        let (profile_path, codex_auth, backup) = staged_launch(&home, &staged);

        restore_launch_auth(&codex_auth, &backup, true, "work").unwrap();

        assert_eq!(read_json(&profile_path), staged);
        assert_eq!(
            read_json(&codex_auth)["tokens"]["refresh_token"],
            "other-refresh"
        );
        assert!(!backup.exists());
    }

    #[test]
    fn restore_ignores_live_credentials_older_than_the_profile() {
        let home = TestAppHome::new();
        let staged = auth_value("a", "refresh-new", "2026-07-20T10:00:00Z");
        let (profile_path, codex_auth, backup) = staged_launch(&home, &staged);

        // A stale copy of the same account must never be written back.
        crate::auth::write_auth(
            &codex_auth,
            &auth_value("a", "refresh-dead", "2026-07-01T00:00:00Z"),
        )
        .unwrap();

        restore_launch_auth(&codex_auth, &backup, true, "work").unwrap();

        assert_eq!(read_json(&profile_path), staged);
    }

    // ── Rollback must never destroy credentials it could not archive ──
    //
    // Once preserving fails, the live auth.json may hold the only refresh_token
    // that still works (OpenAI revokes the previous one the moment Codex uses
    // it). Rolling the backup over it, or deleting it, is unrecoverable; the
    // cost of *not* rolling back is one `codex-switch use <alias>`.

    #[test]
    fn restore_keeps_live_credentials_it_could_not_preserve() {
        let home = TestAppHome::new();
        let staged = auth_value("a", "refresh-old", "2026-07-01T00:00:00Z");
        let (profile_path, codex_auth, backup) = staged_launch(&home, &staged);

        // Newer, but not this profile's account: it cannot be folded into the
        // profile, and it is the only copy of whatever was logged in there.
        let foreign = auth_value("b", "refresh-b", "2026-07-20T10:00:00Z");
        crate::auth::write_auth(&codex_auth, &foreign).unwrap();

        let err = restore_launch_auth(&codex_auth, &backup, true, "work").unwrap_err();

        assert_eq!(
            read_json(&profile_path),
            staged,
            "another account's credentials must not pollute this profile"
        );
        assert_eq!(
            read_json(&codex_auth),
            foreign,
            "the rollback must not overwrite credentials it failed to archive"
        );
        assert!(
            backup.exists(),
            "the pre-launch auth.json must stay on disk so the user can converge by hand"
        );
        let msg = format!("{err:#}");
        assert!(
            msg.contains(&backup.display().to_string()) && msg.contains("codex-switch import"),
            "the refusal must name the backup and how to recover, got: {msg}"
        );
    }

    #[test]
    fn restore_keeps_live_credentials_it_could_not_preserve_without_an_original() {
        let home = TestAppHome::new();
        let staged = auth_value("a", "refresh-old", "2026-07-01T00:00:00Z");
        let (profile_path, codex_auth, backup) = staged_launch(&home, &staged);
        std::fs::remove_file(&backup).unwrap();

        let foreign = auth_value("b", "refresh-b", "2026-07-20T10:00:00Z");
        crate::auth::write_auth(&codex_auth, &foreign).unwrap();

        let err = restore_launch_auth(&codex_auth, &backup, false, "work").unwrap_err();

        assert_eq!(
            read_json(&codex_auth),
            foreign,
            "deleting the staged file would destroy the only copy of these credentials"
        );
        assert_eq!(read_json(&profile_path), staged);
        assert!(format!("{err:#}").contains("codex-switch import"));
    }
}
