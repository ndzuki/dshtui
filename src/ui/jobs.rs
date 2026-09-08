//! Jobs read-only panel (REQ-007 AC-007-13; wire correction: 0.1.2-rc.1 has
//! NO job stop endpoint and the official web jobs panel is read-only — no
//! stop control here, only observability of running/stopping + guidance to
//! the official web).

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use crate::api::types::{SessionJob, SessionJobStatus};
use crate::app::AppState;
use crate::ui::theme::Role;

fn status_style(status: &Option<SessionJobStatus>) -> (Color, &'static str) {
    match status {
        Some(SessionJobStatus::Running) => (Color::Green, "running"),
        Some(SessionJobStatus::Stopping) => (Color::Yellow, "stopping"),
        Some(SessionJobStatus::Completed) => (Color::Cyan, "completed"),
        Some(SessionJobStatus::Killed) => (Color::Red, "killed"),
        Some(SessionJobStatus::Failed) => (Color::Red, "failed"),
        None => (Color::Gray, "?"),
    }
}

/// duration 展示（startedAt/finishedAt 秒差；缺字段 → "—"）。
fn duration(job: &SessionJob) -> String {
    match (job.started_at, job.finished_at) {
        (Some(s), Some(f)) if f >= s => {
            let secs = (f - s) / 1000;
            if secs >= 60 {
                format!("{}m{:02}s", secs / 60, secs % 60)
            } else {
                format!("{secs}s")
            }
        }
        _ => "—".to_string(),
    }
}

/// Jobs 只读面板渲染（`:jobs`；Mode::Jobs）。
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    if app.mode != crate::app::Mode::Jobs || !app.jobs.visible {
        return;
    }
    let j = &app.jobs;
    let accent = app.palette.color(Role::Accent);
    let warn = app.palette.color(Role::Warn);
    let sel = app.palette.color(Role::Selection);

    let width = area.width.saturating_sub(4).min(72);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let height = area.height.saturating_sub(2).min(16);
    let y = area.y + area.height.saturating_sub(height) / 2;
    let overlay = Rect::new(x, y, width, height);
    frame.render_widget(Clear, overlay);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Jobs（只读） ");
    frame.render_widget(block, overlay);
    let inner = Rect::new(
        overlay.x + 1,
        overlay.y + 1,
        width.saturating_sub(2),
        height.saturating_sub(2),
    );

    if j.jobs.is_empty() {
        let empty = Paragraph::new(Line::from(Span::styled(
            " 无运行任务（官方 jobs 镜像为空）",
            Style::default().fg(warn),
        )));
        frame.render_widget(empty, inner);
        return;
    }

    let items: Vec<ListItem> = j
        .jobs
        .iter()
        .enumerate()
        .map(|(i, job)| {
            let (color, st) = status_style(&job.status);
            let text = format!(
                "{} {} · {}",
                st,
                if job.label.is_empty() {
                    job.kind.clone()
                } else {
                    job.label.clone()
                },
                duration(job)
            );
            let detail = job.detail.clone().unwrap_or_default();
            let style = if i == j.selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(sel)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Reset)
            };
            let mut spans = vec![Span::styled(text, style)];
            spans.push(Span::styled(
                format!(" [{}]", job.id),
                Style::default().fg(color),
            ));
            if !detail.is_empty() {
                spans.push(Span::styled(
                    format!(" {detail}"),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let mut list_state = ListState::default();
    list_state.select(Some(j.selected));
    let list = List::new(items).highlight_style(
        Style::default()
            .fg(Color::Black)
            .bg(sel)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_stateful_widget(list, inner, &mut list_state);

    // 底部指引（wire 校正：官方无停止）。
    let hint = Paragraph::new(Line::from(Span::styled(
        " [j/k]移动 [q]关闭 · 官方 0.1.2 无停止端点——停止请到官方 web 操作",
        Style::default().fg(accent),
    )));
    let hint_area = Rect::new(
        inner.x,
        inner.y + inner.height.saturating_sub(1),
        inner.width,
        1,
    );
    frame.render_widget(hint, hint_area);
}

#[cfg(test)]
mod tests {
    use super::*;
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

    fn job(id: &str, status: SessionJobStatus, label: &str) -> SessionJob {
        SessionJob {
            id: id.into(),
            kind: "tool/call".into(),
            label: label.into(),
            status: Some(status),
            started_at: Some(1_000),
            finished_at: Some(160_000),
            ..Default::default()
        }
    }

    #[test]
    fn jobs_panel_renders_rows_and_hint() {
        let mut app = crate::app::AppState::default();
        app.mode = crate::app::Mode::Jobs;
        app.jobs.open();
        app.jobs.replace(vec![
            job("j1", SessionJobStatus::Running, "跑测试"),
            job("j2", SessionJobStatus::Completed, "lint"),
        ]);
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        let text = rendered_text(&terminal);
        assert!(text.contains("Jobs"), "标题, text={text}");
        assert!(text.contains("跑测试"));
        assert!(text.contains("running"));
        assert!(text.contains("completed"));
        assert!(
            text.contains("官方 0.1.2 无停止端点"),
            "wire 校正指引, text={text}"
        );
    }

    #[test]
    fn jobs_panel_empty_and_hidden() {
        let mut app = crate::app::AppState::default();
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        assert!(!rendered_text(&terminal).contains("Jobs（只读）"));
        app.mode = crate::app::Mode::Jobs;
        app.jobs.open();
        app.jobs.replace(Vec::new());
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        assert!(rendered_text(&terminal).contains("无运行任务"));
    }
}
