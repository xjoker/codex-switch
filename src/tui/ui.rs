use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
};

use super::app::{App, Tab, UsageStatus};
use super::keymap;
use super::popup;
use crate::jwt::PlanKind;
use crate::output::{
    format_local_time, format_reset_short, format_reset_time, reset_credits_count,
};
use crate::usage::{UsageInfo, is_available};

// ── RGB-only color palette ───────────────────────────────
// All colors are explicit RGB to avoid mixing ANSI-16 + 24-bit,
// which causes rendering glitches on Windows conhost (cmd.exe / PowerShell).

const BG: Color = Color::Rgb(24, 24, 24); // near-black background
const C_WHITE: Color = Color::Rgb(240, 240, 240); // primary text
const C_GRAY: Color = Color::Rgb(180, 180, 180); // secondary text
const DIM: Color = Color::Rgb(120, 120, 120); // dim labels / placeholders
const C_RED: Color = Color::Rgb(255, 90, 90); // errors, warnings
const C_GREEN: Color = Color::Rgb(80, 220, 120); // OK, active
const C_YELLOW: Color = Color::Rgb(255, 220, 80); // keys, markers
const C_CYAN: Color = Color::Rgb(100, 210, 255); // headers, prompts
const C_MAGENTA: Color = Color::Rgb(220, 130, 255); // team plans
const C_BLUE: Color = Color::Rgb(80, 140, 220); // borders (inactive)
const C_HIGHLIGHT_BG: Color = Color::Rgb(55, 55, 65); // selected row bg

fn base() -> Style {
    Style::default().bg(BG)
}

fn status_message_color(is_error: bool) -> Color {
    if is_error { C_RED } else { C_CYAN }
}

pub fn render(f: &mut Frame, app: &mut App) {
    let area = f.area();

    // Paint the entire area with a solid background first
    f.render_widget(Block::default().style(base()), area);

    let status_height = status_bar_height(app, area.width);

    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),                    // tab bar
            Constraint::Min(6),                       // active tab content
            Constraint::Length(status_height as u16), // status bar
        ])
        .split(area);

    render_tab_bar(f, app, vertical[0]);

    match app.active_tab {
        Tab::Accounts => {
            let content = vertical[1];
            let detail_height = if app.detail_visible {
                detail_panel_height(app).min(content.height.saturating_sub(6))
            } else {
                0
            };
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(6),                // account list
                    Constraint::Length(detail_height), // detail panel
                ])
                .split(content);
            render_account_table(f, app, rows[0]);
            if app.detail_visible {
                render_detail_panel(f, app, rows[1]);
            }
        }
        Tab::Providers => render_providers_tab(f, app, vertical[1]),
    }

    render_status_bar(f, app, vertical[2]);

    // Overlays (rendered last, on top of everything).
    // Help popup takes top priority since the user invoked it explicitly.
    if let Some(state) = app.help_popup.as_mut() {
        render_help_popup(f, state, area);
    } else if let Some(menu) = app.menu.as_mut() {
        menu.render(f, area);
    }
}

fn render_help_popup(f: &mut Frame, state: &mut popup::PopupState, area: ratatui::layout::Rect) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let key_style = Style::default().fg(C_YELLOW).add_modifier(Modifier::BOLD);
    let label_style = Style::default().fg(C_WHITE);
    let heading_style = Style::default().fg(C_CYAN).add_modifier(Modifier::BOLD);
    let dim_style = Style::default().fg(DIM);

    // Compute key column width for alignment within section
    let groups = keymap::help_sections();
    let key_col = groups
        .iter()
        .flat_map(|(_, items)| items.iter())
        .map(|(k, _)| display_width(k))
        .max()
        .unwrap_or(8);

    for (i, (heading, items)) in groups.iter().enumerate() {
        if i > 0 {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(
            (*heading).to_string(),
            heading_style,
        )));
        for (k, label) in items {
            let pad = key_col.saturating_sub(display_width(k));
            let mut spans: Vec<Span<'static>> = Vec::new();
            spans.push(Span::raw("  "));
            spans.push(Span::styled((*k).to_string(), key_style));
            if pad > 0 {
                spans.push(Span::raw(" ".repeat(pad)));
            }
            spans.push(Span::raw("  "));
            spans.push(Span::styled((*label).to_string(), label_style));
            lines.push(Line::from(spans));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  esc / q / h to close \u{2022} j k arrows / PgUp PgDn to scroll",
        dim_style,
    )));

    popup::render_popup(f, "Help", &lines, state, area);
}

fn display_width(s: &str) -> usize {
    Line::from(s).width()
}

#[derive(Debug, PartialEq, Eq)]
struct TableTextWidths {
    alias: u16,
    email: u16,
    plan: u16,
}

fn table_text_widths(
    total_width: u16,
    aliases: &[&str],
    emails: &[&str],
    plans: &[&str],
) -> TableTextWidths {
    let desired = |header: &str, values: &[&str]| {
        values
            .iter()
            .map(|value| u16::try_from(display_width(value)).unwrap_or(u16::MAX))
            .chain(std::iter::once(
                u16::try_from(display_width(header)).unwrap_or(u16::MAX),
            ))
            .max()
            .unwrap_or(0)
    };
    let mut widths = TableTextWidths {
        alias: desired("Alias", aliases).max(5),
        email: desired("Email", emails).max(5),
        plan: desired("Plan", plans).max(4),
    };

    // Borders, column spacing, marker and fixed quota columns consume 64 cells.
    let budget = total_width.saturating_sub(64).max(14);
    let total = u32::from(widths.alias) + u32::from(widths.email) + u32::from(widths.plan);
    let mut excess = total.saturating_sub(u32::from(budget));
    for (width, minimum) in [
        (&mut widths.email, 5_u16),
        (&mut widths.plan, 4_u16),
        (&mut widths.alias, 5_u16),
    ] {
        let shrink = excess.min(u32::from(width.saturating_sub(minimum)));
        *width -= shrink as u16;
        excess -= shrink;
    }
    widths
}

