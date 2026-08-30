//! Single-dialog add/edit form for custom API providers.
//!
//! Add starts typing the alias immediately. Enter commits the field and
//! continues into the next one (Alias → URL → Key → Models). Tab always
//! moves between every field, including env key, wire API, and extra `-c`.
//! `j`/`k` move inside Models, which includes a visible `+ add model` row.
//! Long catalogs pin Alias…Extra and the help line; only the model viewport
//! scrolls, and it follows the cursor.
//! `+` adds and `d`/`-` remove from navigation mode. `f` GETs `{base_url}/models`
//! and fills chat slugs (embedding/reranker omitted). Catalogs larger than 48
//! open a picker (`space` toggle, `/` filter, Enter apply). Edit starts on Base URL
//! so `s` is save, not a character.

use crossterm::event::KeyCode;
use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span},
};

use super::popup::{self, PopupState};
use super::theme::{C_GREEN, C_RED, base, dim, header, key};
use crate::provider::{
    ProviderModel, ProviderProfile, RemoteModel, SMALL_REMOTE_CATALOG_LIMIT, apply_fetched_models,
    apply_picked_models, chat_slugs_from_gateway, fetch_gateway_models_blocking,
};

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
    EnvKey,
    WireApi,
    Extra,
    Models,
}

#[derive(Debug, Clone)]
struct ModelDraft {
    id: String,
    reasoning_idx: usize,
    custom_reasoning: Option<String>,
    no_web_search: bool,
}

fn empty_model_draft() -> ModelDraft {
    ModelDraft {
        id: String::new(),
        reasoning_idx: 0,
        custom_reasoning: None,
        no_web_search: false,
    }
}

struct GatewayPickState {
    slugs: Vec<String>,
    checked: Vec<bool>,
    cursor: usize,
    filter: String,
    filtering: bool,
    message: Option<String>,
}

impl GatewayPickState {
    fn new(slugs: Vec<String>, already: &[ModelDraft]) -> Self {
        let checked = slugs
            .iter()
            .map(|slug| already.iter().any(|draft| draft.id.trim() == slug))
            .collect();
        Self {
            slugs,
            checked,
            cursor: 0,
            filter: String::new(),
            filtering: false,
            message: None,
        }
    }

    fn filtered(&self) -> Vec<usize> {
        let query = self.filter.to_ascii_lowercase();
        self.slugs
            .iter()
            .enumerate()
            .filter(|(_, slug)| query.is_empty() || slug.to_ascii_lowercase().contains(&query))
            .map(|(idx, _)| idx)
            .collect()
    }

    fn selected_count(&self) -> usize {
        self.checked.iter().filter(|checked| **checked).count()
    }

