//! REQ-009 面板渲染集成 golden（tests/monitor_ui.rs）：
//! TestBackend rendered_text 断言（tests/ui_golden.rs 同款口径）——
//! 状态条/roster/详情/问答/统计/帮助/降级/空态。

use dshtui::api::monitor::{WireAgent, WireHistogram, WireKbBucket, WireKbStats};
use dshtui::app::monitor::{MonitorAppState, MonitorEvent};
use dshtui::model::KB_DURATION_BOUNDARIES;
use ratatui::backend::TestBackend;
use ratatui::Terminal;

fn app(kitty: bool, n: usize) -> MonitorAppState {
    let mut app = MonitorAppState::new(kitty, 0.0, 5);
    let entries: Vec<WireAgent> = (0..n)
        .map(|i| WireAgent {
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

fn render_text(app: &MonitorAppState) -> String {
    let backend = TestBackend::new(150, 40);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|f| {
            dshtui::ui::monitor::render(f, app);
        })
        .unwrap();
    let buf = terminal.backend().buffer();
    let mut text = String::new();
    let mut skip = 0usize;
    for cell in buf.content() {
        if skip == 0 && !cell.skip {
            text.push_str(cell.symbol());
        }
        skip =
            std::cmp::max(skip, ratatui::text::Span::raw(cell.symbol()).width()).saturating_sub(1);
    }
    text
}

#[test]
fn status_bar_and_roster_read_agent_server_numbers() {
    let app = app(false, 2);
    let text = render_text(&app);
    assert!(text.contains("活跃 2"));
    assert!(text.contains("完工 7"));
    assert!(text.contains("🏭"));
    assert!(text.contains("实现建造"));
}

#[test]
fn detail_pane_fields_match_agents_entry() {
    let mut app = app(false, 1);
    app.handle_command(dshtui::input::Command::OpenFocused);
    let text = render_text(&app);
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
        assert!(text.contains(field), "详情缺字段 {field}");
    }
    assert!(text.contains("任务0"), "归属任务与 /agents task 一致");
}

#[test]
fn stats_pane_histogram_uses_agent_server_boundaries() {
    let mut app = app(false, 0);
    app.handle(MonitorEvent::PollKb {
        stats: WireKbStats {
            totals: WireKbBucket {
                hits: 8,
                misses: 2,
                empty: 0,
                errs: 0,
                skipped: 0,
                searches: 10,
                avg_ms: 345,
                hist: WireHistogram {
                    boundaries: KB_DURATION_BOUNDARIES.to_vec(),
                    counts: vec![3, 2, 1, 4, 1, 1, 0],
                },
            },
            window: WireKbBucket {
                hist: WireHistogram {
                    boundaries: KB_DURATION_BOUNDARIES.to_vec(),
                    counts: vec![0; 7],
                },
                ..Default::default()
            },
            last_log_at: 0,
            restored: false,
        },
    });
    app.handle_command(dshtui::input::Command::MonitorStats);
    let text = render_text(&app);
    assert!(text.contains("命中率 80%"));
    assert!(text.contains("16000"));
    assert!(text.contains("4000ms"));
}

#[test]
fn non_kitty_degraded_roster_keeps_keys_usable() {
    let app = app(false, 1);
    let text = render_text(&app);
    assert!(text.contains("文本 roster 降级"));
    assert!(text.contains("q 退出"));
    assert!(text.contains("Enter 详情"));
}

#[test]
fn kitty_mode_reserves_blank_town_area() {
    // 帧字节由事件循环直写 backend（TownCanvas 单测断言 a=T/a=f/o=z），
    // UI 层只负责画面区留空与周边 pane 渲染。
    let app = app(true, 0);
    let text = render_text(&app);
    assert!(!text.contains("文本 roster 降级"), "kitty 模式不降级");
    assert!(text.contains("在镇居民"));
}

#[test]
fn empty_and_disconnected_states_are_readable() {
    let mut app = app(true, 0);
    app.handle(MonitorEvent::PollAgentsError {
        error: "connection refused".into(),
    });
    let text = render_text(&app);
    assert!(text.contains("agent-server 未连接"));
    assert!(text.contains("8799"));
}
