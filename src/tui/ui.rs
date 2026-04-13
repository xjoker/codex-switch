use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
};

use super::app::{App, UsageStatus};
use crate::output::{format_local_time, format_reset_short, format_reset_time};
use crate::usage::{UsageInfo, is_available};

/// Base background color for the entire TUI.
/// Uses a very dark gray instead of pure black to improve contrast
/// with DarkGray text on Windows terminals (cmd.exe, PowerShell).
const BG: Color = Color::Rgb(24, 24, 24);

/// Dim foreground — replaces DarkGray which is nearly invisible on
/// dark backgrounds in Windows terminals.
const DIM: Color = Color::Rgb(140, 140, 140);

/// Base style: black background, no foreground override.
/// Every widget should build on top of this to guarantee no terminal-default bleed.
fn base() -> Style {
    Style::default().bg(BG)
}

pub fn render(f: &mut Frame, app: &App) {
    let area = f.area();

    // Paint the entire area with a solid background first
    f.render_widget(Block::default().style(base()), area);

    let status_height = status_bar_height(app, area.width);

    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(6),                    // account list
            Constraint::Length(9),                  // detail panel
            Constraint::Length(status_height as u16), // status bar
        ])
        .split(area);

    render_account_table(f, app, vertical[0]);
    render_detail_panel(f, app, vertical[1]);
    render_status_bar(f, app, vertical[2]);
}

fn render_account_table(f: &mut Frame, app: &App, area: Rect) {
    let hdr = base()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
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
    ])
    .height(1);

    let rows: Vec<Row> = app
        .view_indices
        .iter()
        .enumerate()
        .map(|(view_i, &acc_i)| {
            let entry = &app.accounts[acc_i];
            let is_marked = app.marked.contains(&entry.alias);
            let marker = if is_marked {
                ">"
            } else if entry.is_current {
                "*"
            } else {
                " "
            };
            let marker_style = if is_marked {
                base()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else if entry.is_current {
                base()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD)
            } else {
                base()
            };

            let is_selected = view_i == app.selected;
            let row_style = if is_selected {
                base()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                base().fg(Color::Gray)
            };

            let email = entry.info.email.as_deref().unwrap_or("--").to_string();
            let plan_label = entry.info.plan_label();
            let plan_style = plan_color(entry.info.plan_type.as_deref(), is_selected);

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
            ): (String, Color, String, String, String, Color, String, Color) = match &entry.usage {
                UsageStatus::Idle => (
                    "--".into(),
                    DIM,
                    "--".into(),
                    "--".into(),
                    "--".into(),
                    DIM,
                    "--".into(),
                    DIM,
                ),
                UsageStatus::Loading => (
                    "...".into(),
                    Color::Yellow,
                    "...".into(),
                    "...".into(),
                    "loading".into(),
                    DIM,
                    "loading".into(),
                    DIM,
                ),
                UsageStatus::Error(_) => (
                    "Error".into(),
                    Color::Red,
                    "Err".into(),
                    "Err".into(),
                    "--".into(),
                    DIM,
                    "--".into(),
                    DIM,
                ),
                UsageStatus::Loaded(u) => {
                    let p5 = u
                        .primary
                        .as_ref()
                        .and_then(|w| w.used_percent)
                        .map(|p| format!("{:.0}%", (100.0 - p).max(0.0)))
                        .unwrap_or_else(|| "--".into());
                    let p7 = u
                        .secondary
                        .as_ref()
                        .and_then(|w| w.used_percent)
                        .map(|p| format!("{:.0}%", (100.0 - p).max(0.0)))
                        .unwrap_or_else(|| "--".into());
                    let r5_ts = u.primary.as_ref().and_then(|w| w.resets_at);
                    let r5 = r5_ts.map(format_reset_short).unwrap_or_else(|| "--".into());
                    let r5c = r5_ts
                        .map(|ts| reset_color(ts - now))
                        .unwrap_or(DIM);
                    let r7_ts = u.secondary.as_ref().and_then(|w| w.resets_at);
                    let r7 = r7_ts.map(format_reset_short).unwrap_or_else(|| "--".into());
                    let r7c = r7_ts
                        .map(|ts| reset_color(ts - now))
                        .unwrap_or(DIM);
                    if is_available(u) {
                        ("OK".into(), Color::Green, p5, p7, r5, r5c, r7, r7c)
                    } else {
                        ("Limited".into(), Color::Red, p5, p7, r5, r5c, r7, r7c)
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
            ])
            .height(1)
        })
        .collect();

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
    title.push_str(&format!(" sort:{} ", app.sort_mode.as_str()));

    let mut table_state = TableState::default().with_selected(app.selected);

    let table = Table::new(
        rows,
        [
            Constraint::Length(2),  // marker
            Constraint::Length(14), // alias
            Constraint::Min(18),    // email
            Constraint::Length(16), // plan
            Constraint::Length(8),  // status
            Constraint::Length(6),  // 5h %
            Constraint::Length(6),  // 7d %
            Constraint::Length(14), // 5h reset
            Constraint::Length(14), // 7d reset
        ],
    )
    .header(header)
    .block(
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(base().fg(Color::Rgb(80, 120, 200)))
            .style(base()),
    )
    .row_highlight_style(
        Style::default()
            .bg(Color::Rgb(60, 60, 60))
            .add_modifier(Modifier::BOLD),
    )
    .style(base());

    f.render_stateful_widget(table, area, &mut table_state);
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
        .border_style(base().fg(if entry.is_current {
            Color::Green
        } else {
            Color::Rgb(80, 120, 200)
        }))
        .style(base());

    let inner = block.inner(area);
    f.render_widget(block, area);

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // account info row
            Constraint::Length(1), // spacer
            Constraint::Min(4),    // usage gauges
        ])
        .margin(1)
        .split(inner);

    // Compact info line
    let plan_label = entry.info.plan_label();
    let acct_id = entry.info.account_id.as_deref().unwrap_or("--");
    let email = entry.info.email.as_deref().unwrap_or("--");

    let info_line = Line::from(vec![
        Span::styled("Email ", base().fg(DIM)),
        Span::styled(email, base().fg(Color::White)),
        Span::styled("  ", base()),
        Span::styled("Plan ", base().fg(DIM)),
        Span::styled(
            &plan_label,
            plan_color(entry.info.plan_type.as_deref(), true),
        ),
        Span::styled("  ", base()),
        Span::styled("ID ", base().fg(DIM)),
        Span::styled(
            if acct_id.len() > 20 {
                &acct_id[..20]
            } else {
                acct_id
            },
            base().fg(DIM),
        ),
    ]);
    f.render_widget(Paragraph::new(info_line).style(base()), layout[0]);

    // Usage area
    match &entry.usage {
        UsageStatus::Idle => {
            let p = Paragraph::new("Press r to refresh usage")
                .style(base().fg(DIM));
            f.render_widget(p, layout[2]);
        }
        UsageStatus::Loading => {
            let p = Paragraph::new("Fetching usage...").style(base().fg(Color::Yellow));
            f.render_widget(p, layout[2]);
        }
        UsageStatus::Error(e) => {
            let p = Paragraph::new(format!("Error: {}", e.detail))
                .style(base().fg(Color::Red));
            f.render_widget(p, layout[2]);
        }
        UsageStatus::Loaded(u) => {
            render_usage_gauges(f, u, layout[2]);
        }
    }
}

