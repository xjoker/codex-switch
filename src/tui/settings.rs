//! TUI Settings tab: edit every `config.toml` key the product owns.

use crossterm::event::KeyCode;
use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};

use super::theme::{C_RED, base, dim, header, highlight};
use crate::config::{AppConfig, save as save_config};
use crate::warmup_schedule::{parse_iana_timezone, parse_schedule_time};

pub const LOG_LEVELS: [&str; 5] = ["error", "warn", "info", "debug", "trace"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    ProxyUrl,
    ProxyNoProxy,
    CacheTtl,
    MaxConcurrent,
    TuiRefresh,
    SafetyMargin,
    TeamPriority,
    PollInterval,
    SwitchThreshold,
    CacheRefresh,
    AutoWarmup,
    WarmupTimes,
    Timezone,
    TokenCheck,
    Notify,
    LogLevel,
    DeferSwitch,
    RestoreDelay,
}

const FOCUS_ORDER: &[Focus] = &[
    Focus::ProxyUrl,
    Focus::ProxyNoProxy,
    Focus::CacheTtl,
    Focus::MaxConcurrent,
    Focus::TuiRefresh,
    Focus::SafetyMargin,
    Focus::TeamPriority,
    Focus::PollInterval,
    Focus::SwitchThreshold,
    Focus::CacheRefresh,
    Focus::AutoWarmup,
    Focus::WarmupTimes,
    Focus::Timezone,
    Focus::TokenCheck,
    Focus::Notify,
    Focus::LogLevel,
    Focus::DeferSwitch,
    Focus::RestoreDelay,
];

pub struct SettingsState {
    focus: Focus,
    editing: bool,
    dirty: bool,
    input: String,
    cursor: usize,
    error: Option<String>,
    time_idx: usize,
    pub(crate) draft: AppConfig,
}

pub enum SettingsOutcome {
    Continue,
    Saved { message: String },
}

impl SettingsState {
    pub fn is_editing(&self) -> bool {
        self.editing
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn from_config(config: AppConfig) -> Self {
        Self {
            focus: Focus::ProxyUrl,
            editing: false,
            dirty: false,
            input: String::new(),
            cursor: 0,
            error: None,
            time_idx: 0,
            draft: config,
        }
    }

    pub fn handle_key(&mut self, code: KeyCode) -> SettingsOutcome {
        self.error = None;
        if self.editing {
            return self.handle_edit_key(code);
        }
        match code {
            KeyCode::Char('s') => self.try_save(),
            KeyCode::Down | KeyCode::Char('j') => {
                if self.focus == Focus::WarmupTimes {
                    self.time_move(1);
                } else {
                    self.focus_delta(1);
                }
                SettingsOutcome::Continue
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if self.focus == Focus::WarmupTimes {
                    self.time_move(-1);
                } else {
                    self.focus_delta(-1);
                }
                SettingsOutcome::Continue
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                self.activate();
                SettingsOutcome::Continue
            }
            KeyCode::Left => {
                self.nudge(-1);
                SettingsOutcome::Continue
            }
            KeyCode::Right => {
                self.nudge(1);
                SettingsOutcome::Continue
            }
            KeyCode::Char('+') | KeyCode::Char('=') | KeyCode::Char('a') => {
                if self.focus == Focus::WarmupTimes {
                    self.add_time_and_edit();
                }
                SettingsOutcome::Continue
            }
            KeyCode::Char('-') | KeyCode::Char('_') | KeyCode::Char('d') | KeyCode::Delete => {
                if self.focus == Focus::WarmupTimes {
                    self.remove_time();
                }
                SettingsOutcome::Continue
            }
            _ => SettingsOutcome::Continue,
        }
    }

    fn handle_edit_key(&mut self, code: KeyCode) -> SettingsOutcome {
        match code {
            KeyCode::Esc => {
                self.editing = false;
                self.input.clear();
                self.cursor = 0;
                SettingsOutcome::Continue
            }
            KeyCode::Enter => {
                if let Err(err) = self.commit_edit() {
                    self.error = Some(err);
                } else {
                    self.editing = false;
                    self.input.clear();
                    self.cursor = 0;
                }
                SettingsOutcome::Continue
            }
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                let byte_pos = char_to_byte(&self.input, self.cursor);
                self.input.remove(byte_pos);
                SettingsOutcome::Continue
            }
            KeyCode::Delete => {
                if self.cursor < self.input.chars().count() {
                    let byte_pos = char_to_byte(&self.input, self.cursor);
                    self.input.remove(byte_pos);
                }
                SettingsOutcome::Continue
            }
            KeyCode::Left if self.cursor > 0 => {
                self.cursor -= 1;
                SettingsOutcome::Continue
            }
            KeyCode::Right => {
                if self.cursor < self.input.chars().count() {
                    self.cursor += 1;
                }
                SettingsOutcome::Continue
            }
            KeyCode::Home => {
                self.cursor = 0;
                SettingsOutcome::Continue
            }
            KeyCode::End => {
                self.cursor = self.input.chars().count();
                SettingsOutcome::Continue
            }
            KeyCode::Char(c) if !c.is_control() => {
                let byte_pos = char_to_byte(&self.input, self.cursor);
                self.input.insert(byte_pos, c);
                self.cursor += 1;
                SettingsOutcome::Continue
            }
            _ => SettingsOutcome::Continue,
        }
    }

