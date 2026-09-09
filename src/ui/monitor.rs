//! `dshtui monitor` 面板渲染（REQ-009 §4 输出契约）：状态条 / 图形 roster /
//! 详情 pane / 问答 pane / KB 直方图 / 帮助 / 文本降级。
//!
//! - 状态条数字全部读 agent-server 数据（ADR-008 口径，不自算）；
//! - Kitty → 像素小镇（画面区留空，帧字节由事件循环直写 backend）；
//!   非 Kitty → 文本 roster 降级（D-30，功能键位不变，AC-009-08）；
//! - 直方图桶边界沿用 `KB_DURATION_BOUNDARIES`（AC-009-07）。

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::app::monitor::{MonitorAppState, MonitorMode};
use crate::model::agent_town::phase_name;
use crate::model::{fmt_elapsed, short_session, stage_meta, AgentRosterEntry};

/// 画面区（mouse 命中映射用同一 seam）。
pub fn town_area(app: &MonitorAppState) -> Rect {
    // 与 render 的 body 布局一致：状态条 1 行 + 右侧 roster 32 列。
    let full = Rect::new(0, 0, app.width.max(1), app.height.max(1));
    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(full);
    let body = vert[1];
    let horiz = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(40), Constraint::Length(32)])
        .split(body);
    horiz[0]
}

/// 渲染完整监控界面（Kitty 画面区留空——像素帧由事件循环在 draw 后以
/// MoveTo + kitty 字节直写 backend，不驻留 Buffer，RSS 口径 AC-009-09）。
pub fn render(frame: &mut Frame<'_>, app: &MonitorAppState) {
    let full = frame.area();
    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(full);
    let status = vert[0];
    let body = vert[1];

    // 状态条（季节/时段/活跃/完工，ADR-008 全部读 agent-server 数据）。
    render_status(frame, status, app);

    // Filter 输入行。
    if app.mode == MonitorMode::Filter {
        let fvert = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(body);
        render_filter_line(frame, fvert[1], app);
        render_body(frame, fvert[0], app);
    } else {
        render_body(frame, body, app);
    }

    // 覆盖层：详情 → 问答 → 统计 → 帮助（后渲染者在上）。
    if app.mode == MonitorMode::Detail {
        if let Some(sid) = &app.detail {
            render_detail(frame, frame.area(), app, sid);
        }
    }
    if app.mode == MonitorMode::Chat {
        render_chat(frame, frame.area(), app);
    }
    if app.mode == MonitorMode::Stats {
        render_stats(frame, frame.area(), app);
    }
    if app.help_open {
        render_help(frame, frame.area());
    }
}

fn render_body(frame: &mut Frame<'_>, body: Rect, app: &MonitorAppState) {
    let horiz = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(40), Constraint::Length(32)])
        .split(body);
    let town = horiz[0];
    let roster = horiz[1];

    if app.kitty_capable {
        // Kitty：画面区留空（像素帧由事件循环直写 backend，覆盖该区域）。
        // 空态提示（design-spec §8：无 agent 场景完整可见 + 「小镇空闲」）。
        if !app.scene.has_agents() {
            let msg = if app.connected {
                "🌆 小镇空闲 — 等待 agent 上工…"
            } else {
                "📡 agent-server 未连接，请确认 dsh-agent-server 正在运行（127.0.0.1:8799）"
            };
            let empty = Paragraph::new(Line::from(msg))
                .style(Style::default().fg(Color::DarkGray))
                .alignment(ratatui::layout::Alignment::Center);
            let area = Rect::new(
                town.x,
                town.y + town.height.saturating_sub(1),
                town.width,
                1,
            );
            frame.render_widget(empty, area);
        }
    } else {
        // 非 Kitty：画面区降级纯文本 roster（D-30，AC-009-08）。
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Agent Town（文本 roster 降级 · 非 Kitty 终端） ");
        let mut lines: Vec<Line<'static>> = Vec::new();
        for e in app.filtered_roster() {
            lines.push(roster_line(e, e.session_id.get() == focused_sid(app)));
        }
        if lines.is_empty() {
            lines.push(Line::from("🌆 小镇空闲 — 等待 agent 上工…"));
        }
        if let Some(err) = &app.last_error {
            lines.push(Line::from(format!("⚠️ 连接失败: {err}")));
        }
        lines.push(Line::from(
            "j/k 选择 · Enter 详情 · c 问答 · s 统计 · / 过滤 · q 退出",
        ));
        frame.render_widget(Paragraph::new(lines).block(block), town);
    }

    render_roster(frame, roster, app);
}

