//! Skills catalog (REQ-007 AC-007-18).
//!
//! Read-only directory from `skills/list(sessionId)`: name/description/
//! whenToUse/modelInvocable. Copy reference `/name` (yank); execution goes
//! through the existing `/` slash entry (skills and commands are two
//! independent registries — no fake execute here).

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::AppState;
use crate::ui::theme::Role;

/// Skills 目录渲染（`:skills`；Mode::Skills）。
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    if app.mode != crate::app::Mode::Skills || !app.skills.visible {
        return;
    }
    let sk = &app.skills;
    let accent = app.palette.color(Role::Accent);
    let warn = app.palette.color(Role::Warn);
    let sel = app.palette.color(Role::Selection);

    let width = area.width.saturating_sub(4).min(74);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let height = area.height.saturating_sub(2).min(18);
    let y = area.y + area.height.saturating_sub(height) / 2;
    let overlay = Rect::new(x, y, width, height);
    frame.render_widget(Clear, overlay);

    let status = if sk.loading {
        " 加载中…".to_string()
    } else if let Some(code) = &sk.last_error_code {
        format!(" ✗ {code} ")
    } else {
        String::new()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" Skills{status} "));
    frame.render_widget(block, overlay);
    let inner = Rect::new(
        overlay.x + 1,
        overlay.y + 1,
        width.saturating_sub(2),
        height.saturating_sub(2),
    );

    if sk.items.is_empty() {
        let msg = if sk.loading {
            " 加载中…"
        } else if sk.last_error_code.is_some() {
            " skills/list 失败——q/Esc 关闭"
        } else {
            " 无可用 skills"
        };
        let empty = Paragraph::new(Line::from(Span::styled(msg, Style::default().fg(warn))));
        frame.render_widget(empty, inner);
        return;
    }

    let items: Vec<ListItem> = sk
        .items
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let style = if i == sk.selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(sel)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Reset)
            };
            let mi = if e.model_invocable { "🔧" } else { "📄" };
            let text = format!("{mi} /{}  {}", e.name, e.description);
            let mut spans = vec![Span::styled(text, style)];
            if let Some(w) = &e.when_to_use {
                spans.push(Span::styled(
                    format!("  当: {w}"),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let mut list_state = ListState::default();
    list_state.select(Some(sk.selected));
    let list = List::new(items).highlight_style(
        Style::default()
            .fg(Color::Black)
            .bg(sel)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_stateful_widget(list, inner, &mut list_state);

    let hint = Paragraph::new(Line::from(Span::styled(
        " [j/k]移动 [y]复制 /name（执行走 / 斜杠） [q]关闭",
        Style::default().fg(accent),
    )))
    .wrap(Wrap { trim: false });
    let hint_area = Rect::new(
        inner.x,
        inner.y + inner.height.saturating_sub(1),
        inner.width,
        1,
    );
    frame.render_widget(hint, hint_area);
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn skills_panel_renders_catalog_and_copy_hint() {
        let mut app = crate::app::AppState::default();
        app.mode = crate::app::Mode::Skills;
        app.skills.open();
        app.skills.set_items(vec![
            crate::api::types::SkillEntry {
                name: "bash".into(),
                description: "执行 shell".into(),
                when_to_use: None,
                model_invocable: true,
            },
            crate::api::types::SkillEntry {
                name: "research".into(),
                description: "调研查证".into(),
                when_to_use: Some("需要查证".into()),
                model_invocable: false,
            },
        ]);
        let backend = TestBackend::new(110, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        let text = rendered_text(&terminal);
        assert!(text.contains("Skills"), "标题, text={text}");
        assert!(text.contains("/bash"));
        assert!(text.contains("执行 shell"));
        assert!(text.contains("/research"));
        assert!(text.contains("[y]复制 /name"), "复制提示, text={text}");
    }

    #[test]
    fn skills_panel_empty_and_hidden() {
        let app = crate::app::AppState::default();
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        assert!(!rendered_text(&terminal).contains("Skills"));
        let mut app = crate::app::AppState::default();
        app.mode = crate::app::Mode::Skills;
        app.skills.open();
        app.skills.set_items(vec![]);
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        assert!(rendered_text(&terminal).contains("无可用 skills"));
    }
}
