use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::Value;

/// Single organization/workspace entry
#[derive(Debug, Default, Clone)]
#[allow(dead_code)]
pub struct OrgInfo {
    pub id: String,
    pub title: String,
    pub role: String,
    pub is_default: bool,
}

#[derive(Debug, Default, Clone)]
#[allow(dead_code)]
pub struct AccountInfo {
    pub email: Option<String>,
    pub plan_type: Option<String>,
    pub account_id: Option<String>,
    pub user_id: Option<String>,
    pub workspace_name: Option<String>,
    pub organizations: Vec<OrgInfo>,
}

#[allow(dead_code)]
impl AccountInfo {
    pub fn plan_label(&self) -> String {
        let base = self.plan_type.as_deref().unwrap_or("?").to_string();
        if let Some(name) = &self.workspace_name
            && !name.is_empty()
        {
            return format!("{base} · {name}");
        }
        if let Some(org) = self.organizations.iter().find(|o| o.is_default)
            && !org.title.is_empty()
        {
            return format!("{base} · {}", org.title);
        }
        base
    }

    pub fn is_team(&self) -> bool {
        matches!(self.plan_type.as_deref(), Some("team"))
            || !self.organizations.is_empty()
            || self.workspace_name.is_some()
    }
}

/// Parse account info from an auth.json Value
pub fn parse_account_info(auth: &Value) -> AccountInfo {
    let id_token = auth
        .pointer("/tokens/id_token")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let account_id_from_tokens = auth
        .pointer("/tokens/account_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let claims = decode_jwt_payload(id_token).unwrap_or_default();

    let email = claims
        .get("email")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let auth_claims = claims.get("https://api.openai.com/auth");

    let plan_type = auth_claims
        .and_then(|a| a.get("chatgpt_plan_type"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let user_id = auth_claims
        .and_then(|a| a.get("chatgpt_user_id").or_else(|| a.get("user_id")))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let account_id = auth_claims
        .and_then(|a| a.get("chatgpt_account_id"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_string())
        .or(account_id_from_tokens);

    let workspace_name = extract_workspace_name(&claims);
    let organizations = extract_organizations(&claims);

    AccountInfo {
        email,
        plan_type,
        account_id,
        user_id,
        workspace_name,
        organizations,
    }
}

/// Extract workspace name from JWT claims (team/org accounts)
fn extract_workspace_name(claims: &Value) -> Option<String> {
    // Top-level fields
    for key in &[
        "workspace_name",
        "organization_name",
        "org_name",
        "team_name",
    ] {
        if let Some(v) = claims.get(key).and_then(|v| v.as_str()) {
            let s = v.trim().to_string();
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    // Nested under auth claims
    let auth = claims.get("https://api.openai.com/auth")?;
    for key in &[
        "workspace_name",
        "organization_name",
        "org_name",
        "team_name",
    ] {
        if let Some(v) = auth.get(key).and_then(|v| v.as_str()) {
            let s = v.trim().to_string();
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    // Fallback: default org title from organizations array
    if let Some(orgs) = auth.get("organizations").and_then(|v| v.as_array()) {
        let default = orgs.iter().find(|o| {
            o.get("is_default")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        });
        let candidate = default.or_else(|| orgs.first());
        if let Some(title) = candidate
            .and_then(|o| o.get("title"))
            .and_then(|v| v.as_str())
        {
            let s = title.trim().to_string();
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    None
}

/// Extract organizations list from JWT claims
fn extract_organizations(claims: &Value) -> Vec<OrgInfo> {
    let auth = match claims.get("https://api.openai.com/auth") {
        Some(a) => a,
        None => return vec![],
    };
    let orgs = match auth.get("organizations").and_then(|v| v.as_array()) {
        Some(a) => a,
        None => return vec![],
    };
    orgs.iter()
        .filter_map(|o| {
            let id = o.get("id")?.as_str()?.trim().to_string();
            if id.is_empty() {
                return None;
            }
            Some(OrgInfo {
                id,
                title: o
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string(),
                role: o
                    .get("role")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string(),
                is_default: o
                    .get("is_default")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            })
        })
        .collect()
}

/// Decode the payload section of a JWT token (base64 → JSON)
fn decode_jwt_payload(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&decoded).ok()
}
