//! Trajectory 视图渲染（REQ-005 V0.3；Notes/05 §7 线框为准，D-22 非像素
//! 复刻）。事件表三列（时间/耗时/摘要）、turn/assistant 折叠 `▸/▾`、轨迹
//! 内过滤 overlay（本地 nucleo）、选中行高亮。详情面板见 `ui/detail.rs`。

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::app::{AppState, Mode};
use crate::model::{
    kind_label, FoldState, GroupId, RowId, TrajKind, TrajectoryRow, TrajectoryWindow,
};

use super::format_hhmm;

/// 当前活跃轨迹窗口（无会话 → None）。
fn traj_window(app: &AppState) -> Option<&TrajectoryWindow> {
    app.active_session
        .as_ref()
        .and_then(|id| app.traj_sessions.get(&id.0))
}

/// 折叠标记：turn/assistant 组首行前缀（▸ 折叠可展开 / ▾ 展开可折叠）。
fn fold_marker(row: &TrajectoryRow, fold: &FoldState) -> Option<&'static str> {
    match row.kind() {
        TrajKind::TurnStart => row.turn().map(|t| {
            if fold.is_collapsed(GroupId::Turn(t)) {
                "▸"
            } else {
                "▾"
            }
        }),
        TrajKind::AssistantMessage => match (row.turn(), row.step()) {
            (Some(t), Some(s)) => Some(
                if fold.is_collapsed(GroupId::Assistant { turn: t, step: s }) {
                    "▸"
                } else {
                    "▾"
                },
            ),
            _ => None,
        },
        _ => None,
    }
}

/// 事件行 kind 标签颜色（Notes/05 §7 语义色）。
fn kind_color(kind: TrajKind) -> Color {
    match kind {
        TrajKind::ToolCall => Color::Cyan,
        TrajKind::ToolResult => Color::Green,
        TrajKind::UserMessage => Color::Yellow,
        TrajKind::RequestHeader | TrajKind::Compaction | TrajKind::Unknown => Color::DarkGray,
        _ => Color::Gray,
    }
}

/// 构建一事件行（时间/耗时/摘要；折叠标记 + kind 标签 + 摘要）。
fn event_line(row: &TrajectoryRow, fold: &FoldState, window: &TrajectoryWindow) -> Line<'static> {
    let marker = fold_marker(row, fold).unwrap_or("  ");
    let time = row.time().map(format_hhmm).unwrap_or_default();
    let kind = kind_label(row.kind());
    let kind_span = Span::styled(
        format!("{kind:<12}"),
        Style::default().fg(kind_color(row.kind())),
    );
    // tool/result 行耗时推导（同 callId tool/call → 展示耗时，Notes/05 §7
    // `✓ 12ms` 排障口径；展示层推导，非状态条官方投影）。
    let duration = match row {
        TrajectoryRow::ToolResult { call_id, time, .. } => {
            let started = call_id.as_deref().and_then(|cid| {
                window.raw_rows().find_map(|r| match r {
                    TrajectoryRow::ToolCall {
                        call_id: cc, time, ..
                    } if cc.as_deref() == Some(cid) => *time,
                    _ => None,
                })
            });
            match (started, *time) {
                (Some(s), Some(t)) if t >= s => Some(format!("{}ms", t - s)),
                _ => None,
            }
        }
        _ => None,
    };
    // 事件行摘要：tool/call 显示 `name + args 摘要`（Notes/05 §7 行）；
    // 其余行用行模型 summary。
    let summary: String = match row {
        TrajectoryRow::ToolCall { name, args_raw, .. } => {
            let name = name.clone().unwrap_or_else(|| "tool".into());
            let mut s = name;
            if let Some(args) = args_raw {
                let arg_text = match args {
                    serde_json::Value::String(raw) => raw.clone(),
                    other => other.to_string(),
                };
                if !arg_text.is_empty() {
                    s.push(' ');
                    s.push_str(&arg_text);
                }
            }
            s.chars().take(60).collect()
        }
        _ => row.summary().chars().take(60).collect(),
    };
    let mut spans = vec![
        Span::raw(format!("{marker} {time}  ")),
        kind_span,
        Span::styled(
            format!("{} ", summary),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ];
    if let Some(d) = duration {
        spans.push(Span::styled(
            format!("✓ {d}"),
            Style::default().fg(if row.is_error() {
                Color::Red
            } else {
                Color::Green
            }),
        ));
    }
    if row.is_error() {
        if let Some(err) = row.error() {
            spans.push(Span::styled(
                format!(" ✗ {}:{}", err.name, err.code),
                Style::default().fg(Color::Red),
            ));
        }
    }
    Line::from(spans)
}

