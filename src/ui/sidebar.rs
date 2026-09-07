//! Session and workspace sidebar rendering.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState};
use ratatui::Frame;

use crate::api::types::{SessionId, SessionMeta};
use crate::app::AppState;
use crate::model::{sidebar_rows, SidebarRow};

use super::format_hhmm;

/// Render the workspace/session navigator. 高亮 = 行光标（Focus::Sidebar 下
/// j/k 移动）；会话行前缀 `●`/`○` 表示运行/空闲，`▸/▾` 表示 workspace 折叠。
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    let items = build_items(app);
    let mut list_state = ListState::default();
    let cursor = app.sidebar.cursor.min(items.len().saturating_sub(1));
    list_state.select(Some(cursor));
    let title = format!(" Sessions ({}) ", app.workspaces.session_count());
    let list = List::new(items)
        .block(Block::default().borders(Borders::RIGHT).title(title))
        .highlight_style(Style::default().bg(Color::Blue).fg(Color::White))
        .highlight_symbol("▌");
    frame.render_stateful_widget(list, area, &mut list_state);
}

/// 按视图态渲染行（`sidebar_rows` 单一 seam；行模型与 reducer 光标一致）。
fn build_items(app: &AppState) -> Vec<ListItem<'static>> {
    let active = app.active_session.as_ref();
    let rows = sidebar_rows(&app.sidebar_view, &app.workspaces);
    let mut items: Vec<ListItem<'static>> = Vec::new();
    for row in rows {
        match row {
            SidebarRow::WorkspaceHeader { id, collapsed } => {
                let title = app
                    .workspaces
                    .workspaces
                    .iter()
                    .find(|w| w.id == id)
                    .and_then(|w| w.title.clone())
                    .unwrap_or_else(|| id.0.clone());
                let symbol = if collapsed { "▸" } else { "▾" };
                items.push(ListItem::new(Line::from(vec![Span::styled(
                    format!("{symbol} {title}"),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )])));
            }
            SidebarRow::Session(id) => {
                if let Some(meta) = app.workspaces.sessions.get(&id) {
                    items.push(session_item(meta, active));
                }
            }
        }
    }
    if items.is_empty() {
        items.push(ListItem::new(Line::from(Span::styled(
            "No sessions",
            Style::default().fg(Color::DarkGray),
        ))));
    }
    items
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
        // 已打开的会话与光标高亮分离：仅改前景色（绿色粗体），背景高亮由
        // List cursor 提供（D-034 Prototype PASS）。
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD)
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
    use crate::api::types::WorkspaceId;
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
    fn flat_mode_cursor_maps_to_render_order() {
        // sessions_sorted 按 updated_at_ms 降序 → 行序 s1(200), s2(100)。
        let mut app = AppState::default();
        let mut a = meta("s1");
        a.updated_at_ms = 200;
        let mut b = meta("s2");
        b.updated_at_ms = 100;
        app.workspaces.upsert_session(a);
        app.workspaces.upsert_session(b);
        app.active_session = Some(SessionId("s2".into()));
        // 行模型：flat 平铺 + 光标索引即会话行序。
        let rows = sidebar_rows(&app.sidebar_view, &app.workspaces);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].session_id(), Some(&SessionId("s1".into())));
        assert_eq!(rows[1].session_id(), Some(&SessionId("s2".into())));
        app.sidebar.cursor = 1;
        // render 高亮光标行（List state select）。
        let backend = TestBackend::new(40, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        let text = rendered_text(&terminal);
        assert!(text.contains("Build UI"), "text={text}");
    }

    #[test]
    fn grouped_mode_renders_headers_rows_and_ungrouped() {
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
        // 行序：ws1 header(0), s1(1), s2(2), s3(3)。
        let rows = sidebar_rows(&app.sidebar_view, &app.workspaces);
        assert_eq!(rows.len(), 4);
        assert!(matches!(&rows[0], SidebarRow::WorkspaceHeader { id, .. } if id.0 == "ws1"));
        assert_eq!(rows[1].session_id(), Some(&SessionId("s1".into())));
        assert_eq!(rows[2].session_id(), Some(&SessionId("s2".into())));
        assert_eq!(rows[3].session_id(), Some(&SessionId("s3".into())));

        let backend = TestBackend::new(40, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        let text = rendered_text(&terminal);
        assert!(text.contains("▾ 项目A"), "展开符号, text={text}");
        assert!(text.contains("Ungrouped"), "text={text}");
    }

    #[test]
    fn collapsed_workspace_uses_arrow_and_skips_sessions() {
        let mut app = AppState::default();
        let ws1 = WorkspaceId("ws1".into());
        app.workspaces
            .upsert_workspace(ws1.clone(), Some("项目A".into()));
        app.workspaces
            .upsert_session(meta_titled("s1", "Hidden One"));
        app.workspaces
            .upsert_session(meta_titled("s2", "Hidden Two"));
        app.workspaces
            .attach_session_to_workspace(&ws1, &SessionId("s1".into()));
        app.workspaces
            .attach_session_to_workspace(&ws1, &SessionId("s2".into()));
        app.sidebar_view.collapsed.insert(ws1);
        let rows = sidebar_rows(&app.sidebar_view, &app.workspaces);
        assert_eq!(rows.len(), 1, "折叠 workspace 只留标题行");
        assert!(matches!(
            &rows[0],
            SidebarRow::WorkspaceHeader {
                collapsed: true,
                ..
            }
        ));

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
        app.sidebar_view.expand_all();
        let rows = sidebar_rows(&app.sidebar_view, &app.workspaces);
        assert_eq!(rows.len(), 3);
        assert!(matches!(
            &rows[0],
            SidebarRow::WorkspaceHeader {
                collapsed: false,
                ..
            }
        ));
    }

    #[test]
    fn flat_group_ignores_workspace_collapse() {
        // group=flat：即使 workspace 折叠标记存在也全量平铺（无 header）。
        let mut app = AppState::default();
        app.sidebar_view.group_by = crate::model::GroupBy::Flat;
        app.sidebar_view.collapsed.insert(WorkspaceId("ws1".into()));
        let mut m = meta("s1");
        m.workspace = Some(WorkspaceId("ws1".into()));
        app.workspaces.upsert_session(m);
        let rows = sidebar_rows(&app.sidebar_view, &app.workspaces);
        assert_eq!(rows.len(), 1, "flat 平铺不折叠");
        assert_eq!(rows[0].session_id(), Some(&SessionId("s1".into())));
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
