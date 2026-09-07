//! Trajectory 详情面板（右栏；REQ-005 §4/Notes/05 §8 线框，D-24 字段级）。
//!
//! - 详情文本用 Paragraph 纯文本渲染（规避已验证失败的方案：
//!   ratatui-markdown 0.3.x 渲染 API 坑，来源
//!   `uncategorized/TASK-003-pitfall-2026-09-05-ratatui-markdown-0.md`）；
//! - usage（同 step `assistant/message.usage` 推导）、timing（事件 `time`
//!   推导）以 `[未验证]` 标注推导源；缺省显示 `—`（AC-005-15）；
//! - diff 仅 `tool/result.meta.diff` 存在时展示，否则降级「无 diff」
//!   （AC-005-10）；error 与 result 并列（AC-005-03）；
//! - context breakdown（system/tools/message tokens）只读官方投影，不自算
//!   （ADR-008，REQ-J06）。

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::app::AppState;
use crate::model::{ProjectionSnapshot, TrajectoryDetail};

use super::format_hhmm;

/// 详情面板渲染（Paragraph 纯文本 + scroll）。
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    let mut lines: Vec<Line> = Vec::new();
    let mut title = " Trajectory Details ".to_string();
    if let Some(detail) = &app.traj.detail {
        title = format!(" {} ", detail.title);
        push_detail_lines(&mut lines, detail, app);
    } else {
        lines.push(Line::from(Span::styled(
            "未选中可详查事件（tool/call、tool/result、assistant）",
            Style::default().fg(Color::DarkGray),
        )));
    }
    let paragraph = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(title))
        .scroll((app.traj.detail_scroll as u16, 0));
    frame.render_widget(paragraph, area);
}

/// 逐行构建详情内容（usage/timing `[未验证]` 标注；缺省 `—`）。
fn push_detail_lines(out: &mut Vec<Line<'static>>, detail: &TrajectoryDetail, app: &AppState) {
    // args
    if let Some(args) = &detail.args_text {
        push_label(out, "args", Color::Cyan);
        for line in args.lines() {
            out.push(Line::from(line.to_string()));
        }
        out.push(Line::from(""));
    }
    // result（error 并列，AC-005-03 含错误展示）
    if let Some(result) = &detail.result_text {
        push_label(out, "result", Color::Green);
        for line in result.lines() {
            out.push(Line::from(line.to_string()));
        }
        if let Some(err) = &detail.error {
            out.push(Line::from(Span::styled(
                format!("✗ {}:{}", err.name, err.code),
                Style::default().fg(Color::Red),
            )));
        }
        out.push(Line::from(""));
    }
    // usage（同 step assistant 推导 [未验证]，AC-005-15）
    push_label(out, "usage [推导·未验证]", Color::Cyan);
    if let Some(u) = &detail.usage {
        out.push(Line::from(format!(
            "input {}  output {}  cache_read {}  cache_write {}  think {}",
            u.input.map_or("—".to_string(), |v| v.to_string()),
            u.output.map_or("—".to_string(), |v| v.to_string()),
            u.cache_read.map_or("—".to_string(), |v| v.to_string()),
            u.cache_write.map_or("—".to_string(), |v| v.to_string()),
            u.think.map_or("—".to_string(), |v| v.to_string()),
        )));
    } else {
        out.push(Line::from(Span::styled(
            "—",
            Style::default().fg(Color::DarkGray),
        )));
    }
    // timing（事件 time 推导 [未验证]）
    push_label(out, "timing [推导·未验证]", Color::Cyan);
    if let Some(t) = &detail.timing {
        let secs = t
            .time_seconds
            .map_or("—".to_string(), |v| format!("{v:.1}s"));
        let fmt = |v: Option<i64>| v.map_or("—".to_string(), format_hhmm);
        out.push(Line::from(format!(
            "started {}  duration {}  step_start {}  first_token {}  completed {}",
            fmt(t.started_at),
            secs,
            fmt(t.step_start),
            fmt(t.first_token),
            fmt(t.completed),
        )));
    } else {
        out.push(Line::from(Span::styled(
            "—",
            Style::default().fg(Color::DarkGray),
        )));
    }
    // diff（无 meta/meta 无 diff → 「无 diff」降级，AC-005-10）
    push_label(out, "diff", Color::Cyan);
    match &detail.diff {
        Some(diff) => {
            for line in diff.lines() {
                out.push(Line::from(Span::styled(
                    line.to_string(),
                    Style::default().fg(Color::Yellow),
                )));
            }
        }
        None => out.push(Line::from(Span::styled(
            "无 diff（tool/result 无 meta.diff）",
            Style::default().fg(Color::DarkGray),
        ))),
    }
    // context breakdown（官方投影，ADR-008 不自算；REQ-J06）
    let breakdown = app
        .active_session
        .as_ref()
        .and_then(|sid| app.sessions.get(&sid.0))
        .map(|w| ProjectionSnapshot::new(w.projections().clone()).context_breakdown());
    if let Some(bd) = breakdown {
        if !bd.is_empty() {
            out.push(Line::from(""));
            push_label(out, "context", Color::Magenta);
            for (key, tokens) in bd {
                out.push(Line::from(format!("{key}: {tokens} tok")));
            }
        }
    }
}

fn push_label(out: &mut Vec<Line<'static>>, label: &str, color: Color) {
    out.push(Line::from(Span::styled(
        label.to_string(),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )));
}
