//! Transcript/chat rendering.

use std::collections::HashMap;

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block as TuiBlock, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::api::types::AttachmentId;
use crate::app::{AppState, Mode};
use crate::model::{AttachmentRef, Block, PendingEcho, PendingEchoStatus, TranscriptWindow};

use super::format_hhmm;
use super::markdown;

/// 一行渲染输出：可回溯到窗口块下标（视觉选择/搜索命中/焦点高亮的锚）。
pub struct ChatLine {
    pub block_index: Option<usize>,
    pub line: Line<'static>,
}

/// Render the active transcript window using the app viewport.
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    let Some(window) = app.active_window() else {
        let body = Paragraph::new("No active session. Press f to choose a session.")
            .block(TuiBlock::default().borders(Borders::ALL).title(" Chat "));
        frame.render_widget(body, area);
        return;
    };
    let mut lines = window_lines_with_width(
        window,
        area.width as usize,
        &app.image_meta,
        &app.image_errors,
        app.kitty_capable,
        &app.palette,
    );
    apply_highlights(&mut lines, app);
    draw_lines(
        frame,
        area,
        lines,
        app.viewport.offset,
        app.viewport.follow_tail,
    );
}

/// Render a transcript window without requiring mutable model access.
/// （无高亮：视觉选择/搜索命中样式走 `render` + `apply_highlights`。）
/// `image_meta`/`image_errors` 为 REQ-004 拉取后回填/错误占位标注源；
/// `kitty_capable` 控制非 Kitty 的「系统查看器」提示（AC-004-03/07）。
#[allow(clippy::too_many_arguments)]
pub fn render_window(
    frame: &mut Frame<'_>,
    area: Rect,
    window: &TranscriptWindow,
    offset: usize,
    follow_tail: bool,
    image_meta: &HashMap<AttachmentId, AttachmentRef>,
    image_errors: &HashMap<AttachmentId, String>,
    kitty_capable: bool,
    palette: &crate::ui::theme::Palette,
) {
    let lines = window_lines_with_width(
        window,
        area.width as usize,
        image_meta,
        image_errors,
        kitty_capable,
        palette,
    );
    draw_lines(frame, area, lines, offset, follow_tail);
}

