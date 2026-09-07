//! Connection and projection status bar rendering.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::{AppState, ConnState};
use crate::model::ProjectionSnapshot;
use crate::ui::layout::{color_depth, ColorDepth};

/// 快捷键提示行（FR-001-05，README 键位口径；[/] 搜索为 REQ-002 预留但仍展示）。
const HINT_LINE: &str = "[i]输入 [/]搜索 [f]切换 [?]帮助 [q]退出";
/// IMAGEVIEW 模式提示行（REQ-004 D-14/05 §10：`o` 系统查看器 `y` 复制路径
/// `q` 关闭）。
const IMAGE_HINT_LINE: &str = "[o]系统查看器 [y]复制路径 [q]关闭";
/// REQ-005 Trajectory 列表提示行（Notes/05 §7：j/k 选择 Enter 详情 z 折叠
/// / 搜索 gt 回对话）。
const TRAJ_HINT_LINE: &str = "[j/k]选择 [Enter]详情 [z]折叠 [/]搜索 [gt]回对话";
/// REQ-005 详情子层提示行（Notes/05 §8：y 复制 q 关闭 j/k 滚动）。
const DETAIL_HINT_LINE: &str = "[y]复制 [j/k]滚动 [q]关闭";
/// 窄终端省略快捷键提示行（FR-001-05）。
const MIN_WIDTH_FOR_HINT: u16 = 50;

/// 官方 running 投影（与第一行 ●run 同源，ADR-008 不自算）。
fn official_running(app: &AppState) -> bool {
    let Some(window) = app.active_window() else {
        return false;
    };
    let projections = ProjectionSnapshot::new(window.projections().clone());
    projections.running().unwrap_or_else(|| {
        app.active_session
            .as_ref()
            .and_then(|sid| app.workspaces.sessions.get(sid))
            .is_some_and(|m| m.running)
    })
}

