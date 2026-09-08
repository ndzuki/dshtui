//! ImageView 组件（REQ-004 §4；Notes/05 §10）：Loading 转场 / Rendered
//! （Kitty 协议帧）/ Failed 三态 + 标题行。仅 IMAGEVIEW 模式渲染（模式路由
//! 在 `ui/mod.rs`）；非 Kitty 终端不进入本组件（AC-004-07 直达系统查看器）。

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::app::KittyFrame;
use crate::model::{ImageViewPhase, ImageViewState};

use super::image::render_placeholder_box;

/// 渲染 ImageView（Kitty 态）。`frame_protocol` 为 Rendered 阶段的已编码
/// Kitty 帧（AppState 持有）。
pub fn render(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &ImageViewState,
    frame_protocol: Option<&KittyFrame>,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(area);

    // 标题行：`名称 · 宽×高`（05 §10）+ 同消息多图 pager `(i/N)`（AC-007-06）
    // + zoom 百分比（REQ-007 D-45；非 1.0 时显示，如 `125%`）。
    let pager_tag = view
        .pager
        .as_ref()
        .filter(|p| p.total > 1)
        .map(|p| format!("({}/{}) ", p.index + 1, p.total))
        .unwrap_or_default();
    let zoom_tag = if (view.zoom - 1.0).abs() > f32::EPSILON {
        format!(" {}% ", (view.zoom * 100.0).round() as i64)
    } else {
        String::new()
    };
    let title = match (&view.name, &view.dims) {
        (Some(name), Some(dims)) => format!(" {name} · {dims} "),
        (Some(name), None) => format!(" {name} "),
        (None, Some(dims)) => format!(" image · {dims} "),
        (None, None) => " image ".into(),
    };
    let title_line = Line::from(vec![
        Span::styled(
            pager_tag,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            title,
            Style::default()
                .fg(Color::LightMagenta)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            zoom_tag,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    frame.render_widget(Paragraph::new(title_line), chunks[0]);

    let body = chunks[1];
    match view.phase {
        ImageViewPhase::Loading => {
            // 加载中转场（解码在 spawn_blocking，不阻塞帧循环）。
            frame.render_widget(
                Paragraph::new("加载中…")
                    .block(Block::default().borders(Borders::ALL).title(" ImageView ")),
                body,
            );
        }
        ImageViewPhase::Rendered => match frame_protocol {
            Some(kitty) => {
                let image = ratatui_image::Image::new(kitty.0.as_ref());
                frame.render_widget(image, body);
            }
            None => {
                // 状态与帧不一致（不应发生）：按 Failed 降级，不崩溃。
                render_placeholder_box(frame, body, "ImageView", &["渲染帧缺失".to_string()]);
            }
        },
        ImageViewPhase::Failed => {
            let message = view
                .error
                .as_ref()
                .map(|e| format!("{}: {}", e.code, e.message))
                .unwrap_or_else(|| "图片不可用".into());
            render_placeholder_box(frame, body, "ImageView", &[message]);
        }
        ImageViewPhase::Closed => {
            // 防御：模式残留时占位，不 panic。
            render_placeholder_box(frame, body, "ImageView", &["已关闭".to_string()]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::types::{AttachmentId, SessionSeq};
    use crate::model::{ImageViewPhase, ImageViewState};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn rendered_text(terminal: &Terminal<TestBackend>) -> String {
        // 宽字符占两个 cell（后一 cell 为占位空格）：跳过占位。
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
    fn loading_state_shows_transition_and_title() {
        let mut view = ImageViewState::default();
        view.open_view(
            SessionSeq(7),
            AttachmentId("att-1".into()),
            Some("design.png".into()),
            Some("640x480".into()),
        );
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, f.area(), &view, None)).unwrap();
        let text = rendered_text(&terminal);
        assert!(text.contains("design.png · 640x480"), "text={text}");
        assert!(text.contains("加载中"), "text={text}");
    }

    #[test]
    fn failed_state_shows_error_placeholder_without_crash() {
        let mut view = ImageViewState::default();
        view.open_view(SessionSeq(7), AttachmentId("att-1".into()), None, None);
        view.mark_failed("decode/unsupported".into(), "不支持的媒体类型".into());
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, f.area(), &view, None)).unwrap();
        let text = rendered_text(&terminal);
        assert!(text.contains("decode/unsupported"), "text={text}");
        assert!(text.contains("不支持的媒体类型"), "text={text}");
    }

    #[test]
    fn rendered_without_frame_falls_back_without_panic() {
        let mut view = ImageViewState::default();
        view.open_view(SessionSeq(7), AttachmentId("att-1".into()), None, None);
        view.mark_rendered();
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, f.area(), &view, None)).unwrap();
        let text = rendered_text(&terminal);
        assert!(
            text.contains("渲染帧缺失"),
            "Rendered 但无帧必须降级占位, text={text}"
        );
        assert_eq!(view.phase, ImageViewPhase::Rendered);
    }

    #[test]
    fn zoom_title_tag_shows_percent_when_not_one() {
        // REQ-007 D-45：zoom != 1.0 时标题显示百分比；1.0 不显示。
        let mut view = ImageViewState::default();
        view.open_view(
            SessionSeq(7),
            AttachmentId("att-1".into()),
            Some("a.png".into()),
            None,
        );
        view.mark_rendered();
        view.zoom = 1.25;
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, f.area(), &view, None)).unwrap();
        let text = rendered_text(&terminal);
        assert!(text.contains("125%"), "zoom 标题, text={text}");

        view.zoom_reset();
        terminal.draw(|f| render(f, f.area(), &view, None)).unwrap();
        let text2 = rendered_text(&terminal);
        assert!(!text2.contains("%"), "zoom=1 不显示百分比, text={text2}");
    }
}
