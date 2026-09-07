//! Ratatui view composition for the dshtui application.

pub mod agent_town;
pub mod approval;
pub mod chat;
pub mod command_palette;
pub mod composer;
pub mod detail;
pub mod goal;
pub mod image;
pub mod image_view;
pub mod jobs;
pub mod layout;
pub mod markdown;
pub mod mention;
pub mod model_catalog;
pub mod monitor;
pub mod outline;
pub mod picker;
pub mod search;
pub mod settings;
pub mod sidebar;
pub mod skills;
pub mod status;
pub mod subagent;
pub mod tabs;
pub mod theme;
pub mod trajectory;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::app::AppState;

pub use layout::{
    sidebar_width, split, LayoutAreas, DEFAULT_DETAILS_WIDTH, DETAILS_WIDTH_MAX, DETAILS_WIDTH_MIN,
};

/// 毫秒时间戳 → HH:MM（UTC 换算，纯展示用途；不引入 chrono 等新依赖）。
pub(crate) fn format_hhmm(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let hours = secs.rem_euclid(24 * 3600) / 3600;
    let minutes = secs.rem_euclid(3600) / 60;
    format!("{hours:02}:{minutes:02}")
}

/// Render one complete frame from read-only application state.
pub fn render(frame: &mut Frame<'_>, app: &AppState) {
    // REQ-005：详情宽度参数化（Step 6 从 config 注入 AppState；默认 45）。
    let details_width = app.details_width_cells;
    let areas = split(
        frame.area(),
        app.focus == crate::app::Focus::Details,
        details_width,
    );
    sidebar::render(frame, areas.sidebar, app);

    // 中心区顶部 1 行：Tab 条（Chat / Trajectory，Notes/04 §1 header）。
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(areas.center);
    let tabs_area = vertical[0];
    let center_body = vertical[1];
    tabs::render(frame, tabs_area, app);

    if app.conn == crate::app::ConnState::StartupFailed {
        let body = Paragraph::new(app.guidance_text())
            .block(Block::default().borders(Borders::ALL).title(" Startup "));
        frame.render_widget(body, center_body);
    } else if app.is_reconnecting() && app.active_window().is_none() {
        let body = Paragraph::new("Connection lost. Reconnecting…").block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Reconnecting "),
        );
        frame.render_widget(body, center_body);
    } else if app.mode == crate::app::Mode::Trajectory {
        trajectory::render(frame, center_body, app);
    } else if app.mode == crate::app::Mode::ImageView {
        // REQ-004：IMAGEVIEW 模式中心区渲染 ImageView（仅 Kitty 出现）。
        image_view::render(
            frame,
            center_body,
            &app.image_view,
            app.image_frame.as_ref(),
        );
    } else {
        chat::render(frame, center_body, app);
    }

    if let Some(details) = areas.details {
        // REQ-005：Trajectory 详情子层渲染右栏详情；否则保持既有 Details
        // 会话概览。
        if app.mode == crate::app::Mode::Trajectory && app.traj.detail_open {
            detail::render(frame, details, app);
        } else {
            render_details(frame, details, app);
        }
    }
    status::render(frame, areas.status, app);
    // composer overlay 覆盖 body 底部、状态条上方（仅 INSERT 可见，REQ-002）。
    composer::render(frame, areas.center, app);
    picker::render(frame, frame.area(), app);
    // REQ-003 overlays：大纲 → 搜索 → 审批（后渲染者在上）。
    outline::render(frame, areas.center, app);
    search::render(frame, areas.center, app);
    approval::render(frame, areas.center, app);
    // REQ-006：模型目录 overlay（`M` 打开，在审批之上、帮助之下）。
    model_catalog::render(frame, areas.center, app);
    // REQ-006：命令面板 overlay（`:` 打开）。
    command_palette::render(frame, areas.center, app);
    // REQ-007：@ 提及（AC-007-23；命令面板之上）。
    mention::render(frame, frame.area(), app);
    // REQ-007：subagent 目录（FR-007-01；`:` 打开）。
    subagent::render(frame, frame.area(), app);
    // REQ-007：goal 面板（FR-007-02）。
    goal::render(frame, frame.area(), app);
    // REQ-007：jobs 只读面板。
    jobs::render(frame, frame.area(), app);
    // REQ-007：settings / skills 面板。
    settings::render(frame, frame.area(), app);
    skills::render(frame, frame.area(), app);
    // FR-001-07：帮助 overlay 最后渲染，位于 picker 之上。
    render_help(frame, frame.area(), app);
}

