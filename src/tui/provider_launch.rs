//! Launch picker for a custom API provider: choose one saved model and the
//! reasoning effort for this session. The saved provider file is not written.

use crossterm::event::KeyCode;
use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span},
};

use super::popup::{self, PopupState};
use super::provider_form::{REASONING_CHOICES, reasoning_index};
use super::theme::{base, dim, header, key};
use crate::provider::{ProviderProfile, ReasoningLaunch};

#[derive(Debug, Clone)]
struct LaunchModel {
    id: String,
    saved_reasoning: Option<String>,
    no_web_search: bool,
    is_default: bool,
}

pub struct ProviderLaunchState {
    pub popup: PopupState,
    alias: String,
    models: Vec<LaunchModel>,
    selected: usize,
    reasoning_idx: usize,
}

pub enum LaunchPickerOutcome {
    Continue,
    Cancel,
    Launch {
        alias: String,
        model: String,
        reasoning: ReasoningLaunch,
    },
}

impl ProviderLaunchState {
    pub fn from_profile(profile: &ProviderProfile) -> Self {
        let models: Vec<LaunchModel> = profile
            .models
            .iter()
            .map(|model| LaunchModel {
                id: model.id.clone(),
                saved_reasoning: model.reasoning.clone(),
                no_web_search: model.no_web_search,
                is_default: model.id.trim() == profile.default_model.trim(),
            })
            .collect();
        let selected = models
            .iter()
            .position(|model| model.is_default)
            .unwrap_or(0);
        let reasoning_idx = models
            .get(selected)
            .map(|model| reasoning_index(model.saved_reasoning.as_deref()))
            .unwrap_or(0);
        Self {
            popup: PopupState::new(),
            alias: profile.alias.clone(),
            models,
            selected,
            reasoning_idx,
        }
    }

    fn select(&mut self, idx: usize) {
        if idx >= self.models.len() {
            return;
        }
        self.selected = idx;
        self.reasoning_idx = reasoning_index(self.models[idx].saved_reasoning.as_deref());
    }

    fn reasoning_for_launch(&self) -> ReasoningLaunch {
        if self.reasoning_idx == 0 {
            ReasoningLaunch::Skip
        } else {
            ReasoningLaunch::Effort(REASONING_CHOICES[self.reasoning_idx].to_string())
        }
    }

