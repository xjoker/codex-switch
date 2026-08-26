//! Single-dialog add/edit form for custom API providers.
//!
//! Add starts typing the alias immediately. Enter commits the field and
//! continues into the next one. Tab always moves between fields; `j`/`k`
//! move inside Models, which includes a visible `+ add model` row.
//! `+` adds and `d`/`-` remove from navigation mode. Edit starts on Base URL
//! so `s` is save, not a character.

use crossterm::event::KeyCode;
use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span},
};

use super::popup::{self, PopupState};
use super::theme::{C_GREEN, C_RED, base, dim, header, key};
use crate::provider::{ProviderModel, ProviderProfile};

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
            editing: true,
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
                if self.is_add_row() {
                    self.add_model_and_edit();
                } else {
                    self.begin_edit();
                }
                FormOutcome::Continue
            }
            KeyCode::Tab => {
                self.focus_next();
                if self.mode == FormMode::Add {
                    self.auto_edit_if_typing_field();
                }
                FormOutcome::Continue
            }
            KeyCode::BackTab => {
                self.focus_prev();
                FormOutcome::Continue
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.focus == Focus::Models {
                    self.model_move(1);
                } else {
                    self.focus_next();
                }
                FormOutcome::Continue
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if self.focus == Focus::Models {
                    self.model_move(-1);
                } else {
                    self.focus_prev();
                }
                FormOutcome::Continue
            }
            KeyCode::Char('+') | KeyCode::Char('=') | KeyCode::Char('a') => {
                self.add_model_and_edit();
                FormOutcome::Continue
            }
            KeyCode::Char('-') | KeyCode::Char('_') | KeyCode::Char('d') | KeyCode::Delete => {
                if self.focus == Focus::Models {
                    self.remove_selected_model();
                }
                FormOutcome::Continue
            }
            KeyCode::Char('*') => {
                self.set_default_model();
                FormOutcome::Continue
            }
            KeyCode::Left => {
                self.nudge_reasoning(-1);
                FormOutcome::Continue
            }
            KeyCode::Right => {
                self.nudge_reasoning(1);
                FormOutcome::Continue
            }
            KeyCode::Char('w') => {
                self.toggle_web_search();
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
                let from_models = self.focus == Focus::Models;
                self.commit_edit();
                // Add: Enter walks to the next field so you keep typing.
                // Edit: Enter only commits, so `s` is save rather than a character.
                // Models: after an id is committed, stay in nav for +/- / ←→ / w / * / s.
                let enter_stays = matches!(code, KeyCode::Enter)
                    && (from_models || self.mode == FormMode::Edit);
                if enter_stays {
                    return FormOutcome::Continue;
                }
                self.focus_next();
                self.auto_edit_if_typing_field();
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
        if self.is_add_row() {
            self.add_model_and_edit();
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

    fn add_model_and_edit(&mut self) {
        self.focus = Focus::Models;
        self.models.push(ModelDraft {
            id: String::new(),
            reasoning_idx: 0,
            no_web_search: false,
        });
        self.model_idx = self.models.len() - 1;
        self.input.clear();
        self.cursor = 0;
        self.editing = true;
    }

    fn is_add_row(&self) -> bool {
        self.focus == Focus::Models && self.model_idx >= self.models.len()
    }

    fn remove_selected_model(&mut self) {
        self.focus = Focus::Models;
        if self.is_add_row() {
            return;
        }
        if self.models.len() <= 1 {
            self.error = Some("A provider needs at least one model".into());
            return;
        }
        let removed = self.model_idx;
        self.models.remove(removed);
        if self.default_idx == removed {
            self.default_idx = 0;
        } else if self.default_idx > removed {
            self.default_idx -= 1;
        }
        self.model_idx = self.model_idx.min(self.models.len() - 1);
    }

    fn set_default_model(&mut self) {
        if self.focus != Focus::Models || self.is_add_row() {
            return;
        }
        self.default_idx = self.model_idx;
    }

    fn nudge_reasoning(&mut self, delta: i32) {
        if self.focus != Focus::Models || self.is_add_row() {
            return;
        }
        let idx = &mut self.models[self.model_idx].reasoning_idx;
        let next = *idx as i32 + delta;
        if next >= 0 && (next as usize) < REASONING_CHOICES.len() {
            *idx = next as usize;
        }
    }

    fn toggle_web_search(&mut self) {
        if self.focus != Focus::Models || self.is_add_row() {
            return;
        }
        let model = &mut self.models[self.model_idx];
        model.no_web_search = !model.no_web_search;
    }

    fn auto_edit_if_typing_field(&mut self) {
        match self.focus {
            Focus::Alias | Focus::BaseUrl | Focus::ApiKey => self.begin_edit(),
            Focus::Models => {
                if self.model_idx < self.models.len() && self.models[self.model_idx].id.is_empty() {
                    self.begin_edit();
                }
            }
        }
    }

    fn commit_edit(&mut self) {
        let value = self.input.trim().to_string();
        match self.focus {
            Focus::Alias => self.alias = value,
            Focus::BaseUrl => self.base_url = value,
            Focus::ApiKey => self.api_key = value,
            Focus::Models if self.model_idx < self.models.len() => {
                self.models[self.model_idx].id = value;
            }
            Focus::Models => {}
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

    fn model_move(&mut self, delta: i32) {
        self.focus = Focus::Models;
        let max = self.models.len() as i32; // inclusive: last slot is "+ add model"
        let next = self.model_idx as i32 + delta;
        if next < 0 {
            self.focus_prev();
            self.model_idx = 0;
            return;
        }
        self.model_idx = next.min(max) as usize;
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

pub(crate) fn reasoning_index(value: Option<&str>) -> usize {
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
    let label = base();
    let focus_style = header().add_modifier(ratatui::style::Modifier::UNDERLINED);
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
        Span::styled("Alias     ", dim()),
        Span::styled(
            field_value(form, Focus::Alias, &form.alias, false),
            alias_style,
        ),
        if form.mode == FormMode::Edit {
            Span::styled("  (rename with n on the list)", dim())
        } else {
            Span::styled("", base())
        },
    ]));
    lines.push(Line::from(vec![
        Span::styled("Base URL  ", dim()),
        Span::styled(
            field_value(form, Focus::BaseUrl, &form.base_url, false),
            url_style,
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled("API key   ", dim()),
        Span::styled(
            field_value(form, Focus::ApiKey, &form.api_key, true),
            key_style,
        ),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!("Models ({})", form.models.len()),
        if form.focus == Focus::Models {
            header()
        } else {
            dim()
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
            base().add_modifier(ratatui::style::Modifier::BOLD)
        } else {
            label
        };
        lines.push(Line::from(vec![
            Span::styled(marker, base().fg(C_GREEN)),
            Span::styled(format!("{id}{default}"), row_style),
            Span::styled(format!("  {reasoning}  {search}"), dim()),
        ]));
    }
    let add_selected = form.is_add_row();
    lines.push(Line::from(vec![
        Span::styled(
            if add_selected { "▶ " } else { "  " },
            base().fg(C_GREEN),
        ),
        Span::styled(
            "+ add model",
            if add_selected {
                header()
            } else {
                dim()
            },
        ),
        Span::styled("   d/- remove", dim()),
    ]));
    if let Some(error) = &form.error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            error.clone(),
            base()
                .fg(C_RED)
                .add_modifier(ratatui::style::Modifier::BOLD),
        )));
    }
    lines.push(Line::from(""));
    if form.editing {
        lines.push(Line::from(vec![
            Span::styled("enter", key()),
            Span::styled(" next  ", dim()),
            Span::styled("esc", key()),
            Span::styled(" cancel edit", dim()),
        ]));
    } else {
        lines.push(Line::from(vec![
            Span::styled("tab", key()),
            Span::styled(" field  ", dim()),
            Span::styled("j/k", key()),
            Span::styled(" model  ", dim()),
            Span::styled("enter", key()),
            Span::styled(
                if form.is_add_row() {
                    " add  "
                } else {
                    " edit  "
                },
                dim(),
            ),
            Span::styled("+", key()),
            Span::styled(" add  ", dim()),
            Span::styled("d/-", key()),
            Span::styled(" del  ", dim()),
            Span::styled("←/→", key()),
            Span::styled(" reasoning  ", dim()),
            Span::styled("w", key()),
            Span::styled(" search  ", dim()),
            Span::styled("*", key()),
            Span::styled(" default  ", dim()),
            Span::styled("s", key()),
            Span::styled(" save  ", dim()),
            Span::styled("esc", key()),
            Span::styled(" cancel", dim()),
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
        type_into(&mut form, "openrouter");
        form.handle_key(KeyCode::Enter);
        type_into(&mut form, "https://openrouter.ai/api/v1");
        form.handle_key(KeyCode::Enter);
        type_into(&mut form, "sk-secret");
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
        type_into(&mut form, "r");
        form.handle_key(KeyCode::Enter);
        type_into(&mut form, "ftp://nope");
        form.handle_key(KeyCode::Enter);
        type_into(&mut form, "sk");
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

    #[test]
    fn plus_adds_a_model_from_any_field() {
        let _home = EnvHome::new();
        let original = ProviderProfile::build(
            "demo",
            "https://openrouter.ai/api/v1",
            vec![
                ProviderModel::from_id("minimax/minimax-m3:free"),
                ProviderModel::from_id("liquid/lfm-2.5-2.6b:free"),
            ],
            "sk",
        );
        let mut form = ProviderFormState::edit(&original);
        assert_eq!(form.focus, Focus::BaseUrl);
        assert!(!form.editing);
        form.handle_key(KeyCode::Char('+'));
        assert_eq!(form.models.len(), 3);
        assert_eq!(form.focus, Focus::Models);
        assert!(form.editing, "adding a model must start typing the id");
        type_into(&mut form, "third/model");
        form.handle_key(KeyCode::Enter);
        assert!(!form.editing);
        assert_eq!(form.models[2].id, "third/model");
    }

    #[test]
    fn tab_leaves_models_instead_of_trapping_the_cursor() {
        let original = ProviderProfile::build(
            "demo",
            "https://openrouter.ai/api/v1",
            vec![
                ProviderModel::from_id("a"),
                ProviderModel::from_id("b"),
            ],
            "sk",
        );
        let mut form = ProviderFormState::edit(&original);
        form.handle_key(KeyCode::Tab); // ApiKey
        form.handle_key(KeyCode::Tab); // Models
        assert_eq!(form.focus, Focus::Models);
        let idx = form.model_idx;
        form.handle_key(KeyCode::Tab);
        assert_eq!(form.focus, Focus::BaseUrl);
        assert_eq!(form.model_idx, idx);
        assert!(!form.editing);
    }

    #[test]
    fn enter_on_the_add_row_appends_a_model() {
        let original = ProviderProfile::build(
            "demo",
            "https://openrouter.ai/api/v1",
            vec![ProviderModel::from_id("a"), ProviderModel::from_id("b")],
            "sk",
        );
        let mut form = ProviderFormState::edit(&original);
        form.focus = Focus::Models;
        form.model_idx = form.models.len();
        assert!(form.is_add_row());
        form.handle_key(KeyCode::Enter);
        assert_eq!(form.models.len(), 3);
        assert!(form.editing);
        type_into(&mut form, "c");
        form.handle_key(KeyCode::Enter);
        form.model_idx = 1;
        form.handle_key(KeyCode::Char('d'));
        assert_eq!(form.models.len(), 2);
        assert_eq!(form.models[0].id, "a");
        assert_eq!(form.models[1].id, "c");
    }

    #[test]
    fn form_renders_the_add_model_row() {
        use ratatui::{Terminal, backend::TestBackend};

        let original = ProviderProfile::build(
            "demo",
            "https://openrouter.ai/api/v1",
            vec![ProviderModel::from_id("a")],
            "sk",
        );
        let mut form = ProviderFormState::edit(&original);
        form.focus = Focus::Models;
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| super::render_provider_form(frame, &mut form, frame.area()))
            .unwrap();
        let area = terminal.backend().buffer().area;
        let joined = (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| {
                        terminal
                            .backend()
                            .buffer()
                            .cell((x, y))
                            .expect("cell")
                            .symbol()
                            .to_string()
                    })
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("+ add model"), "{joined}");
        assert!(joined.contains("d/-"), "{joined}");
    }
}
