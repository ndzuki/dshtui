//! 模态 composer 渲染（REQ-002 Step 5）：底部 overlay、仅在 INSERT 显示、
//! 覆盖状态条上方；只读 AppState，不直接改草稿/网络。

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::AppState;

/// composer 最大高度（REQ-002 §4：1–8 行，多行自动增高）。
const MAX_HEIGHT: u16 = 8;

/// 在 body 底部、状态条上方渲染 composer overlay（INSERT 且可见时）。
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    if !app.composer.visible {
        return;
    }
    let height = composer_height(app);
    let composer_area = Rect {
        y: area.y.saturating_add(area.height.saturating_sub(height)),
        height: height.min(area.height),
        ..area
    };
    frame.render_widget(Clear, composer_area);
    let title = if app.composer.steer {
        " Composer • STEER "
    } else {
        " Composer "
    };
    frame.render_widget(
        Paragraph::new(composer_lines(app))
            .block(Block::default().borders(Borders::ALL).title(title))
            .wrap(Wrap { trim: false }),
        composer_area,
    );
}

/// 高度随草稿行数增高：内容行数 + 边框，上限 MAX_HEIGHT。
fn composer_height(app: &AppState) -> u16 {
    let text = app.draft.as_ref().map(|d| d.text.as_str()).unwrap_or("");
    let lines = text.split('\n').count().max(1);
    ((lines + 2).min(MAX_HEIGHT as usize)) as u16
}

/// 草稿内容行：光标用 ▍ 标注（V0.1 单行 + Ctrl/Alt+Enter 换行）。
fn composer_lines(app: &AppState) -> Vec<Line<'static>> {
    let Some(draft) = app.draft.as_ref() else {
        return vec![Line::from(Span::styled(
            "（空输入不发送）",
            Style::default().fg(Color::DarkGray),
        ))];
    };
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut cursor_placed = false;
    let mut char_idx = 0usize;
    for ch in draft.text.chars() {
        if !cursor_placed && char_idx == draft.cursor {
            spans.push(cursor_span());
            cursor_placed = true;
        }
        if ch == '\n' {
            lines.push(Line::from(std::mem::take(&mut spans)));
        } else {
            spans.push(Span::raw(ch.to_string()));
        }
        char_idx += 1;
    }
    if !cursor_placed && char_idx == draft.cursor {
        spans.push(cursor_span());
    }
    lines.push(Line::from(spans));
    lines
}

fn cursor_span() -> Span<'static> {
    Span::styled(
        "▍",
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::types::SessionId;
    use crate::app::AppState;
    use crate::model::DraftState;

    #[test]
    fn composer_height_grows_with_lines_and_caps_at_8() {
        let mut app = AppState::default();
        app.composer.visible = true;
        app.draft = Some(DraftState {
            text: "a".into(),
            cursor: 1,
            bound_session: SessionId("s".into()),
        });
        assert_eq!(composer_height(&app), 3, "单行：1 内容 + 2 边框");
        app.draft = Some(DraftState {
            text: "a\nb\nc\nd\ne\nf\n".into(),
            cursor: 11,
            bound_session: SessionId("s".into()),
        });
        assert_eq!(composer_height(&app), 8, "多行封顶 8");
    }

    #[test]
    fn composer_lines_place_cursor_at_char_offset() {
        let mut app = AppState::default();
        app.composer.visible = true;
        app.draft = Some(DraftState {
            text: "ab".into(),
            cursor: 1,
            bound_session: SessionId("s".into()),
        });
        let lines = composer_lines(&app);
        let line = lines[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(line.contains('▍'), "光标存在, text={line}");
        assert!(line.contains('a') && line.contains('b'), "text={line}");
        assert!(
            line.find('▍') > line.find('a') && line.find('▍') < line.find('b'),
            "光标在 a 之后 b 之前: {line}"
        );
    }
}
