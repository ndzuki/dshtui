//! SEARCH overlay（REQ-003 FR-003-02、Notes/05 §4）：输入行 + 窗口命中列表 +
//! 全历史命中列表（sessionId + snippet）。只读 AppState，纯渲染。

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::app::AppState;

/// 在 body 上方居中渲染搜索 overlay（仅 SEARCH 模式）。
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    if !app.search.open {
        return;
    }
    let overlay = crate::ui::centered_rect(area, 80, 70);
    frame.render_widget(Clear, overlay);
    let mut lines: Vec<Line<'static>> = Vec::new();

    // 输入行：原样查询 + 光标。
    let mut input = vec![
        Span::styled(
            "/",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(app.search.query.clone()),
        Span::styled("▍", Style::default().fg(Color::Yellow)),
    ];
    if app.search.history_loading {
        input.push(Span::styled(
            "  …searching",
            Style::default().fg(Color::DarkGray),
        ));
    }
    lines.push(Line::from(input));

    // 窗口命中列表（`kind: display` 行，cursor 高亮）。
    for (list_idx, &item_index) in app.search.window_matches.iter().enumerate() {
        let Some(item) = app.search_index.items().get(item_index) else {
            continue;
        };
        let mut spans = vec![
            Span::styled(
                format!("{} ", item.kind.label()),
                Style::default().fg(kind_color(&item.kind)),
            ),
            Span::raw(item.display.clone()),
        ];
        let marker = if list_idx == app.search.cursor {
            spans.insert(0, Span::styled(">", Style::default().fg(Color::Yellow)));
            Style::default().fg(Color::Black).bg(Color::Yellow)
        } else {
            Style::default()
        };
        lines.push(Line::from(spans).style(marker));
    }
    if !app.search.history_hits.is_empty() {
        lines.push(Line::from(Span::styled(
            "── 全历史命中 ──",
            Style::default().fg(Color::DarkGray),
        )));
        for (idx, hit) in app.search.history_hits.iter().enumerate() {
            let snippet: String = hit.snippet.chars().take(50).collect();
            let mut spans = vec![Span::raw(format!(
                "{}  {}",
                hit.session_id.0,
                snippet.replace('\n', " ")
            ))];
            if idx == app.search.history_selection {
                spans.insert(0, Span::styled(">", Style::default().fg(Color::Yellow)));
            }
            let style = if idx == app.search.history_selection {
                Style::default().fg(Color::Black).bg(Color::Yellow)
            } else {
                Style::default()
            };
            lines.push(Line::from(spans).style(style));
        }
    }

    // 底部：匹配计数 / 空查询提示 / 错误 / hasMore 提示。
    if let Some(error) = &app.search.history_error {
        lines.push(Line::from(Span::styled(
            format!("  {error}"),
            Style::default().fg(Color::Red),
        )));
    }
    if let Some(hint) = &app.search.history_hint {
        lines.push(Line::from(Span::styled(
            format!("  {hint}"),
            Style::default().fg(Color::Yellow),
        )));
    }
    let footer = match app.search.window_matches.len() {
        0 => "0 matches".to_string(),
        total => format!(
            "{}/{} matches",
            app.search.cursor.saturating_add(1).min(total),
            total
        ),
    };
    lines.push(Line::from(Span::styled(
        format!("  {footer}"),
        Style::default().fg(Color::DarkGray),
    )));

    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Search ")),
        overlay,
    );
}

fn kind_color(kind: &crate::model::SearchKind) -> Color {
    match kind {
        crate::model::SearchKind::Code { .. } => Color::Green,
        crate::model::SearchKind::Link { .. } => Color::Cyan,
        crate::model::SearchKind::Image { .. } => Color::Magenta,
        crate::model::SearchKind::ToolCall { .. } => Color::Yellow,
        crate::model::SearchKind::Text => Color::Gray,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::AppState;
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
    fn search_overlay_shows_query_matches_and_history() {
        let mut app = AppState::default();
        app.search.open = true;
        app.search.query = "/c deploy".into();
        app.search.cursor = 0;
        app.search.window_matches = vec![0];
        app.search_index
            .rebuild(&[crate::model::Block::UserMessage {
                seq: crate::api::types::SessionSeq(1),
                content: "deploy the operator".into(),
                time: None,
            }]);
        app.search.history_hits = vec![crate::api::types::SearchHit {
            session_id: crate::api::types::SessionId("sess-9".into()),
            snippet: "deploy 排查 …".into(),
        }];
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("/c deploy"), "text={rendered}");
        assert!(rendered.contains("deploy the operator"), "text={rendered}");
        assert!(rendered.contains("全历史命中"), "text={rendered}");
        assert!(rendered.contains("sess-9"), "text={rendered}");
        assert!(rendered.contains("1/1 matches"), "text={rendered}");
    }

    #[test]
    fn search_overlay_empty_query_shows_zero_matches_ac003_19() {
        let mut app = AppState::default();
        app.search.open = true;
        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("0 matches"), "text={rendered}");
    }
}
