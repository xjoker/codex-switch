/// TUI menu state machines for Phase 2:
///   - Account menu (single-account actions)
///   - Add menu (OAuth flow choice for new account)
///   - OAuth flow choice (browser vs device code, used by re-login)
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use super::popup::{PopupState, render_popup};

const C_WHITE: Color = Color::Rgb(240, 240, 240);
const DIM: Color = Color::Rgb(120, 120, 120);
const C_RED: Color = Color::Rgb(255, 90, 90);
const C_GREEN: Color = Color::Rgb(80, 220, 120);
const C_YELLOW: Color = Color::Rgb(255, 220, 80);
const C_CYAN: Color = Color::Rgb(100, 210, 255);
const C_PURPLE: Color = Color::Rgb(145, 90, 220);

/// Active menu state. Only one menu is visible at a time.
pub enum MenuState {
    /// Account-scoped action menu (Enter on a single account).
    Account {
        info: Box<AccountMenuInfo>,
        popup: PopupState,
    },
    /// Add new account: choose OAuth flow.
    Add { popup: PopupState },
    /// Re-login: choose OAuth flow for an existing account.
    ReloginFlow {
        alias: String,
        email: Option<String>,
        popup: PopupState,
    },
    /// Batch menu shown when one or more accounts are marked.
    Batch { count: usize, popup: PopupState },
    /// Batch re-login flow chooser (browser vs device code).
    BatchReloginFlow { count: usize, popup: PopupState },
}

#[derive(Debug, Clone)]
pub struct AccountMenuInfo {
    pub alias: String,
    pub email: Option<String>,
    pub account_id: Option<String>,
    pub user_id: Option<String>,
    pub workspace_name: Option<String>,
    pub is_fedramp: bool,
    pub plan_label: String,
    pub plan_type: Option<String>,
    pub is_current: bool,
    pub organizations: Vec<String>,
    pub auth_expiries: Vec<String>,
    pub usage: Option<Box<crate::usage::UsageInfo>>,
    pub usage_meta: Vec<String>,
    pub models: Vec<String>,
    pub reset_cards: Option<u64>,
    pub reset_card_expiries: Vec<String>,
    pub can_consume_reset_card: bool,
}

#[derive(Debug, Clone)]
pub enum MenuAction {
    /// Keep the menu open and ignore the key.
    Noop,
    /// Close the menu, no further action.
    Close,
    /// Switch to alias.
    Use(String),
    /// Open re-login flow chooser for alias.
    ReloginRequest(String, Option<String>),
    /// Trigger re-login with chosen flow.
    Relogin { alias: String, device: bool },
    /// Trigger add-new-account with chosen flow.
    Add { device: bool },
    /// Refresh usage and model metadata for one account.
    RefreshOne(String),
    /// Open rename input for alias.
    Rename(String),
    /// Warmup just this alias.
    WarmupOne(String),
    /// Consume the earliest-expiring reset card for alias.
    ConsumeResetCard(String),
    /// Request delete confirmation for alias.
    DeleteRequest(String),

    // Batch actions ────────────────────────────
    /// Force-refresh all marked accounts.
    BatchRefresh,
    /// Warmup all marked accounts.
    BatchWarmup,
    /// Open OAuth flow chooser for batch re-login.
    BatchReloginRequest,
    /// Re-login marked accounts sequentially using `device` flow.
    BatchRelogin { device: bool },
    /// Request batch-delete confirmation.
    BatchDeleteRequest,
}