/// Render the two-line status bar: official projection fields + shortcut hints.
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    let depth = color_depth();
    let (label, color) = connection_label(app.conn, depth);
    let mut spans = vec![Span::styled(
        format!(" {label} "),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )];

    // 模式指示（REQ-002/003）：INSERT/SEARCH/VISUAL/APPROVAL 高亮；
    // NORMAL 为默认态不重复标注。
    match app.mode {
        crate::app::Mode::Insert => {
            spans.push(Span::styled(
                " INSERT ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
            // REQ-003 AC-003-06：运行中 composer 为 steer，状态条显示 STEER。
            if app.composer.steer {
                spans.push(Span::styled(
                    " STEER ",
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                ));
            }
        }
        crate::app::Mode::Search => {
            spans.push(Span::styled(
                " SEARCH ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        crate::app::Mode::Visual => {
            spans.push(Span::styled(
                " VISUAL ",
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ));
            // §4：VISUAL 选择区反色 + `selected N lines` 计数。
            if let Some(sel) = &app.yank.visual {
                let (start, end) = sel.range();
                spans.push(Span::styled(
                    format!(
                        " selected {} lines",
                        end.saturating_sub(start).saturating_add(1)
                    ),
                    Style::default().fg(Color::Magenta),
                ));
            }
        }
        crate::app::Mode::Approval => {
            spans.push(Span::styled(
                " APPROVAL ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        // REQ-005：Trajectory 模式指示（D-25）。
        crate::app::Mode::Trajectory => {
            spans.push(Span::styled(
                " TRAJ ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
            // 详情子层指示（D-25：Trajectory 内焦点子层）。
            if app.traj.detail_open && app.focus == crate::app::Focus::Details {
                spans.push(Span::styled(
                    " DETAILS ",
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                ));
            }
        }
        _ => {}
    }
    // AC-003-18：不可编程审批降级 → 状态条 `等待审批` 高亮（不弹窗）。
    if app.approval.waiting_hint {
        spans.push(Span::styled(
            " 等待审批（官方 web 完成） ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    // AC-003-05：搜索打开时状态条计数 `3/17 matches`。
    if app.search.open {
        let text = match app.search.window_matches.len() {
            0 => "0 matches".to_string(),
            total => format!(
                "{}/{} matches",
                app.search.cursor.saturating_add(1).min(total),
                total
            ),
        };
        spans.push(Span::styled(
            format!("  {text}"),
            Style::default().fg(Color::Cyan),
        ));
    }
    // 复制成功 toast（AC-003-08；`copied`）。
    if let Some(toast) = &app.yank.toast {
        spans.push(Span::styled(
            format!("  {toast} "),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ));
    }

    if let Some(window) = app.active_window() {
        let projections = ProjectionSnapshot::new(window.projections().clone());
        // AC-001-06：title/cwd/running 优先官方 projections，缺失回退
        // SessionMeta；绝不自算任何统计。
        let meta = app
            .active_session
            .as_ref()
            .and_then(|sid| app.workspaces.sessions.get(sid));
        let title = projections
            .title()
            .or_else(|| meta.and_then(|m| m.title.clone()));
        let cwd = projections
            .cwd()
            .or_else(|| meta.and_then(|m| m.cwd.clone()));
        let running = projections
            .running()
            .unwrap_or_else(|| meta.is_some_and(|m| m.running));
        if let Some(title) = title {
            spans.push(Span::raw(format!(" {}", truncate(&title, 24))));
        }
        if let Some(cwd) = cwd {
            spans.push(Span::styled(
                format!(" {}", truncate(&cwd, 30)),
                Style::default().fg(Color::DarkGray),
            ));
        }
        spans.push(Span::styled(
            if running { " ●run" } else { " ○idle" },
            Style::default().fg(if running { Color::Green } else { Color::Gray }),
        ));
        // 本地停止转场（AC-002-05）：requested 且官方投影仍 running →
        // 「停止中」；投影翻转即结束（FollowSnapshot 清空 requested）。
        if running && app.stop.requested_session.as_ref() == app.active_session.as_ref() {
            spans.push(Span::styled(
                " 停止中…",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        if let Some(model) = projections
            .model_selection()
            .last_used
            .or_else(|| projections.model_selection().next)
        {
            spans.push(Span::raw(format!(" {model}")));
        }
        let context = projections.context_pressure();
        if let (Some(used), Some(total)) = (context.pressure_tokens, context.projected_tokens) {
            spans.push(Span::raw(format!(" ctx {used}/{total}")));
        }
        let usage = projections.token_usage();
        if usage.input.is_some() || usage.output.is_some() {
            spans.push(Span::raw(format!(
                " in:{} out:{}",
                format_optional(usage.input),
                format_optional(usage.output)
            )));
        }
        let stats = projections.session_stats();
        if stats.turns.is_some() || stats.steps.is_some() {
            spans.push(Span::raw(format!(
                " turns:{} steps:{}",
                format_optional(stats.turns),
                format_optional(stats.steps)
            )));
        }
        if app.viewport.follow_tail {
            spans.push(Span::styled(" • tail", Style::default().fg(Color::Green)));
        } else {
            spans.push(Span::styled(
                " • browse",
                Style::default().fg(Color::Yellow),
            ));
        }
    }

    if let Some(error) = &app.last_error {
        spans.push(Span::styled(
            format!("  {error}"),
            Style::default().fg(Color::Red),
        ));
    }
    if let Some(notice) = &app.notice {
        spans.push(Span::styled(
            format!("  {notice}"),
            Style::default().fg(Color::Green),
        ));
    }
    if app.conn == ConnState::StartupFailed {
        spans.push(Span::styled(
            "  dsh web unavailable; press r to retry or q to quit",
            Style::default().fg(Color::Yellow),
        ));
    }
    if app.is_reconnecting() {
        spans.push(Span::styled(
            "  reconnecting…",
            Style::default().fg(Color::Yellow),
        ));
    }

    // 第一行：状态字段。
    let content_area = Rect {
        y: area.y,
        height: 1,
        ..area
    };
    let mut mode_spans = Vec::new();
    if app.mode == crate::app::Mode::ImageView {
        // REQ-004：IMAGEVIEW 模式徽标（Notes/05 §10 状态栏 `IMAGE`）。
        mode_spans.push(Span::styled(
            " IMAGE ",
            Style::default()
                .fg(Color::LightMagenta)
                .add_modifier(Modifier::BOLD),
        ));
    }
    let mut all_spans = mode_spans;
    all_spans.extend(spans);
    frame.render_widget(Paragraph::new(Line::from(all_spans)), content_area);

    // 第二行：快捷键提示（窄终端省略，FR-001-05）；官方投影运行中追加
    // [s]停止（REQ-002 AC-002-05）。
    if area.width >= MIN_WIDTH_FOR_HINT {
        let hint_area = Rect {
            y: area.y.saturating_add(1),
            height: 1,
            ..area
        };
        let mut hint = if app.mode == crate::app::Mode::ImageView {
            IMAGE_HINT_LINE.to_string()
        } else if app.mode == crate::app::Mode::Trajectory
            && app.traj.detail_open
            && app.focus == crate::app::Focus::Details
        {
            // REQ-005 详情子层提示（Notes/05 §8 `DETAILS`）。
            DETAIL_HINT_LINE.to_string()
        } else if app.mode == crate::app::Mode::Trajectory {
            // REQ-005 轨迹列表提示（Notes/05 §7 `TRAJ`）。
            TRAJ_HINT_LINE.to_string()
        } else {
            HINT_LINE.to_string()
        };
        if app.mode != crate::app::Mode::ImageView && official_running(app) {
            hint.push_str(" [s]停止");
        }
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                hint,
                Style::default().fg(Color::DarkGray),
            ))),
            hint_area,
        );
    }
}

/// Render startup/reconnect guidance in a body area when it should be prominent.
pub fn render_guidance(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    let text = if app.conn == ConnState::StartupFailed {
        app.guidance_text()
    } else if app.is_reconnecting() {
        "Connection lost. Reconnecting; live updates are paused.".to_string()
    } else {
        String::new()
    };
    if !text.is_empty() {
        frame.render_widget(Paragraph::new(text), area);
    }
}

/// FR-001-06：连接状态色按颜色能力降级——Basic 只用 8 色，绝不用 RGB。
fn conn_color(basic: Color, c256: u8, rgb: (u8, u8, u8), depth: ColorDepth) -> Color {
    match depth {
        ColorDepth::Basic => basic,
        ColorDepth::C256 => Color::Indexed(c256),
        ColorDepth::TrueColor => Color::Rgb(rgb.0, rgb.1, rgb.2),
    }
}

fn connection_label(state: ConnState, depth: ColorDepth) -> (&'static str, Color) {
    match state {
        ConnState::Connecting => (
            "connecting",
            conn_color(Color::Yellow, 11, (255, 214, 0), depth),
        ),
        ConnState::Ready => ("ready", conn_color(Color::Green, 10, (0, 200, 83), depth)),
        ConnState::Reconnecting => (
            "reconnecting",
            conn_color(Color::Yellow, 11, (255, 214, 0), depth),
        ),
        ConnState::StartupFailed => (
            "startup failed",
            conn_color(Color::Red, 9, (255, 82, 82), depth),
        ),
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn format_optional(value: Option<u64>) -> String {
    value
        .map(|v| v.to_string())
        .unwrap_or_else(|| "-".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::types::{SessionId, SessionMeta};
    use crate::app::AppState;
    use crate::model::Incoming;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

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
    fn status_reads_projection_values_without_inventing_totals() {
        let mut app = AppState::default();
        app.active_session = Some(SessionId("s1".into()));
        app.conn = ConnState::Ready;
        app.sessions.touch("s1", 20).apply(Incoming::Snapshot {
            cursor: None,
            records: vec![],
            has_more: false,
            projections: Some(serde_json::json!({
                "title": "部署排查",
                "cwd": "/home/nd/project",
                "running": true,
                "modelSelection": {"lastUsed": "model-x"},
                "contextPressure": {"pressureTokens": 10, "projectedTokens": 20},
                "tokenUsage": {"input": 4, "output": 6},
                "sessionStats": {"turns": 2, "steps": 3}
            })),
        });
        let backend = TestBackend::new(120, 2);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, Rect::new(0, 0, 120, 2), &app))
            .unwrap();
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("ready"));
        // AC-001-06：title/cwd/running 全部来自官方 projections。
        assert!(rendered.contains("部署排查"), "rendered={rendered}");
        assert!(rendered.contains("/home/nd/project"), "rendered={rendered}");
        assert!(rendered.contains("●run"), "rendered={rendered}");
        assert!(rendered.contains("model-x"));
        assert!(rendered.contains("ctx 10/20"));
        assert!(rendered.contains("in:4 out:6"));
        assert!(rendered.contains("turns:2 steps:3"));
        assert!(
            !rendered.contains("50%"),
            "UI must not invent an official percentage"
        );
    }

    #[test]
    fn status_falls_back_to_session_meta_when_projections_missing() {
        let mut app = AppState::default();
        app.active_session = Some(SessionId("s1".into()));
        app.conn = ConnState::Ready;
        app.workspaces.upsert_session(SessionMeta {
            id: SessionId("s1".into()),
            title: Some("Meta title".into()),
            cwd: Some("/meta/cwd".into()),
            updated_at_ms: 1,
            running: true,
            blank: false,
            origin: None,
            parent_id: None,
            workspace: None,
            last_turn_preview: None,
        });
        // projections 只给 ctx，缺 title/cwd/running → 回退 SessionMeta。
        app.sessions.touch("s1", 20).apply(Incoming::Snapshot {
            cursor: None,
            records: vec![],
            has_more: false,
            projections: Some(serde_json::json!({
                "contextPressure": {"pressureTokens": 1, "projectedTokens": 2}
            })),
        });
        let backend = TestBackend::new(80, 2);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, Rect::new(0, 0, 80, 2), &app))
            .unwrap();
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("Meta title"), "rendered={rendered}");
        assert!(rendered.contains("/meta/cwd"), "rendered={rendered}");
        assert!(rendered.contains("●run"), "rendered={rendered}");
        assert!(rendered.contains("ctx 1/2"));
    }

    #[test]
    fn shortcut_hint_line_shown_when_wide_enough() {
        let mut app = AppState::default();
        app.conn = ConnState::Ready;
        let backend = TestBackend::new(60, 2);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, Rect::new(0, 0, 60, 2), &app))
            .unwrap();
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("[i]输入"), "rendered={rendered}");
        assert!(rendered.contains("[/]搜索"), "rendered={rendered}");
        assert!(rendered.contains("[f]切换"), "rendered={rendered}");
        assert!(rendered.contains("[?]帮助"), "rendered={rendered}");
        assert!(rendered.contains("[q]退出"), "rendered={rendered}");
    }

    #[test]
    fn shortcut_hint_line_omitted_on_narrow_terminal() {
        let mut app = AppState::default();
        app.conn = ConnState::Ready;
        let backend = TestBackend::new(40, 2);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, Rect::new(0, 0, 40, 2), &app))
            .unwrap();
        let rendered = rendered_text(&terminal);
        assert!(!rendered.contains("[i]输入"), "rendered={rendered}");
        assert!(!rendered.contains("[q]退出"), "rendered={rendered}");
    }

    #[test]
    fn connection_color_degrades_by_color_depth() {
        // FR-001-06：Basic 只用 8 色；C256 用索引色；TrueColor 才用 RGB。
        let (label, color) = connection_label(ConnState::Ready, ColorDepth::Basic);
        assert_eq!(label, "ready");
        assert_eq!(color, Color::Green);
        let (_, color) = connection_label(ConnState::Ready, ColorDepth::C256);
        assert_eq!(color, Color::Indexed(10));
        let (_, color) = connection_label(ConnState::Ready, ColorDepth::TrueColor);
        assert_eq!(color, Color::Rgb(0, 200, 83));
        let (_, color) = connection_label(ConnState::StartupFailed, ColorDepth::Basic);
        assert_eq!(color, Color::Red);
    }

    #[test]
    fn insert_mode_and_local_stopping_render_in_status_ac002_05() {
        let mut app = AppState::default();
        app.active_session = Some(SessionId("s1".into()));
        app.conn = ConnState::Ready;
        app.mode = crate::app::Mode::Insert;
        app.stop = crate::app::StopState {
            requested_session: Some(SessionId("s1".into())),
        };
        // 官方投影仍 running：本地「停止中」转场。
        app.sessions.touch("s1", 20).apply(Incoming::Snapshot {
            cursor: None,
            records: vec![],
            has_more: false,
            projections: Some(serde_json::json!({"running": true})),
        });
        let backend = TestBackend::new(120, 2);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, Rect::new(0, 0, 120, 2), &app))
            .unwrap();
        let rendered = rendered_text(&terminal);
        assert!(
            rendered.contains("INSERT"),
            "INSERT 模式指示, text={rendered}"
        );
        assert!(
            rendered.contains("停止中"),
            "本地停止中转场, text={rendered}"
        );
    }

    #[test]
    fn visual_mode_shows_selected_line_count_ac003_12() {
        let mut app = AppState::default();
        app.conn = ConnState::Ready;
        app.mode = crate::app::Mode::Visual;
        app.yank.visual = Some(crate::model::VisualSelection {
            anchor: 2,
            cursor: 4,
            mode: crate::model::VisualMode::Line,
        });
        let backend = TestBackend::new(120, 2);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, Rect::new(0, 0, 120, 2), &app))
            .unwrap();
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("VISUAL"), "text={rendered}");
        assert!(
            rendered.contains("selected 3 lines"),
            "§4 selected N lines 计数, text={rendered}"
        );
    }

    #[test]
    fn stop_hint_only_when_official_projection_running() {
        let mut app = AppState::default();
        app.active_session = Some(SessionId("s1".into()));
        app.conn = ConnState::Ready;
        app.sessions.touch("s1", 20).apply(Incoming::Snapshot {
            cursor: None,
            records: vec![],
            has_more: false,
            projections: Some(serde_json::json!({"running": true})),
        });
        let backend = TestBackend::new(120, 2);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, Rect::new(0, 0, 120, 2), &app))
            .unwrap();
        let rendered = rendered_text(&terminal);
        assert!(
            rendered.contains("[s]停止"),
            "运行中显示停止提示, text={rendered}"
        );

        // 投影 idle：不显示 [s]停止。
        app.sessions.touch("s1", 20).apply(Incoming::Snapshot {
            cursor: None,
            records: vec![],
            has_more: false,
            projections: Some(serde_json::json!({"running": false})),
        });
        terminal
            .draw(|frame| render(frame, Rect::new(0, 0, 120, 2), &app))
            .unwrap();
        let rendered = rendered_text(&terminal);
        assert!(
            !rendered.contains("[s]停止"),
            "idle 不显示停止提示, text={rendered}"
        );
    }
}
