//! Connection and projection status bar rendering.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::{AppState, ConnState};
use crate::model::ProjectionSnapshot;

/// Render the one-line status bar, including official projection fields.
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    let (label, color) = connection_label(app.conn);
    let mut spans = vec![Span::styled(
        format!(" {label} "),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )];

    if let Some(window) = app.active_window() {
        let projections = ProjectionSnapshot::new(window.projections().clone());
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

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
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

fn connection_label(state: ConnState) -> (&'static str, Color) {
    match state {
        ConnState::Connecting => ("connecting", Color::Yellow),
        ConnState::Ready => ("ready", Color::Green),
        ConnState::Reconnecting => ("reconnecting", Color::Yellow),
        ConnState::StartupFailed => ("startup failed", Color::Red),
    }
}

fn format_optional(value: Option<u64>) -> String {
    value
        .map(|v| v.to_string())
        .unwrap_or_else(|| "-".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::types::SessionId;
    use crate::app::AppState;
    use crate::model::Incoming;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

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
                "modelSelection": {"lastUsed": "model-x"},
                "contextPressure": {"pressureTokens": 10, "projectedTokens": 20},
                "tokenUsage": {"input": 4, "output": 6},
                "sessionStats": {"turns": 2, "steps": 3}
            })),
        });
        let backend = TestBackend::new(80, 2);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, Rect::new(0, 0, 80, 1), &app))
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("ready"));
        assert!(rendered.contains("model-x"));
        assert!(rendered.contains("ctx 10/20"));
        assert!(rendered.contains("in:4 out:6"));
        assert!(rendered.contains("turns:2 steps:3"));
        assert!(
            !rendered.contains("50%"),
            "UI must not invent an official percentage"
        );
    }
}