/// FR-001-07：居中带边框的键位帮助面板。
fn render_help(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    if !app.help_open {
        return;
    }
    // 有 `[keymap]` 覆盖行时加高帮助面板（容纳「自定义键位」节）。
    let height_pct = if app.keymap_override_lines.is_empty() {
        80
    } else {
        94
    };
    let overlay = centered_rect(area, 70, height_pct);
    frame.render_widget(Clear, overlay);
    let keys: &[(&str, &str)] = &[
        ("j / k", "上/下滚动"),
        ("Ctrl+d / Ctrl+u", "半页滚动"),
        ("G / gg", "跳到末尾 / 开头"),
        ("f", "会话 picker（Enter 打开，Esc 关闭）"),
        ("o", "打开选中会话 / 光标处链接"),
        ("i", "呼出 composer（无会话时提示）"),
        ("Enter", "发送并收起（空输入不发）"),
        ("Ctrl+Enter / Alt+Enter", "换行"),
        ("Esc", "收起 composer（保留草稿）"),
        ("s", "停止运行中的会话"),
        ("/", "搜索（/c /l /i /t 前缀过滤）"),
        ("n / N", "搜索命中间巡览"),
        ("v / V", "视觉选择（字符 / 行）+ y 复制"),
        ("y", "上下文复制（代码块/链接/工具结果）"),
        ("O", "turnOutline 大纲列表"),
        ("] / [", "跳下一 / 上一轮"),
        ("↑ / ↓", "输入历史（INSERT）"),
        ("h / l", "折叠 / 展开项目"),
        // REQ-006 键位（V0.3，D-034/D-035）：模型目录/侧栏视图/命令面板。
        ("M", "模型目录（搜索 + 热切换）"),
        ("gv", "侧栏视图 groupBy/orderBy（本地）"),
        (":", "命令面板（本地命令 + 斜杠 /xxx）"),
        ("?", "帮助"),
        ("q / Ctrl+c", "退出（运行中先 stop；审批中 q=中止）"),
        ("r", "启动失败时重试探测"),
    ];
    let mut lines = keys
        .iter()
        .map(|(key, desc)| {
            Line::from(vec![
                Span::styled(
                    format!("{key:<16}"),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(*desc),
            ])
        })
        .collect::<Vec<_>>();
    // REQ-007 AC-007-21：`[keymap]` 覆盖生效行（未配置则不显示）。
    if !app.keymap_override_lines.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "自定义键位（config.toml [keymap]，覆盖默认）：",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )));
        for l in &app.keymap_override_lines {
            lines.push(Line::from(Span::styled(
                format!("  {l}"),
                Style::default().fg(Color::Yellow),
            )));
        }
    }
    // 有覆盖节且面板不够高时底部对齐（露出「自定义键位」区；无覆盖保持既有
    // 顶部裁剪行为不变）。
    let mut scroll_rows = 0usize;
    let max_rows = overlay.height.saturating_sub(2) as usize; // 上下边框
    if !app.keymap_override_lines.is_empty() && lines.len() > max_rows {
        scroll_rows = lines.len() - max_rows;
    }
    let panel = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" 帮助 Help "))
        .scroll((scroll_rows as u16, 0));
    frame.render_widget(panel, overlay);
}

pub(crate) fn centered_rect(area: Rect, width_percent: u16, height_percent: u16) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height_percent) / 2),
            Constraint::Percentage(height_percent),
            Constraint::Percentage((100 - height_percent) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_percent) / 2),
            Constraint::Percentage(width_percent),
            Constraint::Percentage((100 - width_percent) / 2),
        ])
        .split(vertical[1])[1]
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
    fn complete_frame_renders_three_breakpoint_shapes() {
        let app = AppState::default();
        for (width, expected_sidebar) in [(140, 32), (110, 24), (80, 12)] {
            let backend = TestBackend::new(width, 20);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|frame| render(frame, &app)).unwrap();
            let areas = split(Rect::new(0, 0, width, 20), false, DEFAULT_DETAILS_WIDTH);
            assert_eq!(areas.sidebar.width, expected_sidebar);
            assert!(areas.details.is_none());
        }
    }

    #[test]
    fn help_overlay_shows_key_table_when_open() {
        let mut app = AppState::default();
        app.help_open = true;
        app.conn = crate::app::ConnState::Ready;
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &app)).unwrap();
        let text = rendered_text(&terminal);
        assert!(text.contains("f"), "help must list `f` key, text={text}");
        assert!(text.contains("q"), "help must list `q` key, text={text}");
        assert!(text.contains("帮助"), "help panel title, text={text}");
        assert!(text.contains("退出"), "help must list quit, text={text}");
        assert!(
            text.contains("picker"),
            "help must list picker, text={text}"
        );
    }

    #[test]
    fn help_overlay_shows_keymap_override_lines_when_configured_ac007_21() {
        let mut app = AppState::default();
        app.help_open = true;
        app.conn = crate::app::ConnState::Ready;
        app.keymap_override_lines = vec![
            "[normal] x → MoveDown（默认 j）".into(),
            "[normal] q unbind（Quit）".into(),
        ];
        let backend = TestBackend::new(90, 26);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &app)).unwrap();
        let text = rendered_text(&terminal);
        assert!(
            text.contains("自定义键位"),
            "帮助显示覆盖节标题, text={text}"
        );
        assert!(text.contains("MoveDown"), "帮助显示生效覆盖行, text={text}");
        assert!(text.contains("unbind"), "帮助显示解绑行, text={text}");
    }

    #[test]
    fn help_overlay_without_overrides_has_no_override_section() {
        let mut app = AppState::default();
        app.help_open = true;
        app.conn = crate::app::ConnState::Ready;
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &app)).unwrap();
        let text = rendered_text(&terminal);
        assert!(
            !text.contains("自定义键位"),
            "无覆盖不显示该节, text={text}"
        );
    }

    #[test]
    fn help_overlay_absent_when_closed() {
        let mut app = AppState::default();
        app.help_open = false;
        app.conn = crate::app::ConnState::Ready;
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &app)).unwrap();
        let text = rendered_text(&terminal);
        // 状态条提示行常驻「[?]帮助」字样，故用面板专属内容断言未打开。
        assert!(!text.contains("半页滚动"), "text={text}");
        assert!(!text.contains("picker"), "text={text}");
    }

    #[test]
    fn format_hhmm_converts_milliseconds_to_utc_clock() {
        assert_eq!(format_hhmm(0), "00:00");
        assert_eq!(format_hhmm(45_240_000), "12:34");
        assert_eq!(format_hhmm(59_000), "00:00");
        assert_eq!(format_hhmm(23 * 3_600_000 + 59 * 60_000), "23:59");
    }
}
