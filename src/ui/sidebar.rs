//! Session and workspace sidebar rendering.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState};
use ratatui::Frame;

use crate::api::types::{SessionId, SessionMeta};
use crate::app::AppState;

use super::format_hhmm;

/// Render the workspace/session navigator.
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    let (items, selected_index) = build_rows(app);
    let mut list_state = ListState::default();
    list_state.select(selected_index);
    let title = format!(" Sessions ({}) ", app.workspaces.session_count());
    let list = List::new(items)
        .block(Block::default().borders(Borders::RIGHT).title(title))
        .highlight_style(Style::default().bg(Color::Blue).fg(Color::White))
        .highlight_symbol("▌");
    frame.render_stateful_widget(list, area, &mut list_state);
}

/// 构建渲染行，并按**渲染顺序**记录选中 session 的真实 item 索引
/// （分组态下 sessions_sorted 的顺序与渲染顺序不一致，此处修复错位）。
fn build_rows(app: &AppState) -> (Vec<ListItem<'static>>, Option<usize>) {
    let selected = app.active_session.as_ref();
    let mut items = Vec::new();
    let mut selected_index = None;

    if app.workspaces.workspaces.is_empty() {
        // 未分组平铺模式：忽略折叠（FR-001-03 只作用于分组态）。
        for meta in app.workspaces.sessions_sorted() {
            push_session(&mut items, &mut selected_index, meta, selected);
        }
    } else {
        for workspace in &app.workspaces.workspaces {
            let collapsed = app.collapsed_workspaces.contains(&workspace.id);
            let title = workspace
                .title
                .as_deref()
                .unwrap_or(workspace.id.0.as_str());
            items.push(ListItem::new(Line::from(vec![Span::styled(
                header_text(title, collapsed),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )])));
            // 折叠的 workspace 跳过其 session_ids 行（▸ + 不渲染子项）。
            if !collapsed {
                for sid in &workspace.session_ids {
                    if let Some(meta) = app.workspaces.sessions.get(sid) {
                        push_session(&mut items, &mut selected_index, meta, selected);
                    }
                }
            }
        }
        let grouped: std::collections::HashSet<&SessionId> = app
            .workspaces
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.session_ids.iter())
            .collect();
        for meta in app.workspaces.sessions_sorted() {
            if !grouped.contains(&meta.id) {
                push_session(&mut items, &mut selected_index, meta, selected);
            }
        }
    }

    if items.is_empty() {
        items.push(ListItem::new(Line::from(Span::styled(
            "No sessions",
            Style::default().fg(Color::DarkGray),
        ))));
    }
    (items, selected_index)
}

fn push_session(
    items: &mut Vec<ListItem<'static>>,
    selected_index: &mut Option<usize>,
    meta: &SessionMeta,
    selected: Option<&SessionId>,
) {
    items.push(session_item(meta, selected));
    if selected.is_some_and(|sid| sid == &meta.id) {
        *selected_index = Some(items.len() - 1);
    }
}

/// 项目标题行符号：展开 ▾ / 折叠 ▸。
fn header_text(title: &str, collapsed: bool) -> String {
    let symbol = if collapsed { "▸" } else { "▾" };
    format!("{symbol} {title}")
}

/// session 行文本：状态标记 + HH:MM 时间 + blank 标记（~）+ 标题。
fn session_text(meta: &SessionMeta) -> String {
    let marker = if meta.running { "●" } else { "○" };
    let title = meta
        .title
        .as_deref()
        .or(meta.last_turn_preview.as_deref())
        .unwrap_or("Untitled session");
    let blank = if meta.blank { "~ " } else { "" };
    format!(
        "{marker} {} {blank}{}",
        format_hhmm(meta.updated_at_ms),
        truncate(title, 80)
    )
}