    fn clamp_cursor(&mut self) {
        let n = self.filtered().len();
        if n == 0 {
            self.cursor = 0;
        } else if self.cursor >= n {
            self.cursor = n - 1;
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        let n = self.filtered().len() as isize;
        if n == 0 {
            self.cursor = 0;
            return;
        }
        let next = (self.cursor as isize + delta).clamp(0, n - 1);
        self.cursor = next as usize;
    }
}

pub(crate) fn reasoning_choice(value: Option<&str>) -> (usize, Option<String>) {
    let Some(value) = value.filter(|v| !v.is_empty()) else {
        return (0, None);
    };
    match REASONING_CHOICES.iter().position(|choice| *choice == value) {
        Some(idx) => (idx, None),
        None => (0, Some(value.to_string())),
    }
}

fn draft_reasoning_label(draft: &ModelDraft) -> &str {
    draft
        .custom_reasoning
        .as_deref()
        .unwrap_or(REASONING_CHOICES[draft.reasoning_idx])
}

pub struct ProviderFormState {
    pub mode: FormMode,
    pub popup: PopupState,
    focus: Focus,
    editing: bool,
    alias: String,
    base_url: String,
    api_key: String,
    original_key: String,
    metadata_fallback: String,
    env_key: String,
    wire_api: String,
    extra_sets: String,
    models: Vec<ModelDraft>,
    model_idx: usize,
    model_scroll: usize,
    default_idx: usize,
    input: String,
    cursor: usize,
    error: Option<String>,
    confirm_remove: bool,
    pick: Option<GatewayPickState>,
    fetched_catalog: Option<Vec<RemoteModel>>,
}

pub enum FormOutcome {
    Continue,
    Cancel,
    Saved {
        profile: Box<ProviderProfile>,
        fetched_catalog: Option<Vec<RemoteModel>>,
    },
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
            original_key: String::new(),
            metadata_fallback: String::new(),
            env_key: String::new(),
            wire_api: "responses".to_string(),
            extra_sets: String::new(),
            models: vec![empty_model_draft()],
            model_idx: 0,
            model_scroll: 0,
            default_idx: 0,
            input: String::new(),
            cursor: 0,
            error: None,
            confirm_remove: false,
            pick: None,
            fetched_catalog: None,
        }
    }

    pub fn edit(profile: &ProviderProfile) -> Self {
        let models: Vec<ModelDraft> = profile
            .models
            .iter()
            .map(|model| {
                let (reasoning_idx, custom_reasoning) =
                    reasoning_choice(model.reasoning.as_deref());
                ModelDraft {
                    id: model.id.clone(),
                    reasoning_idx,
                    custom_reasoning,
                    no_web_search: model.no_web_search,
                }
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
            original_key: profile.api_key.clone(),
            metadata_fallback: profile.metadata_fallback.clone(),
            env_key: profile.env_key.clone(),
            wire_api: profile.wire_api.clone(),
            extra_sets: profile.codex_config.join(", "),
            models: if models.is_empty() {
                vec![empty_model_draft()]
            } else {
                models
            },
            model_idx: default_idx.min(profile.models.len().saturating_sub(1)),
            model_scroll: 0,
            default_idx,
            input: String::new(),
            cursor: 0,
            error: None,
            confirm_remove: false,
            pick: None,
            fetched_catalog: None,
        }
    }

    pub fn title(&self) -> &'static str {
        match self.mode {
            FormMode::Add => "Add provider",
            FormMode::Edit => "Edit provider",
        }
    }

    pub fn handle_key(&mut self, code: KeyCode) -> FormOutcome {
        if self.confirm_remove {
            return self.handle_remove_confirm(code);
        }
        if self.pick.is_some() {
            return self.handle_pick_key(code);
        }
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
                    self.request_remove_model();
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
            KeyCode::Char('f') => {
                self.fetch_from_gateway();
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
                // Add Enter: Alias → URL → Key → Models, skipping env/wire/extra.
                // Tab (and Edit) visit every field, including those extras.
                // Models: after an id is committed, stay in nav for +/- / ←→ / w / * / s.
                let enter_stays =
                    matches!(code, KeyCode::Enter) && (from_models || self.mode == FormMode::Edit);
                if enter_stays {
                    return FormOutcome::Continue;
                }
                if matches!(code, KeyCode::Enter) && self.mode == FormMode::Add {
                    self.focus_next_add_enter();
                } else {
                    self.focus_next();
                }
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
            Focus::EnvKey => self.env_key.clone(),
            Focus::WireApi => self.wire_api.clone(),
            Focus::Extra => self.extra_sets.clone(),
            Focus::Models => self.models[self.model_idx].id.clone(),
        };
        self.input = value;
        self.cursor = self.input.chars().count();
        self.editing = true;
    }

    fn add_model_and_edit(&mut self) {
        self.focus = Focus::Models;
        self.models.push(empty_model_draft());
        self.model_idx = self.models.len() - 1;
        self.input.clear();
        self.cursor = 0;
        self.editing = true;
    }

    fn is_add_row(&self) -> bool {
        self.focus == Focus::Models && self.model_idx >= self.models.len()
    }

    fn request_remove_model(&mut self) {
        if self.is_add_row() {
            return;
        }
        if self.models.len() <= 1 {
            self.error = Some("A provider needs at least one model".into());
            return;
        }
        self.confirm_remove = true;
    }

    fn handle_remove_confirm(&mut self, code: KeyCode) -> FormOutcome {
        match code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                self.confirm_remove = false;
                self.remove_selected_model();
            }
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                self.confirm_remove = false;
            }
            _ => {}
        }
        FormOutcome::Continue
    }

    fn remove_label(&self) -> String {
        let id = self
            .models
            .get(self.model_idx)
            .map(|m| m.id.trim())
            .filter(|id| !id.is_empty())
            .unwrap_or("this model");
        format!("Remove model '{id}'?")
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
        let draft = &mut self.models[self.model_idx];
        if draft.custom_reasoning.take().is_some() {
            if delta > 0 {
                draft.reasoning_idx = 0;
            } else {
                draft.reasoning_idx = REASONING_CHOICES.len() - 1;
            }
            return;
        }
        let idx = &mut draft.reasoning_idx;
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

    fn fetch_from_gateway(&mut self) {
        let url = self.base_url.trim();
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            self.error = Some("Base URL must start with http:// or https://".into());
            return;
        }
        let key = if !self.api_key.trim().is_empty() {
            self.api_key.trim()
        } else if self.mode == FormMode::Edit && !self.original_key.trim().is_empty() {
            self.original_key.trim()
        } else {
            self.error = Some("API key cannot be empty".into());
            return;
        };
        match fetch_gateway_models_blocking(url, key) {
            Ok(remote) => self.ingest_remote(&remote),
            Err(err) => self.error = Some(format!("Fetch models failed: {err}")),
        }
    }

    fn ingest_remote(&mut self, remote: &[RemoteModel]) {
        let slugs = match chat_slugs_from_gateway(remote) {
            Ok(slugs) => slugs,
            Err(err) => {
                self.error = Some(err.to_string());
                return;
            }
        };
        if slugs.len() > SMALL_REMOTE_CATALOG_LIMIT {
            self.fetched_catalog = Some(remote.to_vec());
            self.pick = Some(GatewayPickState::new(slugs, &self.models));
            self.error = None;
            self.popup.reset();
            return;
        }
        if let Err(err) = self.apply_fetched(remote) {
            self.error = Some(err);
        }
    }

    fn handle_pick_key(&mut self, code: KeyCode) -> FormOutcome {
        let filtering = self.pick.as_ref().is_some_and(|pick| pick.filtering);
        if filtering {
            match code {
                KeyCode::Esc => {
                    if let Some(pick) = self.pick.as_mut() {
                        pick.filtering = false;
                    }
                }
                KeyCode::Enter => {
                    if let Some(pick) = self.pick.as_mut() {
                        pick.filtering = false;
                        pick.clamp_cursor();
                    }
                }
                KeyCode::Backspace => {
                    if let Some(pick) = self.pick.as_mut() {
                        pick.filter.pop();
                        pick.clamp_cursor();
                    }
                }
                KeyCode::Char(ch) => {
                    if let Some(pick) = self.pick.as_mut() {
                        pick.filter.push(ch);
                        pick.cursor = 0;
                    }
                }
                _ => {}
            }
            return FormOutcome::Continue;
        }
        match code {
            KeyCode::Esc => {
                self.pick = None;
                self.fetched_catalog = None;
            }
            KeyCode::Char('/') => {
                if let Some(pick) = self.pick.as_mut() {
                    pick.filtering = true;
                    pick.filter.clear();
                    pick.cursor = 0;
                    pick.message = None;
                }
            }
            KeyCode::Char(' ') => self.toggle_pick(),
            KeyCode::Enter => self.apply_pick(),
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(pick) = self.pick.as_mut() {
                    pick.move_cursor(1);
                    pick.message = None;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(pick) = self.pick.as_mut() {
                    pick.move_cursor(-1);
                    pick.message = None;
                }
            }
            KeyCode::PageDown => {
                if let Some(pick) = self.pick.as_mut() {
                    pick.move_cursor(10);
                    pick.message = None;
                }
            }
            KeyCode::PageUp => {
                if let Some(pick) = self.pick.as_mut() {
                    pick.move_cursor(-10);
                    pick.message = None;
                }
            }
            KeyCode::Home => {
                if let Some(pick) = self.pick.as_mut() {
                    pick.cursor = 0;
                    pick.message = None;
                }
            }
            KeyCode::End => {
                if let Some(pick) = self.pick.as_mut() {
                    let last = pick.filtered().len().saturating_sub(1);
                    pick.cursor = last;
                    pick.message = None;
                }
            }
            _ => {}
        }
        FormOutcome::Continue
    }

    fn toggle_pick(&mut self) {
        let Some(pick) = self.pick.as_mut() else {
            return;
        };
        pick.message = None;
        let filtered = pick.filtered();
        let Some(&idx) = filtered.get(pick.cursor) else {
            return;
        };
        pick.checked[idx] = !pick.checked[idx];
    }

    fn apply_pick(&mut self) {
        let Some(pick) = self.pick.as_ref() else {
            return;
        };
        let allowed = pick.slugs.clone();
        let picks: Vec<ProviderModel> = pick
            .slugs
            .iter()
            .zip(pick.checked.iter())
            .filter(|(_, checked)| **checked)
            .map(|(slug, _)| ProviderModel::from_id(slug.clone()))
            .collect();
        if picks.is_empty() {
            if let Some(pick) = self.pick.as_mut() {
                pick.message = Some("pick at least one model".into());
            }
            return;
        }
        let existing = self.existing_models();
        let current_default = self.current_default_id();
        match apply_picked_models(&existing, current_default.as_deref(), &allowed, &picks) {
            Ok((models, default)) => {
                self.set_saved_models(models, default);
                self.pick = None;
            }
            Err(err) => {
                if let Some(pick) = self.pick.as_mut() {
                    pick.message = Some(err.to_string());
                }
            }
        }
    }

    fn existing_models(&self) -> Vec<ProviderModel> {
        self.models
            .iter()
            .filter(|draft| !draft.id.trim().is_empty())
            .map(|draft| ProviderModel {
                id: draft.id.trim().to_string(),
                reasoning: if let Some(custom) = &draft.custom_reasoning {
                    Some(custom.clone())
                } else if draft.reasoning_idx == 0 {
                    None
                } else {
                    Some(REASONING_CHOICES[draft.reasoning_idx].to_string())
                },
                no_web_search: draft.no_web_search,
            })
            .collect()
    }

    fn current_default_id(&self) -> Option<String> {
        self.models
            .get(self.default_idx)
            .map(|draft| draft.id.trim().to_string())
            .filter(|id| !id.is_empty())
    }

    fn apply_fetched(&mut self, remote: &[RemoteModel]) -> Result<(), String> {
        let existing = self.existing_models();
        let current_default = self.current_default_id();
        let (models, default) =
            apply_fetched_models(&existing, current_default.as_deref(), remote, &[])
                .map_err(|err| err.to_string())?;
        self.set_saved_models(models, default);
        self.fetched_catalog = Some(remote.to_vec());
        Ok(())
    }

    fn set_saved_models(&mut self, models: Vec<ProviderModel>, default: String) {
        self.models = models
            .iter()
            .map(|model| {
                let (reasoning_idx, custom_reasoning) =
                    reasoning_choice(model.reasoning.as_deref());
                ModelDraft {
                    id: model.id.clone(),
                    reasoning_idx,
                    custom_reasoning,
                    no_web_search: model.no_web_search,
                }
            })
            .collect();
        self.default_idx = self
            .models
            .iter()
            .position(|draft| draft.id == default)
            .unwrap_or(0);
        self.model_idx = self.default_idx;
        self.focus = Focus::Models;
        self.editing = false;
        self.input.clear();
        self.cursor = 0;
        self.error = None;
    }

    fn auto_edit_if_typing_field(&mut self) {
        match self.focus {
            Focus::Alias
            | Focus::BaseUrl
            | Focus::ApiKey
            | Focus::EnvKey
            | Focus::WireApi
            | Focus::Extra => self.begin_edit(),
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
            Focus::BaseUrl => {
                if self.base_url != value {
                    self.fetched_catalog = None;
                }
                self.base_url = value;
            }
            Focus::ApiKey => {
                if self.api_key != value {
                    self.fetched_catalog = None;
                }
                self.api_key = value;
            }
            Focus::EnvKey => self.env_key = value,
            Focus::WireApi => self.wire_api = value,
            Focus::Extra => self.extra_sets = value,
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
            (_, Focus::ApiKey) => Focus::EnvKey,
            (_, Focus::EnvKey) => Focus::WireApi,
            (_, Focus::WireApi) => Focus::Extra,
            (_, Focus::Extra) => Focus::Models,
            (_, Focus::Models) if self.mode == FormMode::Add => Focus::Alias,
            (_, Focus::Models) => Focus::BaseUrl,
            (FormMode::Edit, Focus::Alias) => Focus::BaseUrl,
        };
    }

    /// Add's Enter path keeps Alias → URL → Key → Models. Tab still visits
    /// env key / wire API / extra `-c`.
    fn focus_next_add_enter(&mut self) {
        if self.focus == Focus::ApiKey {
            self.focus = Focus::Models;
        } else {
            self.focus_next();
        }
    }

    fn focus_prev(&mut self) {
        self.focus = match (self.mode, self.focus) {
            (FormMode::Add, Focus::Alias) => Focus::Models,
            (_, Focus::BaseUrl) if self.mode == FormMode::Add => Focus::Alias,
            (_, Focus::BaseUrl) => Focus::Models,
            (_, Focus::ApiKey) => Focus::BaseUrl,
            (_, Focus::EnvKey) => Focus::ApiKey,
            (_, Focus::WireApi) => Focus::EnvKey,
            (_, Focus::Extra) => Focus::WireApi,
            (_, Focus::Models) => Focus::Extra,
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
            Ok(profile) => FormOutcome::Saved {
                profile: Box::new(profile),
                fetched_catalog: self.fetched_catalog.clone(),
            },
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
                reasoning: if let Some(custom) = &draft.custom_reasoning {
                    Some(custom.clone())
                } else if draft.reasoning_idx == 0 {
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
        profile.metadata_fallback = self.metadata_fallback.clone();
        let env_key = self.env_key.trim();
        if !env_key.is_empty() {
            profile.env_key = env_key.to_string();
        }
        let wire_api = self.wire_api.trim();
        if !wire_api.is_empty() {
            profile.wire_api = wire_api.to_string();
        }
        profile.codex_config = parse_extra_sets(&self.extra_sets);
        profile
            .validate()
            .map_err(|err| err.to_string())
            .map(|()| profile)
    }
}

fn parse_extra_sets(raw: &str) -> Vec<String> {
    let mut entries = Vec::new();
    let mut current = String::new();
    for part in raw.split(',') {
        let trimmed = part.trim();
        if current.is_empty() {
            if !trimmed.is_empty() {
                current = trimmed.to_string();
            }
            continue;
        }
        if looks_like_override_start(trimmed) {
            entries.push(std::mem::take(&mut current));
            current = trimmed.to_string();
        } else {
            current.push(',');
            current.push_str(part);
        }
    }
    if !current.is_empty() {
        entries.push(current);
    }
    entries
}

fn looks_like_override_start(s: &str) -> bool {
    let Some((key, _)) = s.split_once('=') else {
        return false;
    };
    let mut chars = key.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
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

const PICK_LIST_ROWS: usize = 12;

/// Keep `cursor` inside a `vis`-row window of a `len`-item list.
fn clamp_list_scroll(scroll: usize, cursor: usize, len: usize, vis: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let vis = vis.max(1).min(len);
    let max_scroll = len - vis;
    let scroll = scroll.min(max_scroll);
    if cursor < scroll {
        cursor
    } else if cursor >= scroll + vis {
        cursor.saturating_add(1).saturating_sub(vis).min(max_scroll)
    } else {
        scroll
    }
}

fn list_viewport_rows(inner_h: usize, header_len: usize, footer_len: usize) -> usize {
    inner_h
        .saturating_sub(header_len)
        .saturating_sub(footer_len)
        .max(1)
}

fn render_pick(f: &mut Frame, form: &mut ProviderFormState, area: Rect) {
    let Some(pick) = form.pick.as_ref() else {
        return;
    };
    let filtered = pick.filtered();
    let mut lines = Vec::new();
    lines.push(Line::from(Span::styled(
        format!(
            "{} chat models   {} selected",
            pick.slugs.len(),
            pick.selected_count()
        ),
        dim(),
    )));
    let filter_shown = if pick.filtering {
        format!("{}$", pick.filter)
    } else if pick.filter.is_empty() {
        "(none)".to_string()
    } else {
        pick.filter.clone()
    };
    lines.push(Line::from(vec![
        Span::styled("filter  ", dim()),
        Span::styled(filter_shown, if pick.filtering { header() } else { base() }),
    ]));
    lines.push(Line::from(""));
    if filtered.is_empty() {
        lines.push(Line::from(Span::styled("no matches", dim())));
    } else {
        let start = pick
            .cursor
            .saturating_sub(PICK_LIST_ROWS / 2)
            .min(filtered.len().saturating_sub(PICK_LIST_ROWS));
        let end = (start + PICK_LIST_ROWS).min(filtered.len());
        for (vis, &idx) in filtered.iter().enumerate().take(end).skip(start) {
            let selected = vis == pick.cursor;
            let mark = if pick.checked[idx] { "[x]" } else { "[ ]" };
            let marker = if selected { "▶ " } else { "  " };
            let style = if selected {
                base().add_modifier(ratatui::style::Modifier::BOLD)
            } else {
                dim()
            };
            lines.push(Line::from(vec![
                Span::styled(marker, base().fg(C_GREEN)),
                Span::styled(format!("{mark} {}", pick.slugs[idx]), style),
            ]));
        }
    }
    if let Some(message) = &pick.message {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            message.clone(),
            base()
                .fg(C_RED)
                .add_modifier(ratatui::style::Modifier::BOLD),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("space", key()),
        Span::styled(" toggle  ", dim()),
        Span::styled("enter", key()),
        Span::styled(" apply  ", dim()),
        Span::styled("/", key()),
        Span::styled(" filter  ", dim()),
        Span::styled("j/k", key()),
        Span::styled(" move  ", dim()),
        Span::styled("esc", key()),
        Span::styled(" cancel", dim()),
    ]));
    popup::render_popup(f, "Pick models", &lines, &mut form.popup, area);
}

