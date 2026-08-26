//! Single-dialog add/edit form for custom API providers.
//!
//! Navigation mode shows every field at once. Enter edits the focused cell;
//! `s` saves; Esc cancels the edit or the form.

use crossterm::event::KeyCode;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use super::popup::{self, PopupState};
use crate::provider::{ProviderModel, ProviderProfile};

const C_WHITE: Color = Color::Rgb(240, 240, 240);
const DIM: Color = Color::Rgb(120, 120, 120);
const C_RED: Color = Color::Rgb(255, 90, 90);
const C_YELLOW: Color = Color::Rgb(255, 220, 80);
const C_CYAN: Color = Color::Rgb(100, 210, 255);
const C_GREEN: Color = Color::Rgb(80, 220, 120);

/// Reasoning-effort presets. Index 0 skips the override; the rest are saved as
/// `model_reasoning_effort=<v>`. CLI `--reasoning` remains the escape hatch for
/// values Codex accepts that are not listed here.
pub const REASONING_CHOICES: [&str; 7] =
    ["(skip)", "minimal", "low", "medium", "high", "xhigh", "max"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormMode {
    Add,
    Edit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Alias,
    BaseUrl,
    ApiKey,
    Models,
}

#[derive(Debug, Clone)]
struct ModelDraft {
    id: String,
    reasoning_idx: usize,
    no_web_search: bool,
}

pub struct ProviderFormState {
    pub mode: FormMode,
    pub popup: PopupState,
    focus: Focus,
    editing: bool,
    alias: String,
    base_url: String,
    api_key: String,
    original_alias: Option<String>,
    original_key: String,
    models: Vec<ModelDraft>,
    model_idx: usize,
    default_idx: usize,
    input: String,
    cursor: usize,
    error: Option<String>,
}

pub enum FormOutcome {
    Continue,
    Cancel,
    Saved(Box<ProviderProfile>),
}

impl ProviderFormState {
    pub fn add() -> Self {
        Self {
            mode: FormMode::Add,
            popup: PopupState::new(),
            focus: Focus::Alias,
            editing: false,
            alias: String::new(),
            base_url: String::new(),
            api_key: String::new(),
            original_alias: None,
            original_key: String::new(),
            models: vec![ModelDraft {
                id: String::new(),
                reasoning_idx: 0,
                no_web_search: false,
            }],
            model_idx: 0,
            default_idx: 0,
            input: String::new(),
            cursor: 0,
            error: None,
        }
    }

    pub fn edit(profile: &ProviderProfile) -> Self {
        let models: Vec<ModelDraft> = profile
            .models
            .iter()
            .map(|model| ModelDraft {
                id: model.id.clone(),
                reasoning_idx: reasoning_index(model.reasoning.as_deref()),
                no_web_search: model.no_web_search,
            })
            .collect();
        let default_idx = models
            .iter()
            .position(|model| model.id == profile.default_model)
            .unwrap_or(0);
        Self {
            mode: FormMode::Edit,
            popup: PopupState::new(),
            focus: Focus::BaseUrl,
            editing: false,
            alias: profile.alias.clone(),
            base_url: profile.base_url.clone(),
            api_key: String::new(),
            original_alias: Some(profile.alias.clone()),
            original_key: profile.api_key.clone(),
            models: if models.is_empty() {
                vec![ModelDraft {
                    id: String::new(),
                    reasoning_idx: 0,
                    no_web_search: false,
                }]
            } else {
                models
            },
            model_idx: default_idx.min(profile.models.len().saturating_sub(1)),
            default_idx,
            input: String::new(),
            cursor: 0,
            error: None,
        }
    }

    pub fn title(&self) -> &'static str {
        match self.mode {
            FormMode::Add => "Add provider",
            FormMode::Edit => "Edit provider",
        }
    }

    pub fn handle_key(&mut self, code: KeyCode) -> FormOutcome {
        self.error = None;
        if self.editing {
            return self.handle_edit_key(code);
        }
        match code {
            KeyCode::Esc => FormOutcome::Cancel,
            KeyCode::Char('s') => self.try_save(),
            KeyCode::Enter => {
                self.begin_edit();
                FormOutcome::Continue
            }
            KeyCode::Tab | KeyCode::Down => {
                if self.focus == Focus::Models {
                    self.model_select(1);
                } else {
                    self.focus_next();
                }
                FormOutcome::Continue
            }
            KeyCode::BackTab | KeyCode::Up => {
                if self.focus == Focus::Models {
                    self.model_select(-1);
                } else {
                    self.focus_prev();
                }
                FormOutcome::Continue
            }
            KeyCode::Char('j') if self.focus == Focus::Models => {
                self.model_select(1);
                FormOutcome::Continue
            }
            KeyCode::Char('k') if self.focus == Focus::Models => {
                self.model_select(-1);
                FormOutcome::Continue
            }
            KeyCode::Char('+') | KeyCode::Char('=') if self.focus == Focus::Models => {
                self.models.push(ModelDraft {
                    id: String::new(),
                    reasoning_idx: 0,
                    no_web_search: false,
                });
                self.model_idx = self.models.len() - 1;
                self.begin_edit();
                FormOutcome::Continue
            }
            KeyCode::Char('-') | KeyCode::Char('_') if self.focus == Focus::Models => {
                if self.models.len() <= 1 {
                    self.error = Some("A provider needs at least one model".into());
                } else {
                    let removed = self.model_idx;
                    self.models.remove(removed);
                    if self.default_idx == removed {
                        self.default_idx = 0;
                    } else if self.default_idx > removed {
                        self.default_idx -= 1;
                    }
                    self.model_idx = self.model_idx.min(self.models.len() - 1);
                }
                FormOutcome::Continue
            }
            KeyCode::Char('*') if self.focus == Focus::Models => {
                self.default_idx = self.model_idx;
                FormOutcome::Continue
            }
            KeyCode::Left if self.focus == Focus::Models => {
                let idx = &mut self.models[self.model_idx].reasoning_idx;
                if *idx > 0 {
                    *idx -= 1;
                }
                FormOutcome::Continue
            }
            KeyCode::Right if self.focus == Focus::Models => {
                let idx = &mut self.models[self.model_idx].reasoning_idx;
                if *idx + 1 < REASONING_CHOICES.len() {
                    *idx += 1;
                }
                FormOutcome::Continue
            }
            KeyCode::Char('w') if self.focus == Focus::Models => {
                let model = &mut self.models[self.model_idx];
                model.no_web_search = !model.no_web_search;
                FormOutcome::Continue
            }
            _ => FormOutcome::Continue,
        }
    }

    fn handle_edit_key(&mut self, code: KeyCode) -> FormOutcome {
        match code {
            KeyCode::Esc => {
                self.editing = false;
                self.input.clear();
                self.cursor = 0;
                FormOutcome::Continue
            }
            KeyCode::Enter | KeyCode::Tab => {
                self.commit_edit();
                if matches!(code, KeyCode::Tab) {
                    self.focus_next();
                }
                FormOutcome::Continue
            }
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                let byte_pos = char_to_byte(&self.input, self.cursor);
                self.input.remove(byte_pos);
                FormOutcome::Continue
            }
            KeyCode::Delete => {
                let char_count = self.input.chars().count();
                if self.cursor < char_count {
                    let byte_pos = char_to_byte(&self.input, self.cursor);
                    self.input.remove(byte_pos);
                }
                FormOutcome::Continue
            }
            KeyCode::Left if self.cursor > 0 => {
                self.cursor -= 1;
                FormOutcome::Continue
            }
            KeyCode::Right => {
                let char_count = self.input.chars().count();
                if self.cursor < char_count {
                    self.cursor += 1;
                }
                FormOutcome::Continue
            }
            KeyCode::Home => {
                self.cursor = 0;
                FormOutcome::Continue
            }
            KeyCode::End => {
                self.cursor = self.input.chars().count();
                FormOutcome::Continue
            }
            KeyCode::Char(c) => {
                let byte_pos = char_to_byte(&self.input, self.cursor);
                self.input.insert(byte_pos, c);
                self.cursor += 1;
                FormOutcome::Continue
            }
            _ => FormOutcome::Continue,
        }
    }

    fn begin_edit(&mut self) {
        if self.focus == Focus::Alias && self.mode == FormMode::Edit {
            return;
        }
        let value = match self.focus {
            Focus::Alias => self.alias.clone(),
            Focus::BaseUrl => self.base_url.clone(),
            Focus::ApiKey => self.api_key.clone(),
            Focus::Models => self.models[self.model_idx].id.clone(),
        };
        self.input = value;
        self.cursor = self.input.chars().count();
        self.editing = true;
    }

    fn commit_edit(&mut self) {
        let value = self.input.trim().to_string();
        match self.focus {
            Focus::Alias => self.alias = value,
            Focus::BaseUrl => self.base_url = value,
            Focus::ApiKey => self.api_key = value,
            Focus::Models => self.models[self.model_idx].id = value,
        }
        self.editing = false;
        self.input.clear();
        self.cursor = 0;
    }

    fn focus_next(&mut self) {
        self.focus = match (self.mode, self.focus) {
            (FormMode::Add, Focus::Alias) => Focus::BaseUrl,
            (_, Focus::BaseUrl) => Focus::ApiKey,
            (_, Focus::ApiKey) => Focus::Models,
            (_, Focus::Models) if self.mode == FormMode::Add => Focus::Alias,
            (_, Focus::Models) => Focus::BaseUrl,
            (FormMode::Edit, Focus::Alias) => Focus::BaseUrl,
        };
    }

    fn focus_prev(&mut self) {
        self.focus = match (self.mode, self.focus) {
            (FormMode::Add, Focus::Alias) => Focus::Models,
            (_, Focus::BaseUrl) if self.mode == FormMode::Add => Focus::Alias,
            (_, Focus::BaseUrl) => Focus::Models,
            (_, Focus::ApiKey) => Focus::BaseUrl,
            (_, Focus::Models) => Focus::ApiKey,
            (FormMode::Edit, Focus::Alias) => Focus::Models,
        };
    }

    fn model_select(&mut self, delta: i32) {
        if self.models.is_empty() {
            return;
        }
        let len = self.models.len() as i32;
        let next = (self.model_idx as i32 + delta).rem_euclid(len);
        self.model_idx = next as usize;
    }

    fn try_save(&mut self) -> FormOutcome {
        if self.editing {
            self.commit_edit();
        }
        match self.build_profile() {
            Ok(profile) => FormOutcome::Saved(Box::new(profile)),
            Err(err) => {
                self.error = Some(err);
                FormOutcome::Continue
            }
        }
    }

    fn build_profile(&self) -> Result<ProviderProfile, String> {
        let alias = self.alias.trim();
        if alias.is_empty() {
            return Err("Alias cannot be empty".into());
        }
        crate::profile::validate_alias(alias).map_err(|err| format!("Invalid alias: {err}"))?;
        if self.mode == FormMode::Add {
            if crate::provider::exists(alias) {
                return Err(format!("'{alias}' already exists"));
            }
            if crate::profile::list_profiles()
                .map_err(|err| err.to_string())?
                .iter()
                .any(|p| p == alias)
            {
                return Err(format!("'{alias}' already names a ChatGPT profile"));
            }
        }
        if !(self.base_url.starts_with("http://") || self.base_url.starts_with("https://")) {
            return Err("Base URL must start with http:// or https://".into());
        }
        let api_key = if self.api_key.trim().is_empty() {
            if self.mode == FormMode::Edit {
                self.original_key.clone()
            } else {
                return Err("API key cannot be empty".into());
            }
        } else {
            self.api_key.trim().to_string()
        };
        let mut models = Vec::new();
        for draft in &self.models {
            let id = draft.id.trim();
            if id.is_empty() {
                return Err("Each model needs an id".into());
            }
            models.push(ProviderModel {
                id: id.to_string(),
                reasoning: if draft.reasoning_idx == 0 {
                    None
                } else {
                    Some(REASONING_CHOICES[draft.reasoning_idx].to_string())
                },
                no_web_search: draft.no_web_search,
            });
        }
        if models.is_empty() {
            return Err("A provider needs at least one model".into());
        }
        let default_idx = self.default_idx.min(models.len() - 1);
        let mut profile = ProviderProfile::build(alias, self.base_url.trim(), models, api_key);
        profile.default_model = profile.models[default_idx].id.clone();
        if let Some(env_key) = self.original_alias.as_deref().filter(|old| *old == alias) {
            // Keep a user-overridden env_key when the alias did not change.
            if let Ok(existing) = crate::provider::load(env_key) {
                profile.env_key = existing.env_key;
                profile.wire_api = existing.wire_api;
                profile.codex_config = existing.codex_config;
            }
        }
        profile
            .validate()
            .map_err(|err| err.to_string())
            .map(|()| profile)
    }
}