fn render_account_table(f: &mut Frame, app: &App, area: Rect) {
    if app.accounts.is_empty() {
        let block = Block::default()
            .title(" codex-switch ")
            .borders(Borders::ALL)
            .border_style(base().fg(C_BLUE))
            .style(base());
        let hint = Paragraph::new(Line::from(vec![
            Span::styled("No accounts yet. Press ", Style::default().fg(DIM)),
            Span::styled(
                "a",
                Style::default().fg(C_YELLOW).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" to add one, or ", Style::default().fg(DIM)),
            Span::styled(
                "q",
                Style::default().fg(C_YELLOW).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" to quit.", Style::default().fg(DIM)),
        ]))
        .block(block)
        .alignment(ratatui::layout::Alignment::Center);
        f.render_widget(hint, area);
        return;
    }

    let hdr = base().fg(C_CYAN).add_modifier(Modifier::BOLD);
    let header = Row::new(vec![
        Cell::from(" ").style(base().fg(DIM)),
        Cell::from("Alias").style(hdr),
        Cell::from("Email").style(hdr),
        Cell::from("Plan").style(hdr),
        Cell::from("Status").style(hdr),
        Cell::from("5h").style(hdr),
        Cell::from("7d").style(hdr),
        Cell::from("5h Reset").style(hdr),
        Cell::from("7d Reset").style(hdr),
        Cell::from("Cards").style(hdr),
    ])
    .height(1);

    let mut rows: Vec<Row> = Vec::new();
    let mut render_selected: usize = 0;
    for (view_i, &acc_i) in app.view_indices.iter().enumerate() {
        let entry = &app.accounts[acc_i];
        let main_row = {
            let is_marked = app.marked.contains(&entry.alias);
            let marker = if is_marked {
                ">"
            } else if entry.is_current {
                "*"
            } else {
                " "
            };
            let marker_style = if is_marked {
                base().fg(C_YELLOW).add_modifier(Modifier::BOLD)
            } else if entry.is_current {
                base().fg(C_GREEN).add_modifier(Modifier::BOLD)
            } else {
                base()
            };

            let is_selected = view_i == app.selected;
            let row_style = if is_selected {
                base().fg(C_WHITE).add_modifier(Modifier::BOLD)
            } else {
                base().fg(C_GRAY)
            };

            let email = entry.info.email.as_deref().unwrap_or("--").to_string();
            let api_plan = if let UsageStatus::Loaded(u) = &entry.usage {
                u.plan_type.as_deref()
            } else {
                None
            };
            let effective_plan = api_plan.or(entry.info.plan_type.as_deref());
            let plan_label = entry.info.plan_label_with(effective_plan);
            let plan_style = plan_color(effective_plan, is_selected);

            let now = crate::auth::now_unix_secs();

            let (
                status_text,
                status_color,
                pct_5h,
                pct_7d,
                reset_5h,
                reset_5h_color,
                reset_7d,
                reset_7d_color,
                reset_cards,
                reset_cards_color,
            ): (
                String,
                Color,
                String,
                String,
                String,
                Color,
                String,
                Color,
                String,
                Color,
            ) = match &entry.usage {
                UsageStatus::Idle => (
                    "--".into(),
                    DIM,
                    "--".into(),
                    "--".into(),
                    "--".into(),
                    DIM,
                    "--".into(),
                    DIM,
                    "--".into(),
                    DIM,
                ),
                UsageStatus::Loading => (
                    "...".into(),
                    C_YELLOW,
                    "...".into(),
                    "...".into(),
                    "loading".into(),
                    DIM,
                    "loading".into(),
                    DIM,
                    "...".into(),
                    C_YELLOW,
                ),
                UsageStatus::Error(_) => (
                    "Error".into(),
                    C_RED,
                    "Err".into(),
                    "Err".into(),
                    "--".into(),
                    DIM,
                    "--".into(),
                    DIM,
                    "Err".into(),
                    C_RED,
                ),
                UsageStatus::Loaded(u) => {
                    let refreshing = app.is_refreshing(&entry.alias);
                    let over_5h = u.primary.as_ref().is_some_and(|w| {
                        let used = w.used_percent.unwrap_or(0.0);
                        // Suppress pace warning when usage is negligible — a fresh window
                        // always shows used > pace near t=0, which is noise not a real warning.
                        used >= 10.0
                            && crate::usage::visible_pace_percent(w, crate::usage::WINDOW_5H_SECS)
                                .is_some_and(|pace| used > pace)
                    });
                    let over_7d = u.secondary.as_ref().is_some_and(|w| {
                        let used = w.used_percent.unwrap_or(0.0);
                        used >= 10.0
                            && crate::usage::visible_pace_percent(w, crate::usage::WINDOW_7D_SECS)
                                .is_some_and(|pace| used > pace)
                    });
                    let p5 = u
                        .primary
                        .as_ref()
                        .and_then(|w| w.used_percent)
                        .map(|p| {
                            let s = format!("{:.0}%", (100.0 - p).max(0.0));
                            if over_5h { format!("{s}!") } else { s }
                        })
                        .unwrap_or_else(|| "--".into());
                    let p7 = u
                        .secondary
                        .as_ref()
                        .and_then(|w| w.used_percent)
                        .map(|p| {
                            let s = format!("{:.0}%", (100.0 - p).max(0.0));
                            if over_7d { format!("{s}!") } else { s }
                        })
                        .unwrap_or_else(|| "--".into());
                    let r5_ts = u.primary.as_ref().and_then(|w| w.resets_at);
                    let r5 = r5_ts.map(format_reset_short).unwrap_or_else(|| "--".into());
                    let r5c = r5_ts.map(|ts| reset_color(ts - now)).unwrap_or(DIM);
                    let r7_ts = u.secondary.as_ref().and_then(|w| w.resets_at);
                    let r7 = r7_ts.map(format_reset_short).unwrap_or_else(|| "--".into());
                    let r7c = r7_ts.map(|ts| reset_color(ts - now)).unwrap_or(DIM);
                    let card_refreshing = app.reset_card_refresh_tasks.contains_key(&entry.alias);
                    let card_cooling = app
                        .reset_card_cooldown_until
                        .is_some_and(|until| std::time::Instant::now() < until);
                    let (cards, cards_color) =
                        reset_cards_table_state(u, card_refreshing, card_cooling);
                    if refreshing {
                        (
                            "Refresh".into(),
                            C_YELLOW,
                            p5,
                            p7,
                            r5,
                            r5c,
                            r7,
                            r7c,
                            cards,
                            cards_color,
                        )
                    } else if is_available(u) {
                        (
                            "OK".into(),
                            C_GREEN,
                            p5,
                            p7,
                            r5,
                            r5c,
                            r7,
                            r7c,
                            cards,
                            cards_color,
                        )
                    } else {
                        (
                            "Limited".into(),
                            C_RED,
                            p5,
                            p7,
                            r5,
                            r5c,
                            r7,
                            r7c,
                            cards,
                            cards_color,
                        )
                    }
                }
            };

            Row::new(vec![
                Cell::from(Span::styled(marker, marker_style)),
                Cell::from(entry.alias.clone()).style(row_style),
                Cell::from(email).style(row_style),
                Cell::from(plan_label).style(plan_style),
                Cell::from(status_text).style(base().fg(status_color).add_modifier(
                    if is_selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    },
                )),
                Cell::from(pct_5h.clone()).style(usage_pct_style(&pct_5h, is_selected)),
                Cell::from(pct_7d.clone()).style(usage_pct_style(&pct_7d, is_selected)),
                Cell::from(reset_5h).style(base().fg(reset_5h_color)),
                Cell::from(reset_7d).style(base().fg(reset_7d_color)),
                Cell::from(reset_cards).style(base().fg(reset_cards_color)),
            ])
            .height(1)
        };

        if view_i == app.selected {
            render_selected = rows.len();
        }
        rows.push(main_row);
    }

    let loading_count = app.loading_count();
    let mut title = if let Some(s) = &app.search {
        format!(
            " Accounts ({}/{}) [/{s}]",
            app.view_indices.len(),
            app.accounts.len(),
            s = s.query
        )
    } else {
        format!(" Accounts ({})", app.accounts.len())
    };
    if loading_count > 0 {
        title.push_str(&format!(" -- fetching {}...", loading_count));
    }
    if !app.marked.is_empty() {
        title.push_str(&format!(" [{} marked]", app.marked.len()));
    }
    if let Some(secs) = app.auto_refresh_remaining_secs() {
        title.push_str(&format!(" auto:{}", format_auto_refresh_remaining(secs)));
        if app.auto_warmup_enabled {
            title.push_str("+warm");
        }
    }
    title.push_str(&format!(" sort:{} ", app.sort_mode.as_str()));

    let mut table_state = TableState::default().with_selected(render_selected);

    let aliases: Vec<&str> = app
        .view_indices
        .iter()
        .map(|&idx| app.accounts[idx].alias.as_str())
        .collect();
    let emails: Vec<&str> = app
        .view_indices
        .iter()
        .map(|&idx| app.accounts[idx].info.email.as_deref().unwrap_or("--"))
        .collect();
    let plan_labels: Vec<String> = app
        .view_indices
        .iter()
        .map(|&idx| {
            let entry = &app.accounts[idx];
            let api_plan = match &entry.usage {
                UsageStatus::Loaded(u) => u.plan_type.as_deref(),
                _ => None,
            };
            entry
                .info
                .plan_label_with(api_plan.or(entry.info.plan_type.as_deref()))
        })
        .collect();
    let plans: Vec<&str> = plan_labels.iter().map(String::as_str).collect();
    let text_widths = table_text_widths(area.width, &aliases, &emails, &plans);

    let table = Table::new(
        rows,
        [
            Constraint::Length(2),                 // marker
            Constraint::Length(text_widths.alias), // alias
            Constraint::Length(text_widths.email), // email
            Constraint::Length(text_widths.plan),  // plan
            Constraint::Length(8),                 // status
            Constraint::Length(6),                 // 5h %
            Constraint::Length(6),                 // 7d %
            Constraint::Length(12),                // 5h reset
            Constraint::Length(12),                // 7d reset
            Constraint::Length(7),                 // reset cards
        ],
    )
    .header(header)
    .block(
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(base().fg(C_BLUE))
            .style(base()),
    )
    .row_highlight_style(
        Style::default()
            .bg(C_HIGHLIGHT_BG)
            .add_modifier(Modifier::BOLD),
    )
    .style(base());

    f.render_stateful_widget(table, area, &mut table_state);
}

