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
/// 窄终端省略快捷键提示行（FR-001-05）。
const MIN_WIDTH_FOR_HINT: u16 = 50;

/// Render the two-line status bar: official projection fields + shortcut hints.
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    let depth = color_depth();
    let (label, color) = connection_label(app.conn, depth);
    let mut spans = vec![Span::styled(
        format!(" {label} "),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )];

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
    frame.render_widget(Paragraph::new(Line::from(spans)), content_area);

    // 第二行：快捷键提示（窄终端省略，FR-001-05）。
    if area.width >= MIN_WIDTH_FOR_HINT {
        let hint_area = Rect {
            y: area.y.saturating_add(1),
            height: 1,
            ..area
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                HINT_LINE,
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
}