fn session_item(meta: &SessionMeta, selected: Option<&SessionId>) -> ListItem<'static> {
    let text = session_text(meta);
    let style = if selected.is_some_and(|sid| sid == &meta.id) {
        Style::default().fg(Color::White)
    } else if meta.running {
        Style::default().fg(Color::Green)
    } else {
        Style::default().fg(Color::Gray)
    };
    ListItem::new(Line::from(Span::styled(text, style)))
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::types::{SessionId, SessionMeta, WorkspaceId};
    use crate::app::AppState;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn meta(id: &str) -> SessionMeta {
        meta_titled(id, "Build UI")
    }

    fn meta_titled(id: &str, title: &str) -> SessionMeta {
        SessionMeta {
            id: SessionId(id.into()),
            title: Some(title.into()),
            cwd: Some("/tmp/project".into()),
            updated_at_ms: 1,
            running: true,
            blank: false,
            origin: None,
            parent_id: None,
            workspace: None,
            last_turn_preview: None,
        }
    }

    fn rendered_text(terminal: &Terminal<TestBackend>) -> String {
        // 宽字符占两个 cell（后一 cell 为占位空格）：用 Span::width 跳过占位。
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
    fn renders_session_title_and_running_marker() {
        let mut app = AppState::default();
        app.workspaces.upsert_session(meta("s1"));
        let backend = TestBackend::new(32, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        let text = rendered_text(&terminal);
        assert!(text.contains("Build UI"));
        assert!(text.contains("●"));
    }

    #[test]
    fn flat_mode_selection_index_matches_render_order() {
        let mut app = AppState::default();
        // sessions_sorted 按 updated_at_ms 降序 → 渲染顺序 s1(200), s2(100)。
        let mut a = meta("s1");
        a.updated_at_ms = 200;
        let mut b = meta("s2");
        b.updated_at_ms = 100;
        app.workspaces.upsert_session(a);
        app.workspaces.upsert_session(b);
        app.active_session = Some(SessionId("s2".into()));
        let (items, selected) = build_rows(&app);
        assert_eq!(items.len(), 2);
        assert_eq!(selected, Some(1), "选中索引必须按渲染顺序");
    }

    #[test]
    fn grouped_mode_selection_index_counts_headers_and_rows() {
        let mut app = AppState::default();
        app.workspaces
            .upsert_workspace(WorkspaceId("ws1".into()), Some("项目A".into()));
        app.workspaces.upsert_session(meta("s1"));
        app.workspaces.upsert_session(meta("s2"));
        app.workspaces
            .attach_session_to_workspace(&WorkspaceId("ws1".into()), &SessionId("s1".into()));
        app.workspaces
            .attach_session_to_workspace(&WorkspaceId("ws1".into()), &SessionId("s2".into()));
        app.workspaces
            .upsert_session(meta_titled("s3", "Ungrouped"));
        // 渲染顺序：ws1 header(0), s1(1), s2(2), s3(3)。
        app.active_session = Some(SessionId("s2".into()));
        let (items, selected) = build_rows(&app);
        assert_eq!(items.len(), 4);
        assert_eq!(selected, Some(2), "分组态选中索引按渲染 items 计算");
        app.active_session = Some(SessionId("s3".into()));
        assert_eq!(build_rows(&app).1, Some(3));
    }

    #[test]
    fn collapsed_workspace_uses_arrow_and_skips_sessions() {
        let mut app = AppState::default();
        app.workspaces
            .upsert_workspace(WorkspaceId("ws1".into()), Some("项目A".into()));
        app.workspaces
            .upsert_session(meta_titled("s1", "Hidden One"));
        app.workspaces
            .upsert_session(meta_titled("s2", "Hidden Two"));
        app.workspaces
            .attach_session_to_workspace(&WorkspaceId("ws1".into()), &SessionId("s1".into()));
        app.workspaces
            .attach_session_to_workspace(&WorkspaceId("ws1".into()), &SessionId("s2".into()));
        app.collapsed_workspaces.insert(WorkspaceId("ws1".into()));
        app.active_session = Some(SessionId("s1".into()));
        let (items, selected) = build_rows(&app);
        assert_eq!(items.len(), 1, "折叠 workspace 只留标题行");
        assert_eq!(selected, None, "被折叠隐藏的会话不再高亮");

        let backend = TestBackend::new(40, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        let text = rendered_text(&terminal);
        assert!(text.contains("▸"), "折叠符号, text={text}");
        assert!(!text.contains("▾"), "text={text}");
        assert!(!text.contains("Hidden One"), "text={text}");
        assert!(!text.contains("Hidden Two"), "text={text}");

        // 展开后恢复 ▾ 并渲染子会话行。
        app.collapsed_workspaces.clear();
        let (items, selected) = build_rows(&app);
        assert_eq!(items.len(), 3);
        assert_eq!(selected, Some(1));
        assert_eq!(header_text("项目A", false), "▾ 项目A");
        assert_eq!(header_text("项目A", true), "▸ 项目A");
    }

    #[test]
    fn flat_mode_ignores_collapse() {
        let mut app = AppState::default();
        // 未分组平铺模式（无 workspaces）忽略折叠标记。
        let mut m = meta("s1");
        m.workspace = Some(WorkspaceId("ws1".into()));
        app.workspaces.upsert_session(m);
        app.collapsed_workspaces.insert(WorkspaceId("ws1".into()));
        let (items, selected) = build_rows(&app);
        assert_eq!(items.len(), 1, "平铺模式不折叠");
        assert_eq!(selected, None);
    }

    #[test]
    fn session_row_shows_time_and_blank_marker() {
        let mut m = meta_titled("s1", "Blank session");
        m.updated_at_ms = 45_240_000; // 12:34 UTC
        m.blank = true;
        assert_eq!(session_text(&m), "● 12:34 ~ Blank session");
        m.blank = false;
        assert_eq!(session_text(&m), "● 12:34 Blank session");
    }
}