fn usage_gauges_height(usage: &UsageInfo) -> u16 {
    let multi_pool = !usage.additional_limits.is_empty();
    let mut height = 0u16;
    let mut pool_count = 0u16;
    let mut add_pool = |primary: bool, secondary: bool| {
        if pool_count > 0 {
            height = height.saturating_add(1);
        }
        if multi_pool {
            height = height.saturating_add(1);
        }
        height = height.saturating_add(u16::from(primary) * 2);
        height = height.saturating_add(u16::from(secondary) * 2);
        if !primary && !secondary {
            height = height.saturating_add(1);
        }
        pool_count = pool_count.saturating_add(1);
    };
    add_pool(usage.primary.is_some(), usage.secondary.is_some());
    for pool in &usage.additional_limits {
        add_pool(pool.primary.is_some(), pool.secondary.is_some());
    }
    height.max(1)
}

fn detail_panel_height(app: &App) -> u16 {
    let gauges = app
        .selected_account_idx()
        .and_then(|idx| app.accounts.get(idx))
        .and_then(|entry| match &entry.usage {
            UsageStatus::Loaded(usage) => Some(usage_gauges_height(usage)),
            _ => None,
        })
        .unwrap_or(4);
    gauges.saturating_add(4)
}

fn render_detail_panel(f: &mut Frame, app: &App, area: Rect) {
    let entry = match app
        .selected_account_idx()
        .and_then(|idx| app.accounts.get(idx))
    {
        Some(e) => e,
        None => return,
    };

    let title = if entry.is_current {
        format!(" * {} (active) ", entry.alias)
    } else {
        format!(" {} ", entry.alias)
    };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(base().fg(if entry.is_current { C_GREEN } else { C_BLUE }))
        .style(base());

    let inner = block.inner(area);
    f.render_widget(block, area);

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1)])
        .margin(1)
        .split(inner);

    // Usage area
    match &entry.usage {
        UsageStatus::Idle => {
            let p = Paragraph::new("Press r to refresh usage").style(base().fg(DIM));
            f.render_widget(p, layout[0]);
        }
        UsageStatus::Loading => {
            let p = Paragraph::new("Fetching usage...").style(base().fg(C_YELLOW));
            f.render_widget(p, layout[0]);
        }
        UsageStatus::Error(e) => {
            let p = Paragraph::new(format!("Error: {}", e.detail)).style(base().fg(C_RED));
            f.render_widget(p, layout[0]);
        }
        UsageStatus::Loaded(u) => {
            render_usage_gauges(f, u, layout[0]);
        }
    }
}

