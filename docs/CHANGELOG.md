# Changelog

## Unreleased

- **Provider Responses probe** — `POST {base_url}/responses` with only `model` (no `input`) tells whether Codex can use a saved slug without generating tokens. HTTP 400 at validation means the Responses handler ran; HTTP 404 `bad_response_status_code` means Chat Completions only. `codex-switch provider probe <alias> [--model <id>]` prints the verdict; provider `launch` refuses an unsupported slug instead of opening Codex onto a 404. An inconclusive probe (auth, timeout) warns and still launches.

## v20260827.4.0 — 2026-08-27

- **Pick models from a large gateway catalog** — OpenRouter-sized `GET /models` lists are not imported wholesale. `provider add --fetch-models --model …` and `provider fetch-models <alias> --model …` keep only those slugs (and must exist on the gateway). TUI `f` opens a filterable picker (`space` toggle, `/` filter, Enter apply). Small catalogs still import every chat slug.
- **Catalog reasoning only when saved** — Models without `--reasoning` get catalog `effort: none` and `supports_reasoning_summaries: false`, so Codex does not send `reasoning.effort`. That 404s some gateways (Cursor-style `composer-2.5`). Thinking models still advertise low…max when an effort is saved.
- **Provider `launch exec` `-c` placement** — Codex 0.149 applies `-c` on the subcommand (`codex exec -c …`). Flags in front of `exec` are ignored, so the child used the built-in OpenAI provider. `launch` now puts provider overrides after `exec` / `resume` / …, and moves user flags that preceded the subcommand with them. Interactive launch (no subcommand) still has them in front.

## v20260827.3.0 — 2026-08-27

- **Provider models from the gateway or by hand** — Codex `/model` lists only the models saved on the provider. Fill them with `--model`, or import chat slugs from `GET {base_url}/models` (`provider add --fetch-models`, `provider fetch-models <alias>`, TUI `f`). Embedding and reranker ids are omitted. Catalogs larger than 48 models (OpenRouter-sized) are not imported wholesale.

## v20260827.2.0 — 2026-08-27

- **Custom provider model catalog** — Launching a custom API provider writes `$CODEX_SWITCH_HOME/providers/<alias>/models.json` and passes it as `model_catalog_json`, so Codex `/model` lists the provider's slugs and has metadata instead of warning and falling back to a 272k window. Launch `GET`s `{base_url}/models` first; when that omits a window (or the call fails), metadata is filled from a fallback (default: public OpenRouter `/models`, no login; override with `--metadata-fallback` or `CODEX_SWITCH_METADATA_FALLBACK`). The Codex child uses `$CODEX_SWITCH_HOME/providers/<alias>/codex-home` as `CODEX_HOME`; the user's `~/.codex` is not read or written.

## v20260827.1.0 — 2026-08-27

- **Scheduled daemon warmup** — `[daemon] warmup_times = ["08:00", "13:10"]` (`HH:MM`, at most 10) gates background warmup when `auto_warmup` is on. Empty times keep cache-refresh warmup. Slot spacing is unrestricted. `timezone` is an IANA name (`Asia/Shanghai`); empty uses the process local timezone. Due detection catches up the latest overdue slot today only; poll backoff cannot skip a slot. TUI `W` remains a session toggle.
- **TUI Settings tab** — `Tab` cycles Accounts → Providers → Settings. The Settings page edits every `config.toml` key the product owns; `s` saves (Accounts `s` is still sort). Saving rewrites the file. Unsaved edits are kept when leaving the tab; `Tab` does not switch tabs while a field is being edited. `warmup_times` accepts a comma-separated `HH:MM` list (max 10) and stays on the add row after insert.

## v20260826.3.0 — 2026-08-26

- **TUI provider form** — Add still uses Enter for Alias → URL → Key → Models. Tab now visits env key, wire API, and extra `-c` on Add as well as Edit. Extra `-c` values may contain commas. Renaming a provider keeps a custom `env_key`; only the default `CODEX_SWITCH_<ALIAS>_KEY` is re-derived.
- **`launch` argv passthrough** — A known Codex subcommand (`exec`, `resume`, …) or a non-launch flag in the alias slot starts the Codex argv, so `codex-switch launch exec --json "…"` auto-selects instead of looking up alias `exec`. Tokens on both sides of `--` are kept (`launch work exec -- --json` still runs `exec`). Provider `-c` overrides stay in front of a Codex subcommand; a passthrough `--model` / `-m` drops the competing per-model `-c` pairs (`model`, `model_reasoning_effort`, `web_search`), and the launch banner / `--json` envelope report that one-shot model. `--json launch` captures Codex stdout/stderr into the envelope. `--full-auto` is not a current Codex flag.

## v20260826.2.0 — 2026-08-26

