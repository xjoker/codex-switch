//! Shared TUI palette. Every style must set a background so a light terminal
//! (xfce4-terminal / macOS Terminal defaults) cannot wash the designed look
//! back to black-on-white.

use ratatui::style::{Color, Modifier, Style};

pub const BG: Color = Color::Rgb(24, 24, 24);
pub const C_WHITE: Color = Color::Rgb(240, 240, 240);
pub const C_GRAY: Color = Color::Rgb(180, 180, 180);
pub const DIM: Color = Color::Rgb(132, 132, 132);
pub const C_RED: Color = Color::Rgb(255, 90, 90);
pub const C_GREEN: Color = Color::Rgb(80, 220, 120);
pub const C_YELLOW: Color = Color::Rgb(255, 220, 80);
pub const C_CYAN: Color = Color::Rgb(100, 210, 255);
pub const C_MAGENTA: Color = Color::Rgb(220, 130, 255);
pub const C_BLUE: Color = Color::Rgb(80, 140, 220);
pub const C_HIGHLIGHT_BG: Color = Color::Rgb(55, 55, 65);
pub const C_PURPLE: Color = Color::Rgb(175, 120, 240);

pub fn base() -> Style {
    Style::default().bg(BG).fg(C_WHITE)
}

pub fn key() -> Style {
    base().fg(C_YELLOW).add_modifier(Modifier::BOLD)
}

pub fn dim() -> Style {
    base().fg(DIM)
}

pub fn header() -> Style {
    base().fg(C_CYAN).add_modifier(Modifier::BOLD)
}

pub fn highlight() -> Style {
    base()
        .bg(C_HIGHLIGHT_BG)
        .fg(C_WHITE)
        .add_modifier(Modifier::BOLD)
}