fn render_usage_gauges(f: &mut Frame, u: &UsageInfo, area: Rect) {
    let now = crate::auth::now_unix_secs();
    let has_credits = u.credits_balance.is_some();
    let mut constraints = vec![];
    if u.primary.is_some() {
        constraints.push(Constraint::Length(2));
    }
    if u.secondary.is_some() {
        constraints.push(Constraint::Length(2));
    }
    if has_credits {
        constraints.push(Constraint::Length(1));
    }
    if constraints.is_empty() {
        constraints.push(Constraint::Min(1));
    }

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    let mut idx = 0;

    if let Some(w) = &u.primary {
        render_usage_gauge(f, w, "5h", crate::usage::WINDOW_5H_SECS, now, layout[idx]);
        idx += 1;
    }

    if let Some(w) = &u.secondary {
        render_usage_gauge(f, w, "7d", crate::usage::WINDOW_7D_SECS, now, layout[idx]);
        idx += 1;
    }

    if let Some(balance) = u.credits_balance {
        let unlimited = u.unlimited_credits == Some(true);
        let text = if unlimited {
            "Credits: unlimited".to_string()
        } else {
            format!("Credits: ${balance:.2}")
        };
        let p = Paragraph::new(text).style(base().fg(credits_color(balance, unlimited)));
        f.render_widget(p, layout[idx]);
    }

    if u.primary.is_none() && u.secondary.is_none() && !has_credits {
        let p = Paragraph::new("No usage data").style(base().fg(DIM));
        f.render_widget(p, layout[0]);
    }
}

