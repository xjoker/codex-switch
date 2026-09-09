//! TUI Settings tab: edit every `config.toml` key the product owns.

use crossterm::event::KeyCode;
use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};

use super::hitmap::HitMap;
use super::theme::{C_RED, C_YELLOW, base, dim, header, highlight};
use crate::config::{AppConfig, save as save_config};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    ProxyUrl,
    ProxyNoProxy,
    CacheTtl,
    MaxConcurrent,
    TuiRefresh,
    SafetyMargin,
    TeamPriority,
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
    Focus::RestoreDelay,
];

pub struct SettingsState {
    focus: Focus,
    editing: bool,
    dirty: bool,
    input: String,
    cursor: usize,
    error: Option<String>,
    notice: Option<String>,
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

    pub(crate) fn focused_index(&self) -> usize {
        FOCUS_ORDER
            .iter()
            .position(|item| *item == self.focus)
            .unwrap_or(0)
    }

    pub(crate) fn click_field(&mut self, index: usize) {
        let Some(&focus) = FOCUS_ORDER.get(index) else {
            return;
        };
        if self.editing {
            if self.focus == focus {
                return;
            }
            if let Err(err) = self.commit_edit() {
                self.error = Some(err);
                return;
            }
            self.editing = false;
            self.input.clear();
            self.cursor = 0;
        }
        self.error = None;
        self.notice = None;
        self.focus = focus;
        self.activate();
    }

    pub(crate) fn handle_wheel(&mut self, down: bool) {
        if self.editing {
            return;
        }
        self.focus_delta(if down { 1 } else { -1 });
    }

    pub fn from_config(config: AppConfig) -> Self {
        Self {
            focus: Focus::ProxyUrl,
            editing: false,
            dirty: false,
            input: String::new(),
            cursor: 0,
            error: None,
            notice: None,
            draft: config,
        }
    }