fn focused_sid(app: &MonitorAppState) -> String {
    app.focused_entry()
        .map(|e| e.session_id.get())
        .unwrap_or_default()
}

fn render_status(frame: &mut Frame<'_>, area: Rect, app: &MonitorAppState) {
    let pal = crate::model::agent_town::pal_now(app.scene.now_ms);
    let season = pal.season;
    let min = app.scene.min_of_day();
    let active = app.roster.len();
    let finished = app.roster.finished;
    let conn = if app.connected { "●" } else { "○" };
    let conn_style = if app.connected {
        Style::default().fg(Color::Green)
    } else {
        Style::default().fg(Color::Red)
    };
    let text = Line::from(vec![
        Span::styled(
            "🏘️ Agent Town ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!("{}季 {} ", season.emoji(), season.name())),
        Span::raw(format!("{} ", phase_name(min))),
        Span::styled(conn, conn_style),
        Span::raw(format!(" 活跃 {active} ")),
        Span::styled(
            format!("完工 {finished}"),
            Style::default().fg(Color::Green),
        ),
        // 错误/恢复提示（不刷屏：仅连接状态 + 短句）。
        if let Some(err) = &app.last_error {
            Span::styled(
                format!(" · ⚠️ {}", truncate(err, 40)),
                Style::default().fg(Color::Red),
            )
        } else {
            Span::raw("")
        },
    ]);
    frame.render_widget(
        Paragraph::new(text).style(Style::default().bg(Color::Reset)),
        area,
    );
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect::<String>() + "…"
    }
}

/// 图形 roster（职业 emoji/名称/阶段/任务·项目/工作中|休息中/时长，FR-009-05）。
/// 滚动偏移 `roster_scroll`（滚轮）+ 焦点恒可见（clamp，AC-009-05 焦点高亮）。
fn render_roster(frame: &mut Frame<'_>, area: Rect, app: &MonitorAppState) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" 🧑‍🤝‍🧑 在镇居民 ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let items = app.filtered_roster();
    if items.is_empty() {
        let empty = Paragraph::new(Line::from("暂无 agent"))
            .style(Style::default().fg(Color::DarkGray))
            .alignment(ratatui::layout::Alignment::Center);
        frame.render_widget(empty, inner);
        return;
    }
    let h = inner.height as usize;
    // 焦点恒可见：滚动窗口 clamp 到包含 focus 行。
    let scroll = app
        .roster_scroll
        .min(items.len().saturating_sub(1))
        .min(app.focus.min(items.len().saturating_sub(1)))
        .max(app.focus.saturating_sub(h.saturating_sub(1)));
    let focused = focused_sid(app);
    frame.render_widget(
        Paragraph::new(
            items
                .iter()
                .skip(scroll)
                .take(h)
                .map(|e| {
                    let is_focus = e.session_id.get() == focused;
                    // 加油颜色脉冲：cheering 中的 agent 行粉底（FR-009-04）。
                    let cheering = app.scene.npc(&e.session_id).is_some_and(|n| n.cheering);
                    let style = if cheering {
                        Style::default().fg(Color::Black).bg(Color::LightMagenta)
                    } else if is_focus {
                        Style::default().fg(Color::Black).bg(Color::Cyan)
                    } else {
                        Style::default()
                    };
                    Line::styled(
                        format!("{} {}", if is_focus { "▶" } else { " " }, roster_text(e)),
                        style,
                    )
                })
                .collect::<Vec<_>>(),
        ),
        inner,
    );
}

