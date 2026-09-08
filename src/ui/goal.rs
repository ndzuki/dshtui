//! Goal panel (REQ-007 FR-007-02 half; AC-007-11/12/14).
//!
//! Per-session SINGLETON goal (wire correction: no list). Shows a card with
//! objective/phase/revision/rounds; empty state offers create (input
//! sub-stage); mutations are CAS single-flight; clear needs double-confirm.
//! Pure render of `GoalPanelState`.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::AppState;
use crate::ui::theme::Role;

/// Goal 面板渲染（`:goal`；Mode::Goal）。
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    if app.mode != crate::app::Mode::Goal || !app.goals.visible {
        return;
    }
    let g = &app.goals;
    let accent = app.palette.color(Role::Accent);
    let err = app.palette.color(Role::Error);
    let warn = app.palette.color(Role::Warn);
    let ok = app.palette.color(Role::AssistantFg);

    let width = area.width.saturating_sub(4).min(64);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let height = 14.min(area.height.saturating_sub(2));
    let y = area.y + area.height.saturating_sub(height) / 2;
    let overlay = Rect::new(x, y, width, height);
    frame.render_widget(Clear, overlay);

    let block = Block::default().borders(Borders::ALL).title(" Goal ");
    let inner = Rect::new(
        overlay.x + 1,
        overlay.y + 1,
        width.saturating_sub(2),
        height.saturating_sub(2),
    );
    frame.render_widget(block, overlay);

    let mut lines: Vec<Line> = Vec::new();
    // 状态/错误行。
    let status = if let Some(code) = &g.last_error_code {
        format!("✗ {code}")
    } else if g.inflight.is_some() {
        format!("⏳ {}", g.inflight.map(|k| k.as_str()).unwrap_or(""))
    } else {
        String::new()
    };
    if !status.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(" {status}"),
            Style::default().fg(if g.last_error_code.is_some() {
                err
            } else {
                warn
            }),
        )));
    }
    if app.goal_input {
        // 输入子阶段：objective buffer。
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            " objective:",
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        )));
        let input_display = if g.create_objective.is_empty() {
            "（输入后 Enter 提交 / Esc 取消）".to_string()
        } else {
            g.create_objective.clone()
        };
        lines.push(Line::from(Span::styled(
            format!(" ▸ {input_display}"),
            Style::default().fg(Color::White),
        )));
    } else if let Some(goal) = &g.goal {
        let phase = match goal.phase {
            Some(crate::api::types::GoalPhase::Active) => ("active", ok),
            Some(crate::api::types::GoalPhase::Paused) => ("paused", warn),
            Some(crate::api::types::GoalPhase::Blocked) => ("blocked", err),
            Some(crate::api::types::GoalPhase::Complete) => ("complete", ok),
            None => ("?", warn),
        };
        lines.push(Line::from(vec![
            Span::styled(
                " objective ",
                Style::default().fg(accent).add_modifier(Modifier::BOLD),
            ),
            Span::raw(trunc(&goal.objective, 44)),
        ]));
        lines.push(Line::from(vec![
            Span::styled(
                " phase ",
                Style::default().fg(accent).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                phase.0,
                Style::default().fg(phase.1).add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("  rev {}", goal.revision)),
            Span::raw(format!("  rounds {}", goal.rounds_started.unwrap_or(0))),
        ]));
        if let Some(b) = &goal.blocked_reason {
            lines.push(Line::from(Span::styled(
                format!(" blocked: {b}"),
                Style::default().fg(err),
            )));
        }
        if let Some(m) = goal.max_goal_rounds {
            lines.push(Line::from(Span::raw(format!(" maxGoalRounds: {m}"))));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            if g.confirm_pending == Some(crate::model::GoalOpKind::Clear) {
                " clear 当前 goal？Enter 确认 / 其它键取消".to_string()
            } else {
                "[e]编辑 [p]暂停 [r]恢复 [x]完成 [d]clear(确认) [q]关闭".to_string()
            },
            Style::default().fg(if g.confirm_pending.is_some() {
                warn
            } else {
                accent
            }),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            " 无 goal（每会话单例）。",
            Style::default().fg(warn),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            if app.goal_input {
                "（输入后 Enter 提交 / Esc 取消）".to_string()
            } else {
                "[c] 创建 goal（输入 objective） [q] 关闭".to_string()
            },
            Style::default().fg(accent),
        )));
    }

    let para = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(para, inner);
}

fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let t: String = s.chars().take(n).collect();
        format!("{t}…")
    }
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
    fn goal_panel_renders_card_and_actions() {
        let mut app = crate::app::AppState::default();
        app.mode = crate::app::Mode::Goal;
        app.goals.open();
        app.goals.set_goal(
            Some(crate::model::GoalView {
                id: "g1".into(),
                revision: 3,
                objective: "交付 REQ-007".into(),
                phase: Some(crate::api::types::GoalPhase::Active),
                max_goal_rounds: Some(5),
                rounds_started: Some(2),
                ..Default::default()
            }),
            false,
        );
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        let text = rendered_text(&terminal);
        assert!(text.contains("Goal"), "标题, text={text}");
        assert!(text.contains("交付 REQ-007"));
        assert!(text.contains("active"));
        assert!(text.contains("rev 3"));
        assert!(text.contains("[p]暂停"), "操作提示, text={text}");
    }

    #[test]
    fn goal_panel_empty_state_and_hidden() {
        let mut app = crate::app::AppState::default();
        // 未激活不渲染。
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        assert!(!rendered_text(&terminal).contains("无 goal"));
        // 空态。
        app.mode = crate::app::Mode::Goal;
        app.goals.open();
        app.goal_input = false;
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        assert!(rendered_text(&terminal).contains("无 goal"));
        assert!(rendered_text(&terminal).contains("[c] 创建 goal"));
    }
}