/// 事件表中心区渲染（Step 5：完整三列 + 折叠 + 选中高亮 + 过滤条）。
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    let Some(window) = traj_window(app) else {
        let body = Paragraph::new("No active session. Press f to choose a session.")
            .block(Block::default().borders(Borders::ALL).title(" Trajectory "));
        frame.render_widget(body, area);
        return;
    };
    // 过滤命中（AC-005-05 高亮）：命中行在完整列表原位高亮。
    let hit_rows: std::collections::HashSet<RowId> = if app.traj.filter.open {
        app.traj_search_index
            .query(&app.traj.filter.query)
            .into_iter()
            .filter_map(|m| app.traj_search_index.items().get(m.item_index))
            .map(|h| h.row_id)
            .collect()
    } else {
        std::collections::HashSet::new()
    };

    let view = window.view(&app.traj.fold);
    let mut lines: Vec<Line> = Vec::with_capacity(view.len());
    for (i, row) in view.iter().enumerate() {
        let base = event_line(row, &app.traj.fold, window);
        let is_cursor = i == app.traj.cursor && app.mode == Mode::Trajectory;
        let is_hit = hit_rows.contains(&row.id());
        let line = if is_cursor {
            // 选中行反色高亮（光标行）。
            Line::from(
                base.spans
                    .into_iter()
                    .map(|s| s.patch_style(Style::default().bg(Color::DarkGray)))
                    .collect::<Vec<_>>(),
            )
        } else if is_hit {
            Line::from(
                base.spans
                    .into_iter()
                    .map(|s| s.patch_style(Style::default().fg(Color::Cyan)))
                    .collect::<Vec<_>>(),
            )
        } else {
            base
        };
        lines.push(line);
    }
    let title = if app.traj.detail_open {
        " Trajectory • detail "
    } else {
        " Trajectory "
    };
    let paragraph = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(title))
        .scroll(((traj_scroll_top(app, area) as u16), 0));
    frame.render_widget(paragraph, area);

    // 过滤 overlay：底部输入条（query + 命中数）。
    if app.traj.filter.open {
        render_filter_bar(frame, area, app);
    }
}

/// 视口滚动：cursor 居中（选中行始终可见；纯函数，无持久 scroll 状态）。
fn traj_scroll_top(app: &AppState, area: Rect) -> usize {
    let height = area.height.saturating_sub(2) as usize;
    app.traj.cursor.saturating_sub(height.saturating_div(2))
}

/// 过滤输入条（底部一行，仿 composer overlay）。
fn render_filter_bar(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    if area.height < 3 {
        return;
    }
    let bar_area = Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(area.height.saturating_sub(2)),
        width: area.width.saturating_sub(2).max(1),
        height: 1,
    };
    let hits = app.traj_search_index.query(&app.traj.filter.query);
    let text = format!(
        "/ {}   {}/{} matches   [Enter]跳转 [q/Esc]退出",
        app.traj.filter.query,
        if hits.is_empty() {
            0
        } else {
            (app.traj.filter.cursor.min(hits.len() - 1)) + 1
        },
        hits.len()
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            text,
            Style::default().fg(Color::Black).bg(Color::Cyan),
        ))),
        bar_area,
    );
}