/// roster 单行文本（图形/文本双态共用口径）。
fn roster_line(e: &AgentRosterEntry, focused: bool) -> Line<'static> {
    Line::styled(
        format!("{} {}", if focused { "▶" } else { " " }, roster_text(e)),
        Style::default(),
    )
}

fn roster_text(e: &AgentRosterEntry) -> String {
    let meta = stage_meta(e.stage_key());
    let task = e.task_first_line();
    let depth = if e.delegation_depth > 0 {
        format!("  #{}", e.delegation_depth)
    } else {
        String::new()
    };
    let proj = if e.project.is_empty() {
        String::new()
    } else {
        format!(" · {}", e.project)
    };
    format!(
        "{} {}{} · {} · {} · {}{}",
        meta.emoji,
        e.display_name(),
        depth,
        meta.label,
        if e.status == crate::model::AgentStatus::Idle {
            "休息中"
        } else {
            "工作中"
        },
        fmt_elapsed(e.elapsed_sec),
        if task.is_empty() {
            String::new()
        } else {
            format!(" · {task}{proj}")
        },
    )
}

/// 详情 pane（§4 输出契约字段全列：阶段/归属任务/层级/上级/职业/工位/
/// 分区/项目/模型/状态/动作/时长，字段与 /agents 条目一致，AC-009-04）。
fn render_detail(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &MonitorAppState,
    sid: &crate::api::types::SessionId,
) {
    let Some(entry) = app.roster.get(sid) else {
        return;
    };
    let meta = stage_meta(entry.stage_key());
    let npc = app.scene.npc(sid);
    let root_task = entry.task_first_line();
    let parent = entry
        .parent_session_id
        .as_deref()
        .map(|pid| {
            app.roster
                .ordered()
                .iter()
                .find(|e| e.session_id.get() == pid)
                .map(|p| p.display_name())
                .unwrap_or_else(|| short_session(pid))
        })
        .unwrap_or_default();
    let mut rows: Vec<String> = vec![
        format!("名称     {}", entry.display_name()),
        format!("阶段     {} {} {}", meta.label, meta.emoji, entry.phase),
        format!(
            "归属任务 {}",
            if root_task.is_empty() {
                "—".into()
            } else {
                root_task.clone()
            }
        ),
        format!(
            "层级     {}",
            if entry.delegation_depth > 0 {
                format!("子会话 #{depth}", depth = entry.delegation_depth)
            } else {
                "主会话".into()
            }
        ),
    ];
    if let Some(_pid) = &entry.parent_session_id {
        rows.push(format!("上级     {parent}"));
    }
    rows.push(format!("职业     {}", meta.occupation));
    rows.push(format!("工位     {}", meta.building));
    rows.push(format!("分区     {}", meta.zone));
    rows.push(format!(
        "项目     {}",
        if entry.project.is_empty() {
            "—"
        } else {
            entry.project.as_str()
        }
    ));
    if !entry.model.is_empty() {
        rows.push(format!("模型     {}", entry.model));
    }
    rows.push(format!(
        "状态     {}",
        if entry.status == crate::model::AgentStatus::Idle {
            "休息"
        } else {
            "工作中"
        }
    ));
    rows.push(format!(
        "动作     {}",
        npc.map(|n| n.state.action_text()).unwrap_or("—")
    ));
    rows.push(format!("时长     {}", fmt_elapsed(entry.elapsed_sec)));

    let width = 46u16;
    // +2 边框 +1 底部操作行（💬 问答，可点击，AC-009-06 双入口）。
    let height = rows.len() as u16 + 3;
    let pop = centered(area, width, height);
    frame.render_widget(Clear, pop);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(format!(" {} 详情 ", meta.emoji));
    let inner = block.inner(pop);
    frame.render_widget(block, pop);
    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);
    frame.render_widget(
        Paragraph::new(
            rows.iter()
                .map(|r| Line::raw(r.clone()))
                .collect::<Vec<_>>(),
        )
        .wrap(ratatui::widgets::Wrap { trim: false }),
        vert[0],
    );
    // 底部操作行：点击 💬 行 → open_chat（row = detail_chat_row 口径）。
    let hint = Line::from(vec![
        Span::styled("💬 问答", Style::default().fg(Color::Green)),
        Span::raw("（点击或按 c）  "),
        Span::styled("💗 加油 f", Style::default().fg(Color::LightMagenta)),
    ]);
    frame.render_widget(
        Paragraph::new(hint).style(Style::default().bg(Color::DarkGray)),
        vert[1],
    );
}

