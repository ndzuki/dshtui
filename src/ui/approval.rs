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
/// 弹窗不出现也不阻塞）。REQ-006（D-036）：`list_open` 时渲染队列列表
/// （j/k 移动、r 重试、A 批量）；否则渲染单条槽 + 队列摘要 + danger/ack
/// 指示 + 只读 policy 徽标。
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    if !app.approval.visible {
        return;
    }
    if app.approval.list_open {
        render_list(frame, area, app);
        return;
    }
    let overlay = crate::ui::centered_rect(area, 72, 55);
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
    // danger-full-access：第二层风险确认提示（AC-006-16）。
    if app.approval.queue.head_requires_ack() {
        lines.push(Line::from(Span::styled(
            "⚠ 危险操作：需按 [a] 确认风险后，再 [y] 允许（仍仅授权本次）",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )));
    } else if app.approval.acked {
        lines.push(Line::from(Span::styled(
            "风险已确认；仍仅授权本次（allowed-once）",
            Style::default().fg(Color::Green),
        )));
    }
    lines.push(Line::from(""));
    // 键位提示（D-036）：非 danger 项 `a` = 始终允许指引（REQ-003 语义）；
    // danger 项 `a` = 风险确认。队列摘要提示列表入口 [L]。
    let summary = app.approval.queue.summary();
    let a_hint = if app.approval.queue.head_requires_ack() {
        "确认风险"
    } else {
        "始终允许指引"
    };
    lines.push(Line::from(vec![
        Span::styled("[y]", green()),
        Span::raw(" 允许本次  "),
        Span::styled("[n]", red()),
        Span::raw(" 拒绝  "),
        Span::styled("[q/Esc]", yellow()),
        Span::raw(" 中止  "),
        Span::styled("[a]", gray()),
        Span::raw(format!(" {a_hint}  ")),
        Span::styled("[L]", cyan()),
        Span::raw(format!(
            " 列表 (队列 {} 待 / {} 失败)",
            summary.pending, summary.failed
        )),
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
    if let Some(toast) = &app.approval.toast {
        lines.push(Line::from(Span::styled(
            toast.clone(),
            Style::default().fg(Color::Yellow),
        )));
    }
    // AC-006-17：approval/policy 只读徽标（ask|never；无切换入口 D-037）。
    if let Some(policy) = app.approval.policy_display {
        lines.push(Line::from(Span::styled(
            format!("approval policy: {policy}（只读，切换请用官方 web / V0.4）"),
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

/// 审批队列列表视图（REQ-006 D-036）：active 置顶 + pending + failed；
/// 光标行以反色标识；底部提示键位。
fn render_list(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    let overlay = crate::ui::centered_rect(area, 76, 70);
    frame.render_widget(Clear, overlay);
    let items = app.approval.queue.list();
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (idx, item) in items.iter().enumerate() {
        let cursor = idx == app.approval.list_cursor;
        let status = if app.approval.queue.is_failed(&item.event.event_id) {
            "FAILED"
        } else if app
            .approval
            .event
            .as_ref()
            .is_some_and(|ev| ev.event_id == item.event.event_id)
        {
            "ACTIVE"
        } else {
            "pending"
        };
        let (label, summary_text) = item_title(&item.event.raw);
        let mut spans: Vec<Span<'static>> = Vec::new();
        if cursor {
            spans.push(Span::styled(
                "▌",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        let color = match status {
            "FAILED" => Color::Red,
            "ACTIVE" => Color::Green,
            _ => Color::Gray,
        };
        spans.push(Span::styled(
            format!("{status:>7} {label} {summary_text}"),
            Style::default().fg(color),
        ));
        if cursor {
            for s in spans.iter_mut() {
                s.style = s.style.add_modifier(Modifier::REVERSED);
            }
        }
        lines.push(Line::from(spans));
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "（无待处理审批）",
            Style::default().fg(Color::DarkGray),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("[j/k]", gray()),
        Span::raw(" 移动  "),
        Span::styled("[r]", gray()),
        Span::raw(" 重试失败项  "),
        Span::styled("[A]", yellow()),
        Span::raw(" 批量允许  "),
        Span::styled("[y/n]", gray()),
        Span::raw(" 单条决策  "),
        Span::styled("[q/Esc]", gray()),
        Span::raw(" 回单条槽（不中止）"),
    ]));

    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow))
                .title(" Approval 队列 "),
        ),
        overlay,
    );
}

/// 列表行标题：从原始载荷宽松提取（与单条槽 summary 同口径）。
fn item_title(raw: &serde_json::Value) -> (String, String) {
    let get_str = |keys: &[&str]| -> Option<String> {
        keys.iter()
            .find_map(|k| raw.get(k))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    let label = get_str(&["tool", "toolName", "command"])
        .or_else(|| {
            raw.get("agent")
                .and_then(|a| a.get("name"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "approval".into());
    let summary = get_str(&["reason", "summary"])
        .or_else(|| {
            raw.get("request")
                .and_then(|r| r.get("toolName"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .unwrap_or_default();
    (label, summary)
}

fn green() -> Style {
    Style::default()
        .fg(Color::Green)
        .add_modifier(Modifier::BOLD)
}
fn red() -> Style {
    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
}
fn yellow() -> Style {
    Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD)
}
fn gray() -> Style {
    Style::default()
        .fg(Color::Gray)
        .add_modifier(Modifier::BOLD)
}
fn cyan() -> Style {
    Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
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
