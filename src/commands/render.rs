use crate::{color, usage};

/// Prompt the user for Y/n confirmation. Returns false on EOF or explicit "n"/"no".
pub(crate) fn confirm(prompt: &str) -> bool {
    use std::io::{self, Write as _};

    eprint!("{}", color::dim(prompt));
    io::stderr().flush().ok();
    let mut input = String::new();
    match io::stdin().read_line(&mut input) {
        Ok(0) => false, // EOF
        Ok(_) => !matches!(input.trim().to_lowercase().as_str(), "n" | "no"),
        Err(_) => false,
    }
}

/// Prompt the user for y/N confirmation. Only an explicit "y" or "yes" accepts.
pub(crate) fn confirm_default_no(prompt: &str) -> bool {
    use std::io::{self, Write as _};

    eprint!("{}", color::dim(prompt));
    io::stderr().flush().ok();
    let mut input = String::new();
    match io::stdin().read_line(&mut input) {
        Ok(0) => false,
        Ok(_) => matches!(input.trim().to_lowercase().as_str(), "y" | "yes"),
        Err(_) => false,
    }
}

pub(crate) fn term_width() -> usize {
    crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(80)
}

/// Render a progress bar without outer brackets.
/// `=` for used portion, `-` for remaining, `|` for pace marker.
pub(crate) fn render_progress_bar(
    used_pct: f64,
    pace_pct: Option<f64>,
    bar_width: usize,
) -> String {
    let used_pos = ((used_pct / 100.0) * bar_width as f64)
        .round()
        .clamp(0.0, bar_width as f64) as usize;
    let pace_pos = pace_pct.map(|p| {
        ((p / 100.0) * bar_width as f64)
            .round()
            .clamp(0.0, (bar_width.saturating_sub(1)) as f64) as usize
    });

    let mut bar = String::with_capacity(bar_width);
    for i in 0..bar_width {
        if pace_pos == Some(i) {
            bar.push('|');
        } else if i < used_pos {
            bar.push('=');
        } else {
            bar.push('-');
        }
    }
    bar
}

/// Format relative reset time: "~2h17m" or "~4d18h"
pub(crate) fn format_reset_short_relative(w: &usage::WindowUsage) -> String {
    let Some(resets_at) = w.resets_at else {
        return "--".into();
    };
    let remaining_secs = (resets_at - crate::auth::now_unix_secs()).max(0) as u64;
    if remaining_secs == 0 {
        return "expired".into();
    }
    if remaining_secs < 3600 {
        format!("~{}m", remaining_secs / 60)
    } else if remaining_secs < 86400 {
        format!(
            "~{}h{}m",
            remaining_secs / 3600,
            (remaining_secs % 3600) / 60
        )
    } else {
        format!(
            "~{}d{}h",
            remaining_secs / 86400,
            (remaining_secs % 86400) / 3600
        )
    }
}

/// Render one additional-limit pool's window as a compact segment, e.g.
/// "5h [====------] 60% left". Reuses the same bar/color helpers as the
/// primary account's usage line.
fn pool_window_segment(label: &str, w: &usage::WindowUsage, window_secs: i64) -> String {
    let pct = w.used_percent.unwrap_or(0.0);
    let remaining_pct = (100.0 - pct).max(0.0);
    let pace = usage::visible_pace_percent(w, window_secs);
    let bar = render_progress_bar(pct, pace, 10);
    format!(
        "{} [{}] {}",
        color::dim(label),
        color::usage_pct(&bar, pct),
        color::usage_pct(&format!("{remaining_pct:.0}% left"), pct),
    )
}

/// Print one indented sub-line per additional-limit pool (e.g. per-model
/// quota pools on Pro 20x accounts). No-op when there are no additional pools.
pub(crate) fn print_additional_pool_lines(limits: &[usage::AdditionalRateLimit]) {
    for row in usage::additional_pool_rows(limits) {
        let mut segments = vec![format!(
            "  {} {}",
            color::dim("\u{2514}"),
            color::dim(&row.limit_name)
        )];
        if let Some(w) = &row.primary {
            segments.push(pool_window_segment("5h", w, usage::WINDOW_5H_SECS));
        }
        if let Some(w) = &row.secondary {
            segments.push(pool_window_segment("7d", w, usage::WINDOW_7D_SECS));
        }
        if row.unavailable {
            segments.push(color::error("[exhausted]"));
        }
        println!("{}", segments.join("  "));
    }
}