/// 详情 pane 底部 💬 行（终端绝对行号；鼠标点击分流用）。
pub fn detail_chat_row(app: &MonitorAppState) -> u16 {
    // 与 render_detail 布局同口径：基础字段 11 行 + (上级/模型条件行)。
    let entry = app.detail.as_ref().and_then(|sid| app.roster.get(sid));
    let Some(entry) = entry else {
        return 0;
    };
    let mut n = 11usize;
    if entry.parent_session_id.is_some() {
        n += 1;
    }
    if !entry.model.is_empty() {
        n += 1;
    }
    let height = (n as u16 + 3).min(app.height.max(1));
    let pop = centered(
        Rect::new(0, 0, app.width.max(1), app.height.max(1)),
        46,
        height,
    );
    pop.y + pop.height.saturating_sub(2) // 边框内底部行
}

/// 问答 pane（一问一答 + 错误区分网络失败与业务 errorCode，AC-009-06）。
fn render_chat(frame: &mut Frame<'_>, area: Rect, app: &MonitorAppState) {
    let pop = centered(area, 60, 14);
    frame.render_widget(Clear, pop);
    let target = app
        .chat
        .target
        .as_ref()
        .map(|s| s.get())
        .unwrap_or_default();
    let entry = app
        .roster
        .ordered()
        .iter()
        .find(|e| e.session_id.get() == target)
        .cloned();
    let title = match (&entry, &app.chat.session_id) {
        (Some(e), Some(_)) => format!(
            " 💬 问答 · {} · {}{}（多轮） ",
            e.display_name(),
            if e.project.is_empty() {
                ""
            } else {
                e.project.as_str()
            },
            if e.task_first_line().is_empty() {
                String::new()
            } else {
                format!(" · {}", e.task_first_line())
            },
        ),
        (Some(e), None) => format!(" 💬 问答 · {}（新会话） ", e.display_name()),
        _ => " 💬 问答 ".into(),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(title);
    let inner = block.inner(pop);
    frame.render_widget(block, pop);

    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);
    let body_lines: Vec<Line<'static>> = app
        .chat
        .messages
        .iter()
        .rev()
        .take(vert[0].height as usize)
        .rev()
        .map(|m| {
            let (who, style) = match m.who {
                crate::app::monitor::ChatWho::User => ("你", Style::default().fg(Color::Green)),
                crate::app::monitor::ChatWho::Agent => ("agent", Style::default()),
                crate::app::monitor::ChatWho::Error => ("错误", Style::default().fg(Color::Red)),
            };
            Line::from(vec![
                Span::styled(format!("{who}> "), style.add_modifier(Modifier::BOLD)),
                Span::raw(m.text.clone()),
            ])
        })
        .collect();
    frame.render_widget(
        Paragraph::new(body_lines).wrap(ratatui::widgets::Wrap { trim: false }),
        vert[0],
    );

    let prompt = format!(
        "{} 输入问题（Enter 发送 · Esc 关闭）",
        if app.chat.busy {
            "⏳ 思考中… "
        } else {
            "❯ "
        }
    );
    let input_line = Line::from(vec![
        Span::styled(prompt, Style::default().fg(Color::DarkGray)),
        Span::raw(app.chat.input.clone()),
    ]);
    frame.render_widget(Paragraph::new(input_line), vert[1]);
}