fn quota_window_lines(
    window: &crate::usage::WindowUsage,
    fallback_label: &str,
) -> Vec<Line<'static>> {
    const BAR_WIDTH: usize = 22;
    let label = match window.window_minutes {
        Some(minutes) if minutes % 1_440 == 0 => format!("{}d", minutes / 1_440),
        Some(minutes) if minutes % 60 == 0 => format!("{}h", minutes / 60),
        Some(minutes) => format!("{minutes}m"),
        None => fallback_label.to_string(),
    };
    let used = window.used_percent.unwrap_or(0.0).clamp(0.0, 100.0);
    let remaining = (100.0 - used).max(0.0);
    let used_width = ((used / 100.0) * BAR_WIDTH as f64).round() as usize;
    let used_color = if used >= 90.0 {
        C_RED
    } else if used >= 70.0 {
        C_YELLOW
    } else {
        C_GREEN
    };
    let window_secs = window
        .window_minutes
        .map(|minutes| minutes.saturating_mul(60))
        .unwrap_or_else(|| {
            if fallback_label == "5h" {
                crate::usage::WINDOW_5H_SECS
            } else {
                crate::usage::WINDOW_7D_SECS
            }
        });
    let pace = crate::usage::pace_percent(window, window_secs);
    let pace_index = pace.map(|value| {
        ((value / 100.0) * BAR_WIDTH as f64)
            .round()
            .clamp(0.0, (BAR_WIDTH - 1) as f64) as usize
    });
    let mut spans = vec![Span::styled(
        format!("{label:<3} "),
        Style::default().fg(C_WHITE),
    )];
    for index in 0..BAR_WIDTH {
        let (symbol, style) = if Some(index) == pace_index {
            (
                "┃",
                Style::default().fg(C_CYAN).add_modifier(Modifier::BOLD),
            )
        } else if index < used_width {
            ("█", Style::default().fg(used_color))
        } else {
            ("░", Style::default().fg(DIM))
        };
        spans.push(Span::styled(symbol, style));
    }
    spans.push(Span::styled(
        format!("  {remaining:.0}% left"),
        Style::default().fg(if remaining <= 10.0 { C_RED } else { C_YELLOW }),
    ));
    if let Some(pace) = pace {
        let delta = used - pace;
        if delta > 0.0 {
            let seconds = ((delta * window_secs as f64 / 100.0) as i64).max(1);
            spans.push(Span::styled(
                format!(
                    " · {delta:.0}% over pace · rest {}",
                    format_duration(seconds)
                ),
                Style::default().fg(C_YELLOW),
            ));
        }
    }
    let reset_relative = window
        .resets_at
        .map(crate::output::format_reset_time)
        .unwrap_or_else(|| "--".to_string());
    spans.push(Span::styled(
        format!(" · reset {reset_relative}"),
        Style::default().fg(DIM),
    ));
    vec![Line::from(spans)]
}

fn reasoning_style(effort: &str) -> Style {
    match effort {
        "low" => Style::default().fg(C_GREEN),
        "medium" => Style::default().fg(C_CYAN),
        "high" => Style::default().fg(C_YELLOW),
        "xhigh" => Style::default().fg(Color::LightMagenta),
        "max" => Style::default().fg(C_RED),
        "ultra" => Style::default().fg(C_PURPLE),
        _ => Style::default().fg(DIM),
    }
}

fn model_line_spans(model: &str, label_style: Style) -> Vec<Span<'static>> {
    let model = model.trim();
    let Some((name, details)) = model.split_once(" · default ") else {
        return vec![Span::styled(model.to_string(), label_style)];
    };
    let Some((default, allowed)) = details.split_once(" · allowed ") else {
        return vec![Span::styled(model.to_string(), label_style)];
    };

    let mut spans = vec![
        Span::styled(name.to_string(), label_style),
        Span::styled(" · default ", Style::default().fg(DIM)),
        Span::styled(default.to_string(), reasoning_style(default)),
        Span::styled(" · allowed ", Style::default().fg(DIM)),
    ];
    for (index, effort) in allowed.split(", ").enumerate() {
        if index > 0 {
            spans.push(Span::styled(", ", Style::default().fg(DIM)));
        }
        spans.push(Span::styled(effort.to_string(), reasoning_style(effort)));
    }
    spans
}

fn format_duration(seconds: i64) -> String {
    let seconds = seconds.max(0);
    let days = seconds / 86_400;
    let hours = seconds % 86_400 / 3_600;
    let minutes = seconds % 3_600 / 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{}m", minutes.max(1))
    }
}

