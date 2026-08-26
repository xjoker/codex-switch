/// Single source of truth for TUI keybindings.
///
/// Status bar and Help popup both render from this list.
/// Adding/changing a key here updates every surface.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Navigation,
    Selection,
    Account,
    Batch,
    Provider,
    Global,
}

impl Section {
    pub const fn label(self) -> &'static str {
        match self {
            Section::Navigation => "Navigation",
            Section::Selection => "Selection",
            Section::Account => "Account actions  (open via Enter)",
            Section::Batch => "Batch actions  (open via Enter when accounts marked)",
            Section::Provider => "Providers tab",
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
        section: Section::Navigation,
        label: "move selection",
        in_status_bar: true,
    },
    Binding {
        keys: "/",
        section: Section::Navigation,
        label: "search",
        in_status_bar: true,
    },
    Binding {
        keys: "s",
        section: Section::Navigation,
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
        keys: "r",
        section: Section::Account,
        label: "refresh account details",
        in_status_bar: false,
    },
    Binding {
        keys: "o",
        section: Section::Account,
        label: "launch codex",
        in_status_bar: false,
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
        keys: "l",
        section: Section::Provider,
        label: "launch: pick model and reasoning",
        in_status_bar: false,
    },
    Binding {
        keys: "e / enter",
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
    // Global
    Binding {
        keys: "enter",
        section: Section::Global,
        label: "open menu (account or batch)",
        in_status_bar: true,
    },
    Binding {
        keys: "a",
        section: Section::Global,
        label: "add new account",
        in_status_bar: true,
    },
    Binding {
        keys: "r",
        section: Section::Global,
        label: "refresh visible accounts",
        in_status_bar: true,
    },
    Binding {
        keys: "t",
        section: Section::Global,
        label: "toggle auto-refresh",
        in_status_bar: false,
    },
    Binding {
        keys: "W",
        section: Section::Global,
        label: "toggle auto-warmup (auto-refresh + warm whenever 5h expires)",
        in_status_bar: false,
    },
    Binding {
        keys: "i",
        section: Section::Global,
        label: "show / hide account detail panel",
        in_status_bar: true,
    },
    Binding {
        keys: "h",
        section: Section::Global,
        label: "show this help",
        in_status_bar: true,
    },
    Binding {
        keys: "q",
        section: Section::Global,
        label: "quit",
        in_status_bar: true,
    },
];

/// Build help text grouped by section. Returns a list of (heading, lines).
pub fn help_sections() -> Vec<(&'static str, Vec<(&'static str, &'static str)>)> {
    let mut result: Vec<(&'static str, Vec<(&'static str, &'static str)>)> = Vec::new();
    for binding in KEYMAP {
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
    fn status_bar_surfaces_add_account_action() {
        assert!(
            super::status_bar_items()
                .iter()
                .any(|(keys, label)| *keys == "a" && label.contains("add"))
        );
    }

    #[test]
    fn status_bar_surfaces_account_detail_action() {
        assert!(
            super::status_bar_items()
                .iter()
                .any(|(keys, label)| *keys == "i" && label.contains("detail"))
        );
    }
}
