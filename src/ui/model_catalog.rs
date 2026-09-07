//! 模型目录 overlay（REQ-006 FR-006-01，`M` 打开）：输入行 + 命中列表 +
//! 当前/next 标记 + effort 子阶段。只读 AppState，纯渲染；过滤与选中由
//! `CatalogIndex::query` 单一 seam（本地 nucleo 即时，ADR-003）。
//!
//! 阶段渲染：Loading（加载中）/ Error（error.code + 重试提示，AC-006-06）/
//! Ready（命中列表或空态，AC-006-07）；effort 子阶段渲染 reasoning 候选。

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::app::{AppState, CatalogPhase};

/// 渲染模型目录 overlay（仅 open 时）。
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    if !app.model_catalog.visible {
        return;
    }
    let overlay = crate::ui::centered_rect(area, 80, 72);
    frame.render_widget(Clear, overlay);
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(2)])
        .split(overlay);

    // 输入行：`> query` + 命中/总数。
    let total = app.model_catalog.index.len();
    let hits = app.model_catalog.index.query(&app.model_catalog.query);
    let query = Paragraph::new(Line::from(vec![
        Span::styled(
            "> ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(app.model_catalog.query.as_str()),
        Span::styled(
            format!("  {}/{}", hits.len(), total),
            Style::default().fg(Color::DarkGray),
        ),
    ]))
    .block(Block::default().borders(Borders::ALL).title(title(app)));
    frame.render_widget(query, parts[0]);

    let mut lines: Vec<Line<'static>> = Vec::new();
    match app.model_catalog.phase {
        CatalogPhase::Loading => {
            lines.push(Line::from(Span::styled(
                " 加载模型目录…",
                Style::default().fg(Color::DarkGray),
            )));
        }
        CatalogPhase::Error => {
            lines.push(Line::from(Span::styled(
                format!(
                    " ⚠ {}",
                    app.model_catalog
                        .load_error
                        .as_deref()
                        .unwrap_or("模型目录加载失败")
                ),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(Span::styled(
                " [Enter] 重试   [q/Esc] 关闭（网络恢复后重试即成功）",
                Style::default().fg(Color::DarkGray),
            )));
        }
        CatalogPhase::Ready => {
            // effort 子阶段：reasoning 候选选择。
            if let Some(pick) = &app.model_catalog.effort {
                lines.push(Line::from(Span::styled(
                    format!(
                        " {} ({}/{})",
                        pick.model_name, pick.provider_id, pick.model_id
                    ),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )));
                lines.push(Line::from(Span::styled(
                    " reasoning effort（下一次 prompt 生效）",
                    Style::default().fg(Color::DarkGray),
                )));
                for (idx, effort) in pick.efforts.iter().enumerate() {
                    let cursor = idx == pick.cursor;
                    let mut marker = if cursor { "▌" } else { " " };
                    if effort == pick.default.as_deref().unwrap_or("") {
                        marker = if cursor { "▌✓" } else { " ✓" };
                    }
                    let mut line = Line::from(vec![Span::raw(format!("{marker} {effort}"))]);
                    if cursor {
                        for s in line.spans.iter_mut() {
                            s.style = s.style.add_modifier(Modifier::REVERSED);
                        }
                    }
                    lines.push(line);
                }
                if pick.efforts.is_empty() {
                    lines.push(Line::from(Span::styled(
                        " （该模型未声明可选项，直接提交）",
                        Style::default().fg(Color::DarkGray),
                    )));
                }
                lines.push(Line::from(""));
                lines.push(Line::from(vec![
                    Span::styled("[j/k]", gray()),
                    Span::raw(" 选择  "),
                    Span::styled("[Enter]", cyan()),
                    Span::raw(" 确认切换  "),
                    Span::styled("[Esc]", gray()),
                    Span::raw(" 返回目录"),
                ]));
            } else if hits.is_empty() {
                // AC-006-07：明确空态（无匹配 ≠ 错误；退查询词即恢复全量）。
                lines.push(Line::from(Span::styled(
                    " （无匹配模型）",
                    Style::default().fg(Color::DarkGray),
                )));
                if !app.model_catalog.query.is_empty() {
                    lines.push(Line::from(Span::styled(
                        " 清空搜索词（退格）恢复全量；[q/Esc] 关闭",
                        Style::default().fg(Color::DarkGray),
                    )));
                } else {
                    lines.push(Line::from(Span::styled(
                        " 目录为空：官方 modelCatalog 未返回可用模型",
                        Style::default().fg(Color::DarkGray),
                    )));
                }
            } else {
                for (idx, item) in hits.iter().enumerate() {
                    lines.push(catalog_row(item, idx == app.model_catalog.selection, app));
                }
                lines.push(Line::from(""));
                let mut hint = vec![
                    Span::styled("[j/k]", gray()),
                    Span::raw(" 移动  "),
                    Span::styled("[Enter]", cyan()),
                    Span::raw(" 热切换  "),
                    Span::styled("[q/Esc]", gray()),
                    Span::raw(" 关闭"),
                ];
                if app.model_catalog.selecting.is_some() {
                    hint.push(Span::styled(
                        "  切换中…",
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ));
                }
                lines.push(Line::from(hint));
            }
            // selectModel 失败提示（AC-006-09：error.code、当前模型不变）。
            if let Some(code) = &app.model_catalog.last_error_code {
                lines.push(Line::from(Span::styled(
                    format!(" ✗ 模型切换失败（error.code={code}）；当前使用模型不变"),
                    Style::default().fg(Color::Red),
                )));
            }
            if let Some(err) = &app.model_catalog.load_error {
                if app.model_catalog.last_error_code.is_none() {
                    lines.push(Line::from(Span::styled(
                        format!(" ⚠ {err}"),
                        Style::default().fg(Color::Yellow),
                    )));
                }
            }
        }
    }

    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        ),
        parts[1],
    );
}

