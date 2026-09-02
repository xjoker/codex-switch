/// Single source of truth for TUI keybindings.
///
/// Status bar and Help popup both render from this list.
/// Adding/changing a key here updates every surface.
///
/// Binding rules (letter must match the verb shown in the UI):
/// - Same action uses the same key on every tab.
/// - `o` launches Codex. `l` is re-login on Accounts (and batch). Never bind
///   launch to `l`: that letter already means login.
/// - Enter opens the selected row (Accounts: action menu, Providers: launch
///   picker). `e` edits a provider. In a dialog, Enter confirms and Esc cancels.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Navigation,
    Selection,
    Account,
    Batch,
    Provider,
    Settings,
    Logs,
    Global,
}

impl Section {
    pub const fn label(self) -> &'static str {
        match self {
            Section::Navigation => "Navigation",
            Section::Selection => "Selection",
            Section::Account => "Accounts tab",
            Section::Batch => "Batch actions  (open via Enter when accounts marked)",
            Section::Provider => "Providers tab",
            Section::Settings => "Settings tab",
            Section::Logs => "Logs tab",
            Section::Global => "Global",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Binding {
    pub keys: &'static str,
    pub section: Section,
    pub label: &'static str,
    pub in_status_bar: bool,
}

/// Master keymap. Order matters: status bar renders top entries first;
/// Help popup groups by section in the order encountered.
pub const KEYMAP: &[Binding] = &[
    // Navigation
    Binding {
        keys: "j / k / ↑ ↓",
        section: Section::Account,
        label: "move selection",
        in_status_bar: true,
    },
    Binding {
        keys: "/",
        section: Section::Account,
        label: "search",
        in_status_bar: true,
    },
    Binding {
        keys: "tab",
        section: Section::Navigation,
        label: "next tab (Accounts / Providers / Settings / Logs)",
        in_status_bar: false,
    },
    Binding {
        keys: "s",
        section: Section::Account,
        label: "cycle sort (name / quota / status)",
        in_status_bar: false,
    },
    // Selection
    Binding {
        keys: "space",
        section: Section::Selection,
        label: "toggle mark",
        in_status_bar: false,
    },
    Binding {
        keys: "esc",
        section: Section::Selection,
        label: "clear marks / search / popup",
        in_status_bar: false,
    },
    // Account actions (via Enter menu)
    Binding {
        keys: "enter",
        section: Section::Account,
        label: "open selected account actions",
        in_status_bar: true,
    },
    Binding {
        keys: "r",
        section: Section::Account,
        label: "refresh account details",
        in_status_bar: false,
    },
    Binding {
        keys: "o",
        section: Section::Account,
        label: "launch Codex",
        in_status_bar: true,
    },
    Binding {
        keys: "u",
        section: Section::Account,
        label: "use (switch to)",
        in_status_bar: false,
    },
    Binding {
        keys: "l",
        section: Section::Account,
        label: "re-login",
        in_status_bar: false,
    },
    Binding {
        keys: "n",
        section: Section::Account,
        label: "rename",
        in_status_bar: false,
    },
    Binding {
        keys: "w",
        section: Section::Account,
        label: "warmup",
        in_status_bar: false,
    },
    Binding {
        keys: "c",
        section: Section::Account,
        label: "confirm earliest reset card",
        in_status_bar: false,
    },
    Binding {
        keys: "d",
        section: Section::Account,
        label: "delete",
        in_status_bar: false,
    },
    // Batch actions
    Binding {
        keys: "r",
        section: Section::Batch,
        label: "refresh selected",
        in_status_bar: false,
    },
    Binding {
        keys: "w",
        section: Section::Batch,
        label: "warmup selected",
        in_status_bar: false,
    },
    Binding {
        keys: "l",
        section: Section::Batch,
        label: "re-login selected (sequential)",
        in_status_bar: false,
    },
    Binding {
        keys: "d",
        section: Section::Batch,
        label: "delete selected",
        in_status_bar: false,
    },
    // Providers tab
    Binding {
        keys: "j / k / ↑ ↓",
        section: Section::Provider,
        label: "move provider selection",
        in_status_bar: false,
    },
    Binding {
        keys: "enter / o",
        section: Section::Provider,
        label: "launch Codex (pick model, reasoning, extra args)",
        in_status_bar: false,
    },
    Binding {
        keys: "e",
        section: Section::Provider,
        label: "edit provider",
        in_status_bar: false,
    },
    Binding {
        keys: "n",
        section: Section::Provider,
        label: "rename provider",
        in_status_bar: false,
    },
    Binding {
        keys: "a",
        section: Section::Provider,
        label: "add provider",
        in_status_bar: false,
    },
    Binding {
        keys: "d",
        section: Section::Provider,
        label: "remove selected provider",
        in_status_bar: false,
    },
    // Settings tab
    Binding {
        keys: "j / k / ↑ ↓",
        section: Section::Settings,
        label: "move field",
        in_status_bar: false,
    },
    Binding {
        keys: "enter / space",
        section: Section::Settings,
        label: "edit or toggle the focused field",
        in_status_bar: false,
    },
    Binding {
        keys: "← / →",
        section: Section::Settings,
        label: "cycle log level / booleans",
        in_status_bar: false,
    },
    Binding {
        keys: "+ / a",
        section: Section::Settings,
        label: "add warmup HH:MM (max 10; comma-separated list ok)",
        in_status_bar: false,
    },
    Binding {
        keys: "d / -",
        section: Section::Settings,
        label: "remove the selected warmup slot",
        in_status_bar: false,
    },
    Binding {
        keys: "s",
        section: Section::Settings,
        label: "save config.toml",
        in_status_bar: false,
    },
    Binding {
        keys: "esc",
        section: Section::Settings,
        label: "cancel the current field edit",
        in_status_bar: false,
    },
    // Logs tab
    Binding {
        keys: "j / k / ↑ ↓ / PgUp PgDn",
        section: Section::Logs,
        label: "scroll session logs",
        in_status_bar: false,
    },
    Binding {
        keys: "end",
        section: Section::Logs,
        label: "jump to latest log",
        in_status_bar: false,
    },
    // Global
    Binding {
        keys: "a",
        section: Section::Account,
        label: "add new account",
        in_status_bar: true,
    },
    Binding {
        keys: "r",
        section: Section::Account,
        label: "refresh visible accounts",
        in_status_bar: true,
    },
    Binding {
        keys: "t",
        section: Section::Account,
        label: "toggle auto-refresh",
        in_status_bar: false,
    },
    Binding {
        keys: "W",
        section: Section::Account,
        label: "toggle auto-warmup (auto-refresh + warm whenever 5h expires)",
        in_status_bar: false,
    },
    Binding {
        keys: "i",
        section: Section::Account,
        label: "show / hide account detail panel",
        in_status_bar: true,
    },
    Binding {
        keys: "h",
        section: Section::Global,
        label: "show this help (main view)",
        in_status_bar: true,
    },
    Binding {
        keys: "mouse",
        section: Section::Global,
        label: "click tabs/rows; double-click rows opens menus; wheel scrolls logs/help/menus",
        in_status_bar: false,
    },
    Binding {
        keys: "q",
        section: Section::Global,
        label: "quit (main view)",
        in_status_bar: true,
    },
];

/// Build help text grouped by section. Returns a list of (heading, lines).
pub fn help_sections_for(
    active: Section,
) -> Vec<(&'static str, Vec<(&'static str, &'static str)>)> {
    let mut result: Vec<(&'static str, Vec<(&'static str, &'static str)>)> = Vec::new();
    for binding in KEYMAP.iter().filter(|binding| {
        matches!(binding.section, Section::Navigation | Section::Global)
            || binding.section == active
            || (active == Section::Account
                && matches!(binding.section, Section::Selection | Section::Batch))
    }) {
        let heading = binding.section.label();
        if let Some((_, items)) = result.iter_mut().find(|(h, _)| *h == heading) {
            items.push((binding.keys, binding.label));
        } else {
            result.push((heading, vec![(binding.keys, binding.label)]));
        }
    }
    result
}

/// Status bar items in display order.
pub fn status_bar_items() -> Vec<(&'static str, &'static str)> {
    KEYMAP
        .iter()
        .filter(|b| b.in_status_bar)
        .map(|b| (b.keys, b.label))
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn status_bar_surfaces_core_account_actions() {
        let items = super::status_bar_items();
        for (key, verb) in [("a", "add"), ("i", "detail"), ("o", "launch")] {
            assert!(
                items
                    .iter()
                    .any(|(keys, label)| *keys == key && label.to_ascii_lowercase().contains(verb)),
                "missing {key} {verb} from {items:?}"
            );
        }
    }

    #[test]
    fn launch_is_o_on_every_tab_and_l_is_only_login() {
        let launch: Vec<_> = super::KEYMAP
            .iter()
            .filter(|b| {
                matches!(
                    b.section,
                    super::Section::Account | super::Section::Provider
                ) && b.label.to_ascii_lowercase().contains("launch")
            })
            .collect();
        assert!(
            launch
                .iter()
                .all(|b| b.keys.contains('o') && !b.keys.contains('l')),
            "launch must include o and never l: {launch:?}"
        );
        assert!(launch.iter().any(|b| b.section == super::Section::Account));
        assert!(launch.iter().any(|b| b.section == super::Section::Provider));

        let settings_save = super::KEYMAP
            .iter()
            .find(|b| b.section == super::Section::Settings && b.keys == "s")
            .expect("Settings s is save");
        assert!(settings_save.label.contains("save"));

        let ell: Vec<_> = super::KEYMAP.iter().filter(|b| b.keys == "l").collect();
        assert!(
            ell.iter()
                .all(|b| b.label.contains("login") && b.section != super::Section::Provider),
            "l must mean login, never a Providers action: {ell:?}"
        );
    }

    #[test]
    fn help_only_shows_bindings_for_the_active_tab() {
        let providers = super::help_sections_for(super::Section::Provider);
        let provider_text = format!("{providers:?}");
        assert!(provider_text.contains("add provider"));
        assert!(!provider_text.contains("add new account"));
        assert!(!provider_text.contains("refresh visible accounts"));

        let logs = super::help_sections_for(super::Section::Logs);
        let log_text = format!("{logs:?}");
        assert!(log_text.contains("scroll session logs"));
        assert!(!log_text.contains("add provider"));
    }

    #[test]
    fn help_labels_q_and_h_as_main_view_shortcuts() {
        for key in ["h", "q"] {
            let binding = super::KEYMAP
                .iter()
                .find(|binding| binding.section == super::Section::Global && binding.keys == key)
                .unwrap_or_else(|| panic!("missing global {key} binding"));
            assert!(
                binding.label.contains("main view"),
                "{key} must not claim to apply inside forms or popups: {}",
                binding.label
            );
        }
    }
}
