//! Message action menu (REQ-007 AC-007-27/28).
//!
//! Opened from a focused user/assistant message (`m`): lists available
//! actions (assistant → feedback±; user → retry/branch), cursor move, Enter
//! executes (running turn needs a second confirm, AC-007-28). Pure render.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use crate::app::AppState;
use crate::model::MessageActionKind;
use crate::ui::theme::Role;

fn action_label(k: MessageActionKind) -> (&'static str, &'static str) {
    match k {
        MessageActionKind::Branch => ("branch", "从该消息 fork 出新分支会话"),
        MessageActionKind::Retry => ("retry", "重发该消息（新 requestId）"),
        MessageActionKind::FeedbackPositive => ("feedback+", "标记为有帮助（messageFeedback/put）"),
        MessageActionKind::FeedbackNegative => ("feedback-", "标记为需改进"),
    }
}

/// 消息动作菜单渲染（`m`；Mode::MessageAction）。
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    if app.mode != crate::app::Mode::MessageAction || !app.message_action.menu_seq.is_some() {
        return;
    }
    let accent = app.palette.color(Role::Accent);
    let warn = app.palette.color(Role::Warn);
    let sel = app.palette.color(Role::Selection);
    // 动作清单（与 reducer 可用性一致）。
    let target = app.msg_action_target.as_ref();
    let actions: Vec<MessageActionKind> = match target {
        Some(t) => match t.kind {
            crate::app::MsgTargetKind::Assistant if t.message_id.is_some() => {
                vec![
                    MessageActionKind::FeedbackPositive,
                    MessageActionKind::FeedbackNegative,
                ]
            }
            crate::app::MsgTargetKind::User => {
                let mut v = vec![MessageActionKind::Retry];
                if t.is_last_user && !t.running {
                    v.push(MessageActionKind::Branch);
                }
                v
            }
            _ => vec![],
        },
        None => vec![],
    };
    if actions.is_empty() {
        return;
    }

    let width = area.width.min(52);
    let x = area.x + area.width.saturating_sub(width) / 2;
    // D-50：本地降级标记提示额外一行（「已本地记录未提交」）。
    let marker = app.message_action.feedback_marked.as_ref();
    let height = (actions.len() as u16 + 4 + marker.is_some() as u16).min(area.height);
    let y = area.y + area.height.saturating_sub(height) / 2;
    let overlay = Rect::new(x, y, width, height);
    frame.render_widget(Clear, overlay);

    let seq = app.message_action.menu_seq.unwrap_or(0);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" 消息动作 · seq {seq} "));
    frame.render_widget(block, overlay);
    let inner = Rect::new(overlay.x + 1, overlay.y + 1, width - 2, height - 2);

    // 二次确认态：覆盖为单行确认。
    if app.message_action.confirm_running {
        let line = Paragraph::new(Line::from(Span::styled(
            "该轮正在运行——Enter 确认执行 / 其它键取消",
            Style::default().fg(warn),
        )));
        frame.render_widget(line, inner);
        return;
    }

    let items: Vec<ListItem> = actions
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let (name, desc) = action_label(*a);
            let style = if i == app.message_action.menu_cursor {
                Style::default()
                    .fg(Color::Black)
                    .bg(sel)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Reset)
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {name:<12}"), style),
                Span::styled(desc, Style::default().fg(Color::DarkGray)),
            ]))
        })
        .collect();
    let mut state = ListState::default();
    state.select(Some(app.message_action.menu_cursor));
    let list = List::new(items).highlight_style(
        Style::default()
            .fg(Color::Black)
            .bg(sel)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_stateful_widget(list, inner, &mut state);

    let hint = Paragraph::new(Line::from(Span::styled(
        " [j/k]选择 [Enter]执行 [q/Esc]关闭",
        Style::default().fg(accent),
    )));
    let hint_area = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);
    frame.render_widget(hint, hint_area);

    // D-50：本地降级标记提示（feedback 端点不可用 → 已本地记录未提交）。
    if let Some(rating) = marker {
        let mark_line = Paragraph::new(Line::from(Span::styled(
            format!(" ⚠ feedback 已本地记录（{rating}，未提交）——端点恢复后可经官方 web 补交"),
            Style::default().fg(warn),
        )));
        let mark_area = Rect::new(inner.x, inner.y + inner.height - 2, inner.width, 1);
        frame.render_widget(mark_line, mark_area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::MsgTargetKind;
    use crate::model::MessageActionState;
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
    fn message_action_menu_renders_user_and_assistant_actions() {
        let mut app = crate::app::AppState::default();
        app.mode = crate::app::Mode::MessageAction;
        app.message_action.open_menu(6);
        // user 末条静止：retry+branch。
        app.msg_action_target = Some(crate::app::MessageActionTarget {
            session_id: crate::api::types::SessionId::new("s1".into()),
            seq: 6,
            kind: MsgTargetKind::User,
            user_text: Some("再来一次".into()),
            message_id: None,
            running: false,
            is_last_user: true,
        });
        let backend = TestBackend::new(90, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        let text = rendered_text(&terminal);
        assert!(text.contains("消息动作"), "标题, text={text}");
        assert!(text.contains("retry"), "retry 动作, text={text}");
        assert!(text.contains("branch"), "branch 动作, text={text}");
        // assistant：feedback±。
        app.message_action = MessageActionState::default();
        app.message_action.open_menu(5);
        app.msg_action_target = Some(crate::app::MessageActionTarget {
            session_id: crate::api::types::SessionId::new("s1".into()),
            seq: 5,
            kind: MsgTargetKind::Assistant,
            user_text: None,
            message_id: Some("m5".into()),
            running: false,
            is_last_user: false,
        });
        let backend = TestBackend::new(90, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        let text = rendered_text(&terminal);
        assert!(text.contains("feedback+"), "text={text}");
        assert!(text.contains("feedback-"), "text={text}");
        assert!(!text.contains("branch"), "assistant 无 branch, text={text}");
    }

    #[test]
    fn message_action_menu_hidden_when_not_open() {
        let app = crate::app::AppState::default();
        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        assert!(!rendered_text(&terminal).contains("消息动作"));
    }
}