fn reasoning_index(value: Option<&str>) -> usize {
    let Some(value) = value else {
        return 0;
    };
    REASONING_CHOICES
        .iter()
        .position(|choice| *choice == value)
        .unwrap_or(0)
}

fn char_to_byte(s: &str, char_pos: usize) -> usize {
    s.char_indices()
        .nth(char_pos)
        .map(|(byte_idx, _)| byte_idx)
        .unwrap_or(s.len())
}

fn field_value<'a>(
    form: &'a ProviderFormState,
    focus: Focus,
    stored: &'a str,
    secret: bool,
) -> String {
    if form.editing && form.focus == focus {
        if secret {
            format!("{}#", "*".repeat(form.input.chars().count()))
        } else {
            let mut shown = form.input.clone();
            let byte = char_to_byte(&shown, form.cursor);
            shown.insert(byte, '#');
            shown
        }
    } else if secret {
        if stored.is_empty() {
            if form.mode == FormMode::Edit {
                "(keep current)".to_string()
            } else {
                String::new()
            }
        } else {
            "*".repeat(stored.chars().count())
        }
    } else {
        stored.to_string()
    }
}

pub fn render_provider_form(f: &mut Frame, form: &mut ProviderFormState, area: Rect) {
    let key = Style::default().fg(C_YELLOW).add_modifier(Modifier::BOLD);
    let label = Style::default().fg(C_WHITE);
    let dim = Style::default().fg(DIM);
    let header = Style::default().fg(C_CYAN).add_modifier(Modifier::BOLD);
    let focus_style = Style::default()
        .fg(C_CYAN)
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
    let mut lines: Vec<Line<'static>> = Vec::new();

    let alias_style = if form.focus == Focus::Alias {
        focus_style
    } else {
        label
    };
    let url_style = if form.focus == Focus::BaseUrl {
        focus_style
    } else {
        label
    };
    let key_style = if form.focus == Focus::ApiKey {
        focus_style
    } else {
        label
    };

    lines.push(Line::from(vec![
        Span::styled("Alias     ", dim),
        Span::styled(
            field_value(form, Focus::Alias, &form.alias, false),
            alias_style,
        ),
        if form.mode == FormMode::Edit {
            Span::styled("  (rename with n on the list)", dim)
        } else {
            Span::raw("")
        },
    ]));
    lines.push(Line::from(vec![
        Span::styled("Base URL  ", dim),
        Span::styled(
            field_value(form, Focus::BaseUrl, &form.base_url, false),
            url_style,
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled("API key   ", dim),
        Span::styled(
            field_value(form, Focus::ApiKey, &form.api_key, true),
            key_style,
        ),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!("Models ({})", form.models.len()),
        if form.focus == Focus::Models {
            header
        } else {
            header.fg(DIM)
        },
    )));
    for (idx, model) in form.models.iter().enumerate() {
        let selected = form.focus == Focus::Models && idx == form.model_idx;
        let marker = if selected { "▶ " } else { "  " };
        let default = if idx == form.default_idx { " ●" } else { "" };
        let id = if form.editing && selected {
            let mut shown = form.input.clone();
            let byte = char_to_byte(&shown, form.cursor);
            shown.insert(byte, '#');
            shown
        } else if model.id.is_empty() {
            "(id)".to_string()
        } else {
            model.id.clone()
        };
        let reasoning = REASONING_CHOICES[model.reasoning_idx];
        let search = if model.no_web_search {
            "no-search"
        } else {
            "search"
        };
        let row_style = if selected {
            Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD)
        } else {
            label
        };
        lines.push(Line::from(vec![
            Span::styled(marker, Style::default().fg(C_GREEN)),
            Span::styled(format!("{id}{default}"), row_style),
            Span::styled(format!("  {reasoning}  {search}"), dim),
        ]));
    }
    if let Some(error) = &form.error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            error.clone(),
            Style::default().fg(C_RED).add_modifier(Modifier::BOLD),
        )));
    }
    lines.push(Line::from(""));
    if form.editing {
        lines.push(Line::from(vec![
            Span::styled("enter", key),
            Span::styled(" commit  ", dim),
            Span::styled("esc", key),
            Span::styled(" cancel edit", dim),
        ]));
    } else {
        lines.push(Line::from(vec![
            Span::styled("tab", key),
            Span::styled(" field  ", dim),
            Span::styled("enter", key),
            Span::styled(" edit  ", dim),
            Span::styled("+/-", key),
            Span::styled(" model  ", dim),
            Span::styled("←/→", key),
            Span::styled(" reasoning  ", dim),
            Span::styled("w", key),
            Span::styled(" search  ", dim),
            Span::styled("*", key),
            Span::styled(" default  ", dim),
            Span::styled("s", key),
            Span::styled(" save  ", dim),
            Span::styled("esc", key),
            Span::styled(" cancel", dim),
        ]));
    }
    popup::render_popup(f, form.title(), &lines, &mut form.popup, area);
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EnvHome {
        _lock: std::sync::MutexGuard<'static, ()>,
        _dir: tempfile::TempDir,
        prev: Option<std::ffi::OsString>,
    }

    impl EnvHome {
        fn new() -> Self {
            let lock = crate::profile::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let dir = tempfile::tempdir().unwrap();
            let prev = std::env::var_os("CODEX_SWITCH_HOME");
            unsafe {
                std::env::set_var("CODEX_SWITCH_HOME", dir.path());
            }
            Self {
                _lock: lock,
                _dir: dir,
                prev,
            }
        }
    }

    impl Drop for EnvHome {
        fn drop(&mut self) {
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var("CODEX_SWITCH_HOME", v),
                    None => std::env::remove_var("CODEX_SWITCH_HOME"),
                }
            }
        }
    }

    fn type_into(form: &mut ProviderFormState, text: &str) {
        for c in text.chars() {
            form.handle_key(KeyCode::Char(c));
        }
    }

    #[test]
    fn add_form_saves_two_models_with_per_model_settings() {
        let _home = EnvHome::new();
        let mut form = ProviderFormState::add();
        form.handle_key(KeyCode::Enter);
        type_into(&mut form, "openrouter");
        form.handle_key(KeyCode::Enter);
        form.handle_key(KeyCode::Tab);
        form.handle_key(KeyCode::Enter);
        type_into(&mut form, "https://openrouter.ai/api/v1");
        form.handle_key(KeyCode::Enter);
        form.handle_key(KeyCode::Tab);
        form.handle_key(KeyCode::Enter);
        type_into(&mut form, "sk-secret");
        form.handle_key(KeyCode::Enter);
        form.handle_key(KeyCode::Tab);
        form.handle_key(KeyCode::Enter);
        type_into(&mut form, "openai/gpt-5.3-codex");
        form.handle_key(KeyCode::Enter);
        form.handle_key(KeyCode::Char('+'));
        type_into(&mut form, "deepseek/deepseek-r1-0528");
        form.handle_key(KeyCode::Enter);
        form.handle_key(KeyCode::Right);
        form.handle_key(KeyCode::Right);
        form.handle_key(KeyCode::Right);
        form.handle_key(KeyCode::Char('w'));
        form.handle_key(KeyCode::Char('*'));

        let FormOutcome::Saved(profile) = form.handle_key(KeyCode::Char('s')) else {
            panic!("form should save");
        };
        assert_eq!(profile.alias, "openrouter");
        assert_eq!(profile.name, "openrouter");
        assert_eq!(profile.models.len(), 2);
        assert_eq!(profile.models[0].id, "openai/gpt-5.3-codex");
        assert!(profile.models[0].reasoning.is_none());
        assert_eq!(profile.models[1].id, "deepseek/deepseek-r1-0528");
        assert_eq!(profile.models[1].reasoning.as_deref(), Some("medium"));
        assert!(profile.models[1].no_web_search);
        assert_eq!(profile.default_model, "deepseek/deepseek-r1-0528");
        assert_eq!(profile.api_key, "sk-secret");
    }

    #[test]
    fn add_form_rejects_a_bad_base_url_without_closing() {
        let _home = EnvHome::new();
        let mut form = ProviderFormState::add();
        form.handle_key(KeyCode::Enter);
        type_into(&mut form, "r");
        form.handle_key(KeyCode::Enter);
        form.handle_key(KeyCode::Tab);
        form.handle_key(KeyCode::Enter);
        type_into(&mut form, "ftp://nope");
        form.handle_key(KeyCode::Enter);
        form.handle_key(KeyCode::Tab);
        form.handle_key(KeyCode::Enter);
        type_into(&mut form, "sk");
        form.handle_key(KeyCode::Enter);
        form.handle_key(KeyCode::Tab);
        form.handle_key(KeyCode::Enter);
        type_into(&mut form, "m");
        form.handle_key(KeyCode::Enter);
        assert!(matches!(
            form.handle_key(KeyCode::Char('s')),
            FormOutcome::Continue
        ));
        assert!(
            form.error
                .as_deref()
                .is_some_and(|e| e.contains("http://") || e.contains("https://")),
            "error was {:?}",
            form.error
        );
    }

    #[test]
    fn edit_form_keeps_the_key_when_left_blank() {
        let _home = EnvHome::new();
        let original = ProviderProfile::build(
            "keep",
            "https://openrouter.ai/api/v1",
            vec![ProviderModel::from_id("openai/gpt-5.3-codex")],
            "sk-original",
        );
        crate::provider::save(&original).unwrap();
        let mut form = ProviderFormState::edit(&original);
        form.handle_key(KeyCode::Enter);
        for _ in 0..80 {
            form.handle_key(KeyCode::Backspace);
        }
        type_into(&mut form, "https://example.com/v1");
        form.handle_key(KeyCode::Enter);
        let FormOutcome::Saved(profile) = form.handle_key(KeyCode::Char('s')) else {
            panic!("edit should save; error={:?}", form.error);
        };
        assert_eq!(profile.api_key, "sk-original");
        assert_eq!(profile.base_url, "https://example.com/v1");
        assert_eq!(profile.alias, "keep");
    }
}
