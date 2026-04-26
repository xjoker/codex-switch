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
    Global,
}

impl Section {
    pub const fn label(self) -> &'static str {
        match self {
            Section::Navigation => "Navigation",
            Section::Selection => "Selection",
            Section::Account => "Account actions  (open via Enter)",
            Section::Batch => "Batch actions  (open via Enter when accounts marked)",
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
        keys: "f",
        section: Section::Account,
        label: "refresh this one (force)",
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
        in_status_bar: false,
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