fn quota_lines(usage: Option<&crate::usage::UsageInfo>) -> Vec<Line<'static>> {
    let Some(usage) = usage else {
        return vec![Line::from(Span::styled(
            "Usage not loaded",
            Style::default().fg(DIM),
        ))];
    };
    let mut lines = Vec::new();
    let mut add_pool = |name: &str,
                        primary: Option<&crate::usage::WindowUsage>,
                        secondary: Option<&crate::usage::WindowUsage>,
                        unavailable: bool| {
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(vec![
            Span::styled(
                name.to_string(),
                Style::default().fg(C_CYAN).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                if unavailable { "  unavailable" } else { "" },
                Style::default().fg(C_RED),
            ),
        ]));
        if let Some(window) = primary {
            lines.extend(quota_window_lines(window, "5h"));
        }
        if let Some(window) = secondary {
            lines.extend(quota_window_lines(window, "7d"));
        }
        if primary.is_none() && secondary.is_none() {
            lines.push(Line::from(Span::styled(
                "  No active window",
                Style::default().fg(DIM),
            )));
        }
    };
    add_pool(
        "Main",
        usage.primary.as_ref(),
        usage.secondary.as_ref(),
        false,
    );
    for pool in &usage.additional_limits {
        add_pool(
            pool.limit_name.as_deref().unwrap_or("Additional"),
            pool.primary.as_ref(),
            pool.secondary.as_ref(),
            pool.allowed == Some(false) || pool.limit_reached == Some(true),
        );
    }
    lines
}

impl MenuState {
    pub fn account(info: AccountMenuInfo) -> Self {
        MenuState::Account {
            info: Box::new(info),
            popup: PopupState::new(),
        }
    }

    pub fn add() -> Self {
        MenuState::Add {
            popup: PopupState::new(),
        }
    }

    pub fn relogin_flow(alias: String, email: Option<String>) -> Self {
        MenuState::ReloginFlow {
            alias,
            email,
            popup: PopupState::new(),
        }
    }

    pub fn batch(count: usize) -> Self {
        MenuState::Batch {
            count,
            popup: PopupState::new(),
        }
    }

    pub fn batch_relogin_flow(count: usize) -> Self {
        MenuState::BatchReloginFlow {
            count,
            popup: PopupState::new(),
        }
    }

    /// Translate a key press into an action. Returns `Close` to dismiss menu only.
    pub fn handle_key(&mut self, code: ratatui::crossterm::event::KeyCode) -> MenuAction {
        use ratatui::crossterm::event::KeyCode;
        match self {
            MenuState::Account { info, popup } => match code {
                KeyCode::Esc | KeyCode::Char('q') => MenuAction::Close,
                KeyCode::Down | KeyCode::Char('j') => {
                    popup.scroll_down(u16::MAX);
                    MenuAction::Noop
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    popup.scroll_up();
                    MenuAction::Noop
                }
                KeyCode::PageDown => {
                    popup.page_down(5, u16::MAX);
                    MenuAction::Noop
                }
                KeyCode::PageUp => {
                    popup.page_up(5);
                    MenuAction::Noop
                }
                KeyCode::Home => {
                    popup.reset();
                    MenuAction::Noop
                }
                KeyCode::Char('u') => MenuAction::Use(info.alias.clone()),
                KeyCode::Char('l') => {
                    MenuAction::ReloginRequest(info.alias.clone(), info.email.clone())
                }
                KeyCode::Char('n') => MenuAction::Rename(info.alias.clone()),
                KeyCode::Char('r') => MenuAction::RefreshOne(info.alias.clone()),
                KeyCode::Char('w') => MenuAction::WarmupOne(info.alias.clone()),
                KeyCode::Char('c') => MenuAction::ConsumeResetCard(info.alias.clone()),
                KeyCode::Char('d') => MenuAction::DeleteRequest(info.alias.clone()),
                _ => MenuAction::Noop,
            },
            MenuState::Add { .. } => match code {
                KeyCode::Esc | KeyCode::Char('q') => MenuAction::Close,
                KeyCode::Char('b') => MenuAction::Add { device: false },
                KeyCode::Char('d') => MenuAction::Add { device: true },
                _ => MenuAction::Noop,
            },
            MenuState::ReloginFlow { alias, .. } => match code {
                KeyCode::Esc | KeyCode::Char('q') => MenuAction::Close,
                KeyCode::Char('b') => MenuAction::Relogin {
                    alias: alias.clone(),
                    device: false,
                },
                KeyCode::Char('d') => MenuAction::Relogin {
                    alias: alias.clone(),
                    device: true,
                },
                _ => MenuAction::Noop,
            },
            MenuState::Batch { .. } => match code {
                KeyCode::Esc | KeyCode::Char('q') => MenuAction::Close,
                KeyCode::Char('r') => MenuAction::BatchRefresh,
                KeyCode::Char('w') => MenuAction::BatchWarmup,
                KeyCode::Char('l') => MenuAction::BatchReloginRequest,
                KeyCode::Char('d') => MenuAction::BatchDeleteRequest,
                _ => MenuAction::Noop,
            },
            MenuState::BatchReloginFlow { .. } => match code {
                KeyCode::Esc | KeyCode::Char('q') => MenuAction::Close,
                KeyCode::Char('b') => MenuAction::BatchRelogin { device: false },
                KeyCode::Char('d') => MenuAction::BatchRelogin { device: true },
                _ => MenuAction::Noop,
            },
        }
    }

