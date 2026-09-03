//! Transcript/chat rendering.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block as TuiBlock, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::api::types::ChunkRow;
use crate::app::AppState;
use crate::model::{Block, PackedChunks, TranscriptWindow};

/// Render the active transcript window using the app viewport.
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    let Some(window) = app.active_window() else {
        let body = Paragraph::new("No active session. Press f to choose a session.")
            .block(TuiBlock::default().borders(Borders::ALL).title(" Chat "));
        frame.render_widget(body, area);
        return;
    };
    render_window(
        frame,
        area,
        window,
        app.viewport.offset,
        app.viewport.follow_tail,
    );
}

/// Render a transcript window without requiring mutable model access.
pub fn render_window(
    frame: &mut Frame<'_>,
    area: Rect,
    window: &TranscriptWindow,
    offset: usize,
    follow_tail: bool,
) {
    let lines: Vec<Line<'static>> = window.blocks().map(block_line).collect();
    let title = if follow_tail {
        " Chat • live "
    } else {
        " Chat • browsing "
    };
    let paragraph = Paragraph::new(Text::from(lines))
        .block(TuiBlock::default().borders(Borders::ALL).title(title))
        .wrap(Wrap { trim: false })
        .scroll(((offset.min(u16::MAX as usize)) as u16, 0));
    frame.render_widget(paragraph, area);
}

fn block_line(block: &Block) -> Line<'static> {
    match block {
        Block::UserMessage { seq, content, .. } => Line::from(vec![
            Span::styled(
                format!("U {:>5} ", seq.0),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(single_line(content)),
        ]),
        Block::AssistantMessage { seq, chunks, .. } => Line::from(vec![
            Span::styled(
                format!("A {:>5} ", seq.0),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(packed_chunks_text(chunks)),
        ]),
        Block::ToolCall {
            seq, name, call_id, ..
        } => Line::from(vec![
            Span::styled(
                format!("T {:>5} ", seq.0),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(
                "call {}{}",
                name.as_deref().unwrap_or("unknown"),
                call_id
                    .as_deref()
                    .map(|id| format!(" ({id})"))
                    .unwrap_or_default()
            )),
        ]),
        Block::ToolResult {
            seq,
            content,
            is_error,
            ..
        } => Line::from(vec![
            Span::styled(
                format!("{} {:>5} ", if *is_error { "!" } else { "R" }, seq.0),
                Style::default()
                    .fg(if *is_error {
                        Color::Red
                    } else {
                        Color::Magenta
                    })
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(single_line(content)),
        ]),
        Block::RequestHeader { seq, summary } => Line::from(vec![
            Span::styled(
                format!("H {:>5} ", seq.0),
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(single_line(summary)),
        ]),
        Block::Compaction { seq, summary } => Line::from(vec![
            Span::styled(
                format!("C {:>5} ", seq.0),
                Style::default()
                    .fg(Color::LightBlue)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(single_line(summary)),
        ]),
        Block::Image {
            seq,
            attachment_id,
            name,
            dims,
        } => Line::from(vec![
            Span::styled(
                format!("I {:>5} ", seq.0),
                Style::default()
                    .fg(Color::LightMagenta)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(
                "{}{}{}",
                name.as_deref().unwrap_or("image"),
                dims.as_deref().map(|d| format!(" {d}")).unwrap_or_default(),
                attachment_id
                    .as_deref()
                    .map(|id| format!(" [{id}]"))
                    .unwrap_or_default()
            )),
        ]),
        Block::Unknown {
            seq, event_type, ..
        } => Line::from(vec![
            Span::styled(
                format!("? {:>5} ", seq.0),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("unknown event {event_type}")),
        ]),
    }
}

/// Keep packed chunk rows as one rendered summary; never create one widget per delta.
fn packed_chunks_text(chunks: &PackedChunks) -> String {
    if chunks.rows.is_empty() {
        return "(streaming)".to_string();
    }
    let mut parts = Vec::with_capacity(chunks.rows.len());
    for row in &chunks.rows {
        let part = match row {
            ChunkRow::TextChunks(data) => data.texts.join(""),
            ChunkRow::ReasoningChunks(data) => {
                let text = data.texts.join("");
                if text.is_empty() {
                    "reasoning".to_string()
                } else {
                    format!("reasoning: {text}")
                }
            }
            ChunkRow::ToolCallChunks(data) => {
                format!("tool: {}", data.name.as_deref().unwrap_or("call"))
            }
            ChunkRow::Unknown { event_type, .. } => format!("unknown chunk: {event_type}"),
        };
        if !part.is_empty() {
            parts.push(part);
        }
    }
    if parts.is_empty() {
        "(packed chunks)".to_string()
    } else {
        single_line(&parts.join(" "))
    }
}

fn single_line(text: &str) -> String {
    text.replace(['\n', '\r', '\t'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::types::{SessionHistoryRecord, SessionSeq, SessionWireEvent};
    use crate::model::{Incoming, TranscriptWindow};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn event(seq: u64, event_type: &str, content: Option<&str>) -> SessionHistoryRecord {
        SessionHistoryRecord::Event {
            event: SessionWireEvent {
                event_type: event_type.into(),
                seq: Some(SessionSeq(seq)),
                time: None,
                request_id: None,
                ignorable: None,
                source_event_seqs: None,
                surface_op: None,
                data: content.map(|value| serde_json::json!({ "content": value })),
            },
        }
    }

    #[test]
    fn renders_block_variants_and_packed_chunks() {
        let mut window = TranscriptWindow::new(20);
        window.apply(Incoming::Snapshot {
            cursor: None,
            records: vec![
                event(1, "user/message", Some("hello")),
                event(2, "assistant/message", None),
            ],
            has_more: false,
            projections: None,
        });
        window.apply(Incoming::Chunks(crate::api::types::ChunkRow::TextChunks(
            crate::api::types::ChunkData {
                texts: vec!["packed".into(), " text".into()],
                ..Default::default()
            },
        )));
        let backend = TestBackend::new(50, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_window(frame, frame.area(), &window, 0, true))
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("hello"));
        assert!(rendered.contains("packed text"));
        assert!(!rendered.contains("delta"));
    }
}