pub(super) fn render_usage_gauges(f: &mut Frame, u: &UsageInfo, area: Rect) {
    let now = crate::auth::now_unix_secs();
    let multi_pool = !u.additional_limits.is_empty();
    let mut y = area.y;
    let mut render_pool = |f: &mut Frame,
                           name: &str,
                           primary: Option<&crate::usage::WindowUsage>,
                           secondary: Option<&crate::usage::WindowUsage>,
                           unavailable: bool| {
        if y > area.y {
            y = y.saturating_add(1);
        }
        if multi_pool && y < area.bottom() {
            let title = if unavailable {
                format!("{name}  unavailable")
            } else {
                name.to_string()
            };
            f.render_widget(
                Paragraph::new(title).style(base().fg(if unavailable { C_RED } else { C_CYAN })),
                Rect {
                    x: area.x,
                    y,
                    width: area.width,
                    height: 1,
                },
            );
            y = y.saturating_add(1);
        }
        if let Some(window) = primary
            && y < area.bottom()
        {
            let (label, window_secs) =
                quota_window_display(window, "5h", crate::usage::WINDOW_5H_SECS);
            render_usage_gauge(
                f,
                window,
                &label,
                window_secs,
                now,
                Rect {
                    x: area.x,
                    y,
                    width: area.width,
                    height: 2,
                },
            );
            y = y.saturating_add(2);
        }
        if let Some(window) = secondary
            && y < area.bottom()
        {
            let (label, window_secs) =
                quota_window_display(window, "7d", crate::usage::WINDOW_7D_SECS);
            render_usage_gauge(
                f,
                window,
                &label,
                window_secs,
                now,
                Rect {
                    x: area.x,
                    y,
                    width: area.width,
                    height: 2,
                },
            );
            y = y.saturating_add(2);
        }
        if primary.is_none() && secondary.is_none() && y < area.bottom() {
            f.render_widget(
                Paragraph::new("No active window").style(base().fg(DIM)),
                Rect {
                    x: area.x,
                    y,
                    width: area.width,
                    height: 1,
                },
            );
            y = y.saturating_add(1);
        }
    };

    render_pool(f, "Main", u.primary.as_ref(), u.secondary.as_ref(), false);
    for pool in &u.additional_limits {
        render_pool(
            f,
            pool.limit_name.as_deref().unwrap_or("Additional"),
            pool.primary.as_ref(),
            pool.secondary.as_ref(),
            pool.allowed == Some(false) || pool.limit_reached == Some(true),
        );
    }
}

fn quota_window_display(
    window: &crate::usage::WindowUsage,
    fallback_label: &str,
    fallback_secs: i64,
) -> (String, i64) {
    match window.window_minutes {
        Some(minutes) if minutes % 1_440 == 0 => {
            (format!("{}d", minutes / 1_440), minutes.saturating_mul(60))
        }
        Some(minutes) if minutes % 60 == 0 => {
            (format!("{}h", minutes / 60), minutes.saturating_mul(60))
        }
        Some(minutes) => (format!("{minutes}m"), minutes.saturating_mul(60)),
        None => (fallback_label.to_string(), fallback_secs),
    }
}

fn reset_cards_table_text(u: &UsageInfo) -> String {
    reset_credits_count(u)
        .map(|count| count.to_string())
        .or_else(|| u.reset_credits_error.as_ref().map(|_| "err".to_string()))
        .unwrap_or_else(|| "--".to_string())
}

fn reset_cards_table_state(u: &UsageInfo, refreshing: bool, cooling: bool) -> (String, Color) {
    if refreshing {
        return (
            reset_credits_count(u)
                .map(|count| format!("{count}↻"))
                .unwrap_or_else(|| "...".into()),
            C_CYAN,
        );
    }
    if cooling && crate::usage::should_fetch_reset_credit_details(u) {
        return (
            reset_credits_count(u)
                .map(|count| format!("{count}⏳"))
                .unwrap_or_else(|| "wait".into()),
            C_YELLOW,
        );
    }
    (reset_cards_table_text(u), reset_cards_color(u))
}

pub(super) fn reset_cards_color(u: &UsageInfo) -> Color {
    match reset_credits_count(u) {
        Some(0) => DIM,
        Some(_) => crate::usage::earliest_reset_credit(&u.reset_credits)
            .and_then(|credit| credit.expires_at.as_deref())
            .and_then(|expires_at| chrono::DateTime::parse_from_rfc3339(expires_at).ok())
            .map(|expires_at| expires_at.timestamp() - crate::auth::now_unix_secs())
            .map(|remaining| {
                if remaining < 3 * 24 * 60 * 60 {
                    C_RED
                } else if remaining < 7 * 24 * 60 * 60 {
                    C_YELLOW
                } else {
                    C_GREEN
                }
            })
            .unwrap_or_else(|| {
                if u.reset_credits_error.is_some() {
                    C_YELLOW
                } else {
                    DIM
                }
            }),
        None if u.reset_credits_error.is_some() => C_YELLOW,
        None => DIM,
    }
}

/// Top tab bar: Accounts (ChatGPT OAuth) vs Providers (third-party API+key).
fn render_tab_bar(f: &mut Frame, app: &App, area: Rect) {
    let active = base().fg(BG).bg(C_CYAN).add_modifier(Modifier::BOLD);
    let inactive = base().fg(C_GRAY);
    let (accounts_style, providers_style) = match app.active_tab {
        Tab::Accounts => (active, inactive),
        Tab::Providers => (inactive, active),
    };
    let line = Line::from(vec![
        Span::styled(
            format!(" Accounts ({}) ", app.accounts.len()),
            accounts_style,
        ),
        Span::raw("  "),
        Span::styled(
            format!(" Providers ({}) ", app.providers.len()),
            providers_style,
        ),
        Span::styled("   Tab to switch", base().fg(DIM)),
    ]);
    f.render_widget(Paragraph::new(line).style(base()), area);
}

/// Providers tab: read-only list of configured custom API providers. The stored
/// API key is never rendered.
fn render_providers_tab(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(" Custom providers ")
        .borders(Borders::ALL)
        .border_style(base().fg(C_BLUE))
        .style(base());
    let inner = block.inner(area);
    f.render_widget(block, area);

    if app.providers.is_empty() {
        let hint = Paragraph::new(Line::from(vec![
            Span::styled("No custom providers. Add one with ", base().fg(DIM)),
            Span::styled(
                "codex-switch provider add",
                base().fg(C_YELLOW).add_modifier(Modifier::BOLD),
            ),
            Span::styled(".", base().fg(DIM)),
        ]))
        .style(base());
        f.render_widget(hint, inner);
        return;
    }

    let header = Row::new(vec![
        Cell::from(" "),
        Cell::from("Alias"),
        Cell::from("Name"),
        Cell::from("Model"),
        Cell::from("Base URL"),
    ])
    .style(base().fg(C_CYAN).add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = app
        .providers
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let selected = i == app.provider_selected;
            let text_style = if selected {
                base().fg(C_WHITE).add_modifier(Modifier::BOLD)
            } else {
                base().fg(C_GRAY)
            };
            Row::new(vec![
                Cell::from(if selected { "\u{25b6}" } else { " " }).style(base().fg(C_GREEN)),
                Cell::from(p.alias.clone()).style(text_style),
                Cell::from(p.name.clone()).style(text_style),
                Cell::from(p.model.clone()).style(base().fg(C_CYAN)),
                Cell::from(p.base_url.clone()).style(base().fg(DIM)),
            ])
            .height(1)
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(2),
            Constraint::Length(20),
            Constraint::Length(24),
            Constraint::Length(28),
            Constraint::Min(20),
        ],
    )
    .header(header)
    .row_highlight_style(
        Style::default()
            .bg(C_HIGHLIGHT_BG)
            .add_modifier(Modifier::BOLD),
    )
    .style(base());

    let mut state = TableState::default().with_selected(app.provider_selected);
    f.render_stateful_widget(table, inner, &mut state);
}

