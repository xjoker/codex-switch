use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::RequestBuilder;
use serde::Deserialize;

use crate::http_retry::{self, ReplaySafety};

const ACCOUNTS_CHECK_URL: &str = "https://chatgpt.com/backend-api/wham/accounts/check";

#[derive(Debug, Deserialize)]
struct AccountEntry {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    structure: String,
}

#[derive(Debug, Deserialize)]
struct ChatGptAccountEntry {
    account: ChatGptAccountInfo,
}

#[derive(Debug, Deserialize)]
struct ChatGptAccountInfo {
    account_id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    structure: String,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawAccounts {
    List(Vec<AccountEntry>),
    Map(HashMap<String, ChatGptAccountEntry>),
}

impl Default for RawAccounts {
    fn default() -> Self {
        Self::List(Vec::new())
    }
}

#[derive(Debug, Deserialize)]
struct AccountsCheckResponse {
    #[serde(default)]
    accounts: RawAccounts,
    #[serde(default)]
    account_ordering: Vec<String>,
}

impl AccountsCheckResponse {
    fn into_accounts(self) -> Vec<AccountEntry> {
        match self.accounts {
            RawAccounts::List(accounts) => accounts,
            RawAccounts::Map(mut accounts) => self
                .account_ordering
                .iter()
                .filter_map(|id| {
                    let account = accounts.remove(id)?.account;
                    Some(AccountEntry {
                        id: account.account_id?,
                        name: account.name,
                        structure: account.structure,
                    })
                })
                .collect(),
        }
    }