    fn activate(&mut self) {
        match self.focus {
            Focus::TeamPriority => {
                self.draft.use_cfg.team_priority = !self.draft.use_cfg.team_priority;
                self.dirty = true;
            }
            Focus::AutoWarmup => {
                self.draft.daemon.auto_warmup = !self.draft.daemon.auto_warmup;
                self.dirty = true;
            }
            Focus::Notify => {
                self.draft.daemon.notify = !self.draft.daemon.notify;
                self.dirty = true;
            }
            Focus::DeferSwitch => {
                self.draft.daemon.defer_switch_while_codex_running =
                    !self.draft.daemon.defer_switch_while_codex_running;
                self.dirty = true;
            }
            Focus::LogLevel => self.nudge(1),
            Focus::WarmupTimes if self.is_add_time_row() => self.add_time_and_edit(),
            _ => self.begin_edit(),
        }
    }

    fn nudge(&mut self, delta: i32) {
        match self.focus {
            Focus::LogLevel => {
                let current = LOG_LEVELS
                    .iter()
                    .position(|level| *level == self.draft.daemon.log_level)
                    .unwrap_or(0);
                let next = (current as i32 + delta).rem_euclid(LOG_LEVELS.len() as i32) as usize;
                self.draft.daemon.log_level = LOG_LEVELS[next].to_string();
                self.dirty = true;
            }
            Focus::TeamPriority if delta != 0 => {
                self.draft.use_cfg.team_priority = !self.draft.use_cfg.team_priority;
                self.dirty = true;
            }
            Focus::AutoWarmup if delta != 0 => {
                self.draft.daemon.auto_warmup = !self.draft.daemon.auto_warmup;
                self.dirty = true;
            }
            Focus::Notify if delta != 0 => {
                self.draft.daemon.notify = !self.draft.daemon.notify;
                self.dirty = true;
            }
            Focus::DeferSwitch if delta != 0 => {
                self.draft.daemon.defer_switch_while_codex_running =
                    !self.draft.daemon.defer_switch_while_codex_running;
                self.dirty = true;
            }
            _ => {}
        }
    }

    fn begin_edit(&mut self) {
        if self.is_add_time_row() {
            self.add_time_and_edit();
            return;
        }
        let value = match self.focus {
            Focus::ProxyUrl => self.draft.proxy.url.clone().unwrap_or_default(),
            Focus::ProxyNoProxy => self.draft.proxy.no_proxy.clone().unwrap_or_default(),
            Focus::CacheTtl => self.draft.cache.ttl.to_string(),
            Focus::MaxConcurrent => self.draft.network.max_concurrent.to_string(),
            Focus::TuiRefresh => self.draft.tui.auto_refresh_interval_secs.to_string(),
            Focus::SafetyMargin => format_num(self.draft.use_cfg.safety_margin_7d),
            Focus::PollInterval => self.draft.daemon.poll_interval_secs.to_string(),
            Focus::SwitchThreshold => format_num(self.draft.daemon.switch_threshold),
            Focus::CacheRefresh => self.draft.daemon.cache_refresh_interval_secs.to_string(),
            Focus::WarmupTimes => self
                .draft
                .daemon
                .warmup_times
                .get(self.time_idx)
                .cloned()
                .unwrap_or_default(),
            Focus::Timezone => self.draft.daemon.timezone.clone(),
            Focus::TokenCheck => self.draft.daemon.token_check_interval_secs.to_string(),
            Focus::RestoreDelay => self.draft.launch.restore_delay_secs.to_string(),
            Focus::TeamPriority
            | Focus::AutoWarmup
            | Focus::Notify
            | Focus::LogLevel
            | Focus::DeferSwitch => return,
        };
        self.input = value;
        self.cursor = self.input.chars().count();
        self.editing = true;
    }

