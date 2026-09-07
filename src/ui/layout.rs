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

/// Details 列宽度默认/边界（Notes/04 §1：45 列默认，可调 30–60）。
pub const DEFAULT_DETAILS_WIDTH: u16 = 45;
pub const DETAILS_WIDTH_MIN: u16 = 30;
pub const DETAILS_WIDTH_MAX: u16 = 60;

/// Split the screen into sidebar, center, optional details, and status areas.
///
/// Details are opt-in and are suppressed when the terminal cannot leave a
/// useful center column after reserving the sidebar and details widths.
/// `details_width` is clamped to [30, 60] (Notes/04 §1); widths < 120 columns
/// automatically drop the details column.
pub fn split(area: Rect, details_visible: bool, details_width: u16) -> LayoutAreas {
    // 状态区两行：第一行字段，第二行快捷键提示（FR-001-05）。
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(2)])
        .split(area);
    let body = vertical[0];
    let status = vertical[1];
    let sidebar = sidebar_width(body.width);
    let details_width = details_width.clamp(DETAILS_WIDTH_MIN, DETAILS_WIDTH_MAX);
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

/// 终端颜色能力档位（FR-001-06）：着色按档位降级。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorDepth {
    TrueColor,
    C256,
    Basic,
}

/// 从环境变量推断颜色能力（FR-001-06）：
/// `COLORTERM` 含 truecolor/24bit → TrueColor；否则 `TERM` 含 256color →
/// C256；否则 Basic。`get` 注入便于纯函数测试。
pub fn detect_color_depth(get: impl Fn(&str) -> Option<String>) -> ColorDepth {
    if let Some(colorterm) = get("COLORTERM") {
        let value = colorterm.to_ascii_lowercase();
        if value.contains("truecolor") || value.contains("24bit") {
            return ColorDepth::TrueColor;
        }
    }
    if let Some(term) = get("TERM") {
        if term.to_ascii_lowercase().contains("256color") {
            return ColorDepth::C256;
        }
    }
    ColorDepth::Basic
}

/// 渲染路径入口：用 `std::env::var` 包装 `detect_color_depth`。
pub fn color_depth() -> ColorDepth {
    detect_color_depth(|key| std::env::var(key).ok())
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
        assert!(split(area, false, DEFAULT_DETAILS_WIDTH).details.is_none());
        assert!(split(Rect::new(0, 0, 80, 20), true, DEFAULT_DETAILS_WIDTH)
            .details
            .is_none());
        assert!(split(area, true, DEFAULT_DETAILS_WIDTH).details.is_some());
    }

    #[test]
    fn details_width_is_parametrized_and_clamped() {
        // 默认 45 列可调；宽度 clamp 30–60（Notes/04 §1）。
        let area = Rect::new(0, 0, 160, 30);
        let details = split(area, true, DEFAULT_DETAILS_WIDTH)
            .details
            .expect("details 显示");
        assert_eq!(details.width, DEFAULT_DETAILS_WIDTH);
        let too_small = split(area, true, 10).details.expect("clamp 下限 30");
        assert_eq!(too_small.width, 30);
        let too_large = split(area, true, 200).details.expect("clamp 上限 60");
        assert_eq!(too_large.width, 60);
        let custom = split(area, true, 36).details.expect("custom 36");
        assert_eq!(custom.width, 36);
        // <120 列自动收起。
        assert!(split(Rect::new(0, 0, 110, 24), true, DEFAULT_DETAILS_WIDTH)
            .details
            .is_none());
    }

    #[test]
    fn status_area_reserves_two_rows_for_hint_line() {
        let areas = split(Rect::new(0, 0, 140, 20), false, DEFAULT_DETAILS_WIDTH);
        assert_eq!(areas.status.height, 2);
        assert_eq!(areas.status.y + areas.status.height, 20);
    }

    #[test]
    fn color_depth_detects_environment_variants() {
        fn env(entries: Vec<(&'static str, &'static str)>) -> impl Fn(&str) -> Option<String> {
            move |key: &str| {
                entries
                    .iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, v)| v.to_string())
            }
        }
        assert_eq!(
            detect_color_depth(env(vec![("COLORTERM", "truecolor")])),
            ColorDepth::TrueColor
        );
        assert_eq!(
            detect_color_depth(env(vec![("COLORTERM", "24bit")])),
            ColorDepth::TrueColor
        );
        assert_eq!(
            detect_color_depth(env(vec![("COLORTERM", "TRUECOLOR")])),
            ColorDepth::TrueColor,
            "COLORTERM 匹配不区分大小写"
        );
        assert_eq!(
            detect_color_depth(env(vec![("TERM", "xterm-256color")])),
            ColorDepth::C256
        );
        assert_eq!(
            detect_color_depth(env(vec![("TERM", "xterm")])),
            ColorDepth::Basic
        );
        assert_eq!(detect_color_depth(env(vec![])), ColorDepth::Basic);
        // COLORTERM 存在但非 truecolor 时不短路，仍按 TERM 判定。
        assert_eq!(
            detect_color_depth(env(vec![("COLORTERM", "1"), ("TERM", "screen-256color")])),
            ColorDepth::C256
        );
    }
}
