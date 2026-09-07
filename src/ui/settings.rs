//! Settings panel (REQ-007 FR-007-03 half; AC-007-15/16).
//!
//! `settings/describe` namespace tree shown read-only (flattened whitelist
//! scalar rows); Enter edits a whitelisted scalar (buffer sub-stage), Enter
//! commits via `settings/update` with expectedRevision CAS; secrets only show
//! masked set-state (安全边界). applies:restart read for guidance.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::AppState;
use crate::ui::theme::Role;

/// Settings 面板渲染（`:settings`；Mode::Settings）。
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    if app.mode != crate::app::Mode::Settings || !app.settings.visible {
        return;
    }
    let st = &app.settings;
    let accent = app.palette.color(Role::Accent);
    let warn = app.palette.color(Role::Warn);
    let err = app.palette.color(Role::Error);
    let sel = app.palette.color(Role::Selection);

    let width = area.width.saturating_sub(4).min(74);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let height = area.height.saturating_sub(2).min(18);
    let y = area.y + area.height.saturating_sub(height) / 2;
    let overlay = Rect::new(x, y, width, height);
    frame.render_widget(Clear, overlay);

    let status = if st.loading {
        " 加载中…".to_string()
    } else if let Some(code) = &st.last_error_code {
        format!(" ✗ {code} ")
    } else {
        String::new()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" Settings{status} "));
    frame.render_widget(block, overlay);
    let inner = Rect::new(
        overlay.x + 1,
        overlay.y + 1,
        width.saturating_sub(2),
        height.saturating_sub(2),
    );

    // 编辑子阶段：单独输入行。
    if let Some(key) = &st.edit_key {
        let prompt = Paragraph::new(vec![
            Line::from(Span::styled(
                format!(" 编辑 {key}（Enter 提交 / Esc 取消）"),
                Style::default().fg(accent),
            )),
            Line::from(Span::styled(
                format!(" ▸ {}", st.edit_buffer),
                Style::default().fg(Color::White),
            )),
        ])
        .wrap(Wrap { trim: false });
        frame.render_widget(prompt, inner);
        return;
    }

    if st.rows.is_empty() {
        let msg = if st.loading {
            " 加载中…"
        } else if st.last_error_code.is_some() {
            " describe 失败——按 q/Esc 关闭"
        } else {
            " 无白名单可编辑项（只读展示已折叠）"
        };
        let empty = Paragraph::new(Line::from(Span::styled(msg, Style::default().fg(warn))));
        frame.render_widget(empty, inner);
        return;
    }

    let items: Vec<ListItem> = st
        .rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let style = if i == st.selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(sel)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Reset)
            };
            let marker = if r.user_set { "●" } else { "○" };
            let editable = st.writable && !r.secret;
            let text = format!(
                "{marker} {} = {}",
                r.key,
                if r.secret {
                    "••• (set)".to_string()
                } else {
                    r.value_display.clone()
                }
            );
            let mut spans = vec![Span::styled(text, style)];
            if editable {
                spans.push(Span::styled("  [编辑]", Style::default().fg(accent)));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let mut list_state = ListState::default();
    list_state.select(Some(st.selected));
    let list = List::new(items).highlight_style(
        Style::default()
            .fg(Color::Black)
            .bg(sel)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_stateful_widget(list, inner, &mut list_state);

    let hint = Paragraph::new(Line::from(Span::styled(
        " [j/k]移动 [Enter]编辑白名单值 [q]关闭 · secret 只显示 set 状态",
        Style::default().fg(if st.writable { accent } else { warn }),
    )));
    let hint_area = Rect::new(
        inner.x,
        inner.y + inner.height.saturating_sub(1),
        inner.width,
        1,
    );
    frame.render_widget(hint, hint_area);
    let _ = err;
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
    fn settings_panel_renders_rows_edit_hint_and_empty() {
        let mut app = crate::app::AppState::default();
        app.mode = crate::app::Mode::Settings;
        app.settings.open();
        app.settings.set_rows(
            vec![crate::model::SettingsRow {
                key: "locale.preference".into(),
                namespace: "locale".into(),
                value_display: "zh-CN".into(),
                user_set: true,
                secret: false,
                revision: 7,
            }],
            true,
        );
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        let text = rendered_text(&terminal);
        assert!(text.contains("Settings"), "标题, text={text}");
        assert!(text.contains("locale.preference"));
        assert!(text.contains("zh-CN"));
        assert!(text.contains("[编辑]"));
        assert!(
            text.contains("secret 只显示 set 状态"),
            "安全边界提示, text={text}"
        );

        // 空态。
        let mut app = crate::app::AppState::default();
        app.mode = crate::app::Mode::Settings;
        app.settings.open();
        app.settings.set_rows(vec![], true);
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        assert!(rendered_text(&terminal).contains("无白名单可编辑项"));
    }
}