/// overlay 标题：` Model Catalog ` + current → next（ADR-008 只读投影镜像）。
fn title(app: &AppState) -> String {
    let cur = app.model_catalog.current_model.as_deref().unwrap_or("-");
    let next = app.model_catalog.next_model.as_deref().unwrap_or("-");
    if cur == next {
        format!(" Model Catalog · {cur} ")
    } else {
        format!(" Model Catalog · {cur} → {next} ")
    }
}

/// 一行模型命中：光标 + provider/model + 描述 + efforts + current/next 标记。
fn catalog_row(
    item: &crate::model::ModelCatalogItem,
    cursor: bool,
    app: &AppState,
) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let route = item.route();
    if app.model_catalog.current_model.as_deref() == Some(route.as_str()) {
        spans.push(Span::styled(
            "● ",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ));
    } else if app.model_catalog.next_model.as_deref() == Some(route.as_str()) {
        spans.push(Span::styled(
            "→ ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
    } else if cursor {
        spans.push(Span::styled(
            "▌",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
    } else {
        spans.push(Span::raw("  "));
    }
    let name = if item.model_name.is_empty() {
        item.model_id.clone()
    } else {
        item.model_name.clone()
    };
    spans.push(Span::raw(format!("{name}  ")));
    spans.push(Span::styled(
        route.to_string(),
        Style::default().fg(Color::DarkGray),
    ));
    if !item.reasoning_efforts.is_empty() {
        spans.push(Span::styled(
            format!(" [effort: {}]", item.reasoning_efforts.join("|")),
            Style::default().fg(Color::DarkGray),
        ));
    }
    if let Some(desc) = &item.description {
        if !desc.is_empty() {
            spans.push(Span::styled(
                format!(" — {desc}"),
                Style::default().fg(Color::Gray),
            ));
        }
    }
    let mut line = Line::from(spans);
    if cursor {
        for s in line.spans.iter_mut() {
            if s.style.fg != Some(Color::Green) && s.style.fg != Some(Color::Cyan) {
                s.style = s.style.add_modifier(Modifier::REVERSED);
            }
        }
    }
    line
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

    fn ready_catalog(app: &mut AppState) {
        let catalog: crate::api::types::ModelCatalog = serde_json::from_value(serde_json::json!({
            "default": {"provider": "deepseek_official", "model": "deepseek-chat"},
            "routableProviders": ["deepseek_official"],
            "groups": [{
                "id": "deepseek_official",
                "name": "DeepSeek 官方",
                "models": [
                    {"id": "deepseek-chat", "name": "DeepSeek Chat",
                     "reasoning": {"efforts": [{"id": "low", "name": "Low"},
                                               {"id": "high", "name": "High"}],
                                   "defaultEffort": "low"}},
                    {"id": "deepseek-v4-pro", "name": "V4 Pro"}
                ]
            }],
            "failures": []
        }))
        .unwrap();
        app.model_catalog.index.rebuild(&catalog);
        app.model_catalog.phase = CatalogPhase::Ready;
    }

    #[test]
    fn renders_catalog_rows_and_current_marker() {
        let mut app = AppState::default();
        app.mode = Mode::ModelCatalog;
        app.model_catalog.visible = true;
        ready_catalog(&mut app);
        app.model_catalog.current_model = Some("deepseek_official/deepseek-chat".into());
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        let text = rendered_text(&terminal);
        assert!(text.contains("Model Catalog"), "text={text}");
        assert!(text.contains("DeepSeek Chat"), "text={text}");
        assert!(
            text.contains("deepseek_official/deepseek-chat"),
            "text={text}"
        );
        assert!(text.contains("V4 Pro"), "text={text}");
        assert!(text.contains("effort: low|high"), "text={text}");
    }

    #[test]
    fn loading_and_error_phases_are_visible_ac006_06() {
        // Loading。
        let mut app = AppState::default();
        app.mode = Mode::ModelCatalog;
        app.model_catalog.visible = true;
        app.model_catalog.phase = CatalogPhase::Loading;
        let backend = TestBackend::new(100, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        let text = rendered_text(&terminal);
        assert!(text.contains("加载模型目录"), "text={text}");
        // Error。
        let mut app = AppState::default();
        app.mode = Mode::ModelCatalog;
        app.model_catalog.visible = true;
        app.model_catalog.phase = CatalogPhase::Error;
        app.model_catalog.load_error =
            Some("模型目录加载失败: 权限不足（error.code=PERMISSION_DENIED）".into());
        let backend = TestBackend::new(100, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        let text = rendered_text(&terminal);
        assert!(text.contains("模型目录加载失败"), "text={text}");
        assert!(text.contains("PERMISSION_DENIED"), "text={text}");
        assert!(text.contains("重试"), "text={text}");
    }

    #[test]
    fn empty_query_state_shows_hint_not_error_ac006_07() {
        let mut app = AppState::default();
        app.mode = Mode::ModelCatalog;
        app.model_catalog.visible = true;
        ready_catalog(&mut app);
        app.model_catalog.query = "nope".into();
        let backend = TestBackend::new(100, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        let text = rendered_text(&terminal);
        assert!(text.contains("无匹配模型"), "text={text}");
        assert!(!text.contains("加载失败"), "空态不误报错误, text={text}");
    }
}