    fn workspace_name_for(self, account_id: &str) -> WorkspaceLookup {
        let Some(account) = self
            .into_accounts()
            .into_iter()
            .find(|account| account.id == account_id)
        else {
            return WorkspaceLookup::Unlisted;
        };
        let _structure = account.structure;
        match account
            .name
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
        {
            Some(name) => WorkspaceLookup::Named(name),
            None => WorkspaceLookup::Unnamed,
        }
    }
}

/// What an accounts-check response says about one account.
///
/// The three cases used to collapse into `Option<String>`, which was harmless
/// while a `None` only meant "do not cache". Once absence became something
/// worth recording, conflating "the server did not list this account" with
/// "the server listed it and it has no workspace name" would hide a real
/// organisation name for as long as the record lasts. Only the second is an
/// answer — and the first is reachable in practice, since `account_ordering`
/// is optional and the map shape enumerates through it.
#[derive(Debug)]
pub(crate) enum WorkspaceLookup {
    /// Not present in the response. No conclusion can be drawn.
    Unlisted,
    /// Present, with no workspace name — a personal plan.
    Unnamed,
    /// Present, under this workspace.
    Named(String),
}

fn accounts_check_url() -> String {
    std::env::var("CS_ACCOUNTS_CHECK_URL").unwrap_or_else(|_| ACCOUNTS_CHECK_URL.to_string())
}

fn build_accounts_check_request(
    client: &reqwest::Client,
    url: &str,
    access_token: &str,
    account_id: &str,
    is_fedramp: bool,
) -> RequestBuilder {
    let mut request = client
        .get(url)
        .timeout(Duration::from_secs(5))
        .bearer_auth(access_token)
        .header("Accept", "application/json")
        .header("ChatGPT-Account-ID", account_id);
    if is_fedramp {
        request = request.header("X-OpenAI-Fedramp", "true");
    }
    request
}

pub(crate) async fn fetch_workspace_name(
    client: &reqwest::Client,
    access_token: &str,
    account_id: &str,
    is_fedramp: bool,
) -> Result<WorkspaceLookup> {
    if account_id.trim().is_empty() {
        return Ok(WorkspaceLookup::Unlisted);
    }
    let url = accounts_check_url();
    let response = http_retry::send(
        build_accounts_check_request(client, &url, access_token, account_id, is_fedramp),
        ReplaySafety::Idempotent,
    )
    .await
    .with_context(|| "requesting ChatGPT workspace metadata")?;
    let status = response.status;
    if !status.is_success() {
        anyhow::bail!("workspace metadata request failed (HTTP {status})");
    }
    let body = serde_json::from_slice::<AccountsCheckResponse>(&response.body)
        .with_context(|| format!("parsing workspace metadata response (HTTP {status})"))?;
    Ok(body.workspace_name_for(account_id))
}

pub(crate) async fn refresh_for_auth(auth: &serde_json::Value) -> Result<Option<String>> {
    refresh_for_auth_if_needed(auth, true).await
}

pub(crate) async fn refresh_for_auth_if_needed(
    auth: &serde_json::Value,
    force: bool,
) -> Result<Option<String>> {
    let info = crate::jwt::parse_account_info(auth);
    let Some(account_id) = info.account_id.as_deref() else {
        return Ok(None);
    };
    if !force && let Some(resolved) = crate::cache::resolved_workspace_name_async(account_id).await
    {
        return Ok(resolved);
    }
    let Some(access_token) = auth
        .pointer("/tokens/access_token")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
    else {
        return Ok(None);
    };
    let client = crate::auth::build_http_client()?;
    remember_workspace_name(&client, access_token, Some(account_id), info.is_fedramp).await
}

pub(crate) async fn remember_workspace_name(
    client: &reqwest::Client,
    access_token: &str,
    account_id: Option<&str>,
    is_fedramp: bool,
) -> Result<Option<String>> {
    let Some(account_id) = account_id.filter(|value| !value.trim().is_empty()) else {
        return Ok(None);
    };
    let lookup = fetch_workspace_name(client, access_token, account_id, is_fedramp).await?;
    // `Unlisted` is not an answer, so it is not recorded — the same reason a
    // failed request is not. Recording it would hide a real workspace name
    // until the record expired.
    let name = match lookup {
        WorkspaceLookup::Unlisted => return Ok(None),
        WorkspaceLookup::Unnamed => None,
        WorkspaceLookup::Named(name) => Some(name),
    };
    let cache_account_id = account_id.to_string();
    let cache_name = name.clone();
    tokio::task::spawn_blocking(move || {
        crate::cache::set_workspace_name(&cache_account_id, cache_name.as_deref())
    })
    .await
    .context("joining workspace cache update")??;
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<String>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var(key).ok();
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.previous {
                    Some(value) => std::env::set_var(self.key, value),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    /// Nothing listens on this port, so any lookup that actually goes out fails
    /// loudly. Reaching `Ok` is therefore proof the answer came from cache.
    const UNREACHABLE_ACCOUNTS_CHECK: &str = "http://127.0.0.1:1/";

    /// `CODEX_SWITCH_HOME` is process-global and mutated by other test modules
    /// under `profile::TEST_ENV_LOCK`; take it for the whole body. Holding it
    /// across `.await` is safe under `#[tokio::test]`'s current-thread runtime.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn a_confirmed_absence_is_answered_without_another_request() {
        let _env_lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = tempfile::tempdir().unwrap();
        let _home = EnvVarGuard::set("CODEX_SWITCH_HOME", &home.path().display().to_string());
        let _url = EnvVarGuard::set("CS_ACCOUNTS_CHECK_URL", UNREACHABLE_ACCOUNTS_CHECK);

        // Exactly what a successful lookup against a personal plan records.
        crate::cache::set_workspace_name("acct-personal", None).unwrap();

        let auth = serde_json::json!({
            "tokens": {"account_id": "acct-personal", "access_token": "at", "id_token": ""}
        });

        let name = refresh_for_auth_if_needed(&auth, false)
            .await
            .expect("a recorded absence must be answered from cache, not re-requested");

        assert!(name.is_none(), "the account still has no workspace name");
    }

    /// The other half of the contract: `--force` is the escape hatch, so it has
    /// to reach the network even when an absence is on record.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn force_still_asks_the_server_despite_a_recorded_absence() {
        let _env_lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = tempfile::tempdir().unwrap();
        let _home = EnvVarGuard::set("CODEX_SWITCH_HOME", &home.path().display().to_string());
        let _url = EnvVarGuard::set("CS_ACCOUNTS_CHECK_URL", UNREACHABLE_ACCOUNTS_CHECK);

        crate::cache::set_workspace_name("acct-personal", None).unwrap();

        let auth = serde_json::json!({
            "tokens": {"account_id": "acct-personal", "access_token": "at", "id_token": ""}
        });

        refresh_for_auth_if_needed(&auth, true)
            .await
            .expect_err("force must bypass the record and fail on the unreachable endpoint");
    }

    /// An account the response does not mention is not an account confirmed to
    /// have no workspace name. Recording the first as if it were the second
    /// hides a real organisation name until the record expires.
    #[test]
    fn an_account_missing_from_the_response_is_not_a_confirmed_absence() {
        let response: AccountsCheckResponse = serde_json::from_value(serde_json::json!({
            "accounts": [{"id": "acct-other", "name": "Someone Else", "structure": "workspace"}],
            "account_ordering": ["acct-other"]
        }))
        .unwrap();

        assert!(matches!(
            response.workspace_name_for("acct-mine"),
            WorkspaceLookup::Unlisted
        ));
    }