    pub fn render(&mut self, f: &mut Frame, area: Rect) {
        let key_style = Style::default().fg(C_YELLOW).add_modifier(Modifier::BOLD);
        let label_style = Style::default().fg(C_WHITE);
        let dim = Style::default().fg(DIM);
        let header_style = Style::default().fg(C_CYAN);

        match self {
            MenuState::Account { info, popup } => {
                let title = "Account details";
                let mut left_lines = vec![Line::from(Span::styled(
                    "Identity",
                    header_style.add_modifier(Modifier::BOLD),
                ))];
                let mut identity = vec![
                    Span::styled(
                        info.alias.clone(),
                        Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("  ", Style::default()),
                    Span::styled(
                        info.plan_label.clone(),
                        Style::default().fg(C_YELLOW).add_modifier(Modifier::BOLD),
                    ),
                ];
                if info.is_current {
                    identity.push(Span::styled(
                        "  ● active",
                        Style::default().fg(C_GREEN).add_modifier(Modifier::BOLD),
                    ));
                }
                left_lines.push(Line::from(identity));
                if let Some(email) = &info.email {
                    left_lines.push(Line::from(vec![
                        Span::styled("email      ", dim),
                        Span::styled(email.clone(), Style::default().fg(C_WHITE)),
                    ]));
                }
                if info.workspace_name.is_some() || info.plan_type.is_some() {
                    left_lines.push(Line::from(vec![
                        Span::styled("workspace  ", dim),
                        Span::styled(
                            info.workspace_name
                                .clone()
                                .unwrap_or_else(|| "Personal".into()),
                            label_style,
                        ),
                        Span::styled(
                            info.plan_type
                                .as_ref()
                                .map(|value| format!("  ·  {value}"))
                                .unwrap_or_default(),
                            dim,
                        ),
                    ]));
                }
                if let Some(account_id) = &info.account_id {
                    left_lines.push(Line::from(vec![
                        Span::styled("account id ", dim),
                        Span::styled(account_id.clone(), dim),
                    ]));
                }
                if let Some(user_id) = &info.user_id {
                    left_lines.push(Line::from(vec![
                        Span::styled("user id    ", dim),
                        Span::styled(user_id.clone(), dim),
                    ]));
                }
                if info.is_fedramp {
                    left_lines.push(Line::from(vec![
                        Span::styled("route      ", dim),
                        Span::styled("FedRAMP", Style::default().fg(C_YELLOW)),
                    ]));
                }
                for organization in &info.organizations {
                    left_lines.push(Line::from(vec![
                        Span::styled("organization  ", dim),
                        Span::styled(organization.clone(), label_style),
                    ]));
                }
                for expiry in &info.auth_expiries {
                    if let Some((name, details)) = expiry.split_once(" · ") {
                        left_lines.push(Line::from(vec![
                            Span::styled(
                                name.to_string(),
                                Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(format!(" · {details}"), dim),
                        ]));
                    } else {
                        left_lines.push(Line::from(Span::styled(expiry.clone(), dim)));
                    }
                }
                left_lines.push(Line::from(""));
                left_lines.push(Line::from(Span::styled(
                    "Quota pools",
                    header_style.add_modifier(Modifier::BOLD),
                )));
                left_lines.extend(quota_lines(info.usage.as_deref()));
                for item in &info.usage_meta {
                    left_lines.push(Line::from(Span::styled(item.clone(), dim)));
                }
                let cards = info
                    .reset_cards
                    .map(|count| format!("{count} available"))
                    .unwrap_or_else(|| "not available".to_string());
                let cards_color = info
                    .usage
                    .as_deref()
                    .map(super::ui::reset_cards_color)
                    .unwrap_or(DIM);
                let cards_style = Style::default().fg(cards_color);
                left_lines.push(Line::from(vec![
                    Span::styled("Reset cards  ", cards_style.add_modifier(Modifier::BOLD)),
                    Span::styled(cards, cards_style),
                ]));
                for (idx, expiry) in info.reset_card_expiries.iter().enumerate() {
                    let note = if idx == 0 { "  next to use" } else { "" };
                    left_lines.push(Line::from(vec![
                        Span::styled(format!("  #{}  ", idx + 1), dim),
                        Span::styled(expiry.clone(), cards_style),
                        Span::styled(note, dim),
                    ]));
                }
                left_lines.push(Line::from(""));
                left_lines.push(Line::from(Span::styled(
                    format!("Models ({})", info.models.len()),
                    header_style.add_modifier(Modifier::BOLD),
                )));
                for model in &info.models {
                    let mut spans = vec![Span::styled("● ", Style::default().fg(C_CYAN))];
                    spans.extend(model_line_spans(model, label_style));
                    left_lines.push(Line::from(spans));
                }
                left_lines.push(Line::from(""));
                left_lines.push(Line::from(Span::styled(
                    "Actions",
                    header_style.add_modifier(Modifier::BOLD),
                )));
                let actions = [
                    ("u", "use", true),
                    ("r", "refresh", true),
                    ("w", "warmup", true),
                    ("c", "card", info.can_consume_reset_card),
                    ("l", "login", true),
                    ("n", "rename", true),
                    ("d", "delete", true),
                ];
                for row in [&actions[..4], &actions[4..]] {
                    let mut action_spans = Vec::new();
                    for (idx, (key, label, enabled)) in row.iter().enumerate() {
                        if idx > 0 {
                            action_spans.push(Span::styled("  ·  ", dim));
                        }
                        action_spans.push(Span::styled(
                            (*key).to_string(),
                            if *enabled { key_style } else { dim },
                        ));
                        action_spans.push(Span::styled(
                            format!(" {label}"),
                            if *enabled { label_style } else { dim },
                        ));
                    }
                    left_lines.push(Line::from(action_spans));
                }
                left_lines.push(Line::from(""));
                left_lines.push(Line::from(Span::styled(
                    "j k / arrows / PgUp PgDn scroll details · esc / q cancel",
                    dim,
                )));
                render_popup(f, title, &left_lines, popup, area);
            }
            MenuState::Add { popup } => {
                let title = "Add new account";
                let mut lines: Vec<Line<'static>> = Vec::new();
                lines.push(Line::from(Span::styled("Choose OAuth flow:", header_style)));
                lines.push(Line::from(""));
                lines.extend(menu_items(
                    &[
                        ("b", "Browser (PKCE, opens local callback)"),
                        ("d", "Device code (for headless / no browser)"),
                    ],
                    key_style,
                    label_style,
                ));
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled("esc / q to cancel", dim)));
                render_popup(f, title, &lines, popup, area);
            }
            MenuState::ReloginFlow {
                alias,
                email,
                popup,
            } => {
                let header = match email {
                    Some(e) => format!("{alias}  ({e})"),
                    None => alias.clone(),
                };
                let mut lines: Vec<Line<'static>> =
                    vec![Line::from(Span::styled(header, header_style))];
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled("Choose OAuth flow:", header_style)));
                lines.push(Line::from(""));
                lines.extend(menu_items(
                    &[
                        ("b", "Browser (PKCE, opens local callback)"),
                        ("d", "Device code (for headless / no browser)"),
                    ],
                    key_style,
                    label_style,
                ));
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled("esc / q to cancel", dim)));
                render_popup(f, "re-Login", &lines, popup, area);
            }
            MenuState::Batch { count, popup } => {
                let title = "Batch";
                let header = format!("{count} account(s) marked");
                let mut lines: Vec<Line<'static>> = Vec::new();
                lines.push(Line::from(Span::styled(header, header_style)));
                lines.push(Line::from(""));
                lines.extend(menu_items(
                    &[
                        ("r", "Refresh selected"),
                        ("w", "Warmup selected"),
                        ("l", "re-Login selected (sequential)"),
                        ("d", "Delete selected"),
                    ],
                    key_style,
                    label_style,
                ));
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled("esc / q to cancel", dim)));
                render_popup(f, title, &lines, popup, area);
            }
            MenuState::BatchReloginFlow { count, popup } => {
                let mut lines: Vec<Line<'static>> = Vec::new();
                lines.push(Line::from(Span::styled(
                    format!("{count} account(s) marked"),
                    header_style,
                )));
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "Sequential re-login. Browser uses local port 1455 each round.",
                    Style::default().fg(DIM),
                )));
                lines.push(Line::from(""));
                lines.extend(menu_items(
                    &[("b", "Browser (PKCE)"), ("d", "Device code")],
                    key_style,
                    label_style,
                ));
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled("esc / q to cancel", dim)));
                render_popup(f, "Batch re-Login", &lines, popup, area);
            }
        }
    }
}

