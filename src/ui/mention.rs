//! @-mention candidate overlay (REQ-007 AC-007-23; wire correction: official
//! @ candidates = fileReferences (files/dirs) + sessionReferenceResolver
//! (sessions) — models/slash live in the composer `/` menu / command palette).
//!
//! Pure render of `MentionState`: query line + merged candidate list
//! (file/directory and session rows), kind badge, selected highlight.
//! Local nucleo-style filtering is in the model; this file never fetches.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use crate::app::AppState;
use crate::model::mention::MentionKind;
use crate::ui::theme::Role;

/// @ 面板渲染（composer INSERT `@` 触发；Mode::Mention）。
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    if app.mode != crate::app::Mode::Mention || !app.mention.active {
        return;
    }
    let m = &app.mention;
    let palette = &app.palette;
    let accent = palette.color(Role::Accent);
    let sel = palette.color(Role::Selection);
    // 中部偏下小面板（不影响输入行）。
    let width = area.width.min(60);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(12).min(area.height / 2);
    let overlay = Rect::new(x, y, width, area.height.min(12));
    frame.render_widget(Clear, overlay);

    let rows = m.filtered();
    // 首行：query 输入（本地过滤命中数）。
    let hit = if rows.is_empty() {
        "0".to_string()
    } else {
        rows.len().to_string()
    };
    let err = m.last_error_code.as_deref().unwrap_or("");
    let title = format!(" @ 提及 · {hit} 命中 {err}");
    let header = Paragraph::new(format!("@{}", m.query))
        .style(Style::default().fg(accent))
        .block(
            Block::default()
                .borders(Borders::TOP)
                .title(format!(" {title} ")),
        );
    let header_area = Rect::new(overlay.x, overlay.y, overlay.width, 2);
    frame.render_widget(header, header_area);

    if rows.is_empty() {
        let empty = Paragraph::new(Line::from(Span::styled(
            if m.loading {
                " 加载中…"
            } else if m.last_error_code.is_some() {
                " 候选拉取失败——可直接手动输入（不崩）"
            } else {
                " 无候选；继续输入或 Esc 返回"
            },
            Style::default().fg(palette.color(Role::Warn)),
        )));
        let empty_area = Rect::new(overlay.x, overlay.y + 2, overlay.width, 1);
        frame.render_widget(empty, empty_area);
        return;
    }

    let items: Vec<ListItem> = rows
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let (badge, color) = match c.kind {
                MentionKind::File => ("📄", Color::Cyan),
                MentionKind::Session => ("🔶", Color::Magenta),
            };
            let style = if i == m.selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(sel)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(color)
            };
            let text = format!("{badge} {}", c.display);
            ListItem::new(Line::from(Span::styled(text, style)))
        })
        .collect();
    let mut list_state = ListState::default();
    list_state.select(Some(m.selected));
    let list = List::new(items)
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(sel)
                .add_modifier(Modifier::BOLD),
        )
        .block(Block::default().borders(Borders::BOTTOM));
    let list_area = Rect::new(
        overlay.x,
        overlay.y + 2,
        overlay.width,
        overlay.height.saturating_sub(2),
    );
    frame.render_stateful_widget(list, list_area, &mut list_state);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::types::FileReferenceCandidate;
    use crate::app::Mode;
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
    fn mention_overlay_renders_candidates_and_empty_state_ac007_23() {
        let mut app = crate::app::AppState::default();
        app.mode = Mode::Mention;
        // 有候选。
        app.mention.activate();
        app.mention.set_query("src".into());
        app.mention.set_candidates(
            app.mention.generation,
            vec![FileReferenceCandidate {
                path: "src/api/mod.rs".into(),
                kind: "file".into(),
            }],
            vec![],
        );
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        let text = rendered_text(&terminal);
        assert!(text.contains("提及"), "面板标题, text={text}");
        assert!(text.contains("src/api/mod.rs"), "候选行, text={text}");

        // 空候选 + 失败 → 可读降级提示（不崩）。
        let mut app = crate::app::AppState::default();
        app.mode = Mode::Mention;
        app.mention.activate();
        app.mention.last_error_code = Some("transport".into());
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        let text = rendered_text(&terminal);
        assert!(text.contains("手动输入"), "失败降级提示, text={text}");
    }

    #[test]
    fn mention_overlay_hidden_when_not_active() {
        let app = crate::app::AppState::default();
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        let text = rendered_text(&terminal);
        assert!(!text.contains("提及"), "未激活不渲染, text={text}");
    }
}
