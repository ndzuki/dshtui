//! Trajectory 视图渲染（REQ-005 V0.3，Notes/05 §7/§8 线框；D-22 非像素
//! 复刻，05 §7/§8 原型为准）。
//!
//! Step 3 先落地模式机与状态；此处为占位渲染（`render_placeholder`），
//! Step 5 交付完整事件表/折叠/详情面板/搜索高亮。

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::app::AppState;

/// Step 3 占位渲染：展示 Trajectory 状态概览（后续 Step 5 替换为事件表）。
pub fn render_placeholder(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    let window = app
        .active_session
        .as_ref()
        .and_then(|id| app.traj_sessions.get(&id.0));
    let lines: Vec<Line> = match window {
        Some(w) => {
            let view = w.view(&app.traj.fold);
            let mut lines: Vec<Line> = Vec::new();
            lines.push(Line::from(Span::styled(
                format!(
                    "Trajectory — rows {} (cursor {}), detail {}",
                    view.len(),
                    app.traj.cursor,
                    if app.traj.detail_open {
                        "open"
                    } else {
                        "closed"
                    }
                ),
                Style::default().fg(Color::DarkGray),
            )));
            for (i, row) in view.iter().enumerate() {
                let marker = if i == app.traj.cursor { "▶" } else { " " };
                let kind = match row.kind() {
                    crate::model::TrajKind::TurnStart => "turn/start",
                    crate::model::TrajKind::TurnEnd => "turn/end",
                    crate::model::TrajKind::StepStart => "step/start",
                    crate::model::TrajKind::StepEnd => "step/end",
                    crate::model::TrajKind::UserMessage => "user/message",
                    crate::model::TrajKind::AssistantMessage => "assistant/message",
                    crate::model::TrajKind::ToolCall => "tool/call",
                    crate::model::TrajKind::ToolResult => "tool/result",
                    crate::model::TrajKind::RequestHeader => "request/header",
                    crate::model::TrajKind::Compaction => "compaction",
                    crate::model::TrajKind::Unknown => "unknown",
                };
                let summary: String = row.summary().chars().take(48).collect();
                lines.push(Line::from(Span::raw(format!(
                    "{marker} {kind:<18} {summary}"
                ))));
            }
            lines
        }
        None => vec![Line::from("No active session")],
    };
    let panel =
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Trajectory "));
    frame.render_widget(panel, area);
}