    fn commit_edit(&mut self) -> Result<(), String> {
        let raw = self.input.trim().to_string();
        match self.focus {
            Focus::ProxyUrl => {
                self.draft.proxy.url = empty_to_none(raw);
            }
            Focus::ProxyNoProxy => {
                self.draft.proxy.no_proxy = empty_to_none(raw);
            }
            Focus::CacheTtl => self.draft.cache.ttl = parse_u64(&raw, 1, "cache.ttl")?,
            Focus::MaxConcurrent => {
                self.draft.network.max_concurrent = parse_usize(&raw, 1, "network.max_concurrent")?;
            }
            Focus::TuiRefresh => {
                let value = parse_u64(&raw, 30, "tui.auto_refresh_interval_secs")?;
                self.draft.tui.auto_refresh_interval_secs = value;
            }
            Focus::SafetyMargin => {
                self.draft.use_cfg.safety_margin_7d = parse_f64(&raw, "use.safety_margin_7d")?;
            }
            Focus::PollInterval => {
                self.draft.daemon.poll_interval_secs =
                    parse_u64(&raw, 1, "daemon.poll_interval_secs")?;
            }
            Focus::SwitchThreshold => {
                self.draft.daemon.switch_threshold = parse_f64(&raw, "daemon.switch_threshold")?;
            }
            Focus::CacheRefresh => {
                self.draft.daemon.cache_refresh_interval_secs =
                    parse_u64(&raw, 1, "daemon.cache_refresh_interval_secs")?;
            }
            Focus::WarmupTimes => {
                let Some((hour, minute)) = parse_schedule_time(&raw) else {
                    return Err("warmup time must be HH:MM (00-23:00-59)".into());
                };
                let stamp = format!("{hour:02}:{minute:02}");
                if self.time_idx < self.draft.daemon.warmup_times.len() {
                    self.draft.daemon.warmup_times[self.time_idx] = stamp.clone();
                } else {
                    self.draft.daemon.warmup_times.push(stamp.clone());
                }
                self.draft.daemon.warmup_times.sort();
                self.draft.daemon.warmup_times.dedup();
                self.time_idx = self
                    .draft
                    .daemon
                    .warmup_times
                    .iter()
                    .position(|time| *time == stamp)
                    .unwrap_or(0);
            }
            Focus::Timezone => {
                if raw.is_empty() {
                    self.draft.daemon.timezone.clear();
                } else if parse_iana_timezone(&raw).is_none() {
                    return Err(
                        "timezone must be empty (system local) or an IANA name like Asia/Shanghai"
                            .into(),
                    );
                } else {
                    self.draft.daemon.timezone = raw;
                }
            }
            Focus::TokenCheck => {
                self.draft.daemon.token_check_interval_secs =
                    parse_u64(&raw, 1, "daemon.token_check_interval_secs")?;
            }
            Focus::RestoreDelay => {
                self.draft.launch.restore_delay_secs =
                    parse_u64(&raw, 1, "launch.restore_delay_secs")?;
            }
            Focus::TeamPriority
            | Focus::AutoWarmup
            | Focus::Notify
            | Focus::LogLevel
            | Focus::DeferSwitch => {}
        }
        self.dirty = true;
        Ok(())
    }

    fn try_save(&mut self) -> SettingsOutcome {
        if self.editing
            && let Err(err) = self.commit_edit()
        {
            self.error = Some(err);
            return SettingsOutcome::Continue;
        }
        self.editing = false;
        let mut warnings = Vec::new();
        let config = self.draft.clone().normalize(&mut warnings);
        if let Err(err) = save_config(&config) {
            self.error = Some(err.to_string());
            return SettingsOutcome::Continue;
        }
        crate::config::replace_runtime(config.clone());
        self.draft = config;
        self.dirty = false;
        let mut message = "Saved config.toml".to_string();
        if !warnings.is_empty() {
            message.push_str(". ");
            message.push_str(&warnings.join(" "));
        }
        message.push_str(
            ". Restart the daemon to apply poll/token/cache intervals; warmup slots and timezone are re-read about once a minute.",
        );
        SettingsOutcome::Saved { message }
    }