fn render_status_bar(f: &mut Frame, app: &App, area: Rect) {
    // Add-provider wizard prompt takes top priority.
    if let Some(state) = &app.provider_add {
        use super::app::{ProviderAddStep, REASONING_CHOICES};
        let label = Span::styled(
            format!(" Add provider [{}]: ", state.step.prompt()),
            base().fg(C_CYAN).add_modifier(Modifier::BOLD),
        );
        let line = match state.step {
            ProviderAddStep::Reasoning => {
                let choice = REASONING_CHOICES[state.reasoning_idx];
                Line::from(vec![
                    label,
                    Span::styled(
                        format!("< {choice} >"),
                        base().fg(C_WHITE).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("  (←/→ choose, Enter next / Esc cancel)", base().fg(DIM)),
                ])
            }
            ProviderAddStep::WebSearch => {
                let value = if state.no_web_search {
                    "disabled"
                } else {
                    "enabled (default)"
                };
                Line::from(vec![
                    label,
                    Span::styled(
                        format!("[{value}]"),
                        base().fg(C_WHITE).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("  (Space toggle, Enter next / Esc cancel)", base().fg(DIM)),
                ])
            }
            _ => {
                let shown = if state.step.is_secret() {
                    "*".repeat(state.input.chars().count())
                } else {
                    state.input.clone()
                };
                Line::from(vec![
                    label,
                    Span::styled(shown, base().fg(C_WHITE).add_modifier(Modifier::BOLD)),
                    Span::styled("#", base().fg(C_GRAY)),
                    Span::styled("  (Enter next / Esc cancel)", base().fg(DIM)),
                ])
            }
        };
        f.render_widget(Paragraph::new(line).style(base()), area);
        return;
    }

    // Rename input takes top priority
    if let Some(rs) = &app.rename {
        let line = Line::from(vec![
            Span::styled(" Rename: ", base().fg(C_CYAN).add_modifier(Modifier::BOLD)),
            Span::styled(&rs.input, base().fg(C_WHITE).add_modifier(Modifier::BOLD)),
            Span::styled("#", base().fg(C_GRAY)),
            Span::styled("  (Enter confirm / Esc cancel)", base().fg(DIM)),
        ]);
        f.render_widget(Paragraph::new(line).style(base()), area);
        return;
    }

    // Confirmation prompt
    if let Some(confirm) = &app.confirm {
        let msg = match confirm {
            super::app::ConfirmAction::Delete(alias) => {
                format!("Delete profile '{alias}'? (y/n)")
            }
            super::app::ConfirmAction::BatchDelete(aliases) => {
                format!("Delete {} marked profile(s)? (y/n)", aliases.len())
            }
            super::app::ConfirmAction::ConsumeResetCard {
                alias, expires_at, ..
            } => {
                format!(
                    "Confirm reset card for '{alias}' expiring {expires_at}: y to use, any other key cancels"
                )
            }
            super::app::ConfirmAction::RemoveProvider(alias) => {
                format!("Remove provider '{alias}'? (y/n)")
            }
        };
        let line = Line::from(Span::styled(
            msg,
            base().fg(C_RED).add_modifier(Modifier::BOLD),
        ));
        f.render_widget(Paragraph::new(line).style(base()), area);
        return;
    }

    if app.search_active
        && let Some(s) = &app.search
    {
        let line = Line::from(vec![
            Span::styled(" /", base().fg(C_CYAN).add_modifier(Modifier::BOLD)),
            Span::styled(&s.query, base().fg(C_WHITE).add_modifier(Modifier::BOLD)),
            Span::styled("#", base().fg(C_GRAY)),
            Span::styled("  (Enter accept / Esc clear)", base().fg(DIM)),
        ]);
        f.render_widget(Paragraph::new(line).style(base()), area);
        return;
    }

    if let Some(s) = &app.status_msg {
        let msg = Line::from(Span::styled(
            s.as_str(),
            base().fg(status_message_color(app.status_is_error)),
        ));
        f.render_widget(Paragraph::new(msg).style(base()), area);
    } else if !app.marked.is_empty() {
        let line = Line::from(vec![
            Span::styled(" ", base()),
            Span::styled(
                format!("{}", app.marked.len()),
                base().fg(C_YELLOW).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" selected", base().fg(C_YELLOW)),
            Span::styled(" \u{2014} ", base().fg(DIM)),
            Span::styled("enter", base().fg(C_YELLOW).add_modifier(Modifier::BOLD)),
            Span::styled(" for batch \u{2502} ", base().fg(DIM)),
            Span::styled("esc", base().fg(C_YELLOW).add_modifier(Modifier::BOLD)),
            Span::styled(" to clear", base().fg(DIM)),
        ]);
        f.render_widget(Paragraph::new(line).style(base()), area);
    } else if app.active_tab == Tab::Providers {
        let key =
            |k: &'static str| Span::styled(k, base().fg(C_YELLOW).add_modifier(Modifier::BOLD));
        let dim = |t: &'static str| Span::styled(t, base().fg(DIM));
        let line = Line::from(vec![
            dim(" "),
            key("j"),
            dim("/"),
            key("k"),
            dim(" nav \u{2502} "),
            key("a"),
            dim(" add \u{2502} "),
            key("d"),
            dim(" remove \u{2502} "),
            key("Tab"),
            dim(" accounts \u{2502} "),
            key("h"),
            dim(" help \u{2502} "),
            key("q"),
            dim(" quit"),
        ]);
        f.render_widget(Paragraph::new(line).style(base()), area);
    } else {
        let lines = build_help_lines(area.width as usize);
        f.render_widget(Paragraph::new(lines).style(base()), area);
    }

    // Version indicator — always rendered at bottom-right corner
    let version = crate::update::current_version();
    let ver_spans: Vec<Span> = if let Some(latest) = &app.update_available {
        vec![
            Span::styled(" \u{2502} ", base().fg(DIM)),
            Span::styled(format!("v{version}"), base().fg(DIM)),
            Span::styled(format!(" -> v{latest} "), base().fg(C_YELLOW)),
        ]
    } else {
        vec![
            Span::styled(" \u{2502} ", base().fg(DIM)),
            Span::styled(format!("v{version} "), base().fg(DIM)),
        ]
    };
    let ver_width: usize = ver_spans.iter().map(|s| s.width()).sum();
    if (area.width as usize) > ver_width {
        let ver_area = Rect {
            x: area.x + area.width - ver_width as u16,
            y: area.y + area.height.saturating_sub(1),
            width: ver_width as u16,
            height: 1,
        };
        f.render_widget(
            Paragraph::new(Line::from(ver_spans)).style(base()),
            ver_area,
        );
    }
}

/// Render a single usage gauge (5h or 7d) with block chars and pace marker.
fn render_usage_gauge(
    f: &mut Frame,
    w: &crate::usage::WindowUsage,
    label: &str,
    window_secs: i64,
    now: i64,
    area: Rect,
) {
    let used = w.used_percent.unwrap_or(0.0).min(100.0);
    let remaining_pct = (100.0 - used).max(0.0);
    let pace = crate::usage::visible_pace_percent(w, window_secs);
    let over = used >= 10.0 && pace.is_some_and(|p| used > p);
    let reset_str = w
        .resets_at
        .map(format_reset_time)
        .unwrap_or_else(|| "--".into());
    let remaining_secs = w.resets_at.map(|ts| ts - now).unwrap_or(0);

    // Row 1: block-char bar  "5h  ████████░░|░░░░░░░  25% used  75% left"
    let gauge_area = Rect { height: 1, ..area };
    let label_text = format!("{label}  ");
    let suffix = format!("  {used:.0}% used  {remaining_pct:.0}% left");
    let bar_width = (gauge_area.width as usize)
        .saturating_sub(label_text.len())
        .saturating_sub(suffix.len());

    let used_color = if used >= 90.0 {
        C_RED
    } else if over || used >= 70.0 {
        C_YELLOW
    } else {
        C_GREEN
    };
    let used_style = base().fg(used_color);
    let remaining_style = base().fg(remaining_color(remaining_pct));
    let pace_style = base().fg(C_WHITE).add_modifier(Modifier::BOLD);

    // L2: if bar_width is 0 (extremely narrow terminal), skip bar rendering entirely
    if bar_width == 0 {
        let reset_area = Rect {
            y: area.y + 1,
            height: 1,
            ..area
        };
        let reset_text = format!("resets in {reset_str}");
        f.render_widget(
            Paragraph::new(reset_text).style(base().fg(reset_color(remaining_secs))),
            reset_area,
        );
        return;
    }

    let pace_pos = pace.map(|p| {
        ((p / 100.0) * bar_width as f64)
            .round()
            .clamp(0.0, bar_width.saturating_sub(1) as f64) as usize
    });
    let used_pos = ((used / 100.0) * bar_width as f64)
        .round()
        .clamp(0.0, bar_width as f64) as usize;

    let mut spans = vec![Span::styled(label_text.clone(), base().fg(C_WHITE))];

    if let Some(pp) = pace_pos {
        let before_used = pp.min(used_pos);
        let before_remaining = pp.saturating_sub(used_pos);
        let after_used = used_pos.saturating_sub(pp + 1);
        let after_remaining = bar_width.saturating_sub(pp + 1 + after_used);

        if before_used > 0 {
            spans.push(Span::styled("█".repeat(before_used), used_style));
        }
        if before_remaining > 0 {
            spans.push(Span::styled("░".repeat(before_remaining), remaining_style));
        }
        spans.push(Span::styled("|", pace_style));
        if after_used > 0 {
            spans.push(Span::styled("█".repeat(after_used), used_style));
        }
        if after_remaining > 0 {
            spans.push(Span::styled("░".repeat(after_remaining), remaining_style));
        }
    } else {
        if used_pos > 0 {
            spans.push(Span::styled("█".repeat(used_pos), used_style));
        }
        if bar_width > used_pos {
            spans.push(Span::styled(
                "░".repeat(bar_width - used_pos),
                remaining_style,
            ));
        }
    }

    let suffix_color = if over { C_YELLOW } else { DIM };
    spans.push(Span::styled(suffix, base().fg(suffix_color)));

    f.render_widget(Paragraph::new(Line::from(spans)).style(base()), gauge_area);

    // Row 2: "started HH:MM" left, "↑ pace" at pace position, "resets in ..." right
    let reset_area = Rect {
        y: area.y + 1,
        height: 1,
        ..area
    };
    let reset_text = format!("resets in {reset_str}");
    let reset_style = base().fg(reset_color(remaining_secs));
    let started_text = w
        .resets_at
        .map(|ts| format!("started {}", format_local_time(ts - window_secs)))
        .unwrap_or_default();
    let started_len = started_text.len();

    let total_width = reset_area.width as usize;
    let reset_start = total_width.saturating_sub(reset_text.len());

    let row2 = if let Some(pp) = pace_pos {
        let arrow_offset = label_text.len() + pp;
        let pace_label = "\u{2191} pace"; // ↑ pace  (display width = 6, byte len = 8)
        const PACE_LABEL_DISPLAY_WIDTH: usize = 6;
        let pace_end = arrow_offset + PACE_LABEL_DISPLAY_WIDTH;

        // Try to fit: started ... ↑ pace ... resets in ...
        if !started_text.is_empty()
            && started_len + 2 <= arrow_offset
            && pace_end + 2 <= reset_start
        {
            Line::from(vec![
                Span::styled(&started_text, base().fg(DIM)),
                Span::styled(" ".repeat(arrow_offset - started_len), base()),
                Span::styled(pace_label, base().fg(DIM)),
                Span::styled(" ".repeat(reset_start - pace_end), base()),
                Span::styled(reset_text, reset_style),
            ])
        } else if pace_end + 2 <= reset_start {
            // No room for started, show pace + reset
            Line::from(vec![
                Span::styled(" ".repeat(arrow_offset), base()),
                Span::styled(pace_label, base().fg(DIM)),
                Span::styled(" ".repeat(reset_start - pace_end), base()),
                Span::styled(reset_text, reset_style),
            ])
        } else {
            // Tight: started left, reset right
            let mut spans = Vec::new();
            if !started_text.is_empty() && started_len + 2 <= reset_start {
                spans.push(Span::styled(&started_text, base().fg(DIM)));
                spans.push(Span::styled(" ".repeat(reset_start - started_len), base()));
            } else {
                spans.push(Span::styled(" ".repeat(reset_start), base()));
            }
            spans.push(Span::styled(reset_text, reset_style));
            Line::from(spans)
        }
    } else {
        // No pace marker: started left, reset after label offset
        let mut spans = Vec::new();
        if !started_text.is_empty() {
            spans.push(Span::styled(&started_text, base().fg(DIM)));
            let gap = reset_start.saturating_sub(started_len);
            spans.push(Span::styled(" ".repeat(gap), base()));
        } else {
            spans.push(Span::styled(" ".repeat(label_text.len()), base()));
        }
        spans.push(Span::styled(reset_text, reset_style));
        Line::from(spans)
    };

    f.render_widget(Paragraph::new(row2).style(base()), reset_area);
}

// ── Style helpers ─────────────────────────────────────────

/// Color for remaining percentage: green > 30%, yellow > 10%, red <= 10%
fn remaining_color(remaining_pct: f64) -> Color {
    if remaining_pct > 30.0 {
        C_GREEN
    } else if remaining_pct > 10.0 {
        C_YELLOW
    } else {
        C_RED
    }
}

fn plan_color(plan: Option<&str>, is_selected: bool) -> Style {
    let kind = PlanKind::from_wire(plan);
    let fg = match kind {
        PlanKind::Free | PlanKind::Unknown => C_GRAY,
        PlanKind::Go => C_BLUE,
        PlanKind::Plus => C_CYAN,
        PlanKind::ProLite | PlanKind::Pro => C_YELLOW,
        PlanKind::Team | PlanKind::Business | PlanKind::Enterprise | PlanKind::Edu => C_MAGENTA,
    };
    let s = base().fg(fg);
    if is_selected || matches!(kind, PlanKind::Pro | PlanKind::Enterprise) {
        s.add_modifier(Modifier::BOLD)
    } else {
        s
    }
}

/// Color for reset countdown: green = soon (< 1h), yellow = medium (< 4h), red = far (>= 4h)
fn reset_color(remaining_secs: i64) -> Color {
    if remaining_secs < 3600 {
        C_GREEN
    } else if remaining_secs < 14400 {
        C_YELLOW
    } else {
        C_RED
    }
}

fn usage_pct_style(remaining_pct_str: &str, is_selected: bool) -> Style {
    let over_pace = remaining_pct_str.ends_with('!');
    let clean = remaining_pct_str.trim_end_matches('!');
    let fg = if over_pace {
        C_RED
    } else {
        match clean.trim_end_matches('%').parse::<f64>() {
            Ok(n) => remaining_color(n),
            Err(_) => DIM,
        }
    };
    let s = base().fg(fg);
    if is_selected {
        s.add_modifier(Modifier::BOLD)
    } else {
        s
    }
}

fn build_help_lines(width: usize) -> Vec<Line<'static>> {
    let key_style = base().fg(C_YELLOW);
    let sep_style = base().fg(DIM);
    let label_style = base().fg(C_GRAY);
    let space_style = base();
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut spans: Vec<Span<'static>> = vec![Span::styled(" ", space_style)];
    let mut used = 1usize;

    let items = keymap::status_bar_items();
    for (i, (k, label)) in items.iter().enumerate() {
        let key_disp = (*k).to_string();
        let label_short = short_label(label);
        let sep = " \u{2502} ";
        let item_len = key_disp.chars().count()
            + 1
            + label_short.chars().count()
            + if i + 1 < items.len() {
                sep.chars().count()
            } else {
                0
            };
        if used + item_len > width && used > 1 {
            lines.push(Line::from(spans));
            spans = vec![Span::styled(" ", space_style)];
            used = 1;
        }
        spans.push(Span::styled(key_disp, key_style));
        spans.push(Span::styled(" ", space_style));
        spans.push(Span::styled(label_short.to_string(), label_style));
        if i + 1 < items.len() {
            spans.push(Span::styled(sep, sep_style));
        }
        used += item_len;
    }
    if spans.len() > 1 {
        lines.push(Line::from(spans));
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled("", space_style)));
    }
    lines
}