    pub fn handle_key(&mut self, code: KeyCode) -> SettingsOutcome {
        self.error = None;
        self.notice = None;
        if self.editing {
            return self.handle_edit_key(code);
        }
        match code {
            KeyCode::Char('s') => self.try_save(),
            KeyCode::Down | KeyCode::Char('j') => {
                self.focus_delta(1);
                SettingsOutcome::Continue
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.focus_delta(-1);
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
            _ => self.begin_edit(),
        }
    }

    fn nudge(&mut self, delta: i32) {
        match self.focus {
            Focus::TeamPriority if delta != 0 => {
                self.draft.use_cfg.team_priority = !self.draft.use_cfg.team_priority;
                self.dirty = true;
            }
            _ => {}
        }
    }

    fn begin_edit(&mut self) {
        let value = match self.focus {
            Focus::ProxyUrl => self.draft.proxy.url.clone().unwrap_or_default(),
            Focus::ProxyNoProxy => self.draft.proxy.no_proxy.clone().unwrap_or_default(),
            Focus::CacheTtl => self.draft.cache.ttl.to_string(),
            Focus::MaxConcurrent => self.draft.network.max_concurrent.to_string(),
            Focus::TuiRefresh => self.draft.tui.auto_refresh_interval_secs.to_string(),
            Focus::SafetyMargin => format_num(self.draft.use_cfg.safety_margin_7d),
            Focus::RestoreDelay => self.draft.launch.restore_delay_secs.to_string(),
            Focus::TeamPriority => return,
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
            Focus::RestoreDelay => {
                self.draft.launch.restore_delay_secs =
                    parse_u64(&raw, 1, "launch.restore_delay_secs")?;
            }
            Focus::TeamPriority => {}
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
        message.push_str(". Applied in this process now.");
        SettingsOutcome::Saved { message }
    }

    fn focus_delta(&mut self, delta: i32) {
        let idx = FOCUS_ORDER
            .iter()
            .position(|item| *item == self.focus)
            .unwrap_or(0);
        let next = (idx as i32 + delta).rem_euclid(FOCUS_ORDER.len() as i32) as usize;
        self.focus = FOCUS_ORDER[next];
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
    line_focus: &mut Vec<Option<Focus>>,
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
    line_focus.push(Some(focus));
}

pub fn render_settings_tab(
    f: &mut Frame,
    settings: &SettingsState,
    area: Rect,
    hitmap: &mut HitMap,
) {
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
    hitmap.settings_body = Some(area);

    let mut focused_line = 0usize;
    let mut lines = vec![Line::from(Span::styled("Proxy / network / TUI", header()))];
    let mut line_focus: Vec<Option<Focus>> = vec![None];

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
        &mut line_focus,
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
        &mut line_focus,
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
        &mut line_focus,
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
        &mut line_focus,
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
        &mut line_focus,
        &mut focused_line,
    );
    lines.push(Line::from(""));
    line_focus.push(None);
    lines.push(Line::from(Span::styled("Selection", header())));
    line_focus.push(None);
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
        &mut line_focus,
        &mut focused_line,
    );
    push_field(
        settings,
        Focus::TeamPriority,
        "use.team_priority",
        bool_label(settings.draft.use_cfg.team_priority).to_string(),
        &mut lines,
        &mut line_focus,
        &mut focused_line,
    );
    lines.push(Line::from(""));
    line_focus.push(None);
    lines.push(Line::from(Span::styled("Launch", header())));
    line_focus.push(None);
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
        &mut line_focus,
        &mut focused_line,
    );
    lines.push(Line::from(""));
    line_focus.push(None);
    if let Some(error) = &settings.error {
        lines.push(Line::from(Span::styled(error.clone(), base().fg(C_RED))));
        line_focus.push(None);
    } else if let Some(notice) = &settings.notice {
        lines.push(Line::from(Span::styled(
            notice.clone(),
            base().fg(C_YELLOW),
        )));
        line_focus.push(None);
    } else {
        lines.push(Line::from(Span::styled(
            "click field  j/k move  enter edit/toggle  s save  esc cancel edit",
            dim(),
        )));
        line_focus.push(None);
    }

    let visible_height = inner.height as usize;
    let skip = focused_line.saturating_sub(visible_height.saturating_sub(1));
    debug_assert_eq!(lines.len(), line_focus.len());
    let mut fields = Vec::new();
    for (index, focus) in line_focus.into_iter().enumerate() {
        if index < skip {
            continue;
        }
        let visible_row = index - skip;
        if visible_row >= visible_height {
            break;
        }
        let Some(focus) = focus else {
            continue;
        };
        let field = FOCUS_ORDER
            .iter()
            .position(|item| *item == focus)
            .expect("settings line maps to a known field");
        let Ok(row) = u16::try_from(visible_row) else {
            continue;
        };
        fields.push((
            Rect {
                x: inner.x,
                y: inner.y.saturating_add(row),
                width: inner.width,
                height: 1,
            },
            field,
        ));
    }
    hitmap.settings_fields = fields;
    let visible: Vec<Line<'static>> = lines.into_iter().skip(skip).collect();
    f.render_widget(Paragraph::new(visible).style(base()), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyCode;

    #[test]
    fn settings_cover_every_owned_config_key() {
        // Fails to compile if AppConfig gains a product-owned key the form missed.
        let AppConfig {
            proxy,
            cache,
            network,
            tui,
            use_cfg,
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
        let crate::config::LaunchConfig {
            restore_delay_secs: _,
        } = launch;
        assert_eq!(FOCUS_ORDER.len(), 8);
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
        assert_eq!(loaded.launch.restore_delay_secs, 5);
        unsafe {
            match prev_cs {
                Some(v) => std::env::set_var("CODEX_SWITCH_HOME", v),
                None => std::env::remove_var("CODEX_SWITCH_HOME"),
            }
        }
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
