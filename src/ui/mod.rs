//! Ratatui view composition for the dshtui application.

pub mod chat;
pub mod layout;
pub mod picker;
pub mod sidebar;
pub mod status;

use ratatui::layout::Rect;
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::app::AppState;

pub use layout::{sidebar_width, split, LayoutAreas};

/// Render one complete frame from read-only application state.
pub fn render(frame: &mut Frame<'_>, app: &AppState) {
    let areas = split(frame.area(), app.focus == crate::app::Focus::Details);
    sidebar::render(frame, areas.sidebar, app);

    if app.conn == crate::app::ConnState::StartupFailed {
        let body = Paragraph::new(app.guidance_text())
            .block(Block::default().borders(Borders::ALL).title(" Startup "));
        frame.render_widget(body, areas.center);
    } else if app.is_reconnecting() && app.active_window().is_none() {
        let body = Paragraph::new("Connection lost. Reconnecting…").block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Reconnecting "),
        );
        frame.render_widget(body, areas.center);
    } else {
        chat::render(frame, areas.center, app);
    }

    if let Some(details) = areas.details {
        render_details(frame, details, app);
    }
    status::render(frame, areas.status, app);
    picker::render(frame, frame.area(), app);
}

fn render_details(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    let text = app
        .active_window()
        .map(|window| {
            let head = window
                .head_seq()
                .map(|seq| seq.0.to_string())
                .unwrap_or_else(|| "-".into());
            let tail = window
                .tail_seq()
                .map(|seq| seq.0.to_string())
                .unwrap_or_else(|| "-".into());
            format!(
                "Session\n{}\n\nBlocks: {}\nSeq: {head}..{tail}\nMore history: {}",
                app.active_session
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "-".into()),
                window.len(),
                if window.head_has_more() { "yes" } else { "no" }
            )
        })
        .unwrap_or_else(|| "No active session".to_string());
    frame.render_widget(
        Paragraph::new(text).block(Block::default().borders(Borders::LEFT).title(" Details ")),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::AppState;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn complete_frame_renders_three_breakpoint_shapes() {
        let app = AppState::default();
        for (width, expected_sidebar) in [(140, 32), (110, 24), (80, 12)] {
            let backend = TestBackend::new(width, 20);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|frame| render(frame, &app)).unwrap();
            let areas = split(Rect::new(0, 0, width, 20), false);
            assert_eq!(areas.sidebar.width, expected_sidebar);
            assert!(areas.details.is_none());
        }
    }
}