/// Compress verbose keymap labels for status bar.
fn short_label(label: &str) -> &str {
    match label {
        "move selection" => "nav",
        "search" => "search",
        "open menu (account or batch)" => "menu",
        "refresh visible accounts" => "refresh",
        "show / hide account detail panel" => "quota",
        "show this help" => "help",
        "quit" => "quit",
        other => other,
    }
}

fn format_auto_refresh_remaining(secs: u64) -> String {
    if secs == 0 {
        return "now".to_string();
    }
    if secs < 60 {
        return format!("{secs}s");
    }
    let mins = secs / 60;
    let rem = secs % 60;
    if rem == 0 {
        format!("{mins}m")
    } else {
        format!("{mins}m{rem}s")
    }
}

fn status_bar_height(app: &App, width: u16) -> usize {
    if app.status_msg.is_some()
        || app.rename.is_some()
        || app.provider_add.is_some()
        || app.confirm.is_some()
        || app.search_active
        || !app.marked.is_empty()
    {
        return 1;
    }
    if app.active_tab == Tab::Providers {
        return 1;
    }
    build_help_lines(width as usize).len()
}

#[cfg(test)]
mod tests {
    use super::{
        C_BLUE, C_CYAN, C_GRAY, C_GREEN, C_MAGENTA, C_RED, C_YELLOW, DIM, plan_color,
        render_usage_gauges, reset_cards_color, reset_cards_table_state, status_message_color,
        table_text_widths, usage_gauges_height,
    };
    use crate::tui::app::App;
    use crate::usage::{AdditionalRateLimit, ResetCredit, UsageInfo, WindowUsage};
    use ratatui::style::Modifier;
    use ratatui::{Terminal, backend::TestBackend};