fn menu_items(items: &[(&str, &str)], key_style: Style, label_style: Style) -> Vec<Line<'static>> {
    let key_w = items
        .iter()
        .map(|(k, _)| k.chars().count())
        .max()
        .unwrap_or(1);
    items
        .iter()
        .map(|(k, label)| {
            let pad = key_w.saturating_sub(k.chars().count());
            Line::from(vec![
                Span::raw("  "),
                Span::styled((*k).to_string(), key_style),
                Span::raw(" ".repeat(pad)),
                Span::raw("  "),
                Span::styled((*label).to_string(), label_style),
            ])
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend, crossterm::event::KeyCode, style::Color};

    use super::{
        AccountMenuInfo, C_CYAN, C_GREEN, C_PURPLE, C_RED, C_YELLOW, MenuAction, MenuState,
        model_line_spans, quota_lines,
    };
    use crate::usage::{AdditionalRateLimit, ResetCredit, UsageInfo, WindowUsage};

    fn find_text(backend: &TestBackend, needle: &str) -> Option<(u16, u16)> {
        let area = backend.buffer().area;
        for y in 0..area.height {
            let row = (0..area.width)
                .map(|x| {
                    backend
                        .buffer()
                        .cell((x, y))
                        .expect("cell inside test buffer")
                        .symbol()
                })
                .collect::<String>();
            if let Some(x) = row.find(needle) {
                return Some((x as u16, y));
            }
        }
        None
    }

    fn account_menu_with_reset_card_expiring_in(seconds: i64) -> MenuState {
        let expires_at =
            chrono::DateTime::from_timestamp(crate::auth::now_unix_secs() + seconds, 0)
                .unwrap()
                .to_rfc3339();
        let usage = UsageInfo {
            reset_credits_available_count: Some(1),
            reset_credits: vec![ResetCredit {
                id: "card".into(),
                granted_at: None,
                expires_at: Some(expires_at),
            }],
            ..Default::default()
        };

        MenuState::account(AccountMenuInfo {
            alias: "account".into(),
            email: None,
            account_id: None,
            user_id: None,
            workspace_name: None,
            is_fedramp: false,
            plan_label: "Pro".into(),
            plan_type: Some("pro".into()),
            is_current: true,
            organizations: Vec::new(),
            auth_expiries: Vec::new(),
            usage: Some(Box::new(usage)),
            usage_meta: Vec::new(),
            models: Vec::new(),
            reset_cards: Some(1),
            reset_card_expiries: vec!["expires soon".into()],
            can_consume_reset_card: true,
        })
    }

    #[test]
    fn account_details_use_reset_card_expiry_warning_colors() {
        for (seconds, expected) in [
            (2 * 24 * 60 * 60, C_RED),
            (6 * 24 * 60 * 60, C_YELLOW),
            (8 * 24 * 60 * 60, C_GREEN),
        ] {
            let mut menu = account_menu_with_reset_card_expiring_in(seconds);
            let backend = TestBackend::new(100, 30);
            let mut terminal = Terminal::new(backend).unwrap();

            terminal
                .draw(|frame| menu.render(frame, frame.area()))
                .unwrap();

            for needle in ["Reset cards", "1 available", "expires soon"] {
                let pos = find_text(terminal.backend(), needle).expect("reset card detail text");
                assert_eq!(
                    terminal.backend().buffer().cell(pos).unwrap().fg,
                    expected,
                    "{needle} should use the reset-card expiry warning color"
                );
            }
        }
    }

    #[test]
    fn unknown_key_keeps_menu_open() {
        let mut menu = MenuState::add();
        assert!(matches!(
            menu.handle_key(KeyCode::Char('x')),
            MenuAction::Noop
        ));
    }

    #[test]
    fn account_details_navigation_scrolls_popup() {
        let mut menu = MenuState::account(AccountMenuInfo {
            alias: "account".into(),
            email: None,
            account_id: None,
            user_id: None,
            workspace_name: None,
            is_fedramp: false,
            plan_label: "Unknown".into(),
            plan_type: None,
            is_current: false,
            organizations: Vec::new(),
            auth_expiries: Vec::new(),
            usage: None,
            usage_meta: Vec::new(),
            models: Vec::new(),
            reset_cards: None,
            reset_card_expiries: Vec::new(),
            can_consume_reset_card: false,
        });

        assert!(matches!(menu.handle_key(KeyCode::Down), MenuAction::Noop));
        let MenuState::Account { popup, .. } = menu else {
            unreachable!();
        };
        assert_eq!(popup.scroll, 1);
    }

    #[test]
    fn quota_visuals_include_main_and_future_model_pools() {
        let now = crate::auth::now_unix_secs();
        let window = WindowUsage {
            used_percent: Some(80.0),
            resets_at: Some(now + 2 * 60 * 60),
            window_minutes: Some(300),
        };
        let usage = UsageInfo {
            primary: Some(window.clone()),
            additional_limits: vec![AdditionalRateLimit {
                limit_name: Some("GPT-6-Codex-Burst".to_string()),
                metered_feature: Some("codex_futureburst".to_string()),
                primary: Some(window),
                ..Default::default()
            }],
            ..Default::default()
        };
        let text = quota_lines(Some(&usage))
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("Main"));
        assert!(text.contains("GPT-6-Codex-Burst"));
        assert!(text.contains('█'));
        assert!(text.contains('┃'));
        assert!(text.contains("20% left"));
        assert!(text.contains("reset"));
        assert!(text.contains("over pace"));
        assert!(!text.contains("Pace"));
        assert!(!text.contains("Rest"));
    }

    #[test]
    fn model_reasoning_efforts_use_semantic_colors() {
        let spans = model_line_spans(
            "GPT-5.6-Sol · default medium · allowed low, medium, high, xhigh, max, ultra",
            ratatui::style::Style::default(),
        );
        let color_for = |effort: &str| {
            spans
                .iter()
                .find(|span| span.content == effort)
                .and_then(|span| span.style.fg)
        };

        assert_eq!(color_for("low"), Some(C_GREEN));
        assert_eq!(color_for("medium"), Some(C_CYAN));
        assert_eq!(color_for("high"), Some(C_YELLOW));
        assert_eq!(color_for("xhigh"), Some(Color::LightMagenta));
        assert_eq!(color_for("max"), Some(C_RED));
        assert_eq!(color_for("ultra"), Some(C_PURPLE));
    }

    #[test]
    fn realistic_account_detail_keeps_models_in_the_single_column() {
        let now = crate::auth::now_unix_secs();
        let window = WindowUsage {
            used_percent: Some(50.0),
            resets_at: Some(now + 3_600),
            window_minutes: Some(300),
        };
        let usage = UsageInfo {
            primary: Some(window.clone()),
            secondary: Some(window.clone()),
            additional_limits: vec![AdditionalRateLimit {
                limit_name: Some("GPT-5.3-Codex-Spark".into()),
                primary: Some(window.clone()),
                secondary: Some(window),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut menu = MenuState::account(AccountMenuInfo {
            alias: "account".into(),
            email: Some("account@example.com".into()),
            account_id: Some("account-id".into()),
            user_id: Some("user-id".into()),
            workspace_name: Some("Night City".into()),
            is_fedramp: false,
            plan_label: "Pro 20×".into(),
            plan_type: Some("pro".into()),
            is_current: true,
            organizations: vec!["Night City · Owner · default workspace".into()],
            auth_expiries: vec![
                "ID token · expires soon".into(),
                "Access token · expires soon".into(),
            ],
            usage: Some(Box::new(usage)),
            usage_meta: vec!["  updated now".into()],
            models: vec!["  Official Model".into(), "    Official description".into()],
            reset_cards: Some(0),
            reset_card_expiries: Vec::new(),
            can_consume_reset_card: false,
        });
        let backend = TestBackend::new(160, 40);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| menu.render(frame, frame.area()))
            .unwrap();

        let models = find_text(terminal.backend(), "Models").expect("models heading");
        assert!(models.0 < 80, "models should follow the account details");
    }
}
