//! APPROVAL 弹窗（REQ-003 FR-003-05、Notes/05 §9、D-18）：转发的
//! `approval/request` 事件到达强制进入；y/n/q/Esc 决策，a 显示指引；
//! 不可编程降级（AC-003-18）时只提示 `等待审批`，不阻塞。只读 AppState。

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::app::AppState;

/// 渲染审批弹窗（仅 visible 时；AC-003-18 降级 waiting_hint 走状态条，
/// 弹窗不出现也不阻塞）。
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    if !app.approval.visible {
        return;
    }
    let overlay = crate::ui::centered_rect(area, 70, 50);
    frame.render_widget(Clear, overlay);
    let mut lines: Vec<Line<'static>> = Vec::new();

    let Some(event) = &app.approval.event else {
        // 无载荷（异常态）：不渲染弹窗，避免阻塞。
        return;
    };
    // 展示字段以 dsh-user-approval types 为准 `[未验证]`：agent/工具身份、
    // reason、command 等从原始载荷宽松提取，缺失不报错。
    if let Some(summary) = approval_summary(&event.raw) {
        for (label, value) in summary {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{label}: "),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(value),
            ]));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(
            "[y]",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" 允许本次  "),
        Span::styled(
            "[n]",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" 拒绝  "),
        Span::styled(
            "[q/Esc]",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" 中止  "),
        Span::styled(
            "[a]",
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" 始终允许指引"),
    ]));

    if app.approval.reply_inflight {
        lines.push(Line::from(Span::styled(
            "回复中…",
            Style::default().fg(Color::DarkGray),
        )));
    }
    if let Some(outcome) = app.approval.last_outcome {
        lines.push(Line::from(Span::styled(
            format!("上次决策: {}", outcome.as_str()),
            Style::default().fg(Color::DarkGray),
        )));
    }

    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow))
                .title(" Approval "),
        ),
        overlay,
    );
}

/// 从原始审批载荷宽松提取可读字段（`[未验证]` 形状；缺失即省略）。
fn approval_summary(raw: &serde_json::Value) -> Option<Vec<(String, String)>> {
    if !raw.is_object() {
        return None;
    }
    let mut out = Vec::new();
    let get_str = |keys: &[&str]| -> Option<String> {
        keys.iter()
            .find_map(|k| raw.get(k))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    let agent = raw
        .get("agent")
        .or_else(|| raw.get("event").and_then(|e| e.get("agent")));
    if let Some(agent) = agent {
        if let Some(name) = agent
            .get("name")
            .or_else(|| agent.get("kind"))
            .and_then(|v| v.as_str())
        {
            out.push(("agent".into(), name.to_string()));
        }
    }
    if let Some(reason) = get_str(&["reason", "summary"]) {
        out.push(("reason".into(), reason));
    }
    if let Some(cmd) = get_str(&["command", "tool"]) {
        out.push(("command".into(), cmd));
    }
    if out.is_empty() {
        // 退路：展示类型行（如 approval/request），保证弹窗非空。
        out.push(("request".into(), get_str(&["type"]).unwrap_or_default()));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::types::ApprovalEvent;
    use crate::app::{AppState, Mode};
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
    fn approval_modal_shows_summary_and_decision_keys_ac003_07() {
        let mut app = AppState::default();
        app.mode = Mode::Approval;
        app.approval.visible = true;
        app.approval.event = Some(ApprovalEvent {
            client_id: "c-1".into(),
            event_id: "e-1".into(),
            raw: serde_json::json!({
                "type": "approval/request",
                "agent": {"kind": "tool", "name": "bash"},
                "reason": "执行部署命令"
            }),
        });
        let backend = TestBackend::new(80, 16);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("bash"), "text={rendered}");
        assert!(rendered.contains("执行部署命令"), "text={rendered}");
        assert!(
            rendered.contains("[y]") && rendered.contains("允许本次"),
            "text={rendered}"
        );
        assert!(
            rendered.contains("[n]") && rendered.contains("拒绝"),
            "text={rendered}"
        );
        assert!(
            rendered.contains("[q/Esc]") && rendered.contains("中止"),
            "text={rendered}"
        );
        assert!(rendered.contains("[a]"), "text={rendered}");
    }

    #[test]
    fn approval_modal_absent_when_only_waiting_hint_ac003_18() {
        // AC-003-18：不可编程审批只收缩为状态条提示，弹窗不出现。
        let mut app = AppState::default();
        app.approval.waiting_hint = true;
        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        let rendered = rendered_text(&terminal);
        assert!(!rendered.contains("[y]"), "弹窗不出现, text={rendered}");
    }
}
