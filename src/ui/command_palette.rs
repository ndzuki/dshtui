//! 命令面板 overlay（REQ-006 FR-006-03，`:` 打开）。
//!
//! 输入行 + 候选列表：本地 TUI 动作（可执行）+ 远端斜杠命令（`commands/list`
//! 动态注册）+ V0.4 占位（禁用提示）。过滤/选中由
//! `CommandPaletteState::filtered` 单一 seam（reducer 与 UI 共用），本地过滤无
//! 网络往返。

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::app::{AppState, CommandPaletteItem};

/// 渲染命令面板 overlay（仅 open 时）。列表用 List widget（自动滚动到光标行），
/// 底部固定状态行（执行中/结果/提示）。
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    if !app.command_palette.visible {
        return;
    }
    use crate::app::PaletteStage;
    let overlay = crate::ui::centered_rect(area, 76, 76);
    frame.render_widget(Clear, overlay);
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(2),
            Constraint::Length(3),
        ])
        .split(overlay);

    // 子阶段标题/输入提示。
    let (input_title, input_prefix): (&str, &str) = match &app.command_palette.stage {
        Some(PaletteStage::Input { prompt, .. }) => (prompt, "> "),
        Some(PaletteStage::ConfirmDanger { .. }) => ("确认操作", ""),
        None => (" Command ", ": "),
    };
    let query = Paragraph::new(Line::from(vec![
        Span::styled(
            input_prefix,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(app.command_palette.query.as_str()),
    ]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" {input_title} ")),
    );
    frame.render_widget(query, parts[0]);

    // 子阶段主体区。
    match &app.command_palette.stage {
        Some(PaletteStage::Input { .. }) => {
            let lines = vec![Line::from(Span::styled(
                " 输入参数后 [Enter] 提交；[Esc] 取消返回命令列表",
                Style::default().fg(Color::DarkGray),
            ))];
            frame.render_widget(
                Paragraph::new(lines).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Cyan)),
                ),
                parts[1],
            );
        }
        Some(PaletteStage::ConfirmDanger { label, op }) => {
            let lines = vec![
                Line::from(Span::styled(
                    format!(" ⚠ 危险操作：{label}"),
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(
                    format!(" 端点：{}", op.label()),
                    Style::default().fg(Color::Gray),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    " [Enter] 确认执行   [Esc] 取消",
                    Style::default().fg(Color::Yellow),
                )),
            ];
            frame.render_widget(
                Paragraph::new(lines).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Red)),
                ),
                parts[1],
            );
        }
        None => {
            let items = app.command_palette.filtered();
            let list_items: Vec<ratatui::widgets::ListItem> = items
                .iter()
                .map(|item| {
                    let (label, desc, color, suffix) = match item {
                        CommandPaletteItem::Local { label, desc, .. } => {
                            (*label, *desc, Color::Cyan, "")
                        }
                        CommandPaletteItem::Remote { name, desc } => {
                            (name.as_str(), desc.as_str(), Color::Green, "")
                        }
                        CommandPaletteItem::V04 { label, desc } => {
                            (*label, *desc, Color::DarkGray, " (V0.4)")
                        }
                    };
                    let line = Line::from(vec![
                        Span::styled(
                            format!("{label}{suffix}"),
                            Style::default().fg(color).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(format!("  — {desc}"), Style::default().fg(Color::Gray)),
                    ]);
                    ratatui::widgets::ListItem::new(line)
                })
                .collect();
            let mut list_state = ratatui::widgets::ListState::default();
            if !list_items.is_empty() {
                list_state.select(Some(
                    app.command_palette.selection.min(list_items.len() - 1),
                ));
            }
            let empty_note = if items.is_empty() {
                " （无匹配命令）"
            } else {
                ""
            };
            let list = ratatui::widgets::List::new(list_items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Cyan))
                        .title(if empty_note.is_empty() {
                            format!(" {} commands ", items.len())
                        } else {
                            empty_note.to_string()
                        }),
                )
                .highlight_style(Style::default().bg(Color::Blue).fg(Color::White))
                .highlight_symbol("▌");
            frame.render_stateful_widget(list, parts[1], &mut list_state);
        }
    }

    // 底部状态/提示行。
    let mut status_spans: Vec<Span<'static>> = Vec::new();
    if app.command_palette.executing {
        status_spans.push(Span::styled(
            " 执行中… ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if let Some(result) = &app.command_palette.last_result {
        let is_err = app.command_palette.last_error_code.is_some();
        status_spans.push(Span::styled(
            result.clone(),
            Style::default().fg(if is_err { Color::Red } else { Color::Green }),
        ));
    } else if !app.command_palette.remote_fetched {
        status_spans.push(Span::styled(
            "打开会话后可用远端斜杠命令 /xxx",
            Style::default().fg(Color::DarkGray),
        ));
    }
    status_spans.push(Span::raw("   "));
    if app.command_palette.stage.is_some() {
        status_spans.push(Span::styled("[Enter]", cyan()));
        status_spans.push(Span::raw(" 确认  "));
        status_spans.push(Span::styled("[Esc]", gray()));
        status_spans.push(Span::raw(" 取消"));
    } else {
        status_spans.push(Span::styled("[j/k]", gray()));
        status_spans.push(Span::raw(" 移动  "));
        status_spans.push(Span::styled("[Enter]", cyan()));
        status_spans.push(Span::raw(" 执行  "));
        status_spans.push(Span::styled("[q/Esc]", gray()));
        status_spans.push(Span::raw(" 关闭"));
    }
    frame.render_widget(Paragraph::new(Line::from(status_spans)), parts[2]);
}

fn gray() -> Style {
    Style::default()
        .fg(Color::Gray)
        .add_modifier(Modifier::BOLD)
}
fn cyan() -> Style {
    Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{AppState, Mode};
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
    fn renders_local_commands_and_v04_placeholder() {
        let mut app = AppState::default();
        app.mode = Mode::CommandPalette;
        app.command_palette.visible = true;
        // settings 已转真实动作（REQ-007），keymap 仍为 V0.4 占位。
        app.command_palette.query = "settings".into();
        let backend = TestBackend::new(120, 14);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        let text = rendered_text(&terminal);
        assert!(text.contains("Command"), "面板标题, text={text}");
        assert!(text.contains("settings"), "settings 真实动作, text={text}");
        assert!(text.contains("白名单编辑"), "真实 desc, text={text}");
        // keymap 仍 V0.4 占位。
        app.command_palette.query = "keymap".into();
        let backend = TestBackend::new(120, 14);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        let text = rendered_text(&terminal);
        assert!(text.contains("V0.4"), "keymap 仍是 V0.4 占位, text={text}");
        assert!(
            !text.contains("model catalog"),
            "过滤后不显示其它命令, text={text}"
        );
    }

    #[test]
    fn renders_remote_slash_commands_when_fetched() {
        let mut app = AppState::default();
        app.mode = Mode::CommandPalette;
        app.command_palette.visible = true;
        app.command_palette.remote_fetched = true;
        app.command_palette.remote_commands = vec![
            crate::api::types::CommandDescriptor {
                name: "plan".into(),
                description: "Plan mode on/off".into(),
                input: None,
            },
            crate::api::types::CommandDescriptor {
                name: "help".into(),
                description: "Help".into(),
                input: None,
            },
        ];
        app.command_palette.query = "plan".into();
        let backend = TestBackend::new(120, 14);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        let text = rendered_text(&terminal);
        assert!(text.contains("plan"), "斜杠命令, text={text}");
        assert!(text.contains("Plan mode"), "描述, text={text}");
    }

    #[test]
    fn renders_confirm_danger_stage_ac006_02() {
        // Step 6：危险操作二次确认子阶段（不发送前显示）。
        let mut app = AppState::default();
        app.mode = Mode::CommandPalette;
        app.command_palette.visible = true;
        app.command_palette.stage = Some(crate::app::PaletteStage::ConfirmDanger {
            label: "archive session".into(),
            op: crate::app::WorkspaceOperation::ArchiveSession {
                session_id: crate::api::types::SessionId::new("s1".into()),
            },
        });
        let backend = TestBackend::new(120, 22);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        let text = rendered_text(&terminal);
        assert!(text.contains("危险操作"), "text={text}");
        assert!(text.contains("archive session"), "text={text}");
        assert!(text.contains("确认执行"), "text={text}");
        assert!(
            !text.contains("commands"),
            "子阶段不显示命令列表, text={text}"
        );
    }
}