fn render_status_bar(f: &mut Frame, app: &App, area: Rect) {
    // Rename input takes top priority
    if let Some(rs) = &app.rename {
        let line = Line::from(vec![
            Span::styled(
                " Rename: ",
                base()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                &rs.input,
                base()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("#", base().fg(Color::Gray)),
            Span::styled(
                "  (Enter confirm / Esc cancel)",
                base().fg(DIM),
            ),
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
        };
        let line = Line::from(Span::styled(
            msg,
            base().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
        f.render_widget(Paragraph::new(line).style(base()), area);
        return;
    }

    if app.search_active
        && let Some(s) = &app.search
    {
        let line = Line::from(vec![
            Span::styled(
                " /",
                base()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                &s.query,
                base()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("#", base().fg(Color::Gray)),
            Span::styled(
                "  (Enter accept / Esc clear)",
                base().fg(DIM),
            ),
        ]);
        f.render_widget(Paragraph::new(line).style(base()), area);
        return;
    }

    if let Some(s) = &app.status_msg {
        let msg = Line::from(Span::styled(s.as_str(), base().fg(Color::Green)));
        f.render_widget(Paragraph::new(msg).style(base()), area);
    } else {
        let lines = build_help_lines(area.width as usize);
        f.render_widget(Paragraph::new(lines).style(base()), area);
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
    let over = pace.is_some_and(|p| used > p);
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

    let used_style = base().fg(if over { Color::Yellow } else { Color::Green });
    let remaining_style = base().fg(remaining_color(remaining_pct));
    let pace_style = base()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);

    // L2: if bar_width is 0 (extremely narrow terminal), skip bar rendering entirely
    if bar_width == 0 {
        let reset_area = Rect { y: area.y + 1, height: 1, ..area };
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

    let mut spans = vec![Span::styled(label_text.clone(), base().fg(Color::White))];

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
            spans.push(Span::styled("░".repeat(bar_width - used_pos), remaining_style));
        }
    }

    let suffix_color = if over { Color::Yellow } else { DIM };
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
        Color::Green
    } else if remaining_pct > 10.0 {
        Color::Yellow
    } else {
        Color::Red
    }
}

fn plan_color(plan: Option<&str>, is_selected: bool) -> Style {
    let fg = match plan {
        Some("pro") => Color::Yellow,
        Some("plus") => Color::Cyan,
        Some("team") => Color::Magenta,
        _ => DIM,
    };
    let s = base().fg(fg);
    if is_selected {
        s.add_modifier(Modifier::BOLD)
    } else {
        s
    }
}

/// Color for reset countdown: green = soon (< 1h), yellow = medium (< 4h), red = far (>= 4h)
fn reset_color(remaining_secs: i64) -> Color {
    if remaining_secs < 3600 {
        Color::Green
    } else if remaining_secs < 14400 {
        Color::Yellow
    } else {
        Color::Red
    }
}

/// Color for credits balance: green >= $10, yellow >= $2, red < $2
fn credits_color(balance: f64, unlimited: bool) -> Color {
    if unlimited || balance >= 10.0 {
        Color::Green
    } else if balance >= 2.0 {
        Color::Yellow
    } else {
        Color::Red
    }
}

fn usage_pct_style(remaining_pct_str: &str, is_selected: bool) -> Style {
    let fg = match remaining_pct_str.trim_end_matches('%').parse::<f64>() {
        Ok(n) => remaining_color(n),
        Err(_) => DIM,
    };
    let s = base().fg(fg);
    if is_selected {
        s.add_modifier(Modifier::BOLD)
    } else {
        s
    }
}

const HELP_ITEMS: &[(&str, &str)] = &[
    ("jk", " nav "),
    ("Enter", " switch "),
    ("/", " search "),
    ("r", " refresh "),
    ("s", " sort "),
    ("Space", " mark "),
    ("w", " warmup "),
    ("c", " clear "),
    ("n", " rename "),
    ("d", " del "),
    ("q", " quit"),
];

fn build_help_lines(width: usize) -> Vec<Line<'static>> {
    let key_style = base().fg(Color::Yellow);
    let dim_style = base().fg(DIM);
    let space_style = base();
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut spans: Vec<Span<'static>> = vec![Span::styled(" ", space_style)];
    let mut used = 1usize;

    for (k, label) in HELP_ITEMS {
        let item_len = k.len() + label.len();
        if used + item_len > width && used > 1 {
            lines.push(Line::from(spans));
            spans = vec![Span::styled(" ", space_style)];
            used = 1;
        }
        let style = if *k == "jk" { dim_style } else { key_style };
        spans.push(Span::styled(*k, style));
        spans.push(Span::styled(*label, space_style));
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

fn status_bar_height(app: &App, width: u16) -> usize {
    if app.status_msg.is_some() || app.rename.is_some() || app.confirm.is_some() || app.search_active {
        return 1;
    }
    build_help_lines(width as usize).len()
}
