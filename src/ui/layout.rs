//! Adaptive geometry for the main TUI.

use ratatui::layout::{Constraint, Direction, Layout, Rect};

/// The width used by the sidebar at each terminal breakpoint.
pub fn sidebar_width(terminal_width: u16) -> u16 {
    if terminal_width < 90 {
        12
    } else if terminal_width < 120 {
        24
    } else {
        32
    }
}

/// Rectangles for the body, status bar, and optional details column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutAreas {
    pub body: Rect,
    pub sidebar: Rect,
    pub center: Rect,
    pub details: Option<Rect>,
    pub status: Rect,
}

/// Split the screen into sidebar, center, optional details, and status areas.
///
/// Details are opt-in and are suppressed when the terminal cannot leave a
/// useful center column after reserving the sidebar and details widths.
pub fn split(area: Rect, details_visible: bool) -> LayoutAreas {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);
    let body = vertical[0];
    let status = vertical[1];
    let sidebar = sidebar_width(body.width);
    let details_width = 30u16;
    let can_show_details = details_visible
        && body.width >= 120
        && body.width >= sidebar.saturating_add(details_width).saturating_add(1);

    let horizontal = if can_show_details {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(sidebar),
                Constraint::Min(1),
                Constraint::Length(details_width),
            ])
            .split(body)
    } else {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(sidebar), Constraint::Min(1)])
            .split(body)
    };

    LayoutAreas {
        body,
        sidebar: horizontal[0],
        center: horizontal[1],
        details: horizontal.get(2).copied(),
        status,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidebar_breakpoints_match_the_ui_contract() {
        assert_eq!(sidebar_width(160), 32);
        assert_eq!(sidebar_width(119), 24);
        assert_eq!(sidebar_width(90), 24);
        assert_eq!(sidebar_width(89), 12);
    }

    #[test]
    fn details_are_hidden_by_default_and_at_narrow_widths() {
        let area = Rect::new(0, 0, 160, 30);
        assert!(split(area, false).details.is_none());
        assert!(split(Rect::new(0, 0, 80, 20), true).details.is_none());
        assert!(split(area, true).details.is_some());
    }
}