/// KB 统计 pane（totals/window 两段 Unicode 柱状图，桶边界
/// `KB_DURATION_BOUNDARIES`，AC-009-07）。
fn render_stats(frame: &mut Frame<'_>, area: Rect, app: &MonitorAppState) {
    let pop = centered(area, 58, 16);
    frame.render_widget(Clear, pop);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(" 📊 KB 预检索统计 ");
    let inner = block.inner(pop);
    frame.render_widget(block, pop);

    let mut lines: Vec<Line<'static>> = Vec::new();
    match &app.kb {
        Some(s) => {
            lines.push(bucket_summary("累计", &s.totals));
            lines.extend(hist_lines(&s.totals));
            lines.push(Line::from(Span::raw("")));
            lines.push(bucket_summary("当前小时", &s.window));
            lines.extend(hist_lines(&s.window));
            if let Some(err) = &app.kb_error {
                lines.push(Line::styled(
                    format!("⚠️ {err}"),
                    Style::default().fg(Color::Red),
                ));
            }
        }
        None => {
            lines.push(Line::from("等待 /kb-stats 数据…"));
            if let Some(err) = &app.kb_error {
                lines.push(Line::styled(
                    format!("⚠️ {err}"),
                    Style::default().fg(Color::Red),
                ));
            }
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn bucket_summary(label: &str, b: &crate::model::KbBucket) -> Line<'static> {
    let total = b.hits + b.misses;
    let ratio = if total > 0 { b.hits * 100 / total } else { 0 };
    Line::from(vec![
        Span::styled(
            label.to_string(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            " · 命中 {}/{} · 命中率 {}% · 均耗 {}ms · 检索 {} 次",
            b.hits, total, ratio, b.avg_ms, b.searches
        )),
    ])
}

/// Unicode 柱状图（桶边界/计数，renderKBStats 口径）。
fn hist_lines(b: &crate::model::KbBucket) -> Vec<Line<'static>> {
    let max = b.hist.counts.iter().copied().max().unwrap_or(0).max(1);
    let width = 36usize;
    let mut lines = Vec::new();
    for (i, (boundary, count)) in b
        .hist
        .boundaries
        .iter()
        .zip(b.hist.counts.iter())
        .enumerate()
    {
        let bar_len = if max == 0 {
            0
        } else {
            (*count as usize * width / max as usize).max(if *count > 0 { 1 } else { 0 })
        };
        let label = if i == 0 {
            format!("<{}", b.hist.boundaries.get(1).copied().unwrap_or(100))
        } else if i == b.hist.boundaries.len() - 1 {
            format!(">={boundary}")
        } else {
            boundary.to_string()
        };
        let bar = "█".repeat(bar_len);
        lines.push(Line::from(vec![
            Span::styled(
                format!("{label:>6}ms "),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                bar,
                if *count > 0 {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            Span::raw(format!(" {count}")),
        ]));
    }
    lines
}

/// `?` 帮助面板（monitor 键位表）。
fn render_help(frame: &mut Frame<'_>, area: Rect) {
    let pop = centered(area, 52, 14);
    frame.render_widget(Clear, pop);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(" Agent Town 键位 ");
    let text = vec![
        "j/k       焦点上下（roster/画面）",
        "gg/G      首/尾",
        "Enter     打开详情（同 NPC 幂等）",
        "c         打开问答（/agent/chat 多轮）",
        "f         加油 · l 定位",
        "s         KB 预检索统计",
        "/         过滤（phase/status）",
        "鼠标点击   NPC 命中 → 详情 · 滚轮滚动",
        "q         退出（进程消失，资源全释放）",
        "?         本帮助（Esc 关闭）",
    ];
    frame.render_widget(
        Paragraph::new(text.iter().map(|l| Line::raw(*l)).collect::<Vec<_>>()).block(block),
        pop,
    );
}

/// 居中覆盖层矩形。
fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect::new(
        area.x + area.width.saturating_sub(w) / 2,
        area.y + area.height.saturating_sub(h) / 2,
        w,
        h,
    )
}

/// Filter 输入行。
fn render_filter_line(frame: &mut Frame<'_>, area: Rect, app: &MonitorAppState) {
    let line = Line::from(vec![
        Span::styled("❯ 过滤(phase/status): ", Style::default().fg(Color::Cyan)),
        Span::raw(app.filter.clone()),
        Span::styled(
            format!("  [{} 匹配]", app.filtered_roster().len()),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

// ============================ golden 测试 ============================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::monitor::{MonitorEvent, MonitorMode};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn app(kitty: bool, n_agents: usize) -> MonitorAppState {
        let mut app = MonitorAppState::new(kitty, 0.0, 1);
        let entries: Vec<crate::api::monitor::WireAgent> = (0..n_agents)
            .map(|i| crate::api::monitor::WireAgent {
                session_id: format!("session-{i}"),
                phase: "".into(),
                task: format!("任务{i}"),
                project: "proj".into(),
                task_id: "TASK-1".into(),
                status: if i % 2 == 0 { "working" } else { "idle" }.into(),
                task_status: if i % 2 == 0 { "implementing" } else { "" }.into(),
                elapsed: 3661,
                last_event_at: 0,
                seq: 1,
                label: "".into(),
                kind: "session".into(),
                parent_session_id: "".into(),
                delegation_depth: 0,
                provider: "p".into(),
                model: "m".into(),
            })
            .collect();
        app.handle(MonitorEvent::PollAgents {
            entries,
            finished: 7,
            poll_ms: 2.0,
        });
        app
    }

    fn rendered_text(app: &MonitorAppState) -> String {
        let backend = TestBackend::new(130, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                render(f, app);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let mut text = String::new();
        let mut skip = 0usize;
        for cell in buf.content() {
            if skip == 0 && !cell.skip {
                text.push_str(cell.symbol());
            }
            skip = std::cmp::max(skip, ratatui::text::Span::raw(cell.symbol()).width())
                .saturating_sub(1);
        }
        text
    }

    #[test]
    fn status_bar_reads_agent_server_numbers() {
        let app = app(false, 2);
        let text = rendered_text(&app);
        assert!(text.contains("活跃 2"), "状态条活跃数读 roster: {text}");
        assert!(
            text.contains("完工 7"),
            "状态条完工数读 x-agents-finished: {text}"
        );
        assert!(text.contains("春"), "状态条含季节");
    }

    #[test]
    fn roster_pane_lists_agents_with_occupation_and_elapsed() {
        let app = app(false, 2);
        let text = rendered_text(&app);
        assert!(text.contains("🏭"), "implementing 职业 emoji");
        assert!(text.contains("实现建造"), "STAGE label");
        assert!(text.contains("1h1m"), "fmtElapsed 时长");
        assert!(text.contains("工作中") && text.contains("休息中"));
        assert!(text.contains("任务0"));
    }

    #[test]
    fn non_kitty_falls_back_to_text_roster_with_keys_hint() {
        let app = app(false, 1);
        let text = rendered_text(&app);
        assert!(
            text.contains("文本 roster 降级"),
            "非 Kitty 降级标题: {text}"
        );
        assert!(text.contains("q 退出"), "功能键位提示可用（AC-009-08）");
    }

    #[test]
    fn kitty_mode_reserves_blank_town_area_without_crash() {
        // 帧字节由事件循环直写 backend（canvas 层单测断言 a=T/a=f），
        // UI 层只负责画面区留空与周边 pane 渲染。
        let app = app(true, 0);
        let text = rendered_text(&app);
        assert!(!text.contains("文本 roster 降级"), "kitty 模式不降级");
        assert!(text.contains("在镇居民"), "roster pane 正常");
    }

    #[test]
    fn empty_state_and_disconnected_hint() {
        let mut app = app(true, 0);
        app.handle(MonitorEvent::PollAgentsError {
            error: "connection refused".into(),
        });
        let text = rendered_text(&app);
        assert!(text.contains("agent-server 未连接"), "不可达提示: {text}");
        assert!(text.contains("8799"), "启动指引含默认端口");
    }

    #[test]
    fn detail_pane_lists_contract_fields() {
        let mut app = app(false, 1);
        app.handle_command(crate::input::Command::OpenFocused);
        assert_eq!(app.mode, MonitorMode::Detail);
        let text = rendered_text(&app);
        for field in [
            "名称",
            "阶段",
            "归属任务",
            "层级",
            "职业",
            "工位",
            "分区",
            "项目",
            "模型",
            "状态",
            "动作",
            "时长",
        ] {
            assert!(text.contains(field), "详情缺字段 {field}: {text}");
        }
        assert!(text.contains("开发工程师"), "职业来自 STAGE");
        assert!(text.contains("开发工厂"), "工位=职业建筑");
    }

    #[test]
    fn chat_pane_renders_messages_and_busy_state() {
        let mut app = app(false, 1);
        app.handle_command(crate::input::Command::MonitorOpenChat);
        app.handle_command(crate::input::Command::PickerInput("你好".into()));
        let _ = app.handle_command(crate::input::Command::SubmitInput);
        let text = rendered_text(&app);
        assert!(text.contains("问答"), "chat pane 标题");
        assert!(text.contains("思考中"), "busy 状态可见");
        assert!(text.contains("你好"), "用户消息回显");
    }

    #[test]
    fn stats_pane_renders_histogram_with_boundaries() {
        let mut app = app(false, 0);
        let bucket = |hits: i64| crate::api::monitor::WireKbBucket {
            hits,
            misses: 2,
            empty: 0,
            errs: 0,
            skipped: 0,
            searches: hits + 2,
            avg_ms: 345,
            hist: crate::api::monitor::WireHistogram {
                boundaries: crate::model::KB_DURATION_BOUNDARIES.to_vec(),
                counts: vec![3, 2, 1, 4, 1, 1, 0],
            },
        };
        app.handle(MonitorEvent::PollKb {
            stats: crate::api::monitor::WireKbStats {
                totals: bucket(8),
                window: bucket(0),
                last_log_at: 0,
                restored: false,
            },
        });
        app.handle_command(crate::input::Command::MonitorStats);
        let text = rendered_text(&app);
        assert!(text.contains("命中率 80%"), "命中率: {text}");
        assert!(
            text.contains("4000ms"),
            "桶边界沿 KB_DURATION_BOUNDARIES（AC-009-07）"
        );
        assert!(text.contains(">=16000"), "末桶 >=16000 标签");
        assert!(text.contains("<100"), "首桶 <100 标签");
    }

    #[test]
    fn detail_chat_row_lands_inside_popup() {
        // 点击 💬 行（AC-009-06 双入口）：detail 模式下行号须落在详情弹窗内。
        let mut app = app(false, 1);
        app.handle_command(crate::input::Command::OpenFocused);
        let row = detail_chat_row(&app);
        assert!(row > 0 && row < app.height.max(1), "💬 行在弹窗内: {row}");
        let text = rendered_text(&app);
        assert!(text.contains("💬 问答"), "详情底部含 💬 入口: {text}");
    }

    #[test]
    fn help_pane_lists_monitor_keymap() {
        let mut app = app(false, 0);
        app.help_open = true;
        let text = rendered_text(&app);
        for key in ["j/k", "Enter", "c ", "f ", "s ", "/", "q "] {
            assert!(text.contains(key), "帮助缺键位 {key}: {text}");
        }
    }

    #[test]
    fn filter_overlay_shows_query_and_match_count() {
        let mut app = app(false, 2);
        app.handle_command(crate::input::Command::StartSearch);
        app.handle_command(crate::input::Command::PickerInput("implementing".into()));
        let text = rendered_text(&app);
        assert!(text.contains("过滤"), "filter overlay: {text}");
        assert!(text.contains("1 匹配"));
    }
}