    /// The map shape enumerates through `account_ordering`, which is optional.
    /// Without it nothing is listed at all — which must read as "no answer",
    /// not as "every account confirmed to have no name".
    #[test]
    fn a_map_response_without_ordering_yields_no_answer_rather_than_an_absence() {
        let response: AccountsCheckResponse = serde_json::from_value(serde_json::json!({
            "accounts": {
                "acct-team": {"account": {"account_id": "acct-team", "name": "Platform", "structure": "workspace"}}
            }
        }))
        .unwrap();

        assert!(matches!(
            response.workspace_name_for("acct-team"),
            WorkspaceLookup::Unlisted
        ));
    }

    /// The case that *is* an answer: listed, and genuinely has no name.
    #[test]
    fn a_listed_account_without_a_name_is_a_confirmed_absence() {
        let response: AccountsCheckResponse = serde_json::from_value(serde_json::json!({
            "accounts": [{"id": "acct-personal", "name": null, "structure": "personal"}],
            "account_ordering": ["acct-personal"]
        }))
        .unwrap();

        assert!(matches!(
            response.workspace_name_for("acct-personal"),
            WorkspaceLookup::Unnamed
        ));
    }

    /// A name that is only whitespace carries no more information than none at
    /// all, and the display path would render it as an empty workspace.
    #[test]
    fn a_blank_name_counts_as_no_name_rather_than_a_workspace_called_nothing() {
        let response: AccountsCheckResponse = serde_json::from_value(serde_json::json!({
            "accounts": [{"id": "acct", "name": "   ", "structure": "workspace"}],
            "account_ordering": ["acct"]
        }))
        .unwrap();

        assert!(matches!(
            response.workspace_name_for("acct"),
            WorkspaceLookup::Unnamed
        ));
    }

    #[test]
    fn parses_codex_api_list_shape() {
        let response: AccountsCheckResponse = serde_json::from_value(serde_json::json!({
            "accounts": [
                {"id": "acct-personal", "name": "Personal", "structure": "personal"},
                {"id": "acct-team", "name": "Platform Team", "structure": "workspace"}
            ],
            "account_ordering": ["acct-personal", "acct-team"]
        }))
        .unwrap();
        let accounts = response.into_accounts();
        assert_eq!(accounts.len(), 2);
        assert_eq!(accounts[1].id, "acct-team");
        assert_eq!(accounts[1].name.as_deref(), Some("Platform Team"));
    }

    #[test]
    fn parses_chatgpt_map_shape_in_server_order() {
        let response: AccountsCheckResponse = serde_json::from_value(serde_json::json!({
            "accounts": {
                "personal": {"account": {"account_id": "acct-personal", "name": "Personal", "structure": "personal"}},
                "team": {"account": {"account_id": "acct-team", "name": "Platform Team", "structure": "workspace"}}
            },
            "account_ordering": ["team", "personal"]
        }))
        .unwrap();
        let accounts = response.into_accounts();
        assert_eq!(accounts[0].id, "acct-team");
        assert_eq!(accounts[1].id, "acct-personal");
    }

    #[test]
    fn workspace_name_matches_selected_account_and_trims_name() {
        let response: AccountsCheckResponse = serde_json::from_value(serde_json::json!({
            "accounts": [
                {"id": "acct-personal", "name": "Personal", "structure": "personal"},
                {"id": "acct-team", "name": "  Platform Team  ", "structure": "workspace"}
            ]
        }))
        .unwrap();

        assert!(matches!(
            response.workspace_name_for("acct-team"),
            WorkspaceLookup::Named(name) if name == "Platform Team"
        ));
    }

    #[test]
    fn request_matches_codex_headers() {
        let request = build_accounts_check_request(
            &reqwest::Client::new(),
            "https://chatgpt.com/backend-api/wham/accounts/check",
            "secret-token",
            "acct-team",
            true,
        )
        .build()
        .unwrap();
        assert_eq!(
            request.headers()[reqwest::header::AUTHORIZATION],
            "Bearer secret-token"
        );
        assert_eq!(request.headers()["ChatGPT-Account-ID"], "acct-team");
        assert_eq!(request.headers()["X-OpenAI-Fedramp"], "true");
    }
}
