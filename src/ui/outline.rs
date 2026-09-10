//! turnOutline 大纲列表 overlay（REQ-003 FR-003-04、AC-003-09；D-19 `O`
//! 独立键）。列表项来自官方 turnOutline 投影（`{turn,seq,prompt,response}`），
//! 选中 Enter → loadThrough(seq) 落位。只读 AppState。

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::app::AppState;

/// 渲染大纲列表（`O` 打开时）。
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    if !app.outline.open {
        return;
    }
    let overlay = crate::ui::centered_rect(area, 60, 70);
    frame.render_widget(Clear, overlay);
    let mut lines: Vec<Line<'static>> = Vec::new();

    let Some(window) = app.active_window() else {
        lines.push(Line::from(Span::styled(
            "无打开的会话",
            Style::default().fg(Color::DarkGray),
        )));
        frame.render_widget(
            Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Outline ")),
            overlay,
        );
        return;
    };
    let outline = window.turn_outline();
    if outline.is_empty() {
        lines.push(Line::from(Span::styled(
            "无轮次（turnOutline 投影为空）",
            Style::default().fg(Color::DarkGray),
        )));
    }
    for (idx, item) in outline.iter().enumerate() {
        let turn = item
            .turn
            .map(|t| t.to_string())
            .unwrap_or_else(|| "-".into());
        let seq = item
            .seq
            .map(|s| s.get().to_string())
            .unwrap_or_else(|| "-".into());
        let prompt: String = item
            .prompt
            .as_deref()
            .unwrap_or("")
            .chars()
            .take(48)
            .collect();
        let mut spans = vec![
            Span::styled(format!("{turn:>3} "), Style::default().fg(Color::Cyan)),
            Span::styled(format!("[{seq}] "), Style::default().fg(Color::DarkGray)),
            Span::raw(prompt.replace('\n', " ")),
        ];
        if idx == app.outline.selection {
            spans.insert(0, Span::styled(">", Style::default().fg(Color::Yellow)));
        }
        let style = if idx == app.outline.selection {
            Style::default().fg(Color::Black).bg(Color::Yellow)
        } else {
            Style::default()
        };
        lines.push(Line::from(spans).style(style));
    }
    lines.push(Line::from(Span::styled(
        "j/k 选择  Enter 跳转  Esc 关闭",
        Style::default().fg(Color::DarkGray),
    )));

    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Outline ")),
        overlay,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::types::{SessionId, SessionSeq};
    use crate::app::AppState;
    use crate::model::Incoming;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn rendered_text(terminal: &Terminal<TestBackend>) -> String {
        let mut skip = 0usize;
        let mut out = String::new();
        for cell in terminal.backend().buffer().content() {
            if skip == 0 && !cell.skip {
                out.push_str(cell.symbol());
            }
            skip = std::cmp::max(skip, ratatui::text::Span::raw(cell.symbol()).width())
                .saturating_sub(1);
        }
        out
    }

    #[test]
    fn outline_lists_turn_outline_items_with_prompt() {
        let mut app = AppState::default();
        app.active_session = Some(SessionId::new("s1".into()));
        app.outline.open = true;
        app.sessions.touch("s1", 20).apply(Incoming::Snapshot {
            cursor: None,
            records: vec![],
            has_more: false,
            projections: Some(serde_json::json!({
                "turnOutline": [
                    {"turn": 1, "seq": 3, "prompt": "第一轮问题"},
                    {"turn": 2, "seq": 8, "prompt": "第二轮问题"}
                ]
            })),
        });
        let backend = TestBackend::new(80, 14);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("第一轮问题"), "text={rendered}");
        assert!(rendered.contains("第二轮问题"), "text={rendered}");
        assert!(
            rendered.contains("[3]") && rendered.contains("[8]"),
            "text={rendered}"
        );
        assert!(rendered.contains("Enter"), "text={rendered}");
    }

    #[test]
    fn outline_empty_projection_shows_hint() {
        let mut app = AppState::default();
        app.active_session = Some(SessionId::new("s1".into()));
        app.outline.open = true;
        app.sessions.touch("s1", 20).apply(Incoming::Snapshot {
            cursor: None,
            records: vec![],
            has_more: false,
            projections: Some(serde_json::json!({})),
        });
        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("无轮次"), "text={rendered}");
    }

    #[test]
    fn outline_items_come_from_official_turn_outline_projection() {
        // turnOutline 投影 → TranscriptWindow::turn_outline()（缺失即空，不臆造）。
        let mut app = AppState::default();
        app.active_session = Some(SessionId::new("s1".into()));
        app.sessions.touch("s1", 20).apply(Incoming::Snapshot {
            cursor: None,
            records: vec![],
            has_more: false,
            projections: Some(serde_json::json!({
                "turnOutline": [
                    {"turn": 2, "seq": 9, "prompt": "q", "response": "r"}
                ]
            })),
        });
        let outline = app
            .active_window()
            .expect("窗口存在")
            .turn_outline()
            .to_vec();
        assert_eq!(outline.len(), 1);
        assert_eq!(outline[0].turn, Some(2));
        assert_eq!(outline[0].seq, Some(SessionSeq::new(9)));
        assert_eq!(outline[0].prompt.as_deref(), Some("q"));
    }
}