    fn focus_delta(&mut self, delta: i32) {
        let idx = FOCUS_ORDER
            .iter()
            .position(|item| *item == self.focus)
            .unwrap_or(0);
        let next = (idx as i32 + delta).rem_euclid(FOCUS_ORDER.len() as i32) as usize;
        self.focus = FOCUS_ORDER[next];
        if self.focus == Focus::WarmupTimes {
            self.time_idx = if delta < 0 {
                self.draft.daemon.warmup_times.len()
            } else {
                0
            };
        }
    }

    fn time_move(&mut self, delta: i32) {
        let add_row = self.draft.daemon.warmup_times.len() as i32;
        let next = self.time_idx as i32 + delta;
        if next < 0 {
            self.focus_delta(-1);
            return;
        }
        if next > add_row {
            self.focus_delta(1);
            return;
        }
        self.time_idx = next as usize;
    }

    fn is_add_time_row(&self) -> bool {
        self.focus == Focus::WarmupTimes && self.time_idx >= self.draft.daemon.warmup_times.len()
    }

    fn add_time_and_edit(&mut self) {
        self.focus = Focus::WarmupTimes;
        self.time_idx = self.draft.daemon.warmup_times.len();
        self.input.clear();
        self.cursor = 0;
        self.editing = true;
    }

    fn remove_time(&mut self) {
        if self.is_add_time_row() {
            return;
        }
        if self.time_idx < self.draft.daemon.warmup_times.len() {
            self.draft.daemon.warmup_times.remove(self.time_idx);
            self.dirty = true;
            if self.time_idx > 0 && self.time_idx >= self.draft.daemon.warmup_times.len() {
                self.time_idx -= 1;
            }
        }
    }
}

fn empty_to_none(value: String) -> Option<String> {
    if value.is_empty() { None } else { Some(value) }
}

fn parse_u64(raw: &str, min: u64, name: &str) -> Result<u64, String> {
    let value: u64 = raw
        .parse()
        .map_err(|_| format!("{name} must be a number"))?;
    if value < min {
        return Err(format!("{name} must be at least {min}"));
    }
    Ok(value)
}

fn parse_usize(raw: &str, min: usize, name: &str) -> Result<usize, String> {
    let value: usize = raw
        .parse()
        .map_err(|_| format!("{name} must be a number"))?;
    if value < min {
        return Err(format!("{name} must be at least {min}"));
    }
    Ok(value)
}

fn parse_f64(raw: &str, name: &str) -> Result<f64, String> {
    raw.parse().map_err(|_| format!("{name} must be a number"))
}

fn format_num(value: f64) -> String {
    if (value - value.round()).abs() < f64::EPSILON {
        format!("{value:.0}")
    } else {
        value.to_string()
    }
}

fn char_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

