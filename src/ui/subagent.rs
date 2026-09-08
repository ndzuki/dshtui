//! Subagent catalog panel (REQ-007 FR-007-01; AC-007-07~10).
//!
//! Read-only-ish tree of subagents under the active parent session:
//! direct children from `subagents/list` (recursion guided by hasChildren),
//! breadcrumb at the bottom, per-row activity badge, interrupt confirm line.
//! Pure render of `SubagentViewState`.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use crate::app::AppState;
use crate::ui::theme::Role;

/// Subagent 目录渲染（`:subagents`；Mode::Subagent）。
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    if app.mode != crate::app::Mode::Subagent || !app.subagents.visible {
        return;
    }
    let sub = &app.subagents;
    let accent = app.palette.color(Role::Accent);
    let sel = app.palette.color(Role::Selection);
    let warn = app.palette.color(Role::Warn);

    let width = area.width.saturating_sub(4).min(72);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let height = area.height.saturating_sub(2).min(20);
    let y = area.y + area.height.saturating_sub(height) / 2;
    let overlay = Rect::new(x, y, width, height);
    frame.render_widget(Clear, overlay);

    // 标题行：父会话 + 加载/错误。
    let parent = sub.parent_session_id.clone().unwrap_or_default();
    let status = if sub.loading {
        " 加载中…".to_string()
    } else if let Some(code) = &sub.last_error_code {
        format!(" 错误 {code}")
    } else if !sub.parent_available {
        "（父会话不可用）".to_string()
    } else {
        String::new()
    };
    let title_block = Block::default().borders(Borders::TOP).title(format!(
        " 子代理 Subagents · {}{} ",
        trunc(&parent, 30),
        status
    ));
    let rows = sub.flatten();

    if rows.is_empty() && !sub.loading {
        let empty = Paragraph::new(Line::from(Span::styled(
            " 无子代理（父会话未派生子代理；q/Esc 关闭）",
            Style::default().fg(warn),
        )))
        .block(title_block);
        let empty_area = Rect::new(overlay.x, overlay.y, overlay.width, overlay.height);
        frame.render_widget(empty, empty_area);
        return;
    }
    let items: Vec<ListItem> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let indent = "  ".repeat(r.depth.saturating_add(1));
            let icon = if r.node.has_children {
                if r.node.children.is_some() {
                    "▾"
                } else {
                    "▸"
                }
            } else {
                "•"
            };
            let activity_color = match r.node.activity.as_str() {
                "running" => Color::Green,
                "inactive" => Color::Gray,
                _ => Color::DarkGray,
            };
            let label = r.node.label.clone().unwrap_or_else(|| r.node.id.clone());
            let mode_tag = match r.node.mode.as_deref() {
                Some("continuable") => "cont",
                Some("one-shot") => "1x",
                _ => "",
            };
            let style = if i == sub.selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(sel)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Reset)
            };
            let text = format!("{indent}{icon} {label} [{mode_tag}]");
            let mut spans = vec![Span::styled(text, style)];
            spans.push(Span::styled(
                format!(" {} ", r.node.activity),
                Style::default().fg(activity_color),
            ));
            ListItem::new(Line::from(spans))
        })
        .collect();
    let mut list_state = ListState::default();
    list_state.select(Some(sub.selected));
    let list = List::new(items)
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(sel)
                .add_modifier(Modifier::BOLD),
        )
        .block(title_block);
    let list_area = Rect::new(
        overlay.x,
        overlay.y + 1,
        overlay.width,
        overlay.height.saturating_sub(3),
    );
    frame.render_stateful_widget(list, list_area, &mut list_state);

    // 底部提示/面包屑行。
    let crumb = sub.breadcrumbs().join(" / ");
    let hint = if sub.interrupt_target.is_some() {
        "Enter 确认中断 / 其它键取消".to_string()
    } else {
        format!(
            "[j/k]移动 [Enter]展开/折叠 [x]中断 [q]关闭 · {}",
            trunc(&crumb, 60)
        )
    };
    // 底部提示行（在 list block 底边框内一行渲染，无独立边框避免裁剪）。
    let hint_line = Paragraph::new(Line::from(Span::styled(
        hint,
        Style::default().fg(if sub.interrupt_target.is_some() {
            warn
        } else {
            accent
        }),
    )));
    let hint_area = Rect::new(
        overlay.x + 1,
        list_area.y + list_area.height.saturating_sub(1),
        overlay.width.saturating_sub(2),
        1,
    );
    frame.render_widget(hint_line, hint_area);
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
    fn subagent_panel_renders_tree_and_hint() {
        let mut app = crate::app::AppState::default();
        app.mode = crate::app::Mode::Subagent;
        app.subagents.open("p1");
        app.subagents.set_catalog(
            "p1",
            crate::api::types::SubagentCatalog {
                entries: vec![
                    crate::api::types::SubagentListEntry::Child {
                        id: "c1".into(),
                        activity: "running".into(),
                        has_children: true,
                        mode: Some("continuable".into()),
                        label: Some("走查".into()),
                    },
                    crate::api::types::SubagentListEntry::Child {
                        id: "c2".into(),
                        activity: "inactive".into(),
                        has_children: false,
                        mode: Some("one-shot".into()),
                        label: None,
                    },
                ],
                parent_available: true,
            },
        );
        let backend = TestBackend::new(100, 26);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        let text = rendered_text(&terminal);
        assert!(text.contains("子代理"), "标题, text={text}");
        assert!(text.contains("走查"), "label 行, text={text}");
        assert!(text.contains("c2"), "id 行, text={text}");
        assert!(text.contains("running"), "activity, text={text}");
        assert!(text.contains("x]中断"), "hint, text={text}");
    }

    #[test]
    fn subagent_panel_empty_state_and_hidden() {
        let mut app = crate::app::AppState::default();
        // 未激活不渲染。
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        assert!(!rendered_text(&terminal).contains("子代理"));
        // 激活但空目录。
        app.mode = crate::app::Mode::Subagent;
        app.subagents.open("p1");
        app.subagents.set_catalog(
            "p1",
            crate::api::types::SubagentCatalog {
                entries: vec![],
                parent_available: true,
            },
        );
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        assert!(rendered_text(&terminal).contains("无子代理"));
    }
}