pub(crate) fn print_usage_line(u: &usage::UsageInfo) {
    let width = term_width();
    // Each line: "  5h  bar  XXX% left  ~Xh" ≈ bar_width + 30
    let bar_width = if width >= 80 {
        16
    } else if width >= 60 {
        10
    } else {
        6
    };

    if let Some(w) = &u.primary {
        let pct = w.used_percent.unwrap_or(0.0);
        let remaining_pct = (100.0 - pct).max(0.0);
        let pace = usage::visible_pace_percent(w, usage::WINDOW_5H_SECS);
        let over = pct >= 10.0 && pace.is_some_and(|p| pct > p);
        let bar = render_progress_bar(pct, pace, bar_width);
        let reset = format_reset_short_relative(w);
        let warn = if over {
            color::error("!")
        } else {
            String::new()
        };
        println!(
            "  5h  {}  {}{}   {}",
            color::usage_pct(&bar, pct),
            color::usage_pct(&format!("{remaining_pct:>3.0}% left"), pct),
            warn,
            color::dim(&reset),
        );
    }
    if let Some(w) = &u.secondary {
        let pct = w.used_percent.unwrap_or(0.0);
        let remaining_pct = (100.0 - pct).max(0.0);
        let pace = usage::visible_pace_percent(w, usage::WINDOW_7D_SECS);
        let over = pct >= 10.0 && pace.is_some_and(|p| pct > p);
        let bar = render_progress_bar(pct, pace, bar_width);
        let reset = format_reset_short_relative(w);
        let warn = if over {
            color::error("!")
        } else {
            String::new()
        };
        println!(
            "  7d  {}  {}{}   {}",
            color::usage_pct(&bar, pct),
            color::usage_pct(&format!("{remaining_pct:>3.0}% left"), pct),
            warn,
            color::dim(&reset),
        );
    }
    print_additional_pool_lines(&u.additional_limits);
    if let Some(balance) = u.credits_balance {
        let unlimited = u.unlimited_credits == Some(true);
        let text = if unlimited {
            "credits: unlimited".to_string()
        } else {
            usage::format_credits_balance(balance)
        };
        println!("  {}", color::credits(&text, balance, unlimited));
    }
    for line in crate::output::reset_credits_detail_lines(u, 4) {
        println!("  {}", color::dim(&line));
    }
}

#[cfg(test)]
mod tests {
    use super::{format_reset_short_relative, render_progress_bar};
    use crate::usage::WindowUsage;

    #[test]
    fn progress_bar_clamps_used_and_pace_positions() {
        assert_eq!(render_progress_bar(0.0, None, 10), "----------");
        assert_eq!(render_progress_bar(100.0, None, 10), "==========");
        assert_eq!(render_progress_bar(0.0, Some(150.0), 10), "---------|");
        assert_eq!(render_progress_bar(50.0, Some(50.0), 10), "=====|----");
    }

    fn reset_after(seconds: i64) -> WindowUsage {
        WindowUsage {
            resets_at: Some(crate::auth::now_unix_secs() + seconds),
            ..WindowUsage::default()
        }
    }

    #[test]
    fn short_relative_reset_uses_minute_hour_and_day_boundaries() {
        assert_eq!(
            format_reset_short_relative(&reset_after(59 * 60 + 30)),
            "~59m"
        );
        assert_eq!(
            format_reset_short_relative(&reset_after(60 * 60 + 30)),
            "~1h0m"
        );
        assert_eq!(
            format_reset_short_relative(&reset_after(23 * 60 * 60 + 59 * 60 + 30)),
            "~23h59m"
        );
        assert_eq!(
            format_reset_short_relative(&reset_after(24 * 60 * 60 + 30)),
            "~1d0h"
        );
    }
}
