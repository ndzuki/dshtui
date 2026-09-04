//! Transcript/chat rendering.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block as TuiBlock, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::api::types::ChunkRow;
use crate::app::AppState;
use crate::model::{Block, PackedChunks, PendingEcho, PendingEchoStatus, TranscriptWindow};

use super::format_hhmm;

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
    let len = window.len();
    let mut lines: Vec<Line<'static>> = window
        .blocks()
        .enumerate()
        .map(|(i, block)| block_line(block, i + 1 == len))
        .collect();
    // 乐观回显（REQ-002）：durable 之外追加 pending/error 行；durable
    // 同 requestId 到达后 pending 被对账 retire，绝不重复显示。
    for echo in window.pending() {
        lines.push(echo_line(echo));
    }
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

/// 乐观回显行：Pending → `…`（黄色）；Failed → `!`（红色）+ 稳定错误码。
fn echo_line(echo: &PendingEcho) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    match &echo.status {
        PendingEchoStatus::Pending => {
            spans.push(Span::styled(
                "… ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(
                "U echo ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(single_line(&echo.text)));
        }
        PendingEchoStatus::Failed { code, .. } => {
            spans.push(Span::styled(
                "! ",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(
                "U echo ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(
                format!("(发送失败 {code}) "),
                Style::default().fg(Color::Red),
            ));
            spans.push(Span::raw(single_line(&echo.text)));
        }
    }
    Line::from(spans)
}

/// FR-001-04 §4：携带 time 的 Block 渲染 HH:MM 前缀（UTC，纯展示）；
/// 状态标记：running（空 chunks 的 assistant）●、pending（末尾 user 等待回复）…、
/// error（tool result isError）!。
fn block_line(block: &Block, is_last: bool) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    match block {
        Block::UserMessage { seq, content, time } => {
            push_time(&mut spans, *time);
            if is_last {
                // 末尾用户消息：等待 assistant 回复（pending）。
                spans.push(Span::styled(
                    "… ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            spans.push(Span::styled(
                format!("U {:>5} ", seq.0),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(single_line(content)));
        }
        Block::AssistantMessage { seq, chunks, time } => {
            push_time(&mut spans, *time);
            let running = chunks.rows.is_empty();
            if running {
                // 尚无任何 chunk 行：正在流式生成（running）。
                spans.push(Span::styled(
                    "● ",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            spans.push(Span::styled(
                format!("A {:>5} ", seq.0),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(packed_chunks_text(chunks)));
        }
        Block::ToolCall {
            seq,
            name,
            call_id,
            time,
            ..
        } => {
            push_time(&mut spans, *time);
            spans.push(Span::styled(
                format!("T {:>5} ", seq.0),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(format!(
                "call {}{}",
                name.as_deref().unwrap_or("unknown"),
                call_id
                    .as_deref()
                    .map(|id| format!(" ({id})"))
                    .unwrap_or_default()
            )));
        }
        Block::ToolResult {
            seq,
            content,
            is_error,
            time,
            ..
        } => {
            push_time(&mut spans, *time);
            spans.push(Span::styled(
                format!("{} {:>5} ", if *is_error { "!" } else { "R" }, seq.0),
                Style::default()
                    .fg(if *is_error {
                        Color::Red
                    } else {
                        Color::Magenta
                    })
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(single_line(content)));
        }
        Block::RequestHeader { seq, summary } => {
            spans.push(Span::styled(
                format!("H {:>5} ", seq.0),
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(single_line(summary)));
        }
        Block::Compaction { seq, summary } => {
            spans.push(Span::styled(
                format!("C {:>5} ", seq.0),
                Style::default()
                    .fg(Color::LightBlue)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(single_line(summary)));
        }
        Block::Image {
            seq,
            attachment_id,
            name,
            dims,
        } => {
            spans.push(Span::styled(
                format!("I {:>5} ", seq.0),
                Style::default()
                    .fg(Color::LightMagenta)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(format!(
                "{}{}{}",
                name.as_deref().unwrap_or("image"),
                dims.as_deref().map(|d| format!(" {d}")).unwrap_or_default(),
                attachment_id
                    .as_deref()
                    .map(|id| format!(" [{id}]"))
                    .unwrap_or_default()
            )));
        }
        Block::Unknown {
            seq, event_type, ..
        } => {
            spans.push(Span::styled(
                format!("? {:>5} ", seq.0),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(format!("unknown event {event_type}")));
        }
    }
    Line::from(spans)
}

/// time → HH:MM 前缀（缺失时省略）。
fn push_time(spans: &mut Vec<Span<'static>>, time: Option<i64>) {
    if let Some(ms) = time {
        spans.push(Span::styled(
            format!("{} ", format_hhmm(ms)),
            Style::default().fg(Color::DarkGray),
        ));
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
    use crate::api::types::{SessionHistoryRecord, SessionRequestId, SessionSeq, SessionWireEvent};
    use crate::model::{Incoming, TranscriptWindow};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn event(seq: u64, event_type: &str, content: Option<&str>) -> SessionHistoryRecord {
        event_with(seq, event_type, content, None)
    }

    fn event_with(
        seq: u64,
        event_type: &str,
        content: Option<&str>,
        time: Option<i64>,
    ) -> SessionHistoryRecord {
        SessionHistoryRecord::Event {
            event: SessionWireEvent {
                event_type: event_type.into(),
                seq: Some(SessionSeq(seq)),
                time,
                request_id: None,
                ignorable: None,
                source_event_seqs: None,
                surface_op: None,
                data: content.map(|value| serde_json::json!({ "content": value })),
            },
        }
    }

    fn event_data(
        seq: u64,
        event_type: &str,
        data: serde_json::Value,
        time: Option<i64>,
    ) -> SessionHistoryRecord {
        SessionHistoryRecord::Event {
            event: SessionWireEvent {
                event_type: event_type.into(),
                seq: Some(SessionSeq(seq)),
                time,
                request_id: None,
                ignorable: None,
                source_event_seqs: None,
                surface_op: None,
                data: Some(data),
            },
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
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("hello"));
        assert!(rendered.contains("packed text"));
        assert!(!rendered.contains("delta"));
    }

    #[test]
    fn renders_time_prefix_and_running_pending_error_markers() {
        let mut window = TranscriptWindow::new(20);
        window.apply(Incoming::Snapshot {
            cursor: None,
            records: vec![
                // 12:34 UTC = 45_240_000 ms。
                event_with(1, "user/message", Some("q1"), Some(45_240_000)),
                // tool result 带 isError → 保留 `!` 标记。
                event_data(
                    2,
                    "tool/result",
                    serde_json::json!({"content": "boom", "isError": true}),
                    None,
                ),
                // assistant 尚无 chunk 行 → running ●。
                event(3, "assistant/message", None),
            ],
            has_more: false,
            projections: None,
        });
        let backend = TestBackend::new(50, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_window(frame, frame.area(), &window, 0, true))
            .unwrap();
        let rendered = rendered_text(&terminal);
        assert!(
            rendered.contains("12:34"),
            "HH:MM 时间前缀, text={rendered}"
        );
        assert!(rendered.contains("!"), "error 标记保留, text={rendered}");
        assert!(rendered.contains("●"), "running 标记, text={rendered}");
        assert!(rendered.contains("(streaming)"), "text={rendered}");
        assert!(!rendered.contains("…"), "text={rendered}");

        // chunks 到达 → 不再 running；末尾追加 user 消息 → pending …。
        window.apply(Incoming::Chunks(crate::api::types::ChunkRow::TextChunks(
            crate::api::types::ChunkData {
                texts: vec!["done".into()],
                ..Default::default()
            },
        )));
        window.apply(Incoming::FollowEvent(SessionWireEvent {
            event_type: "user/message".into(),
            seq: Some(SessionSeq(4)),
            time: None,
            request_id: None,
            ignorable: None,
            source_event_seqs: None,
            surface_op: None,
            data: Some(serde_json::json!({"content": "q2"})),
        }));
        let backend = TestBackend::new(50, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_window(frame, frame.area(), &window, 0, true))
            .unwrap();
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("…"), "pending 标记, text={rendered}");
        assert!(rendered.contains("done"), "text={rendered}");
        assert!(
            !rendered.contains("●"),
            "chunks 到达后不再 running, text={rendered}"
        );
        assert!(rendered.contains("!"), "error 标记保留, text={rendered}");
    }

    #[test]
    fn optimistic_echo_renders_once_and_reconciles_without_duplicate_ac002_06() {
        let mut window = TranscriptWindow::new(20);
        window.apply(Incoming::Snapshot {
            cursor: None,
            records: vec![event(1, "user/message", Some("old"))],
            has_more: false,
            projections: None,
        });
        window.echo(SessionRequestId("req-echo".into()), "optimistic-msg");
        window.echo(SessionRequestId("req-fail".into()), "failed-msg");
        window.fail_echo(
            &SessionRequestId("req-fail".into()),
            "gateway/bad-request",
            "非法请求",
        );
        let backend = TestBackend::new(80, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_window(frame, frame.area(), &window, 0, true))
            .unwrap();
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("…"), "pending 回显标记, text={rendered}");
        assert!(rendered.contains("optimistic-msg"), "text={rendered}");
        assert!(
            rendered.contains("发送失败"),
            "失败回显错误标记, text={rendered}"
        );
        assert!(rendered.contains("gateway/bad-request"), "text={rendered}");
        assert_eq!(
            rendered.matches("optimistic-msg").count(),
            1,
            "pending 只渲染一条: {rendered}"
        );
        // durable 同 requestId 到达 → 对账 retire，内容只来自 durable 一条。
        window.apply(Incoming::FollowEvent(SessionWireEvent {
            event_type: "user/message".into(),
            seq: Some(SessionSeq(9)),
            time: None,
            request_id: Some("req-echo".into()),
            ignorable: None,
            source_event_seqs: None,
            surface_op: None,
            data: Some(serde_json::json!({"content": "optimistic-msg"})),
        }));
        terminal
            .draw(|frame| render_window(frame, frame.area(), &window, 0, true))
            .unwrap();
        let rendered = rendered_text(&terminal);
        assert_eq!(
            rendered.matches("optimistic-msg").count(),
            1,
            "对账后不重复显示: {rendered}"
        );
        // 失败回显仍在（未被 durable 覆盖）。
        assert!(rendered.contains("发送失败"), "text={rendered}");
        // 尾部 durable user 块的「等待回复」标记不重复渲染 echo 内容。
        assert_eq!(
            rendered.matches("req-echo").count(),
            0,
            "requestId 不出现在正文: {rendered}"
        );
    }
}