pub fn render_provider_form(f: &mut Frame, form: &mut ProviderFormState, area: Rect) {
    if form.pick.is_some() {
        render_pick(f, form, area);
        return;
    }
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
    let env_style = if form.focus == Focus::EnvKey {
        focus_style
    } else {
        label
    };
    let wire_style = if form.focus == Focus::WireApi {
        focus_style
    } else {
        label
    };
    let extra_style = if form.focus == Focus::Extra {
        focus_style
    } else {
        label
    };
    lines.push(Line::from(vec![
        Span::styled("Env key   ", dim()),
        Span::styled(
            field_value(form, Focus::EnvKey, &form.env_key, false),
            env_style,
        ),
        if form.env_key.trim().is_empty() {
            Span::styled("  (default from alias)", dim())
        } else {
            Span::styled("", base())
        },
    ]));
    lines.push(Line::from(vec![
        Span::styled("Wire API  ", dim()),
        Span::styled(
            field_value(form, Focus::WireApi, &form.wire_api, false),
            wire_style,
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled("Extra -c  ", dim()),
        Span::styled(
            field_value(form, Focus::Extra, &form.extra_sets, false),
            extra_style,
        ),
        Span::styled("  (KEY=VALUE, comma-separated)", dim()),
    ]));
    lines.push(Line::from(""));

    let mut footer: Vec<Line<'static>> = Vec::new();
    if let Some(error) = &form.error {
        footer.push(Line::from(""));
        footer.push(Line::from(Span::styled(
            error.clone(),
            base()
                .fg(C_RED)
                .add_modifier(ratatui::style::Modifier::BOLD),
        )));
    }
    footer.push(Line::from(""));
    if form.editing {
        footer.push(Line::from(vec![
            Span::styled("enter", key()),
            Span::styled(" next  ", dim()),
            Span::styled("esc", key()),
            Span::styled(" cancel edit", dim()),
        ]));
    } else {
        footer.push(Line::from(vec![
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
            Span::styled("f", key()),
            Span::styled(" fetch  ", dim()),
            Span::styled("s", key()),
            Span::styled(" save  ", dim()),
            Span::styled("esc", key()),
            Span::styled(" cancel", dim()),
        ]));
    }

    let heading_style = if form.focus == Focus::Models {
        header()
    } else {
        dim()
    };
    let list_len = form.models.len() + 1;
    let inner_h = popup::max_inner_height(area) as usize;
    let vis = list_viewport_rows(inner_h, lines.len() + 1, footer.len());
    if form.focus == Focus::Models {
        form.model_scroll = clamp_list_scroll(form.model_scroll, form.model_idx, list_len, vis);
    } else {
        form.model_scroll = form.model_scroll.min(list_len.saturating_sub(vis));
    }
    let start = form.model_scroll;
    let end = (start + vis).min(list_len);
    let mut heading = vec![Span::styled(
        format!("Models ({})", form.models.len()),
        heading_style,
    )];
    if list_len > vis {
        let pos = if form.model_idx >= form.models.len() {
            "+".to_string()
        } else {
            format!("{}/{}", form.model_idx + 1, form.models.len())
        };
        heading.push(Span::styled(format!("  {pos}"), dim()));
    }
    lines.push(Line::from(heading));
    for idx in start..end {
        if idx < form.models.len() {
            let model = &form.models[idx];
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
            let reasoning = draft_reasoning_label(model);
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
        } else {
            let add_selected = form.is_add_row();
            lines.push(Line::from(vec![
                Span::styled(if add_selected { "▶ " } else { "  " }, base().fg(C_GREEN)),
                Span::styled("+ add model", if add_selected { header() } else { dim() }),
                Span::styled("   d/- remove   f fetch", dim()),
            ]));
        }
    }
    lines.extend(footer);
    form.popup.scroll = 0;
    popup::render_popup(f, form.title(), &lines, &mut form.popup, area);
    if form.confirm_remove {
        let confirm_lines = vec![
            Line::from(Span::styled(
                form.remove_label(),
                base()
                    .fg(C_RED)
                    .add_modifier(ratatui::style::Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled("y", key()),
                Span::styled(" remove  ", dim()),
                Span::styled("n/esc", key()),
                Span::styled(" keep it", dim()),
            ]),
        ];
        let mut confirm_popup = PopupState::new();
        popup::render_popup(f, "Confirm", &confirm_lines, &mut confirm_popup, area);
    }
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

        let FormOutcome::Saved { profile, .. } = form.handle_key(KeyCode::Char('s')) else {
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
        let FormOutcome::Saved { profile, .. } = form.handle_key(KeyCode::Char('s')) else {
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
            vec![ProviderModel::from_id("a"), ProviderModel::from_id("b")],
            "sk",
        );
        let mut form = ProviderFormState::edit(&original);
        form.handle_key(KeyCode::Tab); // ApiKey
        form.handle_key(KeyCode::Tab); // EnvKey
        form.handle_key(KeyCode::Tab); // WireApi
        form.handle_key(KeyCode::Tab); // Extra
        form.handle_key(KeyCode::Tab); // Models
        assert_eq!(form.focus, Focus::Models);
        let idx = form.model_idx;
        form.handle_key(KeyCode::Tab);
        assert_eq!(form.focus, Focus::BaseUrl);
        assert_eq!(form.model_idx, idx);
        assert!(!form.editing);
    }

    #[test]
    fn add_form_enter_skips_optional_fields_and_tab_visits_them() {
        let _home = EnvHome::new();
        let mut form = ProviderFormState::add();
        type_into(&mut form, "or");
        form.handle_key(KeyCode::Enter);
        type_into(&mut form, "https://example.com/v1");
        form.handle_key(KeyCode::Enter);
        type_into(&mut form, "sk");
        form.handle_key(KeyCode::Enter);
        assert_eq!(
            form.focus,
            Focus::Models,
            "Enter after API key must land on Models"
        );

        let mut tab = ProviderFormState::add();
        type_into(&mut tab, "or");
        tab.handle_key(KeyCode::Enter);
        type_into(&mut tab, "https://example.com/v1");
        tab.handle_key(KeyCode::Enter);
        type_into(&mut tab, "sk");
        tab.handle_key(KeyCode::Tab);
        assert_eq!(
            tab.focus,
            Focus::EnvKey,
            "Tab after API key must visit Env key"
        );
        tab.handle_key(KeyCode::Tab);
        assert_eq!(tab.focus, Focus::WireApi);
        tab.handle_key(KeyCode::Tab);
        assert_eq!(tab.focus, Focus::Extra);
        type_into(&mut tab, "temperature=0, foo=a, b");
        tab.handle_key(KeyCode::Tab);
        assert_eq!(tab.focus, Focus::Models);
        type_into(&mut tab, "m");
        tab.handle_key(KeyCode::Enter);
        let FormOutcome::Saved { profile, .. } = tab.handle_key(KeyCode::Char('s')) else {
            panic!("add should save; error={:?}", tab.error);
        };
        assert_eq!(profile.env_key, "CODEX_SWITCH_OR_KEY");
        assert_eq!(profile.wire_api, "responses");
        assert_eq!(
            profile.codex_config,
            ["temperature=0".to_string(), "foo=a, b".to_string()]
        );
    }

    #[test]
    fn parse_extra_sets_keeps_commas_inside_a_value() {
        assert_eq!(
            super::parse_extra_sets("temperature=0, foo=a, b, bar=1"),
            ["temperature=0", "foo=a, b", "bar=1"]
        );
        assert_eq!(super::parse_extra_sets("  "), Vec::<String>::new());
        assert_eq!(
            super::parse_extra_sets("sandbox_permissions=[\"a\",\"b\"]"),
            [r#"sandbox_permissions=["a","b"]"#]
        );
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
        assert_eq!(form.models.len(), 3, "d must wait for confirmation");
        form.handle_key(KeyCode::Char('y'));
        assert_eq!(form.models.len(), 2);
        assert_eq!(form.models[0].id, "a");
        assert_eq!(form.models[1].id, "c");
    }

    #[test]
    fn removing_a_model_asks_before_deleting() {
        let original = ProviderProfile::build(
            "demo",
            "https://openrouter.ai/api/v1",
            vec![
                ProviderModel::from_id("keep"),
                ProviderModel::from_id("drop"),
            ],
            "sk",
        );
        let mut form = ProviderFormState::edit(&original);
        form.focus = Focus::Models;
        form.model_idx = 1;
        form.handle_key(KeyCode::Char('d'));
        assert!(form.confirm_remove);
        assert_eq!(form.models.len(), 2);

        form.handle_key(KeyCode::Char('n'));
        assert!(!form.confirm_remove);
        assert_eq!(form.models.len(), 2);
        assert_eq!(form.models[1].id, "drop");

        form.handle_key(KeyCode::Char('d'));
        form.handle_key(KeyCode::Esc);
        assert!(!form.confirm_remove);
        assert_eq!(form.models.len(), 2);

        form.handle_key(KeyCode::Char('-'));
        form.handle_key(KeyCode::Char('y'));
        assert_eq!(form.models.len(), 1);
        assert_eq!(form.models[0].id, "keep");
        assert!(!form.confirm_remove);
    }

    #[test]
    fn last_model_cannot_be_removed_even_with_confirm() {
        let original = ProviderProfile::build(
            "demo",
            "https://openrouter.ai/api/v1",
            vec![ProviderModel::from_id("only")],
            "sk",
        );
        let mut form = ProviderFormState::edit(&original);
        form.focus = Focus::Models;
        form.handle_key(KeyCode::Char('d'));
        assert!(!form.confirm_remove);
        assert_eq!(form.models.len(), 1);
        assert!(
            form.error
                .as_deref()
                .is_some_and(|e| e.contains("at least one model")),
            "error was {:?}",
            form.error
        );
    }

    #[test]
    fn confirm_remove_popup_names_the_model() {
        use ratatui::{Terminal, backend::TestBackend};

        let original = ProviderProfile::build(
            "demo",
            "https://openrouter.ai/api/v1",
            vec![
                ProviderModel::from_id("keep"),
                ProviderModel::from_id("drop"),
            ],
            "sk",
        );
        let mut form = ProviderFormState::edit(&original);
        form.focus = Focus::Models;
        form.model_idx = 1;
        form.handle_key(KeyCode::Char('d'));
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
        assert!(joined.contains("Confirm"), "{joined}");
        assert!(joined.contains("Remove model 'drop'?"), "{joined}");
        assert!(joined.contains("y") && joined.contains("n/esc"), "{joined}");
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

    fn numbered_models(n: usize) -> Vec<ProviderModel> {
        (0..n)
            .map(|i| ProviderModel::from_id(format!("model-{i:02}")))
            .collect()
    }

    fn render_form_text(form: &mut ProviderFormState, width: u16, height: u16) -> String {
        use ratatui::{Terminal, backend::TestBackend};

        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| super::render_provider_form(frame, form, frame.area()))
            .unwrap();
        let area = terminal.backend().buffer().area;
        (0..area.height)
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
            .join("\n")
    }

    #[test]
    fn clamp_list_scroll_keeps_the_cursor_inside_the_window() {
        assert_eq!(clamp_list_scroll(0, 0, 10, 3), 0);
        assert_eq!(clamp_list_scroll(0, 2, 10, 3), 0);
        assert_eq!(clamp_list_scroll(0, 3, 10, 3), 1);
        assert_eq!(clamp_list_scroll(0, 9, 10, 3), 7);
        assert_eq!(clamp_list_scroll(5, 2, 10, 3), 2);
        assert_eq!(clamp_list_scroll(100, 0, 10, 3), 0);
        assert_eq!(clamp_list_scroll(4, 4, 4, 10), 0);
        assert_eq!(clamp_list_scroll(0, 0, 0, 3), 0);
    }

    #[test]
    fn list_viewport_rows_leaves_at_least_one_row() {
        assert_eq!(list_viewport_rows(20, 8, 2), 10);
        assert_eq!(list_viewport_rows(8, 8, 2), 1);
    }

    #[test]
    fn long_model_list_follows_the_cursor_and_keeps_the_form_header() {
        let original = ProviderProfile::build(
            "demo",
            "https://openrouter.ai/api/v1",
            numbered_models(47),
            "sk",
        );
        let mut form = ProviderFormState::edit(&original);
        form.focus = Focus::Models;
        form.model_idx = 30;

        let joined = render_form_text(&mut form, 80, 24);
        assert!(joined.contains("Alias"), "{joined}");
        assert!(joined.contains("Base URL"), "{joined}");
        assert!(joined.contains("model-30"), "{joined}");
        assert!(joined.contains("31/47"), "{joined}");
        assert!(
            !joined.contains("model-00"),
            "top of the list must scroll away\n{joined}"
        );
        assert!(
            !joined.contains("model-46"),
            "bottom of the list stays below the viewport\n{joined}"
        );
        assert!(joined.contains("tab"), "{joined}");
        assert!(joined.contains("j/k"), "{joined}");
    }

    #[test]
    fn long_model_list_shows_the_add_row_when_selected() {
        let original = ProviderProfile::build(
            "demo",
            "https://openrouter.ai/api/v1",
            numbered_models(47),
            "sk",
        );
        let mut form = ProviderFormState::edit(&original);
        form.focus = Focus::Models;
        form.model_idx = form.models.len();

        let joined = render_form_text(&mut form, 80, 24);
        assert!(joined.contains("Alias"), "{joined}");
        assert!(joined.contains("+ add model"), "{joined}");
        assert!(joined.contains("Models (47)  +"), "{joined}");
        assert!(
            !joined.contains("model-00"),
            "add row must scroll the list\n{joined}"
        );
    }

    #[test]
    fn long_model_list_keeps_the_first_row_at_the_top() {
        let original = ProviderProfile::build(
            "demo",
            "https://openrouter.ai/api/v1",
            numbered_models(47),
            "sk",
        );
        let mut form = ProviderFormState::edit(&original);
        form.focus = Focus::Models;
        form.model_idx = 0;

        let joined = render_form_text(&mut form, 80, 24);
        assert!(joined.contains("model-00"), "{joined}");
        assert!(
            !joined.contains("+ add model"),
            "add row stays below a top-aligned window\n{joined}"
        );
    }

    #[test]
    fn edit_form_keeps_custom_reasoning_and_extra_overrides() {
        let _home = EnvHome::new();
        let mut original = ProviderProfile::build(
            "keep",
            "https://openrouter.ai/api/v1",
            vec![ProviderModel {
                id: "m".into(),
                reasoning: Some("custom-effort".into()),
                no_web_search: false,
            }],
            "sk-original",
        );
        original.env_key = "MY_CUSTOM_KEY".into();
        original.metadata_fallback = "none".into();
        original.wire_api = "responses".into();
        original.codex_config = vec!["temperature=0".into(), "foo=bar".into()];
        crate::provider::save(&original).unwrap();

        let mut form = ProviderFormState::edit(&original);
        let FormOutcome::Saved { profile, .. } = form.handle_key(KeyCode::Char('s')) else {
            panic!("edit should save; error={:?}", form.error);
        };
        assert_eq!(
            profile.models[0].reasoning.as_deref(),
            Some("custom-effort")
        );
        assert_eq!(profile.env_key, "MY_CUSTOM_KEY");
        assert_eq!(profile.metadata_fallback, "none");
        assert_eq!(profile.wire_api, "responses");
        assert_eq!(profile.codex_config, ["temperature=0", "foo=bar"]);
    }

    #[test]
    fn fetch_fills_chat_slugs_and_drops_embeddings() {
        let original = ProviderProfile::build(
            "demo",
            "https://example.test/v1",
            vec![ProviderModel::from_id("composer-2.5")],
            "sk",
        );
        let mut form = ProviderFormState::edit(&original);
        form.apply_fetched(&[
            crate::provider::RemoteModel {
                slug: "glm-5.3-flash".into(),
                display_name: None,
                description: None,
                context_window: Some(1_048_576),
                input_modalities: vec![],
            },
            crate::provider::RemoteModel {
                slug: "Qwen/Qwen3-Embedding-0.6B".into(),
                display_name: None,
                description: None,
                context_window: Some(8_192),
                input_modalities: vec![],
            },
            crate::provider::RemoteModel {
                slug: "gemini-3-flash".into(),
                display_name: None,
                description: None,
                context_window: Some(8_192),
                input_modalities: vec![],
            },
        ])
        .expect("gateway chat slugs");
        let ids: Vec<&str> = form.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["glm-5.3-flash", "gemini-3-flash"]);
        assert_eq!(form.models[form.default_idx].id, "glm-5.3-flash");
        let FormOutcome::Saved {
            fetched_catalog: Some(catalog),
            ..
        } = form.handle_key(KeyCode::Char('s'))
        else {
            panic!("fetched metadata must leave the form with the saved profile");
        };
        assert_eq!(catalog[0].context_window, Some(1_048_576));
    }

    fn remote(slug: &str) -> crate::provider::RemoteModel {
        crate::provider::RemoteModel {
            slug: slug.into(),
            display_name: None,
            description: None,
            context_window: None,
            input_modalities: vec![],
        }
    }

    fn large_remote() -> Vec<crate::provider::RemoteModel> {
        let mut rows: Vec<_> = (0..crate::provider::SMALL_REMOTE_CATALOG_LIMIT)
            .map(|i| remote(&format!("vendor/pad-{i}")))
            .collect();
        rows.push(remote("openai/gpt-4.1-nano"));
        rows.push(remote("deepseek/deepseek-r1-0528"));
        rows
    }

    #[test]
    fn fetch_large_catalog_opens_a_picker_instead_of_replacing() {
        let original = ProviderProfile::build(
            "or",
            "https://openrouter.ai/api/v1",
            vec![ProviderModel::from_id("composer-2.5")],
            "sk",
        );
        let mut form = ProviderFormState::edit(&original);
        form.ingest_remote(&large_remote());
        assert!(form.pick.is_some(), "large catalog must open the picker");
        assert_eq!(form.models[0].id, "composer-2.5");
        assert!(
            form.error.is_none(),
            "picker is not an error: {:?}",
            form.error
        );
    }

    #[test]
    fn picker_space_and_enter_saves_checked_slugs() {
        let original = ProviderProfile::build(
            "or",
            "https://openrouter.ai/api/v1",
            vec![ProviderModel::from_id("composer-2.5")],
            "sk",
        );
        let mut form = ProviderFormState::edit(&original);
        form.ingest_remote(&large_remote());
        form.handle_key(KeyCode::Char('/'));
        for ch in "nano".chars() {
            form.handle_key(KeyCode::Char(ch));
        }
        form.handle_key(KeyCode::Enter);
        form.handle_key(KeyCode::Char(' '));
        form.handle_key(KeyCode::Char('/'));
        for ch in "deepseek-r1".chars() {
            form.handle_key(KeyCode::Char(ch));
        }
        form.handle_key(KeyCode::Enter);
        form.handle_key(KeyCode::Char(' '));
        form.handle_key(KeyCode::Enter);
        assert!(form.pick.is_none());
        let ids: Vec<&str> = form.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["openai/gpt-4.1-nano", "deepseek/deepseek-r1-0528"]
        );
    }

    #[test]
    fn picker_esc_keeps_the_existing_models() {
        let original = ProviderProfile::build(
            "or",
            "https://openrouter.ai/api/v1",
            vec![ProviderModel::from_id("composer-2.5")],
            "sk",
        );
        let mut form = ProviderFormState::edit(&original);
        form.ingest_remote(&large_remote());
        form.handle_key(KeyCode::Esc);
        assert!(form.pick.is_none());
        assert_eq!(form.models[0].id, "composer-2.5");
    }

    #[test]
    fn picker_enter_without_a_check_stays_open() {
        let original = ProviderProfile::build(
            "or",
            "https://openrouter.ai/api/v1",
            vec![ProviderModel::from_id("composer-2.5")],
            "sk",
        );
        let mut form = ProviderFormState::edit(&original);
        form.ingest_remote(&large_remote());
        form.handle_key(KeyCode::Enter);
        assert!(form.pick.is_some());
        assert_eq!(
            form.pick.as_ref().and_then(|p| p.message.as_deref()),
            Some("pick at least one model")
        );
    }

    #[test]
    fn picker_renders_gateway_slugs() {
        use ratatui::{Terminal, backend::TestBackend};

        let original = ProviderProfile::build(
            "or",
            "https://openrouter.ai/api/v1",
            vec![ProviderModel::from_id("composer-2.5")],
            "sk",
        );
        let mut form = ProviderFormState::edit(&original);
        form.ingest_remote(&large_remote());
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
        assert!(joined.contains("Pick models"), "{joined}");
        assert!(joined.contains("vendor/pad-0"), "{joined}");
        assert!(joined.contains("[ ]"), "{joined}");
    }

    #[test]
    fn fetch_without_a_url_reports_an_error() {
        let mut form = ProviderFormState::add();
        form.handle_key(KeyCode::Esc);
        form.handle_key(KeyCode::Char('f'));
        assert!(
            form.error
                .as_deref()
                .is_some_and(|e| e.contains("Base URL")),
            "error was {:?}",
            form.error
        );
    }
}