- **TUI launch** — Launch Codex from the TUI with `o` on both tabs: Accounts starts the selected ChatGPT profile (list or account menu); Providers opens a picker for a saved model and a one-shot reasoning override, then Enter (or `o`) starts Codex. On the Providers list, Enter also opens that picker; `e` edits. `l` remains re-login on Accounts. Codex runs in the foreground and the TUI resumes when it exits.
- **TUI provider form** — Add starts typing the alias immediately; Enter commits each field and continues to the next. Tab moves between fields; `j`/`k` move inside Models. A visible `+ add model` row adds another model (also `+` / `=` / `a`); `d` / `-` / Delete ask for confirmation before removing, and the last model cannot be removed. Edit still starts on Base URL in navigation mode so `s` is save.
- **TUI palette** — Every TUI style now sets the designed dark background with the foreground, so a light terminal (xfce4-terminal / macOS Terminal defaults) cannot wash the yellow keys and cyan headings back to black-on-white. The dashboard also keeps that palette when `NO_COLOR` is set; `NO_COLOR` still disables color on CLI output.
- **Custom API providers** — Save an OpenRouter-style endpoint under `$CODEX_SWITCH_HOME/providers/` and start Codex with `codex-switch launch <alias>`. One provider holds several models; reasoning effort and `web_search` are per model. The TUI Providers tab uses a single form dialog to add or edit (`a` / `e`), plus rename (`n`) and remove (`d`). `launch --model` selects a saved model. The API key is read from a hidden prompt or `--api-key-stdin`, stored mode `0600`, and injected into the child environment; it never appears on the command line, and `$CODEX_HOME` is not written. `use` and auto-select stay ChatGPT-only. Codex currently speaks only `wire_api = "responses"`, so Chat Completions-only vendors (including DeepSeek's official API) must be reached through a Responses-capable gateway. See [Custom API providers](wiki/Providers.md).

## v20260811.3.0 — 2026-08-11

- **Reset Card details refresh without blocking the account table** — Main Usage results render immediately while card details refresh in a serialized background queue. The Cards column shows a cyan refresh marker during work and a yellow waiting marker during HTTP 429 cooldown, while preserving the last known unexpired cards.
- **One rate limit stops every later Reset Card GET** — All card-detail readers share one paced request gate. The first HTTP 429 records the server's wait hint, opens a global cooldown, and returns without replaying; HTTP 401 also stops immediately. Only an explicit zero clears cards, and request generations prevent a stale account response from restoring cards after newer Usage data.

## v20260811.2.0 — 2026-08-11

- **Reset Card discovery** — Treat a missing main Usage reset-card summary as unknown instead of zero, then query card details through the existing serialized path so unexpired cards remain visible.
- **Reset Card 429 isolation** — Keep the detail endpoint on its bounded 1→2 second exponential retry so one limited account cannot hold the global detail queue for 30→60 seconds.

## v20260811.1.0 — 2026-08-11

- **HTTP 429 handling now slows down instead of amplifying rate limits** — The default TUI refresh interval is five minutes. Replay-safe GET requests honor `Retry-After` and response-body hints, otherwise use bounded exponential backoff with jitter; non-replayable authentication and warmup POST requests wait after a 429 without being replayed. Usage retries stop at one layer instead of multiplying across nested loops.
- **Reset-card consumption is fail-closed against duplicate charges** — One confirmation sends at most one consume POST, and ambiguous transport, rate-limit, server, or response errors require verification instead of automatic replay. Confirmations bind the exact card ID the user saw, the TUI blocks another consume after success or an unknown outcome, and a zero-wait per-profile OS lock rejects concurrent processes rather than queueing a stale confirmation that could select the next card.

## v20260810.3.0 — 2026-08-10

- **Reset-card expiry warnings are consistent inside account details** — The account detail popup now colors the Reset cards heading, available count, and expiry row with the same earliest-expiry thresholds as the account table: red below three days, yellow below seven days, and green otherwise. A failed detail fetch stays yellow, while a positive count without a trustworthy expiry stays neutral instead of being misreported as safely green.

## v20260810.2.0 — 2026-08-10

- **Reset-card detail refresh avoids endpoint-wide rate limits** — Card details are requested only when the primary usage response reports missing Card records, and those secondary requests are serialized across accounts. HTTP 429 retries now honor numeric `Retry-After` values and otherwise use exponential backoff, preventing a six-account refresh from repeatedly stampeding the auxiliary endpoint.

## v20260810.1.0 — 2026-08-10

- **Reset cards survive transient detail-fetch failures** — The reset-card endpoint now retries transport failures, HTTP 429/5xx responses, and malformed JSON up to three times. If all attempts fail, the TUI keeps a count-matched cached card list while still reporting that fresh details are unavailable, so one flaky secondary request no longer replaces useful Card data with `err`.
- **Reset cards warn before they expire** — The TUI Card count turns yellow when the earliest valid card has less than seven days remaining and red below three days, making the warning visible without opening the account menu.
- **Dependency and automation maintenance** — Updated `libc`, `thiserror`, `toml`, `serde_json`, and `serde`, and configured Dependabot to target the active `dev` branch instead of the repository default branch.

## v20260804.1.0 — 2026-08-04

- **`self-update --version` rejects anything that is not a version number** — The argument becomes a path segment in a GitHub release lookup, and `..` inside a URL path is resolved rather than treated as text, so a malformed value could have pointed the lookup at another repository's release metadata. The value is now both percent-encoded and rejected outright before any request is made, and a typo is reported as a bad argument instead of surfacing as a confusing 404.
- **`launch.restore_delay_secs = 0` is corrected instead of silently obeyed** — At zero the original `auth.json` was restored before Codex had finished reading the staged one, so the session ran on the wrong account with nothing reporting it. Zero now falls back to three seconds and reports the correction at startup, matching how every other interval in the configuration is already treated.
- **Warmup asks for the model list once, and asks again when the account's pools change** — A warmup previously fetched `/models` twice and discarded the second answer whenever the account had no additional quota pool. The whole resolved set is now cached from a single response. The cache is keyed by the quota pools it was built from, so an account that gains a model pool while a long-running daemon is warming it up gets that new pool warmed rather than continuing on the pool list frozen at the daemon's first warmup.
- **Log retention runs on a budget instead of on every record** — Enforcement previously walked the log directory once per written record. It now runs when either a minute has passed or a megabyte has been appended, whichever comes first, which bounds how far the directory can drift past its size limit to a single byte budget.
- **Two credential switches in the same second both keep a backup** — Backup files are stamped with nanoseconds rather than seconds, so a `use` immediately followed by a `launch` no longer has the second backup overwrite the first and quietly leave fewer real recovery points than the retention limit promises. Existing second-resolution backups continue to sort correctly alongside the new names.
- **The Windows installer no longer risks adding an empty PATH entry** — The user `PATH` is rebuilt from its entries instead of being concatenated. A user with no existing user-scoped `PATH`, or one ending in a separator, previously ended up with an empty element, which Windows resolves to the current working directory when searching for an executable. Uninstalling removes the entry by exact match rather than by wildcard pattern.
- **Workflow action pins are consistent across CI and release** — `actions/checkout` and `dtolnay/rust-toolchain` were pinned to two different commits under one version comment, so the audited commit was ambiguous and CI and release could resolve different toolchains. Every workflow now pins the same commit, and each pin's comment names the exact release it refers to.

## v20260731.1.0 — 2026-07-31

- **Interactive sign-in waits for the user instead of a retry counter** — Browser login now allows ten minutes for its callback, while device-code login uses its full fifteen-minute lifetime. Pending and transient device responses continue until that hard deadline, deterministic rejection still fails immediately, and Ctrl+C stops a whole TUI batch rather than advancing to the next account.

## v20260730.3.0 — 2026-07-30

- **Cross-day stable promotion is explicit** — The release guide now defines the calendar component as the date the final dev version is allocated, requires the same accepted commit and version to be promoted even when the stable tag is created on a later day, and uses full branch refspecs in the stable push example.

## v20260730.2.0 — 2026-07-30

- **The daemon no longer switches credentials underneath an active Codex session** — Process detection now classifies the actual Codex subcommand instead of scanning every prompt word for infrastructure names such as `login` or `mcp-server`. Unix reads structured process arguments where the platform exposes them, while Windows follows Win32 quoting and backslash rules, so quoted prompts and attached global option values retain their intended boundaries.
- **Concurrent opportunistic refreshes now share one HTTP client** — The refresh batch builds the client once before its start budget begins and clones it into each task, instead of repeating synchronous TLS and proxy initialization inside every spawned task. This preserves the rule that the budget only limits opening new rotations and still waits for every started single-use refresh-token rotation to be saved through the existing compare-and-swap boundary.

## v20260730.1.0 — 2026-07-30

- **TUI refresh is incremental instead of blanking the screen** — Reloads keep the last known quota visible while each account refreshes independently. Request generations discard late results from an older profile list, and one coalesced pending refresh preserves a stronger manual request without spawning duplicate work.
- **Credential writes now enforce identity and workspace policy at the final boundary** — Usage API validation proves that an imported bearer can access a workspace, but a Team workspace ID is shared by several users and is not proof that the dump owns an existing profile. Imports therefore always create a uniquely named profile and never overwrite one. A token rotated before identity validation fails is preserved in a private, non-activatable recovery file instead of being discarded; valid rescues still create a unique profile. Switch, save, import, re-login, launch staging, launch read-back, and refresh persistence all enforce Codex's managed-workspace policy, and every refresh path uses a refresh-token compare-and-swap so a concurrent login wins.
- **Windows credential storage and daemon shutdown are safer** — Private directories and temporary files receive an exact protected DACL for the current user, SYSTEM, and Administrators before credential bytes are written; inherited and unrelated explicit ACEs are removed. Windows daemon stop sends a PID-generation-bound graceful request and never force-kills a trusted process after a timeout, because it may be completing a single-use token rotation; the command reports the draining state and asks the user to retry. v1 structured PID files remain readable during upgrade.
- **Daemon and OAuth work is bounded** — Daemon account checks honor `max_concurrent`. OAuth callback connections are parsed concurrently under a fixed limit, so several half-open localhost connections cannot serialize five-second waits ahead of the valid browser callback.
- **Self-update verifies build provenance** — Release archives are attested by the pinned GitHub Actions release workflow. The updater still verifies SHA-256, then fails closed unless `gh attestation verify` validates the Sigstore bundle for this repository, workflow, exact tag ref, and the full commit SHA currently reached by that tag. The tag is read again before replacement so a move during verification aborts the update; CI action references are pinned to full commit SHAs.

## v20260729.2.0 — 2026-07-29

- **An account that needs a new sign-in no longer costs a round trip every time you look** — When the auth server states that a profile's refresh token is spent, that answer cannot change until you sign in again. Nothing recorded it, so every `list` and every TUI refresh presented the same dead credential and waited for the same rejection, then did it once more in the background after the screen had already been drawn. The verdict is now remembered against the credential that earned it, so those accounts are drawn from cache and the background pass skips them. On a slow path to the auth server this was the whole delay: a six-profile `list` with three such accounts went from over twenty seconds to a fifth of one. Signing in again clears the record on its own, because it is keyed to the credential rather than the profile name — no sign-in path has to remember to clear anything. `--force` asks the server regardless, and only the two verdicts that name a spent credential are remembered; wording that a proxy or gateway could also produce still stops the retry loop without outliving the command.
- **Accounts on a personal plan are no longer looked up on every command** — Workspace names exist only for organisation accounts. The server confirming that a personal account has none was discarded rather than stored, and the check that decided whether to ask could not tell "never looked up" from "looked up, and there is none", so it asked again on every `list` and every TUI refresh. Confirmed absences are now recorded for a day, which is short enough that joining an organisation still surfaces its name on its own; `--force` shows it at once. A lookup that fails is still not recorded, since a network failure is not an answer.
- **Background refreshes no longer re-ask questions only a person can mean** — Bypassing the cache covered two different intentions behind one flag: wanting usage numbers that are not stale, and wanting a rejected credential presented again. The daemon's polling and cache-refresh passes wanted the first and silently got both, so a profile the server had already refused was retried on every interval with nobody reading the result. The two are now separate, and only an explicit `--force` re-presents a refused credential.

## v20260729.1.0 — 2026-07-29

- **Import rescues a rotated token even when the refreshed credentials are malformed** — Import checks the file's structure twice: once as read, and once more after validating usage against the server. That second check inspects the value the server has just refreshed, so a malformed reply failed it at a point where the source file's token had already been spent — and the failure was reported as an ordinary structural problem, discarding the only credential the server still accepts. It is now saved to a profile first, the same as every other failure that follows a rotation.
- **Ctrl+C during the `launch` swap no longer strands your credentials** — `launch` moves the live `auth.json` aside, puts a profile's credentials in its place, and restores them a few seconds later. The interrupt handler that guarantees the restore was only registered once that waiting period began, leaving the swap itself unprotected: an interrupt arriving in that window terminated the process outright, with the profile still live and the original file left under a backup name nothing had printed. The handler is now in place before the first byte moves. It listens for Ctrl+C alone, so `launch` keeps responding to `kill` normally once the restore is done.
- **A daemon that fails to start is no longer left running** — `daemon start` spawns the daemon detached and waits for it to publish its PID file. If that took longer than two seconds the wait gave up and reported a failed start, but the process was still on its way: it would come up moments later, and the obvious retry then refused with "already running". The wait now allows ten seconds — it still returns the instant the daemon is ready, so the longer bound only affects how quickly a genuinely broken start is reported — and stops the process if that passes, so a reported failure means nothing is running.
- **`daemon status` shows when polling is suspended** — After repeated failures the daemon backs off, for up to sixteen polling intervals. Nothing said so, so it read as healthy while deliberately idle, and anyone who had just fixed the cause had no way to tell how long the fix would take to take effect. Status now reports the remaining time and that a restart applies it immediately.

## v20260728.2.0 — 2026-07-28

- **`daemon stop` no longer fails silently** — The shutdown listener was rebuilt on every pass of the event loop, so while another branch was busy — the polling branch makes a full HTTP round trip — no signal handler was registered and the request to stop was discarded rather than deferred. The daemon then stayed deaf to further attempts, because the handler that replaces the default terminate behaviour remains installed. It kept polling and switching accounts after being told to stop, and only a forced kill ended it. The listener is now registered once and stays alive for the life of the loop.
- **Credentials refreshed by Codex during `launch` survive** — `launch` stages a profile, waits a few seconds and restores the previous file. Codex refreshes on startup when the stored timestamp is older than eight days, so a profile that had not been used in a while was rotated by Codex itself inside that window, and the restore then overwrote the freshly issued token with the one the server had just invalidated. The account looked fine for the rest of that session and only broke the next time it was selected. The restore now saves newer credentials belonging to the same account before it runs, on the normal, interrupted and failed-spawn paths alike. When they cannot be saved — they belong to another account, or the write fails — the rollback stops instead of proceeding, because overwriting them destroys the only copy the server still accepts, while a wrong account in place is fixed with one command.
- **A refresh already sent is never abandoned** — The background refresh of soon-to-expire tokens launched every candidate and then gave the whole batch a few seconds. Anything unfinished was cancelled, so if the server had already consumed a token but answered slowly, the replacement was thrown away and that profile was left holding a credential that no longer works. A single-use credential cannot be recalled once the request is out, so the budget now decides only whether to start another refresh; everything started is awaited to completion, and profiles never contacted keep what they have.
- **`warmup` reports a token it could not save** — Three refresh points logged a failed write and continued with the new token held in memory, so the command could report success while the only usable copy existed nowhere on disk. Each now fails that account with wording that separates a local write problem from a refresh the server refused, and the remaining accounts continue.
- **`login` writes to the profile you name** — The requested alias was consulted only when nothing matched the credentials by identity. With one address covering several workspaces this meant `login <alias>` could land on a sibling profile: the one named stayed broken while a working one had its token replaced. The alias now decides, and a profile holding a different account is refused rather than reassigned.
- **A concurrent refresh no longer looks like a dead account** — The daemon and the CLI can refresh the same profile at once. The server accepts one of them, and the other was told its token had already been used, which was reported as needing a new sign-in and removed the account from the daemon's candidates — while the winning refresh had already stored a working token. The stored credentials are now re-read once before that conclusion is drawn.
- **Import no longer discards a token it just rotated** — When validation refreshed the credentials and a later step failed, the newly issued token was dropped and the source file kept the one the server had already invalidated, reported as an ordinary skipped file. Rotated credentials are now saved before the failure is reported, and both the summary and the machine-readable report distinguish that case. A directory import also exposes a lost credential at the top level of the JSON report instead of only inside the skipped list.
- **Stale credentials cannot overwrite newer ones** — Synchronising a profile from the live file compared identity but not age, and the confirmation treated a bare Enter as yes, so an out-of-date file could replace a working token. Every write into an existing profile now passes one gate, which compares the refresh token first: identical tokens are not a conflict, and only when they differ does the timestamp decide — it must be present, readable and strictly newer. Equal, missing and unreadable timestamps report a conflict rather than being waved through, since a rewound clock could otherwise make a revoked token look like the newer one. A profile predating these timestamps is stopped at most once: `codex-switch use` pushes its credentials back, after which both sides agree and it is stamped from then on. The prompt also shows where each side stands.
- **Ambiguous accounts are no longer resolved by guessing** — With one address covering several workspaces, the automatic read-back, `codex-switch save` and the import writer all picked the first candidate in sort order and could write to the wrong profile. They now list the candidates and ask for an explicit choice — and an alias given on the command line is obeyed rather than overridden by the identity lookup, which previously sent users down the very path the ambiguity message recommended.
- **Sign-in survives a brief network failure** — The final token exchange made a single attempt, and the retry meant to cover transport failures never ran, because a refused connection and a dropped TLS handshake are not classified as connection errors by the HTTP client. A definite rejection still fails immediately, since the authorisation code cannot be reused.
- **Rejected refreshes explain themselves** — A refresh the server declines now reports its reason, such as a token that has already been spent, instead of an internal parsing message, and stops retrying at once rather than spending six requests per account.
- **System proxy and certificate settings are honoured** — A proxy configured in macOS System Settings or the Windows registry was ignored, and only the bundled certificate list was trusted, so an intercepting proxy — a debugging tool or a corporate gateway — broke every request while the browser worked. The operating system's settings and certificate store are now used, with the bundled list retained for environments that ship none, and a rejected certificate chain now says what to do.
- **Credential files are always written atomically** — The backup and restore around `launch` copied files in place, which could leave a truncated credential file behind if the process died mid-copy, and set permissions only as a best effort. Both now use the same private atomic write as the rest of the tool.

## v20260728.1.0 — 2026-07-28

- **Accounts no longer break after a failed usage refresh** — The auth server rotates `refresh_token` on every use and permanently rejects the previous one. When anything failed after a refresh had already succeeded — a network error, an unparseable response, a usage endpoint answering 401 — the newly issued credentials were discarded, and the retry replayed the token the server had just invalidated. A single transient failure was enough to leave a profile unusable until you signed in again. Rotated credentials are now saved before anything else can fail, and each retry presents the current token.
- **A rejected refresh says why** — Refresh failures surfaced as `invalid type: map, expected a string` because the server's error object did not fit the expected shape, hiding the actual reason. The server's own code and message are now reported, for example `refresh_token_reused: Your refresh token has already been used ... Please try signing in again`. Failures that re-signing cannot fix stop immediately instead of spending six auth round trips per account, so a listing that previously took a minute against a slow network now finishes in seconds.
- **Browser sign-in survives a network blip** — The final token exchange made a single attempt, and the retry meant to cover transport failures never fired: reqwest classifies both a refused connection and a TLS handshake that dies mid-negotiation as a request error, not a connect error. Transient transport failures and 5xx responses now retry with backoff. A 4xx still fails at once, because the authorization code is single-use and retrying it cannot succeed.
- **System proxy settings are honored on macOS and Windows** — Disabling default features had also dropped reqwest's system-proxy support, so a proxy configured in macOS System Settings or the Windows registry was ignored and connections failed in ways the browser did not. Environment variables and `--proxy` were unaffected and still take precedence.

## v20260718.1.0 — 2026-07-18

- **Dependency refresh** — Bumped `toml` from 1.1.2 to 1.1.3 (routine bugfix release, no security advisory).
- **Wiki is now the complete documentation** — `docs/wiki/` pages carry the full user and contributor guides (getting started, features, command reference, configuration, updating, troubleshooting, architecture, onboarding), rewritten against the current CLI behavior; the former `docs/FEATURES.md`, `COMMANDS.md`, `CONFIGURATION.md`, `TROUBLESHOOTING.md`, `ARCHITECTURE.md`, and `DEVELOPMENT.md` were merged into them. Newly documented behavior includes external-login detection, the full TUI keymap, log retention, log-level precedence, and the `CODEX_SWITCH_HOME` daemon-service caveat.
- **Homebrew dev hint follows the published Wiki** — The Homebrew-to-dev guidance now links to the published Wiki page, which always tracks `dev`, instead of a repository file on `master` that goes stale between stable releases.
- **Focused READMEs** — `README.md` and `README_CN.md` now cover the pitch, quick start, a breaking-changes upgrade notice (calendar versioning, user-owned Unix installs), and Wiki entry points instead of duplicating the full documentation.

## v20260714.3.0 — 2026-07-14

- **Trustworthy integration coverage** — Scoring and mock HTTP tests now exercise the production ranking, token refresh, retry, malformed-response, and failure paths instead of duplicating implementation logic in test code.
- **Boundary regression coverage** — Added focused tests for launch auth restoration, JWT plan detection, login PKCE/state generation, checksum parsing, progress rendering, and popup width limits.
- **Unicode-safe popup truncation** — Popup lines now truncate by terminal grapheme width so CJK text and VS16/ZWJ emoji stay within the requested width without splitting a displayed character.

## v20260714.2.0 — 2026-07-14

- **Reliable legacy upgrade gate on macOS 26** — Release CI now fetches exact Release metadata with the job token and serves it to the reviewed v0.0.19 fixture over loopback, while the fixture still downloads and verifies the real published assets before replacing itself.
- **Deterministic legacy runner coverage** — Pinned the legacy macOS upgrade check to macOS 26 so the `macos-latest` migration cannot silently alternate between macOS 15 and 26.

## v20260714.1.0 — 2026-07-14

- **Friendly legacy migration** — Replaced the duplicated `ERROR`/`Error:` output and internal installation jargon with one actionable setup message that preserves profiles and clearly separates the recommended user install from an intentional system install.
- **Automatic GitHub Wiki publication** — Added a least-privilege GitHub Actions workflow that publishes reviewed `docs/wiki/` pages from `dev`, skips stale runs, and uses no long-lived personal access token.
- **Version source of truth** — Added the root `VERSION` file and made release CI validate the synchronized Cargo manifest before building dev or stable artifacts.

## v20260713.6.0 — 2026-07-13

- **Repository-backed GitHub Wiki** — Added task-oriented Wiki sources with English as the canonical language, a Chinese companion entry point, and a development-release testing guide.
- **Homebrew channel guidance** — Documented stable updates, opting into direct dev builds, returning to the stable channel, and restoring Homebrew ownership without deleting profiles.
- **Correct Homebrew dev prompt** — Replaced the stale README anchor and the unusable uninstall-then-self-update command with the reviewed development-release instructions.

## v20260713.5.0 — 2026-07-13

- **Reliable legacy upgrade gate** — Release runs retry the v0.0.19 self-update check during the short GitHub Release propagation window, while still failing after a bounded timeout.

## v20260713.4.0 — 2026-07-13

- **Legacy install provenance** — Explicit Unix `--system` installs now carry a marker; markerless `/usr/local/bin` binaries are treated as legacy and `self-update` prints the matching stable/dev user-level installer command before any network request.
- **v0.0.19 upgrade gate** — Release runs now execute the official `v0.0.19` updater on macOS, Linux, and Windows and verify that it installs the newly published version.
- **One-time Unix migration** — The verified installer moves only the executable to `~/.local/bin`, removes the old `/usr/local/bin` copy and marker with one sudo authorization, and preserves all data under `~/.codex-switch`.

## v20260713.3.0 — 2026-07-13

- **Dev update compatibility** — Bumped the rolling dev base to `20260713.3.0` so clients on `20260713.2.0-dev` can receive the corrected updater guidance.
- **Path-aware self-update recovery** — Unix user installs no longer recommend `sudo`; legacy or explicit `/usr/local/bin` installs distinguish one-time installer migration from intentional `--system` updates; Windows user installs recommend closing running processes instead of Administrator PowerShell.
- **Release migration prompt** — Dev and stable GitHub Release bodies now show the matching one-time installer command for older macOS/Linux direct installs. The installer remains the sole migration owner and preserves profiles and configuration.

## v20260713.2.0 — 2026-07-13

- **Dev update compatibility** — Bumped the rolling dev base to `20260713.2.0` so clients on `20260713.1.0-dev` and earlier development builds can receive the corrected release candidate.

### Added

- **Contributor and maintainer documentation** — Added canonical English feature, architecture, development, contribution, and Wiki-maintenance guides, plus curated Wiki source pages for user and coding-agent onboarding.
- **Three-host CI quality gate** — A dedicated CI workflow now runs tests, Clippy, and debug builds on Linux, macOS, and Windows for `dev` pushes and pull requests. Linux additionally checks formatting, dependency advisories, and shell syntax; Windows parses the PowerShell installer.
- **Verified installers** — Unix and PowerShell installers now download the matching `.sha256` asset and reject malformed or mismatched checksums before extracting release content.
- **TUI home-panel toggle** — The `i` key shows or hides the compact quota panel on the home screen, keeping the account list focused while preserving a fast quota glance.
- **Session-aware daemon switching** — The daemon holds a pending switch while an interactive Codex session (`codex`, `codex resume`, `codex exec`) is running and retries on the next poll; Codex MCP servers and `app-server` hosts do not block. Configurable via `daemon.defer_switch_while_codex_running` (default on).
- **Daemon observability** — The daemon writes an atomic state snapshot (`daemon-state.json`: last poll, last switch, pending switch, last error, backoff) surfaced by `daemon status`. All commands write daily logs to `~/.codex-switch/logs/`; retention is the latest three calendar days and 10 MiB total. Failure backoff suspends only the poll timer instead of the whole loop.
- **Windows switch notifications** — `daemon.notify = true` now shows a toast on Windows (WinRT via PowerShell), matching the existing macOS and Linux notifications.
- **Unified candidate scoring** — CLI `use` and the daemon build and score switch candidates through one shared helper; the daemon now honors the API `plan_type` over stale JWT claims (plan downgrades) the same way the CLI does.
- **Plan-aware labels and colors** — CLI and TUI now recognize Go, distinguish `Pro 5×` (`prolite`) from `Pro 20×` (`pro`), normalize workspace plan names, preserve unknown backend values, and use a shared semantic color family without relying on color alone.
- **Authoritative workspace names** — Login and explicit account refreshes now mirror Codex's authenticated `accounts/check` request, match the selected account ID, cache the returned workspace name outside `auth.json`, and expose it in human and JSON output without guessing from an unrelated default organization.
- **Reset-card aware auto-switching** — `codex-switch use`/`launch` (no alias) now consider reset cards when the whole pool is exhausted: `--consume-card` (or an interactive y/N prompt) consumes the earliest-expiring card to revive an account instead of settling for an exhausted one; non-interactive/JSON runs without the flag surface a `hint` instead of consuming anything.
- **Per-model quota pools** — `list`/`use`/`best` surface `additional_rate_limits[]` pools; the TUI renders the main and every additional pool as separate 5h/7d progress bars. Warmup generically matches `codex_*` pool names to authenticated model names/slugs, so Pro 20× Spark and future model pools can each be activated without hardcoded model exceptions.

### Changed

- **User-owned direct installation** — macOS and Linux now install to `$HOME/.local/bin` by default and configure the user's shell PATH; Windows keeps its existing `%LOCALAPPDATA%` installation. Unix administrators can explicitly select `/usr/local/bin` with `--system`, while Homebrew remains package-manager owned.
- **Calendar versioning** — Release bases now use SemVer-compatible `YYYYMMDD.N.0` values, starting with `20260712.1.0`; `N` starts at 1 each day and increments for additional same-day releases. Existing `0.0.x` stable and dev builds remain directly upgradable because the calendar version sorts higher under SemVer.
- **Short dev versions** — Rolling dev releases now use `YYYYMMDD.N.0-dev` without an appended timestamp. Additional same-day releases must increment `N` before publishing.
- **Codex 0.144.1 authentication alignment** — Browser and device login follow the current Codex callback and polling contracts, refresh responses preserve omitted tokens, managed authentication policy is enforced, custom CA settings are honored, and `CODEX_HOME` uses the same empty-value fallback as Codex.
- **File credential store requirement** — `codex-switch` now requires Codex's file-backed credential store and rejects explicit `keyring`, `auto`, or `ephemeral` modes because reliable profile switching depends on the live `auth.json`.
- **Usage and reset-credit alignment** — Usage, models, and warmup requests carry workspace/FedRAMP routing headers; empty or structurally drifted usage responses are rejected; account-limited state is persisted; reset credits support no-expiry entries; consume retries reuse one redemption request ID and only `code=reset` is success.
- **Cross-platform daemon lifecycle** — launchd, systemd user services, and Windows Task Scheduler preserve `CODEX_HOME`; PID files include executable identity and an active OS lock; stale or legacy PID data is never trusted for signaling; zero daemon intervals normalize to 60/300/300 seconds.
- **Fail-fast configuration** — An existing unreadable, malformed, or dangling-symlink `config.toml` now reports the real error instead of silently starting with defaults; defaults remain limited to a genuinely missing file.
- **Internal module layout** — The oversized `usage.rs` (2,645 lines) and `main.rs` (1,740 lines) were split into focused submodules (`usage/{scoring,api,reset_credits,parse}` and `commands/`) as a pure mechanical move; public paths, behavior, and tests are unchanged.
- **TUI account information hierarchy** — The home screen keeps focused 5h/7d gauges for the main and additional quota pools. Enter opens one scrollable account-details page with identity, quota pools, reset cards, compact model capabilities, and actions.
- **Readable account metadata** — Account details label organizations and ID/access-token expiry, hide raw reset-card and organization IDs, and format displayed dates in the host system timezone with an explicit UTC offset. Each quota window uses one compact row with a visible pace marker, remaining percentage, conditional over-pace/rest hint, and local reset time; models show official allowed/default reasoning levels with semantic colors.

### Fixed

- **Accurate model-pool pace windows** — Additional quota pools that expose a single seven-day window are normalized to the 7d slot, and the TUI derives both its label and pace duration from the API window instead of assuming every primary slot is 5h.
- **Legacy direct-install updates** — The Unix installer migrates old `/usr/local/bin` direct installs without leaving a shadowing binary, and self-update now reports an unwritable install directory before downloading the release archive.
- **Warmup deprecated-model fallback** — Warmup no longer substitutes the removed hardcoded `gpt-5.3-codex` model when the official models endpoint is unavailable. Model discovery retries transient network/5xx/429 failures three times, then reports the real discovery error instead of issuing a guaranteed-invalid request; the 400 refresh path follows the same rule.
- **Cross-process auth transactions** — Switch, launch staging/restoration, refresh, warmup, re-login, import, rename, and delete paths serialize through stable OS file locks. Lock timeout reports the holder without unlinking a live lock, launch does not hold the auth write lock while the Codex child runs, and refreshed tokens cannot be written into another account after a concurrent switch.
- **Cross-process cache writes** — Cache read/modify/write operations now use an OS file lock and unique temporary files; auth, current-profile, and cache replacements use a cross-platform atomic replace path that can overwrite existing files on Windows.
- **Windows daemon detection** — Task Scheduler installation checks now honor the command exit status, and `tasklist` PID parsing reads the correct CSV column.
- **Non-interactive switching** — `--json use` and non-TTY callers now fail with an actionable error instead of emitting a hidden overwrite prompt when the live auth file is untracked.
- **Application home override** — The cross-platform `CODEX_SWITCH_HOME` environment variable replaces the internal test-only name and consistently relocates profiles, cache, locks, and daemon state.
- **Deeper Codex 0.144.1 contract alignment** — Token refreshes send the same JSON body as Codex and stamp `last_refresh`; logins persist `auth_mode` and run the post-login API key exchange; a missing `account_id` is stored as null; id_token email parsing falls back to the profile claim; forced workspaces pre-restrict the OAuth consent page via `allowed_workspace_id`; and the HTTP User-Agent matches the upstream `codex_cli_rs/<version>` shape.
- **Release supply-chain pinning** — Third-party GitHub Actions in the release workflow are pinned to commit SHAs and `cross` installs from a fixed revision.
- **Safe uninstall order** — Installers stop and remove the daemon service before deleting the binary, PATH entry, or data; service cleanup failures abort removal so a running daemon is not orphaned.
- **Documentation drift** — Removed nonexistent `use --force` and `codex --quiet` examples; documented self-update channel preservation, Windows `.zip` artifacts, Windows Task Scheduler support, daemon cache/warmup settings, and the file-auth prerequisite.
- **Safer CLI and TUI feedback** — Profile deletion now defaults to `y/N`, requires `--yes` for JSON/non-interactive callers, refuses the active profile, and archives deleted credentials for recovery; all-skipped directory imports return a structured failure and nonzero exit; device login stops after three consecutive polling failures; empty account lists, TUI add discovery, menu key handling, and error coloring now provide actionable feedback.
- **Platform-specific update guidance** — Homebrew dev-channel errors no longer recommend a mutable `master` pipe, and Windows privilege failures no longer suggest `sudo`.
- **Case-insensitive checksum verification** — `self-update` compares release SHA-256 digests case-insensitively, so an uppercase checksum file no longer rejects a valid archive; the comparison and the malformed-usage-response rejection are now locked by unit tests.
- **Nested debug-log redaction** — Sensitive token fields in `--debug` HTTP body logs are masked at any nesting depth (objects and arrays), not only at the top level.
- **Per-account warmup model cache** — The warmup model cache is keyed by account, so one account's resolved model is never reused for another plan tier (avoiding wasteful 400 retries) and cache invalidation only affects the failing account; the `codex --version` probe now runs on the blocking thread pool instead of stalling an async worker.
- **Official model metadata and additional-quota warmup** — TUI model names and reasoning capabilities now come from the authenticated Codex models response instead of UI constants; cached additional quota pools retain their warmup eligibility, including Pro 20× Spark and future matching pools, and the account page refreshes when a first-time model fetch completes.
- **Token-expiry clarity** — The account page labels JWT expiration in local time, distinguishes an expired ID token from its issuance time, and proactive refresh considers both access and ID token expiry.
- **Self-update trust model documented** — README clarifies that the release `.sha256` guards download integrity only; the trust anchor is the GitHub Release over TLS, and there is no independent code signature yet.

## v0.0.21 — 2026-07-10

### Added

- **Daemon status diagnostics JSON** — `codex-switch --json daemon status` now reports running state, PID, PID file path, stale PID cleanup status, and the active daemon config summary for scriptable diagnostics.
- **Daemon background cache refresh** — The Beta daemon now refreshes all saved profile usage into `cache.json` on `daemon.cache_refresh_interval_secs` (default 300s), and can optionally warm up inactive quota windows when `daemon.auto_warmup = true`.
- **Windows daemon service support** — `daemon install|uninstall` now uses Windows Task Scheduler (`schtasks.exe`) with an on-logon trigger, while `daemon start|stop|status` can manage Windows daemon processes through the shared PID file.

### Changed

- **Dependency refresh** — Bumped `rand` from 0.10.1 to 0.10.2.

### Fixed

- **Self-update restarts a running daemon** — `self-update` now stops a running Beta daemon before replacing the binary and restarts it afterward. Installed services use the native manager on all supported platforms: macOS LaunchAgent, Linux systemd user service, and Windows Task Scheduler.
- **Device login polling compatibility** — `login --device` now handles OAuth standard polling errors such as `authorization_pending` and `slow_down` instead of failing early. Original fix contributed by @WhymustIhaveaname in PR #44; expanded on `dev` with OpenAI nested-error handling, unknown-error retry behavior, and tests.
- **TUI OAuth output redraw** — TUI add/re-login flows now reset and clear the terminal before and after browser/device OAuth output, preventing long authorization URLs from leaving the TUI visually misaligned when control returns.

## v0.0.20 — 2026-07-02

### Added

- **Manual reset cards visibility** — Usage fetch now reads Codex manual reset card count and detailed expiry data from `rate-limit-reset-credits`, without failing the main usage request when that secondary endpoint is unavailable.
- **Reset card consume flow** — `codex-switch reset-card <alias>` and TUI `Enter > c` can consume the earliest-expiring available Codex reset card, with an explicit confirmation prompt before any consume request is sent.
- **CLI reset card display** — `codex-switch list` now prints reset card count, next expiry, and up to three card expiry lines. `--json` includes `reset_credits_available_count`, `reset_credits`, and `reset_credits_error`.
- **TUI reset card display** — Account rows show reset card counts, selected-account details stay compact, and the account context panel shows the full expiry list sorted by earliest expiry.

### Changed

- **Stable version base set to 0.0.20** — This is the formal release base after the reset card work stabilized on the rolling `dev` channel.
- **Dependency refresh** — Merged `anyhow` 1.0.103 patch update and upgraded `fs4` to 1.1.0 with the required `FileExt::lock` / `FileExt::try_lock` API adaptation.

### Fixed

- **Reset card cache invalidation after consume** — Successful consume now invalidates the alias usage cache before refreshing, so the CLI and TUI do not keep showing already-used reset cards until the cache TTL expires.

## v0.0.19 — 2026-06-29

### Fixed

- **Daemon free-account switching logic** — Free accounts have no `primary` window (7d usage is remapped to `secondary`), so `check_and_switch` now falls back to `secondary` when `primary` is absent. Previously the daemon always read 0% usage for free accounts and never triggered a switch, even when quota was exhausted.
- **Lock file seek after truncate** — `write_lock_holder` now calls `seek(0)` after `set_len(0)` to ensure the PID is written at the beginning of the file, preventing potential corruption if the file pointer was not at position zero.
- **Semaphore error handling in warmup** — `sem.acquire().await` result is now checked instead of silently unwrapped, returning a clear error if the semaphore is unexpectedly closed.

### Changed

- **Daemon candidate queries run concurrently** — `check_and_switch` now fetches usage for all candidate accounts via a `JoinSet` instead of a sequential `for` loop, reducing each poll cycle's wall-clock time from `O(N × network_timeout)` to `O(network_timeout)`.
- **`lock_live_auth` no longer blocks the async runtime** — Both `launch_cmd` call sites now wrap the blocking lock-acquire-and-file-copy sequence in `tokio::task::spawn_blocking`, preventing up to 15 seconds of tokio worker stalls.
- **`cache::get` on async hot path** — `select_best_profile` now uses the existing `cache::get_async` wrapper instead of calling the synchronous `cache::get` directly on a tokio worker thread.
- **Migrated `fs2` → `fs4`** — Replaced the unmaintained `fs2` crate (last release 2018) with its actively maintained successor `fs4`, which provides improved cross-platform file locking behavior.

## v0.0.18 — 2026-06-08

### Fixed

- **`auth.json` is now written atomically** — `write_auth` writes to a temp file (with `0600` permissions applied before the swap) and `rename`s it into place, matching the cache writer. A crash or full disk mid-write can no longer leave a truncated `auth.json` that Codex fails to parse. This path runs on every profile switch, token refresh, and relogin.
- **`launch` no longer races or leaks its auth backup** — The temporary backup uses a unique per-invocation name (PID + timestamp) so two concurrent `launch` commands cannot clobber each other's backup, and the backup is set to `0600` so other local users cannot read its tokens during the restore window. Pressing Ctrl+C during the restore delay now restores the original `auth.json` before exiting instead of leaving the staged profile in place.
- **`daemon start` reliability** — Detached start now polls for the daemon's PID file (up to 2s) instead of a fixed 200ms liveness probe, so slow disks / CI / containers no longer report a false "exited immediately". `daemon start` now returns a clear "Unix only" error on Windows rather than silently appearing to succeed while the daemon cannot be managed.
- **PID file write is atomic** — The PID is written through the just-created exclusive file handle, closing the window where a concurrent reader could see a created-but-empty PID file and mis-detect the daemon as not running.
- **TUI first-run account setup** — `codex-switch tui` no longer exits with a CLI hint when no profiles exist. The TUI now opens normally and shows an empty-state prompt ("No accounts yet. Press 'a' to add one") so users can add their first account without leaving the TUI.

### Security

- **Token-bearing debug logs are redacted** — `--debug` / `RUST_LOG=debug` output no longer prints raw access/refresh/id tokens from token-refresh and usage responses; sensitive fields are masked, so debug output is safe to share in bug reports.

### Changed

- **Dependency refresh** — Updated all transitive dependencies within their semver ranges (`rustls`, `tokio`, `hyper`, `reqwest`, and others). `cargo audit` reports no vulnerabilities.
- **Cache I/O moved off the async hot path** — On the high-concurrency usage-fetch path, cache reads/writes now run on a blocking thread so they no longer stall tokio workers when many profiles are refreshed at once.
- Resolved all outstanding `clippy` warnings.

## v0.0.17 — 2026-05-11

### Fixed

- **Warmup retry no longer depends on the `warmed_at` cache flag** — Removed the one-hour warmup success marker from `cache.json`; CLI and TUI now skip warmup only when cached or loaded usage data proves an active quota window. This prevents a 200 OK warmup ping that did not consume quota from blocking all later retries.
- **Warmup "already active" detection requires real elapsed usage** — A quota window is considered active for warmup skipping only when it has non-zero usage and has been running for at least 5 minutes. Fresh windows near the full 5h/7d duration remain retryable, covering accounts stuck around 1% usage after a no-op ping.
- **Warmup skip logic is shared** — Centralized active-window detection in `usage.rs` so CLI `codex-switch warmup` and TUI Enter > `w` use the same rules for 5h and 7d windows.
- **Pace warnings are suppressed for negligible usage** — CLI and TUI no longer show the over-pace `!` warning or yellow/red emphasis below 10% used, avoiding noisy warnings immediately after a fresh window starts.

## v0.0.16 — 2026-05-06

### Fixed

- **Warmup never sticks for some accounts ("already active or in flight" loop)** — The ChatGPT `responses` endpoint can return 200 OK on a warmup ping without actually consuming quota; `set_warmed()` was then called regardless, flagging the account as warmed in the disk cache for 1h and short-circuiting all subsequent warmup attempts. Now real usage data is authoritative: if loaded or disk-cached usage shows `used == 0`, the account is treated as not warmed regardless of the `warmed_at` flag. Affects both TUI Enter > w and CLI `codex-switch warmup`
- **ChatGPT usage API structural changes — free account 7d data restored** — The `wham/usage` API changed: free accounts now return a single 7d window in the `primary_window` slot (`limit_window_seconds: 604800`) with `secondary_window: null`, instead of the previous 5h+7d dual-window structure. The parser now reads `limit_window_seconds` to detect this layout and remaps the window to the `secondary` (7d) slot so scoring, eligibility, and display continue to work correctly. Plus/Pro accounts retain the original dual-window structure and are unaffected
- **plan_type now sourced from usage API** — Previously read exclusively from the JWT `chatgpt_plan_type` claim, which could be stale after a plan change. Now the `plan_type` field returned by the usage API is treated as authoritative and overrides the JWT value for both CLI list display and scoring (`is_free` / `is_team` gates). TUI table and account-detail popup updated accordingly; JSON `--json` output also reflects the live value
- **Credits section no longer shows red `$0.00` for Plus accounts** — The API added a `credits.has_credits` boolean; Plus/Pro accounts with no pay-per-use credits return `has_credits: false`. The parser now gates balance extraction on this field, so `credits_balance` stays `None` and the credits row is hidden rather than showing a misleading `$0.00` in red
- **`credits.balance` string format** — The API changed `balance` from a JSON number to a string (`"0"` instead of `0`). Both formats are now accepted
- **Warmup "already active" check covers free accounts** — The warmup skip logic in both TUI and CLI only checked the `primary` (5h) window; after the API change free accounts have `primary = None`, causing them to be re-warmed on every invocation. Now both `primary` and `secondary` windows are checked, so a free account with an active 7d window is correctly identified as already active
- **TUI Quota sort handles free accounts** — `get_5h_used_pct` returned `999.0` when `primary` was absent, pushing free accounts to the bottom of Quota sort regardless of their actual 7d usage. Now falls back to `secondary.used_percent` when `primary` is `None`
- **JSON output `plan` field** — `account_to_json` now accepts the live API `plan_type` as an override, so `codex-switch list --json` emits the correct plan even when the JWT claim is stale

## v0.0.15 — 2026-04-29

### Added

- **Skip warmup for accounts warmed within the last hour** — `use` and TUI both check `is_warmed`/`set_warmed` to avoid redundant warmup requests when an account was already warmed recently
- **Dynamic warmup model selection** — Warmup now resolves the target model via the API per process (cached behind a tokio `Mutex`) instead of using a hardcoded slug, keeping warmup correct as upstream models change
- **TUI Add account (`a`)** — Add a brand-new account directly from the main view; popup chooses between Browser (PKCE) and Device code OAuth flows. The TUI suspends during OAuth and re-opens with the new profile loaded
- **TUI re-Login (Enter > l)** — Re-authorize the selected profile from the Account menu; supports both Browser and Device code flows. If the profile is currently active, the live `~/.codex/auth.json` is also refreshed
- **TUI Account menu (Enter)** — Pressing Enter on a selected account opens a popup with: u Use, l re-Login, n reName, w Warmup, f reFresh this one, d Delete
- **TUI Batch menu (Enter when accounts marked)** — Pressing Enter while one or more accounts are marked opens a batch popup with: r Refresh selected, w Warmup selected, l re-Login selected (sequential), d Delete selected
- **TUI Help (`h`)** — Help popup lists every binding, grouped by section. Sources from a single `keymap` module so it stays in sync with the status bar
- **TUI popups adapt to screen size** — All popups (Help, Account menu, Batch menu, OAuth flow chooser) center on screen, clamp width/height to the terminal, support vertical scrolling with a block-character scrollbar, and fall back to a one-line message when the terminal is too small

### Changed

- **`use` no longer probes Codex process state** — Removed Codex process detection from the `use` command path; switching auth no longer depends on what Codex is doing
- **TUI keymap redesigned** — All shortcuts are pure lowercase (uppercase silently treated as the same key); bindings no longer overlap. Top-level keys: `j/k` nav, `space` mark, `enter` menu, `/` search, `r` refresh visible, `s` sort, `t` auto-refresh, `a` add account, `h` help, `q` quit, `esc` clear marks/search. Removed top-level `c/d/n/w/a-as-auto` — those operations now live in the Enter menu (which surfaces them with discoverable labels)
- **TUI status bar simplified** — Shows only the 6 most-used bindings (`j/k nav │ enter menu │ / search │ r refresh │ h help │ q quit`); full list moved to the Help popup
- **TUI marks no longer change `r`/`refresh` scope implicitly** — Top-level `r` always refreshes the visible (search-filtered) view. Batch refresh / warmup / re-login / delete are explicit actions in the Enter > Batch menu, eliminating the "why did only some get refreshed" surprise

### Fixed

- **TUI popup scroll clamps to content bounds** — When the terminal shrank or popup content shortened, the persisted scroll offset could exceed the visible range and leave the popup blank until manually scrolled. Render path now writes the clamped value back to popup state
- **`auth.lock` self-heal + 15s stale-takeover** — Opening `~/.codex-switch/auth.lock` now recovers automatically when the file was left owned by `root` from a prior `sudo` invocation (unlinks and recreates without sudo). Acquisition uses `try_lock_exclusive` polling with a 15s deadline; after the deadline the holder is treated as stale and the lock file is unlinked + recreated to take over (orphan inode keeps any old fd's lock harmless). Best-effort `pid epoch_secs` is written to the lock file for diagnostics

### Security

- **`rustls-webpki` 0.103.12 → 0.103.13** — RUSTSEC-2026-0104 (HIGH, DoS via panic on malformed CRL BIT STRING)
- **`libc` 0.2.185 → 0.2.186, `zip` 8.5.1 → 8.6.0** — dependabot bumps

## v0.0.14 — 2026-04-14

### Added

- **`launch` subcommand** — `codex-switch launch [alias] [-- args...]` starts Codex CLI with a specific profile's auth, transparently forwarding all arguments. Omit alias to auto-select the best account using the adaptive scoring algorithm. Auth is swapped only for the few seconds codex needs to read it, then immediately restored — other commands are not blocked during the session
- **Over-pace warning indicator** — TUI table 5h/7d columns and CLI `list` output now show a red `!` suffix (e.g., `91%!`) when actual usage exceeds the expected pace, making it easy to spot accounts being consumed too fast
- **TUI version display** — Current version shown at bottom-right of the status bar; background update check (non-blocking, silent on failure) shows yellow `-> vX.Y.Z` hint when a new version is available
- **TUI gauge color severity** — Usage gauge bars now change color based on consumption: green (<70%), yellow (70-89% or over-pace), red (>=90%)

### Changed

- **Full RGB color palette** — TUI now uses explicit RGB color values instead of ANSI 16-color codes, fixing rendering inconsistencies between Windows cmd.exe, PowerShell, and Unix terminals. All elements (text, borders, backgrounds, gauges) use a unified palette
- **Forced dark background** — TUI renders a consistent dark background (`Rgb(24,24,24)`) regardless of terminal theme, fixing unreadable output on PowerShell blue and other non-dark terminals
- **Default log level changed to `error`** — Non-debug CLI and daemon modes no longer emit INFO/WARN tracing output to stderr, preventing log pollution in TUI and CLI output. Use `--debug` or `RUST_LOG=` to opt in
- **Daemon default log level** — `[daemon] log_level` default changed from `"info"` to `"error"` in config

### Fixed

- **Auth guard safety** — `launch` backup restoration only removes the `.json.bak` file after confirming the restore copy succeeded. Previously, a failed restore would still delete the backup, potentially losing credentials
- **Launch auth locking** — `launch` now holds `auth.lock` for the entire child process lifetime, preventing concurrent `launch`/`use`/`login` from corrupting `auth.json` or overwriting the backup
- **Unix signal exit code** — `launch` now propagates child exit codes using the `128+signal` convention instead of returning `-1` when the child is killed by a signal
- **TUI update check channel** — Dev builds now check the dev release channel instead of stable, preventing false "update available" notifications when already on the latest dev build
- **TUI poll_update cleanup** — Oneshot channel is now cleared when the sender drops (check failed or no update), stopping unnecessary polling every 100ms
- **`rand` security fix** — Bumped `rand` from 0.10.0 to 0.10.1 (fixes unsound behavior with custom loggers)

## v0.0.13 — 2026-04-11

### Added

- **Background daemon (Beta)** — `codex-switch daemon start|stop|status|install|uninstall` runs a background process that monitors the current account's usage and auto-switches when the configured threshold is exceeded. Supports macOS LaunchAgent and Linux systemd user service installation. Marked Beta: use with care
- **Mock testing infrastructure** — HTTP-level integration tests with a real mock server exercising the full fetch → parse → score pipeline
- **Daemon integration test** — End-to-end test covering daemon start, automatic switch, status, and stop lifecycle

### Changed

- **Adaptive scoring algorithm** — Replaced three separate selection modes (`max-remaining`, `drain-first`, `round-robin`) with a single adaptive algorithm that automatically adjusts strategy based on pool state. Config options `mode` and `min_remaining` are removed; `team_priority` (default: `true`) is the only new option
- **Team priority** — Team plan accounts now receive a +500 scoring bonus by default, ensuring they are used first. Set `team_priority = false` in config to disable
- **Pace-aware headroom** — Scoring now uses burn rate to project effective remaining time instead of static remaining percentage
- **Pool-adaptive drain** — Drain bonus only activates within 60 minutes of 5h reset, with weight scaled by pool exhaustion ratio
- **7d sustainability** — Budget-per-window calculation replaces static safety margin for more accurate 7d health assessment
- **README tagline simplified** — Removed self-promotional language from project description

### Fixed

- **PID file TOCTOU race** — Daemon PID file now uses atomic `O_CREAT|O_EXCL` creation, preventing double-instance under launchd/systemd restart racing
- **PID file leak on panic** — RAII guard ensures the PID file is cleaned up even when the daemon loop panics
- **AppleScript notification injection** — Notification messages are now sanitized to printable ASCII with proper backslash and quote escaping
- **CODEX_HOME path traversal** — `CODEX_HOME` environment variable now rejects paths containing `..` components
- **`update_tokens` silent data loss** — Previously skipped token updates without error when `auth.json` lacked a `tokens` object; now returns an error
- **Daemon backoff blocks shutdown** — Backoff sleep during consecutive failures now uses a nested `select!` so SIGTERM is handled immediately
- **Daemon tick burst after backoff** — Both poll and token-check intervals now use `MissedTickBehavior::Skip` to prevent accumulated tick storms
- **Daemon process signals via PATH** — `process_alive` and `stop` now use `libc::kill` directly instead of spawning `kill` from PATH
- **`daemon_log_level` tracing dependency** — Pre-tracing config probe no longer calls the full `load_from_file()` which triggered silent `tracing::warn!` calls
- **Non-daemon log level silent** — Non-daemon commands now default to `codex_switch=info` when `RUST_LOG` is not set, instead of producing no output
- **`parse_window` false positive** — Window parsing now requires `used_percent` to be present; a window with only `reset_at` no longer creates a misleading `has_5h_data=true` with `used_5h=0.0`
- **Warmup token errors silenced** — `update_tokens` failures in warmup are now logged via `warn!` instead of being silently discarded with `let _ =`
- **Service install idempotency** — `daemon install` now detects and warns about existing service files, unloading/stopping the old service before reinstalling
- **Daemon detach startup detection** — `daemon start` now checks if the spawned child is still alive after 200ms, failing early with a diagnostic message instead of printing success
- **Test env var leak** — HTTP integration tests no longer mutate `CS_USAGE_URL` via `set_var`, eliminating cross-test pollution and multi-thread UB
- **Test mock token handler** — Mock `/oauth/token` now validates `grant_type=refresh_token` and returns 400 on malformed requests
- **Test pool_exhausted hardcoded** — Timeline tests now compute `pool_exhausted` dynamically instead of hardcoding values

### Removed

- **Selection modes** — `ConfigSelectMode` enum, `-m` CLI flag, `min_remaining` config option, and three separate scoring functions (`score`, `score_drain_first`, `score_round_robin`) have been removed. The unified algorithm subsumes all three strategies

## v0.0.11 — 2026-04-07

### Added

- **`warmup` command** — `codex-switch warmup [alias]` sends a minimal Codex request (`ping`) to activate the 5h/7d quota window countdown for a fresh account. Omit alias to warm up all saved profiles concurrently. Already-active accounts (reset time still in the future) are automatically skipped. Supports `--json` output with per-account results and a top-level `ok` field
- **TUI warmup** — Press `w` to warm up the selected account or `W` to warm up all accounts. Usage is automatically refreshed after warmup completes
- **Pace Marker** — Usage bars (CLI and TUI) now display a `|` pace marker showing expected consumption based on elapsed window time, making it easy to see if you're ahead or behind budget
- **Dev update channel** — `self-update --dev` installs the latest dev build; `self-update --stable` switches back. Without flags, auto-detects the current channel. Legacy `0.0.x` dev builds used timestamped semver; current calendar versions use the shorter `YYYYMMDD.N.0-dev` form.
- **Install scripts enhanced** — `install.sh --dev` / `$env:CS_DEV="1"` for dev channel install; `install.sh --uninstall` / `$env:CS_UNINSTALL="1"` for clean removal. Homebrew-installed versions are detected and blocked from direct-install to prevent PATH conflicts
- **Startup auth change detection** — On launch, codex-switch compares the live `~/.codex/auth.json` against all saved profiles. If a new account is detected (e.g., the user ran `codex login`), it prompts to save as a new profile. If tokens were refreshed for an existing account, it prompts to update the corresponding profile
- **Non-interactive safety** — When stdin is not a TTY (pipes, cron, CI), startup detection informs without silently mutating state. EOF on stdin is treated as rejection

### Changed

- **TUI mark-aware operations unified** — `r` (refresh) and `w` (warmup) now operate on marked accounts when marks exist, or all accounts when none are marked. Separate `b` (batch refresh) and `W` (warmup all) keys removed
- **TUI usage gauges redesigned** — Bars now use block characters (`█` used, `░` remaining) with a `|` pace marker and an `XX% used / YY% left` suffix for clearer at-a-glance quota visibility. The pace marker label (`↑ pace`) is right-aligned alongside `resets in …` on row 2, and is suppressed automatically when there is not enough terminal width to avoid overlap
- **JSON usage output extended** — `JsonWindow` now includes `remaining_percent`, `pace_percent`, and `over_pace` fields (all `skip_serializing_if = None`; existing fields are unchanged)
- **Linux static linking** — Linux release binaries now use musl (static linking) instead of glibc, eliminating `GLIBC_2.xx not found` errors on older distributions. ARM64 Linux builds use `cross` for proper musl cross-compilation
- **Cargo package name** — The crate `name` field in `Cargo.toml` changed from `cs` to `codex-switch`. The binary name was already `codex-switch` and is unaffected; only `cargo install`/source build workflows that referenced the old package name need updating

### Fixed

- **Gauge row-2 text overlap** — When the quota window is nearly expired and the pace marker sits near the right edge, `resets in …` is now right-aligned and `↑ pace` is only shown when there is space, preventing the two strings from running together
- **Zero-width bar crash** — On extremely narrow terminals where `bar_width` computes to 0, the gauge now skips bar rendering entirely and shows only the reset time, instead of placing a pace marker at an out-of-bounds position
- **Warmup `--json` double output** — On partial failure, `warmup_cmd` previously emitted a results object then let `main()` emit a second error object. Now a single `{"ok": false, "results": […]}` object is printed and the process exits with code 1
- **Warmup UTF-8 truncation panic** — HTTP error bodies were truncated by byte index, which panicked on multi-byte UTF-8 characters. Now truncated by Unicode scalar count via `chars().take(160)`
- **TUI warmup rate limiting** — `warmup_all` (`W`) now respects `network.max_concurrent` via the shared `usage_limiter` semaphore, consistent with usage fetch behaviour
- **TUI warmup dedup and stale refresh** — Rapid `w`/`W` presses no longer trigger multiple forced refreshes for the same account. On warmup success the account always gets a fresh usage fetch so the newly opened quota window appears immediately
- **`auto_track_current` current pointer sync** — When `auth.json` matches an existing saved profile but the `current` file points elsewhere, the pointer is now updated automatically instead of being left stale
- **Empty `account_id` treated as present** — `/tokens/account_id: ""` was not filtered, preventing the email-only identity fallback. Empty strings are now treated as `None`, consistent with JWT claims filtering
- **`list` respects startup detection** — When startup auth detection already handled the live `auth.json`, `list` no longer runs `auto_track_current()` a second time, preventing silent saves after an explicit user rejection
- **Dev update 404 handling** — `check_for_dev_update` now uses proper HTTP status code detection instead of string matching; network errors are propagated while 404 (no dev release) returns `None`
- **Dev update redundant reinstall** — `self_update_dev` now compares versions before downloading, avoiding re-download when already on the latest dev build
- **Dev version extraction safety** — `extract_release_version` only attempts dev name parsing when `tag_name == "dev"`, preventing false matches on stable releases
- **Cache lock poisoning silent bypass** — `CACHE_LOCK.lock().ok()` silently dropped poisoned locks; now propagates error via `map_err`
- **Token refresh ignored OAuth error field** — `do_refresh_token` logged `error` field as warning but continued parsing tokens; now bails immediately
- **`make_unique_alias` infinite loop** — Long aliases near `MAX_ALIAS_LEN` caused suffix truncation to produce the same candidate forever; now capped at 1000 retries
- **Home directory fallback to `.`** — `codex_auth_path()` and `app_home()` fell back to current directory when `dirs::home_dir()` returned `None`; now returns an error
- **Warmup with expired tokens** — Warmup used stored `access_token` without refresh, failing with 401 for expired tokens; now pre-refreshes expiring tokens and retries on 401/403
- **TUI round-robin state drift** — TUI switches did not update `cache.last_used`, causing round-robin to diverge from CLI behavior
- **TUI profile load errors silenced** — `list_profiles()` errors were swallowed via `unwrap_or_default()`; now logs a warning
- **Version comparison silent failure** — `compare_versions` returned `None` on unparseable semver without logging; `self-update` would report "already up to date" instead of flagging the issue

## v0.0.10 — 2026-04-02

### Changed

- **Dependency upgrades** — Bumped `rand` 0.9→0.10, `sha2` 0.10→0.11, `toml` 1.1.0→1.1.1, `zip` 2.4→8.4
- **Windows terminal recommendation** — README now recommends Windows Terminal over Git Bash (mintty) for TUI, due to known crossterm compatibility issues

### Fixed

- **Clippy warnings** — Replaced manual `Default` impl with `#[derive(Default)]` on `ConfigSelectMode`; used `RangeInclusive::contains` in test assertion

## v0.0.9 — 2026-03-29

### Added

- **Selection modes for `use` command** — Three modes control auto-select behavior via `--mode`/`-m` flag or `[use].mode` config:
  - `max-remaining` (default): pick the account with the most remaining 5h quota
  - `drain-first`: prefer accounts whose 5h reset is imminent — spend "free" quota first, save slow-to-reset quota as reserve. Accounts below `min_remaining` threshold (default 5%) are demoted
  - `round-robin`: rotate through eligible accounts evenly by least-recently-used order; team accounts are preferred in a higher tier
- **7d-aware scoring** — All scoring modes now consider the 7d (weekly) window as a safety modifier:
  - Two-phase selection: eligibility gate filters exhausted or 7d-critical accounts, then scoring ranks the rest
  - 7d health adjustment (max-remaining & drain-first): additive penalty (-300 to 0) when 7d remaining falls below `safety_margin_7d` (default 20%), with up to 80% relief when 7d resets within 48h
  - Accounts with critically low 7d remaining and distant reset are marked ineligible (unless all accounts are in this state)
- **Round-robin last-used tracking** — `cache.json` now tracks when each profile was last selected by `use`, enabling fair rotation
- **New config options** — `[use]` section in `config.toml`:
  - `mode` — default selection mode (default: `max-remaining`)
  - `min_remaining` — drain-first 5h demotion threshold in % (default: 5)
  - `safety_margin_7d` — 7d safety margin in % (default: 20)
- **JSON output includes mode** — `codex-switch --json use` now includes the `mode` field in the response

### Changed

- **Explicit `use <alias>` updates round-robin history** — Manual account switches are now tracked for round-robin rotation, preventing re-selection of a just-used account
- **Cache rename preserves last-used data** — `rename` now independently migrates both usage cache and last-used timestamps

## v0.0.8 — 2026-03-28

### Fixed

- **Profile dedup incorrectly merges different users in the same Team** — `find_profile_by_identity()` previously matched on `account_id` alone, but OpenAI's `chatgpt_account_id` is a workspace-level identifier shared across all members. Users with different emails but the same Team workspace were incorrectly merged into one profile. Now requires both `account_id` AND `email` to match for dedup. ([#8](https://github.com/xjoker/codex-switch/issues/8))

## v0.0.7 — 2026-03-27

### Added

- **`rename` CLI command** — `codex-switch rename <old> <new>`, previously only available in TUI
- **Team account priority** — `codex-switch use` auto-selection gives Team plan accounts a +20 scoring bonus, preferring them when quotas are similar
- **Alias validation** — All user-facing alias inputs are validated against a safe character set, preventing path traversal attacks
- **Common usage scenarios in README** — Shell integration, CI automation, and optional cron-based token refresh
- **Troubleshooting section in README** — `--debug` usage guide and issue reporting instructions

### Changed

- **ASCII-only terminal output** — All user-visible CLI/TUI strings now use pure ASCII characters for Windows GBK codepage compatibility
- **`--json` scope clarified** — Help text and README now accurately state that `tui` and `open` do not support `--json`
- **README documentation sync** — Chinese README aligned with English version (added HTTPS_PROXY example, Building section, consistent chapter ordering)
- **Version examples updated** — README and CLI help examples updated to `0.0.7`

### Fixed

- **TUI refresh race condition** — Token writeback to live `auth.json` now re-checks the active profile before writing, preventing stale tokens from overwriting a freshly switched account
- **Auth file permissions** — `auth.json` files are now created with mode `0600` and directories with `0700` on Unix, preventing other users from reading tokens
- **Proxy credential leak** — `--debug` logging now sanitizes `user:pass` in proxy URLs to `***:***`
- **Silent error swallowing** — Config load failures, cache write failures, and token refresh persistence failures now emit `tracing::warn!` instead of being silently ignored
- **Clippy warnings** — Resolved 4 `if_same_then_else` warnings in color threshold logic
- **Dead code cleanup** — Removed 2 unnecessary `#[allow(dead_code)]` annotations
- **Rename cache cleanup** — Renaming a profile now migrates its cache entry to the new alias

## v0.0.6 — 2026-03-26

### Fixed

- **OAuth login broken since v0.0.4** — redirect_uri changed from `http://localhost:1455/...` to `http://127.0.0.1:1455/...`, which doesn't match OpenAI's registered URI, causing `unknown_error` on login
- **Removed random port fallback for PKCE login** — OAuth redirect_uri must exactly match the registered `localhost:1455`; random port fallback is only valid for device code flow

## v0.0.5 — 2026-03-26

### Added

- **OAuth port fallback** — When port 1455 is occupied, login automatically falls back to a random available port instead of failing
- **Token pre-refresh** — Access tokens expiring within 60 seconds are proactively refreshed before usage API requests, reducing 401 retry latency
- **Process detection on switch** — `use` command detects running Codex processes and warns before switching; use `--force` to override
- **Credits balance display** — Usage output now shows `credits_balance` and `unlimited_credits` in CLI, TUI, and `--json` output (backward-compatible with APIs that don't return credits data)
- **Unit test coverage** — 21 new unit tests covering JWT parsing/expiration, usage data parsing/scoring/availability, auth JSON structure, and cache deserialization compatibility

## v0.0.4 — 2026-03-26

### Fixed

- **Improved error diagnostics** — All HTTP requests (token exchange, device code, usage API, token refresh) now display the full error source chain instead of a generic "error sending request" message, making proxy/TLS/network issues immediately diagnosable
- **HTTP client timeouts** — Added `connect_timeout(30s)` and `timeout(60s)` to the shared HTTP client, preventing indefinite hangs when a proxy or upstream is unresponsive
- **File I/O error context** — `write_auth`, `backup_auth`, profile directory operations (`list`, `delete`, `rename`, `import`), and `open` command now include file/directory paths in error messages
- **OAuth callback error context** — Local callback server (`accept`/`read`) errors now indicate they occurred during the OAuth login flow rather than showing raw socket errors
- **Usage API error clarity** — HTTP error responses from the usage API now clearly identify the failing endpoint instead of showing bare status codes
- **TUI error context** — Terminal draw/event errors now include operation context for easier troubleshooting

## v0.0.3 — 2026-03-26

### Added

- **Manual self-update** — New `codex-switch self-update` command for direct installs, plus `self-update --check` for on-demand release checks
- **Checksum-verified updates** — Direct-install self-update validates the release `.sha256` before replacing the current executable
- **Install-source awareness** — Homebrew installs are detected and redirected to `brew upgrade xjoker/tap/codex-switch`
- **Recursive directory import** — `import <path>` now accepts directories, scans recursively for `.json` files, validates them, and reports imported vs skipped files
- **Import stage reporting** — Directory import surfaces failures as `file_format`, `structure`, `usage_validation`, or `save`
- **Progress reporting** — `use`, `list`, and bulk `import` show a single-line progress indicator for large batches
- **Per-account cache timestamps** — Cached usage now preserves the original refresh time and exposes it as `usage.fetched_at` in JSON output
- **Device Code Flow** — `login --device` for headless servers without a browser (RFC 8628, polls `deviceauth` endpoint)
- **`--debug` flag** — Enable debug logging for HTTP requests, API responses, and cache status
- **`--json-pretty` flag** — Pretty-printed JSON output in addition to existing `--json` compact mode

### Changed

- **Manual-only update checks** — No automatic update checks on startup, `use`, `list`, or TUI launch
- **Cache scheduling** — `use`, `list`, and TUI now refresh only stale accounts by default; force-refresh paths still bypass cache
- **JSON hygiene** — Human messages and progress output are routed away from stdout so `--json` stays machine-readable
- **Network concurrency** — Configurable max concurrent usage requests (`[network] max_concurrent`, default 20) now applies across CLI and TUI refresh paths
- **`list` replaces `status`** — `codex-switch list` now fetches live usage (previously only `status` did); `status` command removed

### Fixed

- **Active profile deletion** — CLI now rejects deleting the currently active profile, matching TUI behavior
- **Zero-concurrency config** — Invalid `network.max_concurrent = 0` is normalized instead of hanging refresh paths
- **TUI refresh behavior** — TUI preloads cache first, refreshes only stale entries, and no longer emits stray auto-track text into the UI
- **Device flow polling** — OAuth device code flow now handles `slow_down` correctly and exits cleanly on Ctrl+C
- **Import validation** — `auth.json` imports now require valid token structure and perform a real account usability check unless explicitly skipped in tests

## v0.0.2 — 2026-03-25

### Changed

- **Homebrew release automation** — Homebrew formula updates now run inside the main release workflow instead of a separate post-release workflow
- **Multi-platform formula generation** — Release automation now generates checksums and Homebrew formula entries for all supported macOS and Linux targets
- **Dependency refresh** — Upgraded core crates including `dirs` 6.x, `toml` 1.x, and `rand` 0.9

### Fixed

- **Release packaging** — Replaced the single-URL Homebrew bump action with a custom multi-asset formula generator so `brew upgrade` tracks the correct archive for each platform
- **Login RNG compatibility** — OAuth PKCE state/verifier generation now uses the current `rand` 0.9 API

## v0.0.1 — Initial Release

### Features

- **Profile Management** — `codex-switch use`, `list`, `delete`, `import`, `login` commands
- **Interactive TUI** — ratatui-based terminal UI with account list, usage gauges, keyboard navigation (`j/k`, `Enter`, `r`, `n`, `d`)
- **Usage Dashboard** — Real-time 5h/7d quota monitoring via ChatGPT wham API with color-coded status (OK/Limited/Error)
- **Smart Auto-Switch** — `codex-switch use` without alias auto-selects the best account (7d limit checked first, then 5h remaining %)
- **OAuth PKCE Login** — Browser-based login flow with local callback server on port 1455
- **Token Auto-Refresh** — Automatic refresh_token flow on HTTP 401/403, persists new tokens
- **Auto-Detection** — `list`, `tui` auto-discover and save untracked `~/.codex/auth.json`
- **Deduplication** — Login/import matches by account_id > email, updates existing profiles instead of creating duplicates
- **TUI Rename** — Rename profiles in-place with `n` key
- **TUI Delete** — Delete profiles with `d` key (confirmation required, cannot delete active profile)
- **Proxy Support** — HTTP/HTTPS/SOCKS4/SOCKS5/SOCKS5H with authentication; CLI `--proxy`, `CS_PROXY` env, config file, standard env vars
- **Config File** — `~/.codex-switch/config.toml` for persistent proxy settings
- **Color Output** — `--color auto|always|never`, respects `NO_COLOR` env and terminal capability detection
- **JSON Output** — `--json` flag for all commands
- **Team/Org Display** — Shows workspace/organization name in plan label (e.g., "team · Personal")
- **Local Timezone** — Reset times displayed in local timezone (e.g., "2h30m (14:30)")
- **Retry with Backoff** — Network requests retry up to 3 times with 1-2s delay
- **Backup Management** — Auto-backup auth.json on switch, keeps only last 3 backups
- **Cross-Platform** — macOS (amd64/arm64), Linux (amd64/arm64), Windows (amd64/arm64)
- **One-Liner Install** — `curl | bash` for macOS/Linux, `irm | iex` for Windows
- **Homebrew** — `brew install xjoker/tap/codex-switch`

### Build Targets

| Platform | Architecture | Asset |
|----------|-------------|-------|
| macOS | Apple Silicon | `cs-darwin-arm64.tar.gz` |
| macOS | Intel | `cs-darwin-amd64.tar.gz` |
| Linux | x86_64 | `cs-linux-amd64.tar.gz` |
| Linux | ARM64 | `cs-linux-arm64.tar.gz` |
| Windows | x86_64 | `cs-windows-amd64.zip` |
| Windows | ARM64 | `cs-windows-arm64.zip` |
