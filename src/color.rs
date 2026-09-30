use std::sync::OnceLock;

use owo_colors::OwoColorize;

use crate::cli::ColorMode;
use crate::jwt::PlanKind;

static ENABLED: OnceLock<bool> = OnceLock::new();

/// Initialize color support. Call once at startup.
pub fn init(mode: ColorMode) {
    let enabled = match mode {
        ColorMode::Always => true,
        ColorMode::Never => false,
        ColorMode::Auto => {
            // Respect NO_COLOR convention (https://no-color.org)
            if std::env::var_os("NO_COLOR").is_some() {
                return ENABLED.set(false).unwrap_or(());
            }
            // Check if stdout is a terminal with color support
            supports_color::on(supports_color::Stream::Stdout).is_some()
        }
    };
    let _ = ENABLED.set(enabled);
}

/// Whether color output is enabled.
pub fn enabled() -> bool {
    *ENABLED.get_or_init(|| {
        if std::env::var_os("NO_COLOR").is_some() {
            return false;
        }
        supports_color::on(supports_color::Stream::Stdout).is_some()
    })
}

// ── Styled output helpers for CLI ─────────────────────────

/// Green text for success
pub fn success(s: &str) -> String {
    if enabled() {
        format!("{}", s.green())
    } else {
        s.to_string()
    }
}

/// Red text for errors
pub fn error(s: &str) -> String {
    if enabled() {
        format!("{}", s.red())
    } else {
        s.to_string()
    }
}

/// Yellow text for warnings
pub fn warn(s: &str) -> String {
    if enabled() {
        format!("{}", s.yellow())
    } else {
        s.to_string()
    }
}

/// Dim/gray text
pub fn dim(s: &str) -> String {
    if enabled() {
        format!("{}", s.dimmed())
    } else {
        s.to_string()
    }
}

/// Bold text
pub fn bold(s: &str) -> String {
    if enabled() {
        format!("{}", s.bold())
    } else {
        s.to_string()
    }
}

/// Green bold for active marker
pub fn active(s: &str) -> String {
    if enabled() {
        format!("{}", s.green().bold())
    } else {
        s.to_string()
    }
}

/// Color a usage percentage: green < 70, yellow < 90, red >= 90
pub fn usage_pct(s: &str, pct: f64) -> String {
    if !enabled() {
        return s.to_string();
    }
    if pct >= 90.0 {
        format!("{}", s.red())
    } else if pct >= 70.0 {
        format!("{}", s.yellow())
    } else {
        format!("{}", s.green())
    }
}

/// Keep finite credit balances neutral; only zero or negative balances warn.
pub fn credits(s: &str, balance: f64, unlimited: bool) -> String {
    if !enabled() {
        return s.to_string();
    }
    if unlimited {
        format!("{}", s.green())
    } else if balance <= 0.0 {
        format!("{}", s.red())
    } else {
        s.to_string()
    }
}

/// Color a status tag: OK = green, Limited = red, Error = red
pub fn status_tag(tag: &str) -> String {
    if !enabled() {
        return format!("[{tag}]");
    }
    match tag {
        "OK" => format!("[{}]", tag.green()),
        "Limited" | "Error" => format!("[{}]", tag.red()),
        _ => format!("[{tag}]"),
    }
}

/// Color a plan label by type
pub fn plan(label: &str, plan_type: Option<&str>) -> String {
    if !enabled() {
        return format!("[{label}]");
    }
    colored_plan(label, plan_type)
}

fn colored_plan(label: &str, plan_type: Option<&str>) -> String {
    match PlanKind::from_wire(plan_type) {
        PlanKind::Free | PlanKind::Unknown => format!("[{}]", label.bright_black()),
        PlanKind::Go => format!("[{}]", label.bright_blue()),
        PlanKind::Plus => format!("[{}]", label.cyan()),
        PlanKind::ProLite => format!("[{}]", label.yellow()),
        PlanKind::Pro => format!("[{}]", label.bright_yellow().bold()),
        PlanKind::Team | PlanKind::Business | PlanKind::Edu => {
            format!("[{}]", label.magenta())
        }
        PlanKind::Enterprise => format!("[{}]", label.bright_magenta().bold()),
    }
}

#[cfg(test)]
mod tests {
    use super::colored_plan;

    #[test]
    fn plan_colors_distinguish_go_and_both_pro_tiers() {
        assert!(colored_plan("Go", Some("go")).contains("\u{1b}[94m"));
        assert!(colored_plan("Pro 5×", Some("prolite")).contains("\u{1b}[33m"));
        let pro = colored_plan("Pro 20×", Some("pro"));
        assert!(pro.contains("\u{1b}[93m"));
        assert!(pro.contains("\u{1b}[1m"));
    }
}
