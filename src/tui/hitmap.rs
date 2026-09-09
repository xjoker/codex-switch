//! Mouse hit regions recorded during the last TUI frame.
//!
//! Render populates this map; the event loop reads it on click/scroll.
//! Regions must use the same layout constraints as `ui::render`.

use crossterm::event::KeyCode;
use ratatui::layout::Rect;

use super::app::Tab;

#[derive(Debug, Clone, Default)]
pub struct HitMap {
    pub tabs: Vec<(Rect, Tab)>,
    pub account_list: Option<ListHit>,
    pub provider_list: Option<ListHit>,
    pub logs: Option<Rect>,
    /// Settings field rows after auto-scroll (index into the settings focus order).
    pub settings_fields: Vec<(Rect, usize)>,
    /// Settings panel body, used for wheel focus movement.
    pub settings_body: Option<Rect>,
    pub overlay: OverlayHit,
    /// Clickable actions rendered in the bottom status/help bar.
    pub footer_actions: Vec<(Rect, KeyCode)>,
    /// Clickable key-equivalent actions inside the active dismissible menu.
    pub menu_actions: Vec<(Rect, KeyCode)>,
    /// Clickable controls inside a modal overlay (forms, launch picker, confirm).
    pub overlay_clicks: Vec<(Rect, OverlayClick)>,
    /// Modal popup panel, used to decide whether a wheel event is inside it.
    pub overlay_panel: Option<Rect>,
}

#[derive(Debug, Clone, Copy)]
pub struct ListHit {
    /// Data rows only (border + header excluded).
    pub rows_area: Rect,
    /// First visible row index after table auto-scroll.
    pub offset: usize,
    pub row_count: usize,
}

#[derive(Debug, Clone, Copy, Default)]
pub enum OverlayHit {
    #[default]
    None,
    /// Help / menus: click outside dismisses; wheel scrolls when over panel.
    Dismissible { panel: Rect },
    /// Forms / launch / confirm / text edit: no click-through to the page behind.
    /// Overlay-local hits live in `overlay_clicks`.
    Modal,
}

/// A mouse target recorded while a modal overlay is visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayClick {
    Key(KeyCode),
    ProviderField(ProviderField),
    ProviderModel(usize),
    ProviderPick(usize),
    ProviderPickFilter,
    LaunchModel(usize),
    LaunchReasoning,
    LaunchArgs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderField {
    Alias,
    BaseUrl,
    RequireHttps,
    ApiKey,
    EnvKey,
    WireApi,
    Extra,
}

impl HitMap {
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn contains(area: Rect, column: u16, row: u16) -> bool {
        column >= area.x
            && row >= area.y
            && column < area.x.saturating_add(area.width)
            && row < area.y.saturating_add(area.height)
    }

    /// Map a click inside a scrolled table to a row index, or `None`.
    pub fn list_index_at(list: &ListHit, column: u16, row: u16) -> Option<usize> {
        if !Self::contains(list.rows_area, column, row) {
            return None;
        }
        let rel = usize::from(row.saturating_sub(list.rows_area.y));
        let idx = list.offset.saturating_add(rel);
        (idx < list.row_count).then_some(idx)
    }

    pub fn tab_at(&self, column: u16, row: u16) -> Option<Tab> {
        self.tabs
            .iter()
            .find(|(area, _)| Self::contains(*area, column, row))
            .map(|(_, tab)| *tab)
    }

    pub fn menu_action_at(&self, column: u16, row: u16) -> Option<KeyCode> {
        self.menu_actions
            .iter()
            .find(|(area, _)| Self::contains(*area, column, row))
            .map(|(_, key)| *key)
    }

    pub fn footer_action_at(&self, column: u16, row: u16) -> Option<KeyCode> {
        self.footer_actions
            .iter()
            .find(|(area, _)| Self::contains(*area, column, row))
            .map(|(_, code)| *code)
    }

    pub fn settings_field_at(&self, column: u16, row: u16) -> Option<usize> {
        self.settings_fields
            .iter()
            .find(|(area, _)| Self::contains(*area, column, row))
            .map(|(_, index)| *index)
    }

    pub fn overlay_click_at(&self, column: u16, row: u16) -> Option<OverlayClick> {
        self.overlay_clicks
            .iter()
            .rev()
            .find(|(area, _)| Self::contains(*area, column, row))
            .map(|(_, click)| *click)
    }
}

/// Content rows under a bordered table that has a 1-row header.
pub fn table_rows_area(table_area: Rect) -> Rect {
    let inner = Rect {
        x: table_area.x.saturating_add(1),
        y: table_area.y.saturating_add(1),
        width: table_area.width.saturating_sub(2),
        height: table_area.height.saturating_sub(2),
    };
    Rect {
        x: inner.x,
        y: inner.y.saturating_add(1),
        width: inner.width,
        height: inner.height.saturating_sub(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_index_accounts_for_header_and_offset() {
        let list = ListHit {
            rows_area: Rect {
                x: 1,
                y: 3,
                width: 40,
                height: 5,
            },
            offset: 2,
            row_count: 10,
        };
        assert_eq!(HitMap::list_index_at(&list, 5, 3), Some(2));
        assert_eq!(HitMap::list_index_at(&list, 5, 5), Some(4));
        assert_eq!(HitMap::list_index_at(&list, 5, 8), None); // past visible
        assert_eq!(HitMap::list_index_at(&list, 0, 3), None); // left of area
    }

    #[test]
    fn table_rows_area_skips_border_and_header() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 10,
        };
        let rows = table_rows_area(area);
        assert_eq!(rows.x, 1);
        assert_eq!(rows.y, 2); // border + header
        assert_eq!(rows.width, 18);
        assert_eq!(rows.height, 7); // 10 - 2 border - 1 header
    }
}
