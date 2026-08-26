/// Generic popup rendering with screen-size adaptation.
///
/// Centers a bordered box on screen, clamps to terminal bounds,
/// and supports vertical scrolling when content exceeds available height.
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};

use super::theme::{BG, C_RED, base, dim, header};

/// Minimum terminal size below which we abort popup rendering.
const MIN_TERM_W: u16 = 20;
const MIN_TERM_H: u16 = 6;

pub struct PopupState {
    pub scroll: u16,
}

impl PopupState {
    pub const fn new() -> Self {
        Self { scroll: 0 }
    }

    pub fn scroll_down(&mut self, max: u16) {
        if self.scroll < max {
            self.scroll = self.scroll.saturating_add(1);
        }
    }

    pub fn scroll_up(&mut self) {
        self.scroll = self.scroll.saturating_sub(1);
    }

    pub fn page_down(&mut self, page: u16, max: u16) {
        self.scroll = self.scroll.saturating_add(page).min(max);
    }

    pub fn page_up(&mut self, page: u16) {
        self.scroll = self.scroll.saturating_sub(page);
    }

    pub fn reset(&mut self) {
        self.scroll = 0;
    }
}

/// Render a popup with `lines` content, centered on screen.
///
/// - `title` shown in border
/// - `lines` plain text lines (already styled if needed via Line)
/// - `state` for scroll offset (use a fresh state for non-scrolling popups)
///
/// If terminal is too small, renders a single-line fallback at the bottom
/// of `screen` instead of the popup.
///
/// Returns the inner content area width (so callers can do their own
/// truncation if needed); caller may ignore.
pub fn render_popup(
    f: &mut Frame,
    title: &str,
    lines: &[Line<'_>],
    state: &mut PopupState,
    screen: Rect,
) {
    if screen.width < MIN_TERM_W || screen.height < MIN_TERM_H {
        render_too_small_fallback(f, screen);
        return;
    }

    // Measure content
    let content_h = lines.len() as u16;
    let content_w: u16 = lines.iter().map(|l| l.width() as u16).max().unwrap_or(0);

    let title_w = (title.len() as u16).saturating_add(4); // "─ title ─" + corners
    let needed_w = content_w.saturating_add(4).max(title_w); // 2 border + 2 padding
    let needed_h = content_h.saturating_add(2); // 2 border

    // Clamp to screen, leaving 2 cols / 1 row margin where possible
    let max_w = screen.width.saturating_sub(2).max(MIN_TERM_W);
    let max_h = screen.height.saturating_sub(2).max(MIN_TERM_H);
    let w = needed_w.min(max_w);
    let h = needed_h.min(max_h);

    let x = screen.x + screen.width.saturating_sub(w) / 2;
    let y = screen.y + screen.height.saturating_sub(h) / 2;
    let area = Rect {
        x,
        y,
        width: w,
        height: h,
    };

    f.render_widget(Clear, area);

    let block = Block::default()
        .title(format!(" {title} "))
        .borders(Borders::ALL)
        .border_style(header())
        .style(base());
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Inner usable area accounting for 1-col left/right padding
    let pad_left = 1u16;
    let pad_right = 1u16;
    let usable_w = inner.width.saturating_sub(pad_left + pad_right);
    let visible_h = inner.height;

    let total_lines = lines.len() as u16;
    let scrollable = total_lines > visible_h;
    let max_scroll = total_lines.saturating_sub(visible_h);
    let scroll = state.scroll.min(max_scroll);
    state.scroll = scroll; // clamp persisted scroll to actual content bounds

    // Truncate lines that exceed usable_w (with ellipsis)
    let truncated: Vec<Line<'static>> = lines
        .iter()
        .map(|l| truncate_line(l, usable_w as usize))
        .collect();

    let visible_slice: &[Line<'static>] = if scrollable {
        let start = scroll as usize;
        let end = (start + visible_h as usize).min(truncated.len());
        &truncated[start..end]
    } else {
        &truncated[..]
    };

    let content_area = Rect {
        x: inner.x + pad_left,
        y: inner.y,
        width: usable_w,
        height: visible_h,
    };
    f.render_widget(
        Paragraph::new(visible_slice.to_vec()).style(base()),
        content_area,
    );

    // Scrollbar on right edge inside border
    if scrollable && inner.width >= 1 && visible_h > 0 {
        render_scrollbar(f, inner, scroll, max_scroll, visible_h, total_lines);
    }
}

fn render_scrollbar(
    f: &mut Frame,
    inner: Rect,
    scroll: u16,
    max_scroll: u16,
    visible_h: u16,
    total_lines: u16,
) {
    let bar_x = inner.x + inner.width.saturating_sub(1);
    let bar_h = visible_h;
    if bar_h == 0 || total_lines == 0 {
        return;
    }

    // Thumb height proportional to visible/total
    let thumb_h = ((bar_h as f64 * visible_h as f64 / total_lines as f64).round() as u16)
        .max(1)
        .min(bar_h);
    let thumb_pos = if max_scroll == 0 {
        0
    } else {
        ((bar_h.saturating_sub(thumb_h)) as f64 * scroll as f64 / max_scroll as f64).round() as u16
    };

    // Track
    for i in 0..bar_h {
        let cell_y = inner.y + i;
        let in_thumb = i >= thumb_pos && i < thumb_pos + thumb_h;
        let (ch, style) = if in_thumb {
            ("\u{2588}", header()) // █
        } else {
            ("\u{258C}", dim()) // ▌ (subtle track)
        };
        let area = Rect {
            x: bar_x,
            y: cell_y,
            width: 1,
            height: 1,
        };
        f.render_widget(Paragraph::new(Span::styled(ch, style)), area);
    }
}

fn render_too_small_fallback(f: &mut Frame, screen: Rect) {
    let msg = "Screen too small — resize terminal";
    let h = 1u16;
    let y = screen.y + screen.height.saturating_sub(h);
    let area = Rect {
        x: screen.x,
        y,
        width: screen.width,
        height: h,
    };
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(msg).style(
            Style::default()
                .fg(C_RED)
                .bg(BG)
                .add_modifier(Modifier::BOLD),
        ),
        area,
    );
}