fn bool_label(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

fn field_value(settings: &SettingsState, focus: Focus, value: &str) -> String {
    if settings.editing && settings.focus == focus {
        let mut chars: Vec<char> = settings.input.chars().collect();
        let idx = settings.cursor.min(chars.len());
        chars.insert(idx, '▏');
        return chars.into_iter().collect();
    }
    if value.is_empty() {
        return "(empty)".to_string();
    }
    value.to_string()
}

fn push_field(
    settings: &SettingsState,
    focus: Focus,
    name: &str,
    value: String,
    lines: &mut Vec<Line<'static>>,
    focused_line: &mut usize,
) {
    if settings.focus == focus {
        *focused_line = lines.len();
    }
    let style = if settings.focus == focus {
        highlight()
    } else {
        base()
    };
    lines.push(Line::from(vec![
        Span::styled(format!("{name:<22}"), dim()),
        Span::styled(value, style),
    ]));
}

pub fn render_settings_tab(f: &mut Frame, settings: &SettingsState, area: Rect) {
    let title = if settings.dirty {
        " Settings * "
    } else {
        " Settings "
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(base().fg(super::theme::C_BLUE))
        .style(base());
    let inner = block.inner(area);
    f.render_widget(block, area);

    let focus_style = highlight();
    let label = base();
    let mut focused_line = 0usize;
    let mut lines = vec![Line::from(Span::styled("Proxy / network / TUI", header()))];

    push_field(
        settings,
        Focus::ProxyUrl,
        "proxy.url",
        field_value(
            settings,
            Focus::ProxyUrl,
            settings.draft.proxy.url.as_deref().unwrap_or(""),
        ),
        &mut lines,
        &mut focused_line,
    );
    push_field(
        settings,
        Focus::ProxyNoProxy,
        "proxy.no_proxy",
        field_value(
            settings,
            Focus::ProxyNoProxy,
            settings.draft.proxy.no_proxy.as_deref().unwrap_or(""),
        ),
        &mut lines,
        &mut focused_line,
    );
    push_field(
        settings,
        Focus::CacheTtl,
        "cache.ttl",
        field_value(
            settings,
            Focus::CacheTtl,
            &settings.draft.cache.ttl.to_string(),
        ),
        &mut lines,
        &mut focused_line,
    );
    push_field(
        settings,
        Focus::MaxConcurrent,
        "network.max_concurrent",
        field_value(
            settings,
            Focus::MaxConcurrent,
            &settings.draft.network.max_concurrent.to_string(),
        ),
        &mut lines,
        &mut focused_line,
    );
    push_field(
        settings,
        Focus::TuiRefresh,
        "tui.auto_refresh_secs",
        field_value(
            settings,
            Focus::TuiRefresh,
            &settings.draft.tui.auto_refresh_interval_secs.to_string(),
        ),
        &mut lines,
        &mut focused_line,
    );
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("Selection", header())));
    push_field(
        settings,
        Focus::SafetyMargin,
        "use.safety_margin_7d",
        field_value(
            settings,
            Focus::SafetyMargin,
            &format_num(settings.draft.use_cfg.safety_margin_7d),
        ),
        &mut lines,
        &mut focused_line,
    );
    push_field(
        settings,
        Focus::TeamPriority,
        "use.team_priority",
        bool_label(settings.draft.use_cfg.team_priority).to_string(),
        &mut lines,
        &mut focused_line,
    );
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Daemon  (TUI W is a separate session toggle)",
        header(),
    )));
    push_field(
        settings,
        Focus::PollInterval,
        "poll_interval_secs",
        field_value(
            settings,
            Focus::PollInterval,
            &settings.draft.daemon.poll_interval_secs.to_string(),
        ),
        &mut lines,
        &mut focused_line,
    );
    push_field(
        settings,
        Focus::SwitchThreshold,
        "switch_threshold",
        field_value(
            settings,
            Focus::SwitchThreshold,
            &format_num(settings.draft.daemon.switch_threshold),
        ),
        &mut lines,
        &mut focused_line,
    );
    push_field(
        settings,
        Focus::CacheRefresh,
        "cache_refresh_secs",
        field_value(
            settings,
            Focus::CacheRefresh,
            &settings
                .draft
                .daemon
                .cache_refresh_interval_secs
                .to_string(),
        ),
        &mut lines,
        &mut focused_line,
    );
    push_field(
        settings,
        Focus::AutoWarmup,
        "auto_warmup",
        bool_label(settings.draft.daemon.auto_warmup).to_string(),
        &mut lines,
        &mut focused_line,
    );

    let times = &settings.draft.daemon.warmup_times;
    if times.is_empty() && settings.focus != Focus::WarmupTimes {
        push_field(
            settings,
            Focus::WarmupTimes,
            "warmup_times",
            "(none: warm on cache refresh when auto_warmup is on)".into(),
            &mut lines,
            &mut focused_line,
        );
    } else {
        lines.push(Line::from(Span::styled(
            format!("{:<22}", "warmup_times"),
            dim(),
        )));
        for (idx, time) in times.iter().enumerate() {
            let focused = settings.focus == Focus::WarmupTimes && settings.time_idx == idx;
            if focused {
                focused_line = lines.len();
            }
            let display = if settings.editing && focused {
                field_value(settings, Focus::WarmupTimes, time)
            } else {
                time.clone()
            };
            let marker = if focused { ">" } else { " " };
            lines.push(Line::from(vec![
                Span::styled(format!("  {marker} "), dim()),
                Span::styled(display, if focused { focus_style } else { label }),
            ]));
        }
        let add_focused = settings.focus == Focus::WarmupTimes && settings.time_idx >= times.len();
        if add_focused {
            focused_line = lines.len();
        }
        let add_label = if settings.editing && add_focused {
            field_value(settings, Focus::WarmupTimes, "")
        } else {
            "+ add time".to_string()
        };
        lines.push(Line::from(vec![
            Span::styled(if add_focused { "  > " } else { "    " }, dim()),
            Span::styled(add_label, if add_focused { focus_style } else { dim() }),
        ]));
    }

    let tz_value = if settings.draft.daemon.timezone.is_empty()
        && !(settings.editing && settings.focus == Focus::Timezone)
    {
        "(system local)".to_string()
    } else {
        field_value(settings, Focus::Timezone, &settings.draft.daemon.timezone)
    };
    push_field(
        settings,
        Focus::Timezone,
        "timezone",
        tz_value,
        &mut lines,
        &mut focused_line,
    );

    push_field(
        settings,
        Focus::TokenCheck,
        "token_check_secs",
        field_value(
            settings,
            Focus::TokenCheck,
            &settings.draft.daemon.token_check_interval_secs.to_string(),
        ),
        &mut lines,
        &mut focused_line,
    );
    push_field(
        settings,
        Focus::Notify,
        "notify",
        bool_label(settings.draft.daemon.notify).to_string(),
        &mut lines,
        &mut focused_line,
    );
    push_field(
        settings,
        Focus::LogLevel,
        "log_level",
        settings.draft.daemon.log_level.clone(),
        &mut lines,
        &mut focused_line,
    );
    push_field(
        settings,
        Focus::DeferSwitch,
        "defer_while_codex",
        bool_label(settings.draft.daemon.defer_switch_while_codex_running).to_string(),
        &mut lines,
        &mut focused_line,
    );
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("Launch", header())));
    push_field(
        settings,
        Focus::RestoreDelay,
        "restore_delay_secs",
        field_value(
            settings,
            Focus::RestoreDelay,
            &settings.draft.launch.restore_delay_secs.to_string(),
        ),
        &mut lines,
        &mut focused_line,
    );
    lines.push(Line::from(""));
    if let Some(error) = &settings.error {
        lines.push(Line::from(Span::styled(error.clone(), base().fg(C_RED))));
    } else {
        lines.push(Line::from(Span::styled(
            "j/k move  enter edit/toggle  ←/→ cycle  +/- add time  d remove  s save  esc cancel edit",
            dim(),
        )));
        lines.push(Line::from(Span::styled(
            "Empty timezone = system local. Slots are HH:MM in that zone. Empty warmup_times + auto_warmup on = warm during cache refresh.",
            dim(),
        )));
    }

    let visible_height = inner.height as usize;
    let skip = focused_line.saturating_sub(visible_height.saturating_sub(1));
    let visible: Vec<Line<'static>> = lines.into_iter().skip(skip).collect();
    f.render_widget(Paragraph::new(visible).style(base()), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyCode;

    #[test]
    fn toggling_auto_warmup_and_adding_a_slot_round_trips() {
        let mut settings = SettingsState::from_config(AppConfig::default());
        while settings.focus != Focus::AutoWarmup {
            settings.handle_key(KeyCode::Down);
        }
        settings.handle_key(KeyCode::Enter);
        assert!(settings.draft.daemon.auto_warmup);

        while settings.focus != Focus::WarmupTimes {
            settings.handle_key(KeyCode::Down);
        }
        settings.handle_key(KeyCode::Char('+'));
        for ch in ['0', '8', ':', '0', '0'] {
            settings.handle_key(KeyCode::Char(ch));
        }
        settings.handle_key(KeyCode::Enter);
        assert_eq!(
            settings.draft.daemon.warmup_times,
            vec!["08:00".to_string()]
        );
    }

    #[test]
    fn invalid_warmup_time_is_rejected() {
        let mut settings = SettingsState::from_config(AppConfig::default());
        while settings.focus != Focus::WarmupTimes {
            settings.handle_key(KeyCode::Down);
        }
        settings.handle_key(KeyCode::Char('+'));
        for ch in ['2', '5', ':', '0', '0'] {
            settings.handle_key(KeyCode::Char(ch));
        }
        settings.handle_key(KeyCode::Enter);
        assert!(settings.error.as_ref().is_some_and(|e| e.contains("HH:MM")));
        assert!(settings.draft.daemon.warmup_times.is_empty());
    }

    #[test]
    fn down_from_the_add_time_row_leaves_warmup_times() {
        let mut settings = SettingsState::from_config(AppConfig::default());
        while settings.focus != Focus::WarmupTimes {
            settings.handle_key(KeyCode::Down);
        }
        assert!(settings.is_add_time_row());
        settings.handle_key(KeyCode::Down);
        assert_eq!(settings.focus, Focus::Timezone);
    }

    #[test]
    fn timezone_edit_accepts_iana_and_rejects_garbage() {
        let mut settings = SettingsState::from_config(AppConfig::default());
        while settings.focus != Focus::Timezone {
            settings.handle_key(KeyCode::Down);
        }
        settings.handle_key(KeyCode::Enter);
        for ch in "Asia/Shanghai".chars() {
            settings.handle_key(KeyCode::Char(ch));
        }
        settings.handle_key(KeyCode::Enter);
        assert_eq!(settings.draft.daemon.timezone, "Asia/Shanghai");

        settings.handle_key(KeyCode::Enter);
        settings.handle_key(KeyCode::Backspace);
        settings.handle_key(KeyCode::Char('x'));
        settings.handle_key(KeyCode::Enter);
        assert!(
            settings.error.as_ref().is_some_and(|e| e.contains("IANA")),
            "{:?}",
            settings.error
        );
        assert_eq!(settings.draft.daemon.timezone, "Asia/Shanghai");
    }

    #[test]
    fn try_save_persists_warmup_times() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let prev_cs = std::env::var_os("CODEX_SWITCH_HOME");
        unsafe {
            std::env::set_var("CODEX_SWITCH_HOME", dir.path());
        }
        let mut settings = SettingsState::from_config(AppConfig::default());
        settings.draft.daemon.auto_warmup = true;
        settings.draft.daemon.warmup_times = vec!["08:00".into(), "13:10".into()];
        settings.draft.daemon.timezone = "Asia/Shanghai".into();
        match settings.try_save() {
            SettingsOutcome::Saved { message } => {
                assert!(message.contains("Saved config.toml"), "{message}");
            }
            SettingsOutcome::Continue => panic!("save should succeed"),
        }
        let loaded = crate::config::load_current().expect("config.toml after save");
        assert!(loaded.daemon.auto_warmup);
        assert_eq!(
            loaded.daemon.warmup_times,
            vec!["08:00".to_string(), "13:10".to_string()]
        );
        assert_eq!(loaded.daemon.timezone, "Asia/Shanghai");
        unsafe {
            match prev_cs {
                Some(v) => std::env::set_var("CODEX_SWITCH_HOME", v),
                None => std::env::remove_var("CODEX_SWITCH_HOME"),
            }
        }
    }

    #[test]
    fn settings_cover_every_owned_config_key() {
        // Fails to compile if AppConfig gains a product-owned key the form missed.
        let AppConfig {
            proxy,
            cache,
            network,
            tui,
            use_cfg,
            daemon,
            launch,
        } = AppConfig::default();
        let crate::config::ProxyConfig {
            url: _,
            no_proxy: _,
        } = proxy;
        let crate::config::CacheConfig { ttl: _ } = cache;
        let crate::config::NetworkConfig { max_concurrent: _ } = network;
        let crate::config::TuiConfig {
            auto_refresh_interval_secs: _,
        } = tui;
        let crate::config::UseConfig {
            safety_margin_7d: _,
            team_priority: _,
        } = use_cfg;
        let crate::config::DaemonConfig {
            poll_interval_secs: _,
            switch_threshold: _,
            cache_refresh_interval_secs: _,
            auto_warmup: _,
            warmup_times: _,
            timezone: _,
            token_check_interval_secs: _,
            notify: _,
            log_level: _,
            defer_switch_while_codex_running: _,
        } = daemon;
        let crate::config::LaunchConfig {
            restore_delay_secs: _,
        } = launch;
        assert_eq!(FOCUS_ORDER.len(), 18);
    }

    fn type_value(settings: &mut SettingsState, value: &str) {
        settings.handle_key(KeyCode::Enter);
        for _ in 0..32 {
            settings.handle_key(KeyCode::Backspace);
        }
        for ch in value.chars() {
            settings.handle_key(KeyCode::Char(ch));
        }
        settings.handle_key(KeyCode::Enter);
    }

    fn move_to(settings: &mut SettingsState, target: Focus) {
        for _ in 0..FOCUS_ORDER.len() * 4 {
            if settings.focus == target {
                return;
            }
            settings.handle_key(KeyCode::Down);
        }
        panic!("did not reach {target:?}");
    }

    #[test]
    fn every_field_edits_and_saves() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let prev_cs = std::env::var_os("CODEX_SWITCH_HOME");
        unsafe {
            std::env::set_var("CODEX_SWITCH_HOME", dir.path());
        }
        let mut settings = SettingsState::from_config(AppConfig::default());

        type_value(&mut settings, "socks5h://127.0.0.1:1080");
        move_to(&mut settings, Focus::ProxyNoProxy);
        type_value(&mut settings, "localhost");
        move_to(&mut settings, Focus::CacheTtl);
        type_value(&mut settings, "120");
        move_to(&mut settings, Focus::MaxConcurrent);
        type_value(&mut settings, "8");
        move_to(&mut settings, Focus::TuiRefresh);
        type_value(&mut settings, "60");
        move_to(&mut settings, Focus::SafetyMargin);
        type_value(&mut settings, "15");
        move_to(&mut settings, Focus::TeamPriority);
        settings.handle_key(KeyCode::Enter);
        move_to(&mut settings, Focus::PollInterval);
        type_value(&mut settings, "90");
        move_to(&mut settings, Focus::SwitchThreshold);
        type_value(&mut settings, "70");
        move_to(&mut settings, Focus::CacheRefresh);
        type_value(&mut settings, "240");
        move_to(&mut settings, Focus::AutoWarmup);
        settings.handle_key(KeyCode::Enter);
        move_to(&mut settings, Focus::WarmupTimes);
        settings.handle_key(KeyCode::Char('+'));
        for ch in ['0', '8', ':', '0', '0'] {
            settings.handle_key(KeyCode::Char(ch));
        }
        settings.handle_key(KeyCode::Enter);
        move_to(&mut settings, Focus::Timezone);
        type_value(&mut settings, "UTC");
        move_to(&mut settings, Focus::TokenCheck);
        type_value(&mut settings, "180");
        move_to(&mut settings, Focus::Notify);
        settings.handle_key(KeyCode::Enter);
        move_to(&mut settings, Focus::LogLevel);
        settings.handle_key(KeyCode::Right);
        move_to(&mut settings, Focus::DeferSwitch);
        settings.handle_key(KeyCode::Enter);
        move_to(&mut settings, Focus::RestoreDelay);
        type_value(&mut settings, "5");
        assert!(settings.is_dirty());

        match settings.try_save() {
            SettingsOutcome::Saved { .. } => {}
            SettingsOutcome::Continue => panic!("save should succeed: {:?}", settings.error),
        }
        assert!(!settings.is_dirty());

        let loaded = crate::config::load_current().expect("saved config");
        assert_eq!(
            loaded.proxy.url.as_deref(),
            Some("socks5h://127.0.0.1:1080")
        );
        assert_eq!(loaded.proxy.no_proxy.as_deref(), Some("localhost"));
        assert_eq!(loaded.cache.ttl, 120);
        assert_eq!(loaded.network.max_concurrent, 8);
        assert_eq!(loaded.tui.auto_refresh_interval_secs, 60);
        assert_eq!(loaded.use_cfg.safety_margin_7d, 15.0);
        assert!(!loaded.use_cfg.team_priority);
        assert_eq!(loaded.daemon.poll_interval_secs, 90);
        assert_eq!(loaded.daemon.switch_threshold, 70.0);
        assert_eq!(loaded.daemon.cache_refresh_interval_secs, 240);
        assert!(loaded.daemon.auto_warmup);
        assert_eq!(loaded.daemon.warmup_times, vec!["08:00".to_string()]);
        assert_eq!(loaded.daemon.timezone, "UTC");
        assert_eq!(loaded.daemon.token_check_interval_secs, 180);
        assert!(loaded.daemon.notify);
        assert_eq!(loaded.daemon.log_level, "warn");
        assert!(!loaded.daemon.defer_switch_while_codex_running);
        assert_eq!(loaded.launch.restore_delay_secs, 5);
        unsafe {
            match prev_cs {
                Some(v) => std::env::set_var("CODEX_SWITCH_HOME", v),
                None => std::env::remove_var("CODEX_SWITCH_HOME"),
            }
        }
    }

    #[test]
    fn editing_a_warmup_slot_keeps_focus_on_the_new_time() {
        let mut cfg = AppConfig::default();
        cfg.daemon.warmup_times = vec!["08:00".into(), "13:00".into()];
        let mut settings = SettingsState::from_config(cfg);
        move_to(&mut settings, Focus::WarmupTimes);
        settings.handle_key(KeyCode::Down);
        assert_eq!(settings.time_idx, 1);
        type_value(&mut settings, "07:00");
        assert_eq!(
            settings.draft.daemon.warmup_times,
            vec!["07:00".to_string(), "08:00".to_string()]
        );
        assert_eq!(settings.time_idx, 0);
        assert_eq!(settings.focus, Focus::WarmupTimes);
    }

    #[test]
    fn rejected_edit_does_not_mark_dirty() {
        let mut settings = SettingsState::from_config(AppConfig::default());
        move_to(&mut settings, Focus::CacheTtl);
        type_value(&mut settings, "0");
        assert!(settings.error.is_some());
        assert!(!settings.is_dirty());
        assert_eq!(settings.draft.cache.ttl, 300);
    }
}