    fn row_text(backend: &TestBackend, y: u16) -> String {
        let area = backend.buffer().area;
        (0..area.width)
            .map(|x| {
                backend
                    .buffer()
                    .cell((x, y))
                    .expect("cell inside test buffer")
                    .symbol()
            })
            .collect()
    }

    #[test]
    fn status_message_color_distinguishes_errors_from_information() {
        assert_eq!(status_message_color(false), C_CYAN);
        assert_eq!(status_message_color(true), C_RED);
    }

    #[test]
    fn providers_tab_lists_custom_providers_without_the_key() {
        let mut app = App::new();
        app.active_tab = crate::tui::app::Tab::Providers;
        app.providers.push(crate::provider::ProviderProfile {
            alias: "openrouter".into(),
            provider_id: "openrouter".into(),
            name: "OpenRouter".into(),
            base_url: "https://openrouter.ai/api/v1".into(),
            env_key: "CODEX_SWITCH_OPENROUTER_KEY".into(),
            model: "openai/gpt-5.3-codex".into(),
            wire_api: "responses".into(),
            codex_config: Vec::new(),
            api_key: "sk-secret-1234".into(),
        });

        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| super::render(f, &mut app)).unwrap();

        let joined = (0..30)
            .map(|y| row_text(terminal.backend(), y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            joined.contains("Custom providers"),
            "the panel header must render:\n{joined}"
        );
        assert!(joined.contains("openrouter"));
        assert!(joined.contains("https://openrouter.ai/api/v1"));
        assert!(
            !joined.contains("sk-secret-1234"),
            "the API key must never render in the panel"
        );
    }