/// Truncate a Line so its display width <= max_width, appending "…" if cut.
fn truncate_line(line: &Line<'_>, max_width: usize) -> Line<'static> {
    if line.width() <= max_width {
        // Clone owned
        let spans: Vec<Span<'static>> = line
            .spans
            .iter()
            .map(|s| Span::styled(s.content.to_string(), s.style))
            .collect();
        return Line::from(spans);
    }
    if max_width == 0 {
        return Line::from(Span::raw(""));
    }

    let target = max_width.saturating_sub(1); // reserve 1 col for ellipsis
    let mut acc: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for span in &line.spans {
        let span_w = span.width();
        if used + span_w <= target {
            acc.push(Span::styled(span.content.to_string(), span.style));
            used += span_w;
            continue;
        }

        let mut partial = String::new();
        for grapheme in span.styled_graphemes(Style::default()) {
            let grapheme_width = Span::raw(grapheme.symbol).width();
            if used + grapheme_width > target {
                break;
            }
            partial.push_str(grapheme.symbol);
            used += grapheme_width;
        }
        if !partial.is_empty() {
            acc.push(Span::styled(partial, span.style));
        }
        break;
    }
    acc.push(Span::styled("\u{2026}".to_string(), dim()));
    Line::from(acc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    fn content(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn truncate_line_returns_original_when_shorter_or_exact_width() {
        let short = Line::from("abc");
        let exact = Line::from("abcd");

        assert_eq!(content(&truncate_line(&short, 4)), "abc");
        assert_eq!(content(&truncate_line(&exact, 4)), "abcd");
    }

    #[test]
    fn truncate_line_handles_cjk_and_emoji_by_display_width() {
        let truncated = truncate_line(&Line::from("中🙂abc"), 4);

        assert!(truncated.width() <= 4, "line was {truncated:?}");
        assert!(content(&truncated).ends_with('…'));
    }

    #[test]
    fn truncate_line_keeps_unicode_grapheme_clusters_intact() {
        let heart = truncate_line(&Line::from("❤️a"), 2);
        let developer = truncate_line(&Line::from("👩‍💻ab"), 3);

        assert!(heart.width() <= 2, "line was {heart:?}");
        assert_eq!(content(&developer), "👩‍💻…");
        assert_eq!(developer.width(), 3);
    }

    #[test]
    fn truncate_line_preserves_partial_content_across_spans() {
        let line = Line::from(vec![
            Span::styled("ab", Style::default().fg(Color::Red)),
            Span::styled("cdef", Style::default().fg(Color::Blue)),
        ]);

        let truncated = truncate_line(&line, 5);

        assert_eq!(content(&truncated), "abcd…");
        assert_eq!(truncated.width(), 5);
        assert_eq!(truncated.spans[0].style.fg, Some(Color::Red));
        assert_eq!(truncated.spans[1].style.fg, Some(Color::Blue));
    }

    #[test]
    fn popup_paints_the_designed_dark_background() {
        use ratatui::{Terminal, backend::TestBackend};

        let backend = TestBackend::new(40, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = PopupState::new();
        let lines = vec![Line::from(Span::styled(
            "hello",
            super::super::theme::key(),
        ))];
        terminal
            .draw(|f| render_popup(f, "Title", &lines, &mut state, f.area()))
            .unwrap();
        let cell = terminal
            .backend()
            .buffer()
            .cell((20, 6))
            .expect("cell inside popup");
        assert_eq!(
            cell.bg,
            Color::Rgb(24, 24, 24),
            "popup must pin the designed background, got {:?}",
            cell.bg
        );
    }
}