fn draw_lines(
    frame: &mut Frame<'_>,
    area: Rect,
    lines: Vec<ChatLine>,
    offset: usize,
    follow_tail: bool,
) {
    let lines: Vec<Line<'static>> = lines.into_iter().map(|l| l.line).collect();
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

/// 窗口全部行：durable 块（markdown 块可多行）+ 乐观回显行。默认宽度 80
/// （无上下文渲染）；`render`/`render_window` 用真实宽度走
/// `window_lines_with_width`。
pub fn window_lines(window: &TranscriptWindow) -> Vec<ChatLine> {
    window_lines_with_width(
        window,
        80,
        &HashMap::new(),
        &HashMap::new(),
        true,
        &crate::ui::theme::Palette::default(),
    )
}

/// 窗口全部行（指定宽度：markdown 换行与渲染缓存 key 都依赖它）。
/// `image_meta`/`image_errors`/`kitty_capable`：REQ-004 图片占位标注源。
pub fn window_lines_with_width(
    window: &TranscriptWindow,
    width: usize,
    image_meta: &HashMap<AttachmentId, AttachmentRef>,
    image_errors: &HashMap<AttachmentId, String>,
    kitty_capable: bool,
    palette: &crate::ui::theme::Palette,
) -> Vec<ChatLine> {
    let len = window.len();
    let mut out = Vec::new();
    for (i, block) in window.blocks().enumerate() {
        for line in block_lines(
            block,
            i + 1 == len,
            width,
            image_meta,
            image_errors,
            kitty_capable,
            palette,
        ) {
            out.push(ChatLine {
                block_index: Some(i),
                line,
            });
        }
    }
    // 乐观回显（REQ-002）：durable 之外追加 pending/error 行；durable
    // 同 requestId 到达后 pending 被对账 retire，绝不重复显示。
    for echo in window.pending() {
        // echo 前缀色走 palette（UserFg/Warn/Error 角色）。
        out.push(ChatLine {
            block_index: None,
            line: echo_line(echo, palette),
        });
    }
    out
}

/// 视觉选择反色 + 搜索当前命中高亮（AC-003-05/12）。块级语义：选择区间
/// 与命中块由 model 层给出，这里只做样式叠加。
pub fn apply_highlights(lines: &mut [ChatLine], app: &AppState) {
    // VISUAL：选择区间整块反色。
    if let Some(sel) = &app.yank.visual {
        let (start, end) = sel.range();
        for chat in lines.iter_mut() {
            if let Some(i) = chat.block_index {
                if (start..=end).contains(&i) {
                    chat.line.style = chat.line.style.add_modifier(Modifier::REVERSED);
                }
            }
        }
    }
    // SEARCH：当前匹配块黄底高亮（与反色选择区分）。
    if app.mode == Mode::Search && app.search.open {
        if let Some(&item_index) = app.search.window_matches.get(app.search.cursor) {
            if let Some(item) = app.search_index.items().get(item_index) {
                if let Some(block_idx) = app.active_window().and_then(|w| w.offset_of(item.seq)) {
                    for chat in lines.iter_mut() {
                        if chat.block_index == Some(block_idx) {
                            chat.line.style = chat.line.style.fg(Color::Black).bg(Color::Yellow);
                        }
                    }
                }
            }
        }
    }
}

/// 乐观回显行：Pending → `…`（黄色）；Failed → `!`（红色）+ 稳定错误码。
fn echo_line(echo: &PendingEcho, palette: &crate::ui::theme::Palette) -> Line<'static> {
    use crate::ui::theme::Role;
    let mut spans: Vec<Span<'static>> = Vec::new();
    match &echo.status {
        PendingEchoStatus::Pending => {
            spans.push(Span::styled(
                "… ",
                Style::default()
                    .fg(palette.color(Role::Warn))
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(
                "U echo ",
                Style::default()
                    .fg(palette.color(Role::UserFg))
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(single_line(&echo.text)));
        }
        PendingEchoStatus::Failed { code, .. } => {
            spans.push(Span::styled(
                "! ",
                Style::default()
                    .fg(palette.color(Role::Error))
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(
                "U echo ",
                Style::default()
                    .fg(palette.color(Role::UserFg))
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(
                format!("(发送失败 {code}) "),
                Style::default().fg(palette.color(Role::Error)),
            ));
            spans.push(Span::raw(single_line(&echo.text)));
        }
    }
    Line::from(spans)
}

/// FR-001-04 §4：携带 time 的 Block 渲染 HH:MM 前缀（UTC，纯展示）；
/// 状态标记：running（空 chunks 的 assistant）●、pending（末尾 user 等待回复）…、
/// error（tool result isError）!。助手块经 markdown 渲染（REQ-003），可多行。
/// `image_meta`/`image_errors`/`kitty_capable` 为 REQ-004 图片占位标注源。
#[allow(clippy::too_many_arguments)]
fn block_lines(
    block: &Block,
    is_last: bool,
    width: usize,
    image_meta: &HashMap<AttachmentId, AttachmentRef>,
    image_errors: &HashMap<AttachmentId, String>,
    kitty_capable: bool,
    palette: &crate::ui::theme::Palette,
) -> Vec<Line<'static>> {
    use crate::ui::theme::Role;
    let mut spans: Vec<Span<'static>> = Vec::new();
    match block {
        Block::UserMessage { seq, content, time } => {
            push_time(&mut spans, *time);
            if is_last {
                // 末尾用户消息：等待 assistant 回复（pending）。
                spans.push(Span::styled(
                    "… ",
                    Style::default()
                        .fg(palette.color(Role::Warn))
                        .add_modifier(Modifier::BOLD),
                ));
            }
            spans.push(Span::styled(
                format!("U {:>5} ", seq.0),
                Style::default()
                    .fg(palette.color(Role::UserFg))
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(single_line(content)));
            vec![Line::from(spans)]
        }
        Block::AssistantMessage {
            seq, chunks, time, ..
        } => {
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
                    .fg(palette.color(Role::AssistantFg))
                    .add_modifier(Modifier::BOLD),
            ));
            if running {
                spans.push(Span::raw("(streaming)"));
                return vec![Line::from(spans)];
            }
            // REQ-003：助手内容 markdown 渲染（标题/列表/表格/代码高亮）。
            // 流式中未闭合代码块先纯文本（AC-003-02）；渲染结果走
            // markdown::markdown_lines 缓存（懒执行，Notes/06 §5）。
            let text = crate::model::search::chunks_text(chunks);
            if markdown::has_unclosed_fence(&text) {
                spans.push(Span::raw(single_line(&text)));
                return vec![Line::from(spans)];
            }
            let lines = markdown::markdown_lines(&text, width);
            if lines.is_empty() {
                spans.push(Span::raw("(packed chunks)"));
                return vec![Line::from(spans)];
            }
            // 首行接在 A/seq 前缀后；后续行等宽缩进保持对齐。
            let mut iter = lines.iter();
            let first = iter.next().expect("lines 非空");
            spans.extend(first.spans.clone());
            let mut out = vec![Line::from(spans)];
            let indent = Span::raw("        "); // 8 空格，与 "A 12345 " 对齐
            for line in iter {
                let mut line_spans = vec![indent.clone()];
                line_spans.extend(line.spans.clone());
                out.push(Line::from(line_spans));
            }
            out
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
                    .fg(palette.color(Role::ToolFg))
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
            vec![Line::from(spans)]
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
                        palette.color(Role::Error)
                    } else {
                        palette.color(Role::Code)
                    })
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(single_line(content)));
            vec![Line::from(spans)]
        }
        Block::RequestHeader { seq, summary } => {
            spans.push(Span::styled(
                format!("H {:>5} ", seq.0),
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(single_line(summary)));
            vec![Line::from(spans)]
        }
        Block::Compaction { seq, summary } => {
            spans.push(Span::styled(
                format!("C {:>5} ", seq.0),
                Style::default()
                    .fg(Color::LightBlue)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(single_line(summary)));
            vec![Line::from(spans)]
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
            // AC-004-01：恒占位 `名称 · 宽×高`；拉取成功后以 AttachmentRef
            // 回填刷新标注（占位位置不变）；失败 → 错误占位 + 可读提示。
            let meta = attachment_id
                .as_deref()
                .and_then(|id| image_meta.get(&AttachmentId(id.to_string())));
            let error = attachment_id
                .as_deref()
                .and_then(|id| image_errors.get(&AttachmentId(id.to_string())));
            let display_name = meta
                .and_then(|m| m.name.as_deref())
                .or(name.as_deref())
                .unwrap_or("image");
            let display_dims = match meta {
                Some(m) if m.width > 0 && m.height > 0 => {
                    format!("{}x{}", m.width, m.height)
                }
                _ => dims.clone().unwrap_or_else(|| "?".into()),
            };
            if let Some(err) = error {
                spans.push(Span::raw(format!("{display_name} · ")));
                spans.push(Span::styled(
                    format!("{display_dims} ✗ {err}"),
                    Style::default().fg(palette.color(Role::Error)),
                ));
            } else {
                spans.push(Span::raw(format!("{display_name} · {display_dims}")));
            }
            if error.is_none() && !kitty_capable {
                // AC-004-03/07: show a visible fallback box/path hint in the transcript.
                spans.push(Span::styled(
                    " [image placeholder] [o]system viewer",
                    Style::default().fg(Color::DarkGray),
                ));
                if let Some(id) = attachment_id {
                    spans.push(Span::styled(
                        format!(" path:{id}"),
                        Style::default().fg(Color::DarkGray),
                    ));
                }
            }
            vec![Line::from(spans)]
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
            vec![Line::from(spans)]
        }
    }
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
            .draw(|frame| {
                render_window(
                    frame,
                    frame.area(),
                    &window,
                    0,
                    true,
                    &HashMap::new(),
                    &HashMap::new(),
                    true,
                    &crate::ui::theme::Palette::default(),
                )
            })
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
            .draw(|frame| {
                render_window(
                    frame,
                    frame.area(),
                    &window,
                    0,
                    true,
                    &HashMap::new(),
                    &HashMap::new(),
                    true,
                    &crate::ui::theme::Palette::default(),
                )
            })
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
            .draw(|frame| {
                render_window(
                    frame,
                    frame.area(),
                    &window,
                    0,
                    true,
                    &HashMap::new(),
                    &HashMap::new(),
                    true,
                    &crate::ui::theme::Palette::default(),
                )
            })
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
            .draw(|frame| {
                render_window(
                    frame,
                    frame.area(),
                    &window,
                    0,
                    true,
                    &HashMap::new(),
                    &HashMap::new(),
                    true,
                    &crate::ui::theme::Palette::default(),
                )
            })
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
            .draw(|frame| {
                render_window(
                    frame,
                    frame.area(),
                    &window,
                    0,
                    true,
                    &HashMap::new(),
                    &HashMap::new(),
                    true,
                    &crate::ui::theme::Palette::default(),
                )
            })
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

    #[test]
    fn palette_roles_drive_user_prefix_color_ac007_20() {
        // 默认 dark：UserFg=Cyan；palette 覆盖 accent/user_fg 后渲染必须读到
        // 覆盖色（证明 theme/palette 真实接入渲染，而非仅解析）。
        let mut window = TranscriptWindow::new(20);
        window.apply(Incoming::Snapshot {
            cursor: None,
            records: vec![event(1, "user/message", Some("hi"))],
            has_more: false,
            projections: None,
        });
        let default_palette = crate::ui::theme::Palette::default();
        let mut overrides = std::collections::BTreeMap::new();
        overrides.insert("user_fg".to_string(), "#ff0000".to_string());
        let red_palette = crate::ui::theme::Palette::build("dark", &overrides);

        // 默认 palette 渲染：U 前缀 cell 为 Cyan。
        let backend = TestBackend::new(30, 4);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_window(
                    frame,
                    frame.area(),
                    &window,
                    0,
                    true,
                    &HashMap::new(),
                    &HashMap::new(),
                    true,
                    &default_palette,
                )
            })
            .unwrap();
        let default_fg = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .find(|c| c.symbol() == "U")
            .map(|c| c.fg)
            .expect("U 前缀存在");
        assert_eq!(default_fg, ratatui::style::Color::Cyan);

        // 覆盖 palette 渲染：U 前缀 cell 变红。
        let backend = TestBackend::new(30, 4);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_window(
                    frame,
                    frame.area(),
                    &window,
                    0,
                    true,
                    &HashMap::new(),
                    &HashMap::new(),
                    true,
                    &red_palette,
                )
            })
            .unwrap();
        let red_fg = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .find(|c| c.symbol() == "U")
            .map(|c| c.fg)
            .expect("U 前缀存在");
        assert_eq!(red_fg, ratatui::style::Color::Rgb(255, 0, 0));
    }
}