    #[test]
    fn reset_card_column_distinguishes_refreshing_and_cooling_down() {
        let usage = UsageInfo::default();
        assert_eq!(reset_cards_table_state(&usage, true, false).0, "...");
        assert_eq!(reset_cards_table_state(&usage, false, true).0, "wait");

        let known = UsageInfo {
            reset_credits_available_count: Some(2),
            ..UsageInfo::default()
        };
        assert_eq!(reset_cards_table_state(&known, true, false).0, "2↻");
        assert_eq!(reset_cards_table_state(&known, false, true).0, "2⏳");
    }

    fn reset_credit_expiring_in(seconds: i64) -> ResetCredit {
        ResetCredit {
            id: format!("credit-{seconds}"),
            granted_at: None,
            expires_at: Some(
                chrono::DateTime::from_timestamp(crate::auth::now_unix_secs() + seconds, 0)
                    .unwrap()
                    .to_rfc3339(),
            ),
        }
    }

    #[test]
    fn reset_card_color_warns_for_the_earliest_expiring_available_card() {
        let red = UsageInfo {
            reset_credits_available_count: Some(2),
            reset_credits: vec![
                reset_credit_expiring_in(10 * 24 * 60 * 60),
                reset_credit_expiring_in(2 * 24 * 60 * 60),
            ],
            ..Default::default()
        };
        let yellow = UsageInfo {
            reset_credits_available_count: Some(1),
            reset_credits: vec![reset_credit_expiring_in(6 * 24 * 60 * 60)],
            ..Default::default()
        };
        let green = UsageInfo {
            reset_credits_available_count: Some(1),
            reset_credits: vec![reset_credit_expiring_in(8 * 24 * 60 * 60)],
            ..Default::default()
        };

        assert_eq!(reset_cards_color(&red), C_RED);
        assert_eq!(reset_cards_color(&yellow), C_YELLOW);
        assert_eq!(reset_cards_color(&green), C_GREEN);
    }

    #[test]
    fn reset_card_color_does_not_mark_unknown_expiry_as_green() {
        let fetch_error = UsageInfo {
            reset_credits_available_count: Some(1),
            reset_credits_error: Some("HTTP 429".into()),
            ..Default::default()
        };
        let unknown_expiry = UsageInfo {
            reset_credits_available_count: Some(1),
            ..Default::default()
        };

        assert_eq!(reset_cards_color(&fetch_error), C_YELLOW);
        assert_eq!(reset_cards_color(&unknown_expiry), DIM);
    }

    #[test]
    fn additional_quota_pool_expands_the_main_detail_panel() {
        let window = WindowUsage {
            used_percent: Some(25.0),
            resets_at: Some(1_000_000),
            window_minutes: Some(300),
        };
        let usage = UsageInfo {
            primary: Some(window.clone()),
            secondary: Some(window.clone()),
            additional_limits: vec![AdditionalRateLimit {
                limit_name: Some("GPT-6-Codex-Burst".to_string()),
                metered_feature: Some("codex_futureburst".to_string()),
                primary: Some(window.clone()),
                secondary: Some(window),
                ..Default::default()
            }],
            ..Default::default()
        };

        assert_eq!(usage_gauges_height(&usage), 11);
    }

    #[test]
    fn additional_primary_slot_uses_its_real_seven_day_window_for_label_and_pace() {
        let usage = UsageInfo {
            additional_limits: vec![AdditionalRateLimit {
                limit_name: Some("GPT-5.3-Codex-Spark".to_string()),
                metered_feature: Some("codex_bengalfox".to_string()),
                primary: Some(WindowUsage {
                    used_percent: Some(8.0),
                    resets_at: Some(crate::auth::now_unix_secs() + 6 * 24 * 60 * 60),
                    window_minutes: Some(7 * 24 * 60),
                }),
                ..Default::default()
            }],
            ..Default::default()
        };
        let backend = TestBackend::new(100, 10);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| render_usage_gauges(frame, &usage, frame.area()))
            .unwrap();

        let row = (0..10)
            .map(|y| row_text(terminal.backend(), y))
            .find(|line| line.contains("7d"))
            .expect("the real seven-day window label must be rendered");
        assert!(!row.contains("5h"));
        let label_x = row.find("7d").unwrap();
        let pace_x = row.find('|').expect("pace marker");
        assert!(
            pace_x > label_x + 6,
            "seven-day pace must not be clamped to the start of the bar: {row}"
        );
    }

    #[test]
    fn plan_color_uses_semantic_plan_families() {
        assert_eq!(plan_color(Some("go"), false).fg, Some(C_BLUE));
        assert_eq!(plan_color(Some("plus"), false).fg, Some(C_CYAN));
        assert_eq!(plan_color(Some("prolite"), false).fg, Some(C_YELLOW));
        let pro = plan_color(Some("pro"), false);
        assert_eq!(pro.fg, Some(C_YELLOW));
        assert!(pro.add_modifier.contains(Modifier::BOLD));
        assert_eq!(plan_color(Some("team"), false).fg, Some(C_MAGENTA));
        assert_eq!(plan_color(Some("business"), false).fg, Some(C_MAGENTA));
        assert_eq!(plan_color(Some("future_plan"), false).fg, Some(C_GRAY));
    }

    #[test]
    fn account_table_columns_expand_to_fit_names_when_space_is_available() {
        let widths = table_text_widths(
            180,
            &["oai001_20x", "a-very-long-account-alias"],
            &["oai001@ozi.xyz"],
            &["Pro 20×", "Team - NightCity Workspace"],
        );

        assert!(widths.alias >= "a-very-long-account-alias".chars().count() as u16);
        assert!(widths.plan >= "Team - NightCity Workspace".chars().count() as u16);
    }

    #[test]
    fn account_table_columns_fit_an_eighty_column_terminal() {
        let widths = table_text_widths(
            80,
            &["a-very-long-account-alias"],
            &["a-very-long-address@example.com"],
            &["Team - NightCity Workspace"],
        );

        assert!(widths.alias + widths.email + widths.plan <= 16);
    }

    #[test]
    fn account_table_columns_use_extra_space_beyond_the_old_caps() {
        let alias = "a".repeat(45);
        let email = format!("{}@example.com", "e".repeat(40));
        let plan = format!("Team - {}", "Workspace".repeat(5));
        let widths = table_text_widths(260, &[&alias], &[&email], &[&plan]);

        assert_eq!(widths.alias, alias.len() as u16);
        assert_eq!(widths.email, email.len() as u16);
        assert_eq!(widths.plan, plan.len() as u16);
    }
}