    pub fn handle_key(&mut self, code: KeyCode) -> LaunchPickerOutcome {
        match code {
            KeyCode::Esc => LaunchPickerOutcome::Cancel,
            KeyCode::Enter | KeyCode::Char('o') => {
                let Some(model) = self.models.get(self.selected) else {
                    return LaunchPickerOutcome::Continue;
                };
                LaunchPickerOutcome::Launch {
                    alias: self.alias.clone(),
                    model: model.id.clone(),
                    reasoning: self.reasoning_for_launch(),
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected + 1 < self.models.len() {
                    self.select(self.selected + 1);
                }
                LaunchPickerOutcome::Continue
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if self.selected > 0 {
                    self.select(self.selected - 1);
                }
                LaunchPickerOutcome::Continue
            }
            KeyCode::Left => {
                if self.reasoning_idx > 0 {
                    self.reasoning_idx -= 1;
                } else {
                    self.reasoning_idx = REASONING_CHOICES.len() - 1;
                }
                LaunchPickerOutcome::Continue
            }
            KeyCode::Right => {
                self.reasoning_idx = (self.reasoning_idx + 1) % REASONING_CHOICES.len();
                LaunchPickerOutcome::Continue
            }
            _ => LaunchPickerOutcome::Continue,
        }
    }
}

pub fn render_provider_launch(f: &mut Frame, state: &mut ProviderLaunchState, area: Rect) {
    let mut lines: Vec<Line<'static>> = Vec::new();

    lines.push(Line::from(vec![
        Span::styled("Provider  ", dim()),
        Span::styled(state.alias.clone(), header()),
    ]));
    lines.push(Line::from(Span::styled(
        "Pick a saved model. Reasoning applies to this launch only.",
        dim(),
    )));
    lines.push(Line::from(""));

    for (idx, model) in state.models.iter().enumerate() {
        let selected = idx == state.selected;
        let marker = if selected { "▶ " } else { "  " };
        let default = if model.is_default { " ● default" } else { "" };
        let search = if model.no_web_search {
            "  no-web-search"
        } else {
            ""
        };
        let style = if selected { header() } else { base() };
        lines.push(Line::from(vec![
            Span::styled(marker, key()),
            Span::styled(format!("{}{default}{search}", model.id), style),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("reasoning  ", dim()),
        Span::styled(REASONING_CHOICES[state.reasoning_idx].to_string(), header()),
        Span::styled("   (this session)", dim()),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("j/k", key()),
        Span::styled(" model  ", dim()),
        Span::styled("←/→", key()),
        Span::styled(" reasoning  ", dim()),
        Span::styled("enter/o", key()),
        Span::styled(" launch  ", dim()),
        Span::styled("esc", key()),
        Span::styled(" cancel", dim()),
    ]));

    popup::render_popup(f, "Launch provider", &lines, &mut state.popup, area);
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyCode;

    use super::{LaunchPickerOutcome, ProviderLaunchState};
    use crate::provider::{ProviderModel, ProviderProfile, ReasoningLaunch};

    fn profile() -> ProviderProfile {
        let mut p = ProviderProfile::build(
            "or",
            "https://openrouter.ai/api/v1",
            vec![
                ProviderModel::from_id("minimax/minimax-m3:free"),
                ProviderModel {
                    id: "liquid/lfm-2.5-2.6b:free".into(),
                    reasoning: Some("high".into()),
                    no_web_search: true,
                },
            ],
            "sk-test",
        );
        p.default_model = "minimax/minimax-m3:free".into();
        p
    }

    #[test]
    fn picker_starts_on_default_and_can_select_another_model_with_reasoning() {
        let mut picker = ProviderLaunchState::from_profile(&profile());
        assert!(matches!(
            picker.handle_key(KeyCode::Down),
            LaunchPickerOutcome::Continue
        ));
        assert!(matches!(
            picker.handle_key(KeyCode::Right),
            LaunchPickerOutcome::Continue
        ));
        let LaunchPickerOutcome::Launch {
            alias,
            model,
            reasoning,
        } = picker.handle_key(KeyCode::Enter)
        else {
            panic!("enter should launch");
        };
        assert_eq!(alias, "or");
        assert_eq!(model, "liquid/lfm-2.5-2.6b:free");
        // saved high (index of "high") then one Right → xhigh
        assert_eq!(reasoning, ReasoningLaunch::Effort("xhigh".into()));
    }

    use ratatui::{Terminal, backend::TestBackend};

    fn row_text(backend: &TestBackend, y: u16) -> String {
        let area = backend.buffer().area;
        (0..area.width)
            .map(|x| {
                backend
                    .buffer()
                    .cell((x, y))
                    .expect("cell")
                    .symbol()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn picker_renders_models_and_session_reasoning() {
        let mut picker = ProviderLaunchState::from_profile(&profile());
        picker.handle_key(KeyCode::Down);
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| super::render_provider_launch(frame, &mut picker, frame.area()))
            .unwrap();
        let joined = (0..20)
            .map(|y| row_text(terminal.backend(), y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("Launch provider"));
        assert!(joined.contains("liquid/lfm-2.5-2.6b:free"));
        assert!(joined.contains("high"));
        assert!(joined.contains("this session"));
        assert!(joined.contains("enter/o"));
        assert!(!joined.contains("sk-test"));
    }

    #[test]
    fn picker_can_skip_saved_reasoning_for_this_launch() {
        let mut picker = ProviderLaunchState::from_profile(&profile());
        picker.handle_key(KeyCode::Down);
        // saved high is index 4; four Left steps reach (skip)
        for _ in 0..4 {
            picker.handle_key(KeyCode::Left);
        }
        let LaunchPickerOutcome::Launch { reasoning, .. } = picker.handle_key(KeyCode::Enter)
        else {
            panic!("enter should launch");
        };
        assert_eq!(reasoning, ReasoningLaunch::Skip);
    }

    #[test]
    fn picker_o_confirms_launch_like_enter() {
        let mut picker = ProviderLaunchState::from_profile(&profile());
        let LaunchPickerOutcome::Launch { alias, model, .. } =
            picker.handle_key(KeyCode::Char('o'))
        else {
            panic!("o should launch from the picker");
        };
        assert_eq!(alias, "or");
        assert_eq!(model, "minimax/minimax-m3:free");
    }
}
