//! 顶部 Tab 条（REQ-005 FR-005-01：Chat / Trajectory，`gt`/`gT`/`1`–`9`
//! 切换；Notes/04 §1 header tabs）。

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Tabs;
use ratatui::Frame;

use crate::app::{AppState, Mode};

/// 顶部 Tab 行（占 center 顶部 1 行）。当前视图高亮；Trajectory 高亮显示
/// TRAJ 也由状态条指示（D-25）。
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    let selected = if app.mode == Mode::Trajectory { 1 } else { 0 };
    let titles: Vec<Line> = [" Chat ", " Trajectory "]
        .into_iter()
        .map(|t| Line::from(Span::styled(t, Style::default())))
        .collect();
    let tabs = Tabs::new(titles)
        .select(selected)
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(tabs, area);
}
