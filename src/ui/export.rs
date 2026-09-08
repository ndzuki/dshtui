//! Session export overlay (REQ-007 AC-007-17).
//!
//! Primary path = official same-origin HTTP `/api/session.export` ZIP
//! (byte-identical). Path is user-editable; Enter downloads with progress;
//! Esc during download marks cancel (completion cleans the temp file).

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::AppState;
use crate::model::export::ExportPhase;
use crate::ui::theme::Role;

/// Export 面板渲染（`:export`；Mode::Export）。
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    if app.mode != crate::app::Mode::Export || !app.export.visible {
        return;
    }
    let e = &app.export;
    let accent = app.palette.color(Role::Accent);
    let err = app.palette.color(Role::Error);
    let ok = app.palette.color(Role::AssistantFg);

    let width = area.width.saturating_sub(4).min(70);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let height = 9.min(area.height.saturating_sub(2));
    let y = area.y + area.height.saturating_sub(height) / 2;
    let overlay = Rect::new(x, y, width, height);
    frame.render_widget(Clear, overlay);
    let block = Block::default().borders(Borders::ALL).title(" Export ");
    frame.render_widget(block, overlay);
    let inner = Rect::new(
        overlay.x + 1,
        overlay.y + 1,
        width.saturating_sub(2),
        height.saturating_sub(2),
    );

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(
            " session ",
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        ),
        Span::raw(e.session_id.clone().unwrap_or_default()),
    ]));
    match e.phase {
        ExportPhase::PickingPath => {
            lines.push(Line::from(vec![
                Span::styled(
                    " path ",
                    Style::default().fg(accent).add_modifier(Modifier::BOLD),
                ),
                Span::raw(if e.path.is_empty() {
                    "（输入目标 zip 路径）".to_string()
                } else {
                    e.path.clone()
                }),
            ]));
            lines.push(Line::from(Span::styled(
                " [Enter] 下载官方 ZIP（session.jsonl+subagents+media） [Esc] 取消",
                Style::default().fg(ok),
            )));
        }
        ExportPhase::Downloading => {
            lines.push(Line::from(Span::styled(
                format!(" ⏳ 下载中… {} 字节", e.bytes_streamed),
                Style::default().fg(Color::Yellow),
            )));
            lines.push(Line::from(Span::styled(
                " [Esc] 取消（完成后清理临时文件）",
                Style::default().fg(accent),
            )));
        }
        ExportPhase::Rebuilding => {
            // D-46 兜底：官方路由不可用 → session/page 全量重建 JSONL。
            lines.push(Line::from(Span::styled(
                format!(
                    " ♻ 官方导出路由不可用，本地重建 JSONL… 已收集 {} 条 records",
                    e.records_collected
                ),
                Style::default().fg(Color::Yellow),
            )));
            lines.push(Line::from(Span::styled(
                " [Esc] 取消（完成后清理临时文件）",
                Style::default().fg(accent),
            )));
        }
        ExportPhase::Done => {
            lines.push(Line::from(Span::styled(
                format!(
                    " ✓ 已写入 {}（{} 字节）——任意键关闭",
                    e.path, e.bytes_streamed
                ),
                Style::default().fg(ok),
            )));
        }
        ExportPhase::Failed => {
            lines.push(Line::from(Span::styled(
                format!(
                    " ✗ 导出失败: {}",
                    e.last_error_code.clone().unwrap_or_default()
                ),
                Style::default().fg(err),
            )));
            lines.push(Line::from(Span::styled(
                " [Esc] 关闭（可重开重试）",
                Style::default().fg(accent),
            )));
        }
        ExportPhase::Idle => {}
    }
    let para = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(para, inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ExportState;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn rendered_text(terminal: &Terminal<TestBackend>) -> String {
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
    fn export_overlay_renders_path_pick_and_done() {
        let mut app = crate::app::AppState::default();
        app.mode = crate::app::Mode::Export;
        let mut state = ExportState::default();
        state.open("sess-1", "out.zip");
        app.export = state;
        let backend = TestBackend::new(100, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        let text = rendered_text(&terminal);
        assert!(text.contains("Export"), "标题, text={text}");
        assert!(text.contains("sess-1"));
        assert!(text.contains("out.zip"));
        assert!(text.contains("下载官方 ZIP"), "text={text}");
    }

    #[test]
    fn export_overlay_hidden_when_not_active() {
        let app = crate::app::AppState::default();
        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        assert!(!rendered_text(&terminal).contains("Export"));
    }
}
