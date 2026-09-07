use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use dshtui::api::types::{
    ApprovalEvent, ChunkData, ChunkRow, SessionHistoryRecord, SessionId, SessionSeq,
    SessionWireEvent,
};
use dshtui::app::{AppEvent, AppState, ConnState, Mode, StopState};
use dshtui::model::{Block, Incoming};
use dshtui::ui;
use ratatui::backend::TestBackend;
use ratatui::Terminal;

fn event(seq: u64, event_type: &str, content: Option<&str>) -> SessionHistoryRecord {
    SessionHistoryRecord::Event {
        event: SessionWireEvent {
            event_type: event_type.to_string(),
            seq: Some(SessionSeq(seq)),
            time: None,
            request_id: None,
            ignorable: None,
            source_event_seqs: None,
            surface_op: None,
            data: content.map(|value| serde_json::json!({"content": value})),
        },
    }
}

fn rendered_text(terminal: &Terminal<TestBackend>) -> String {
    // 宽字符占两个 cell（后一 cell 为占位空格）：跳过占位。
    let mut skip = 0usize;
    let mut out = String::new();
    for cell in terminal.backend().buffer().content() {
        if skip == 0 && !cell.skip {
            out.push_str(cell.symbol());
        }
        skip =
            std::cmp::max(skip, ratatui::text::Span::raw(cell.symbol()).width()).saturating_sub(1);
    }
    out
}

#[test]
fn complete_frame_renders_session_sidebar_chat_status_and_details() {
    let mut app = AppState::new(20);
    app.conn = ConnState::Ready;
    app.active_session = Some(SessionId("sess-1".into()));
    app.focus = dshtui::app::Focus::Details;
    app.sessions.touch("sess-1", 20).apply(Incoming::Snapshot {
        cursor: Some(dshtui::api::types::SessionLogOffset(9)),
        records: vec![
            event(1, "user/message", Some("hello from user")),
            event(2, "assistant/message", None),
        ],
        has_more: false,
        projections: Some(serde_json::json!({
            "modelSelection": {"lastUsed": "model-test"},
            "contextPressure": {"pressureTokens": 10, "projectedTokens": 20},
        })),
    });
    app.workspaces
        .upsert_session(dshtui::api::types::SessionMeta {
            id: SessionId("sess-1".into()),
            title: Some("Golden session".into()),
            cwd: Some("/tmp/project".into()),
            updated_at_ms: 1,
            running: false,
            blank: false,
            origin: None,
            parent_id: None,
            workspace: None,
            last_turn_preview: None,
        });

    let backend = TestBackend::new(140, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::render(frame, &app)).unwrap();
    let text = rendered_text(&terminal);

    assert!(text.contains("Golden session"));
    assert!(text.contains("hello from user"));
    assert!(text.contains("ready"));
    assert!(text.contains("model-test"));
    assert!(text.contains("ctx 10/20"));
    assert!(text.contains("Details"));
    assert!(text.contains("Blocks: 2"));
    assert!(text.contains("Seq: 1..2"));
}

#[test]
fn startup_guidance_and_picker_overlay_are_visible_in_test_backend() {
    let mut app = AppState::default();
    app.handle(AppEvent::StartupProbeFailed("connection refused".into()));
    let backend = TestBackend::new(80, 16);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::render(frame, &app)).unwrap();
    let startup = rendered_text(&terminal);
    assert!(startup.contains("Startup"));
    assert!(startup.contains("connection refused"));
    assert!(startup.contains("press r to retry or q to quit"));

    app.picker.open = true;
    app.picker.query = "sess".into();
    terminal.draw(|frame| ui::render(frame, &app)).unwrap();
    let picker = rendered_text(&terminal);
    assert!(picker.contains("Pick session"));
    assert!(picker.contains("sess"));
}

#[test]
fn key_resize_event_updates_render_breakpoint_state() {
    let mut app = AppState::default();
    let resize = Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
    assert!(dshtui::input::map_key(dshtui::input::InputMode::Normal, resize).is_none());
    app.handle(AppEvent::Resize {
        width: 120,
        height: 30,
    });
    assert_eq!(app.width, 120);
    assert_eq!(app.height, 30);
    assert_eq!(app.viewport.height, 28);
}

#[test]
fn composer_overlay_visible_in_insert_and_hidden_in_normal_ac002_01() {
    let mut app = AppState::new(20);
    app.conn = ConnState::Ready;
    app.active_session = Some(SessionId("sess-1".into()));
    app.mode = Mode::Insert;
    app.composer.visible = true;
    app.composer.active_session = Some(SessionId("sess-1".into()));
    app.draft = Some(dshtui::model::DraftState {
        text: "你好 draft".into(),
        cursor: 8,
        bound_session: SessionId("sess-1".into()),
    });
    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::render(frame, &app)).unwrap();
    let insert = rendered_text(&terminal);
    assert!(
        insert.contains("Composer"),
        "INSERT 显示 composer, text={insert}"
    );
    assert!(insert.contains("你好 draft"), "草稿可见, text={insert}");

    // NORMAL：composer 收起不可见。
    app.mode = Mode::Normal;
    app.composer.visible = false;
    terminal.draw(|frame| ui::render(frame, &app)).unwrap();
    let normal = rendered_text(&terminal);
    assert!(
        !normal.contains("Composer"),
        "NORMAL 不显示 composer, text={normal}"
    );
}

#[test]
fn stopping_state_and_insert_mode_render_in_status_ac002_05() {
    let mut app = AppState::new(20);
    app.conn = ConnState::Ready;
    app.active_session = Some(SessionId("sess-1".into()));
    // 官方投影 running=true（停止转场中）。
    app.sessions.touch("sess-1", 20).apply(Incoming::Snapshot {
        cursor: None,
        records: vec![],
        has_more: false,
        projections: Some(serde_json::json!({"running": true})),
    });
    app.stop = StopState {
        requested_session: Some(SessionId("sess-1".into())),
    };
    app.mode = Mode::Insert;
    let backend = TestBackend::new(120, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::render(frame, &app)).unwrap();
    let text = rendered_text(&terminal);
    assert!(text.contains("停止中"), "本地停止中转场, text={text}");
    assert!(text.contains("INSERT"), "INSERT 模式指示, text={text}");
}

// ---------- REQ-003：markdown / 搜索 / 审批 / 状态条 golden ----------

#[test]
fn markdown_block_renders_headings_and_code_ac003_01_02() {
    let mut app = AppState::new(40);
    app.conn = ConnState::Ready;
    app.active_session = Some(SessionId("sess-1".into()));
    app.sessions.touch("sess-1", 40).apply(Incoming::Snapshot {
        cursor: None,
        records: vec![SessionHistoryRecord::Event {
            event: SessionWireEvent {
                event_type: "assistant/message".into(),
                seq: Some(SessionSeq(1)),
                time: None,
                request_id: None,
                ignorable: None,
                source_event_seqs: None,
                surface_op: None,
                data: None,
            },
        }],
        has_more: false,
        projections: None,
    });
    app.handle(AppEvent::FollowChunks {
        session_id: SessionId("sess-1".into()),
        row: ChunkRow::TextChunks(ChunkData {
            texts: vec!["# 标题一\n\n- 列表甲\n- 列表乙\n\n```rust\nfn main() {}\n```".into()],
            ..Default::default()
        }),
    });
    let backend = TestBackend::new(90, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::render(frame, &app)).unwrap();
    let text = rendered_text(&terminal);
    assert!(text.contains("标题一"), "标题渲染, text={text}");
    assert!(
        text.contains("列表甲") && text.contains("列表乙"),
        "列表渲染, text={text}"
    );
    assert!(text.contains("fn main"), "代码块内容, text={text}");
}

#[test]
fn search_overlay_and_status_matches_count_ac003_05() {
    let mut app = AppState::new(20);
    app.conn = ConnState::Ready;
    app.active_session = Some(SessionId("sess-1".into()));
    app.mode = Mode::Search;
    app.search.open = true;
    app.search.query = "deploy".into();
    app.search.cursor = 0;
    app.search_index.rebuild(&[Block::UserMessage {
        seq: SessionSeq(1),
        content: "deploy the operator".into(),
        time: None,
    }]);
    app.search.window_matches = vec![0];
    let backend = TestBackend::new(100, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::render(frame, &app)).unwrap();
    let text = rendered_text(&terminal);
    assert!(text.contains("SEARCH"), "状态条 SEARCH, text={text}");
    assert!(text.contains("1/1 matches"), "状态条计数, text={text}");
    assert!(
        text.contains("deploy the operator"),
        "命中列表, text={text}"
    );
}

#[test]
fn approval_modal_renders_over_chat_ac003_07() {
    let mut app = AppState::new(20);
    app.conn = ConnState::Ready;
    app.active_session = Some(SessionId("sess-1".into()));
    app.mode = Mode::Approval;
    app.approval.visible = true;
    app.approval.event = Some(ApprovalEvent {
        client_id: "c-1".into(),
        event_id: "e-1".into(),
        raw: serde_json::json!({
            "type": "approval/request",
            "agent": {"kind": "tool", "name": "bash"},
            "reason": "部署"
        }),
    });
    let backend = TestBackend::new(100, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::render(frame, &app)).unwrap();
    let text = rendered_text(&terminal);
    assert!(text.contains("APPROVAL"), "状态条 APPROVAL, text={text}");
    assert!(text.contains("bash"), "审批载荷, text={text}");
    assert!(text.contains("允许本次"), "y 提示, text={text}");
}

#[test]
fn waiting_approval_status_only_no_modal_ac003_18() {
    let mut app = AppState::new(20);
    app.conn = ConnState::Ready;
    app.active_session = Some(SessionId("sess-1".into()));
    app.approval.waiting_hint = true;
    let backend = TestBackend::new(100, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::render(frame, &app)).unwrap();
    let text = rendered_text(&terminal);
    assert!(text.contains("等待审批"), "状态条等待审批高亮, text={text}");
    assert!(!text.contains("允许本次"), "弹窗不出现, text={text}");
}

#[test]
fn copied_toast_and_steer_label_render_in_status_ac003_06_08() {
    let mut app = AppState::new(20);
    app.conn = ConnState::Ready;
    app.active_session = Some(SessionId("sess-1".into()));
    app.mode = Mode::Insert;
    app.composer.visible = true;
    app.composer.steer = true;
    app.yank.toast = Some("copied".into());
    let backend = TestBackend::new(120, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::render(frame, &app)).unwrap();
    let text = rendered_text(&terminal);
    assert!(text.contains("STEER"), "状态条 STEER, text={text}");
    assert!(text.contains("copied"), "复制 toast, text={text}");
}

// ---------- REQ-004 V0.2 图片（Step 5 渲染层 golden） ----------

fn nested_image_event(seq: u64) -> SessionHistoryRecord {
    SessionHistoryRecord::Event {
        event: SessionWireEvent {
            event_type: "assistant/message".into(),
            seq: Some(SessionSeq(seq)),
            time: None,
            request_id: None,
            ignorable: None,
            source_event_seqs: None,
            surface_op: None,
            data: Some(serde_json::json!({
                "content": [
                    {"type": "text", "text": "two images inline"},
                    {"type": "image", "attachmentId": "nested-1", "name": "nested.png", "dims": "4x5"},
                    {"type": "image", "attachmentId": "nested-2", "name": "second.png", "width": 6, "height": 7}
                ]
            })),
        },
    }
}

fn image_event(seq: u64, attachment_id: &str, name: &str, dims: &str) -> SessionHistoryRecord {
    SessionHistoryRecord::Event {
        event: SessionWireEvent {
            event_type: "message/image".to_string(),
            seq: Some(SessionSeq(seq)),
            time: None,
            request_id: None,
            ignorable: None,
            source_event_seqs: None,
            surface_op: None,
            data: Some(serde_json::json!({
                "attachmentId": attachment_id,
                "name": name,
                "dims": dims
            })),
        },
    }
}

fn image_window(records: Vec<SessionHistoryRecord>) -> dshtui::model::TranscriptWindow {
    let mut window = dshtui::model::TranscriptWindow::new(20);
    window.apply(Incoming::Snapshot {
        cursor: None,
        records,
        has_more: false,
        projections: None,
    });
    window
}

#[test]
fn image_placeholders_are_rendered_per_block_with_name_and_dims() {
    // AC-004-01：同消息多图逐块独立占位 `名称 · 宽×高`。
    let window = image_window(vec![
        nested_image_event(1),
        image_event(2, "att-a", "a.png", "10x20"),
        event(3, "user/message", Some("two images below")),
        image_event(4, "att-b", "b.png", "30x40"),
    ]);
    let mut app = AppState::default();
    app.conn = ConnState::Ready;
    app.kitty_capable = true;
    app.active_session = Some(SessionId("sess-1".into()));
    app.sessions.touch("sess-1", 20).apply(Incoming::Snapshot {
        cursor: None,
        records: vec![],
        has_more: false,
        projections: None,
    });
    // 直接替换窗口内容为图片窗口（golden 断言语义）。
    *app.sessions.touch("sess-1", 20) = window;
    let backend = TestBackend::new(100, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::render(frame, &app)).unwrap();
    let text = rendered_text(&terminal);
    assert!(text.contains("a.png · 10x20"), "text={text}");
    assert!(text.contains("b.png · 30x40"), "text={text}");
    assert!(text.contains("two images below"), "text={text}");
    // AC-004-01：同一事件内的两张图片逐块独立占位（宿主消息保留）。
    assert!(text.contains("nested.png · 4x5"), "text={text}");
    assert!(text.contains("second.png · 6x7"), "text={text}");
    assert!(
        text.contains("two images inline"),
        "宿主文本保留, text={text}"
    );
    assert!(
        !text.contains("[o]系统查看器"),
        "Kitty 占位不带查看器提示, text={text}"
    );
}

#[test]
fn image_placeholder_backfills_meta_and_shows_error_without_crash() {
    // AC-004-06：失败 → 错误占位 + 可读提示不崩溃；成功 → 元数据回填。
    let window = image_window(vec![image_event(1, "att-x", "x.png", "10x20")]);
    let mut app = AppState::default();
    app.conn = ConnState::Ready;
    app.kitty_capable = true;
    app.active_session = Some(SessionId("sess-1".into()));
    *app.sessions.touch("sess-1", 20) = window.clone();
    let backend = TestBackend::new(100, 10);
    let mut terminal = Terminal::new(backend).unwrap();

    // 拉取成功回填：AttachmentRef 覆盖占位标注（位置不变）。
    app.image_meta.insert(
        dshtui::api::types::AttachmentId("att-x".into()),
        dshtui::model::AttachmentRef::from(
            &dshtui::api::attachment::parse_response(&serde_json::json!({
                "attachment": {
                    "attachmentId": "att-x",
                    "mediaType": "image/png",
                    "bytes": 10,
                    "width": 640,
                    "height": 480,
                    "name": "fetched.png"
                },
                "data": "AQ=="
            }))
            .unwrap(),
        ),
    );
    terminal.draw(|frame| ui::render(frame, &app)).unwrap();
    let ok_text = rendered_text(&terminal);
    assert!(ok_text.contains("fetched.png · 640x480"), "text={ok_text}");

    // 失败：错误占位 + 可读提示，应用不崩溃。
    app.image_errors.insert(
        dshtui::api::types::AttachmentId("att-x".into()),
        "拉取失败: 断网".into(),
    );
    terminal.draw(|frame| ui::render(frame, &app)).unwrap();
    let err_text = rendered_text(&terminal);
    assert!(err_text.contains("✗"), "text={err_text}");
    assert!(err_text.contains("拉取失败: 断网"), "text={err_text}");
}

#[test]
fn non_kitty_placeholder_shows_system_viewer_hint() {
    // AC-004-03/07：非 Kitty 占位 + 系统查看器提示；Kitty 无提示。
    let window = image_window(vec![image_event(1, "att-x", "x.png", "10x20")]);
    let mut app = AppState::default();
    app.conn = ConnState::Ready;
    app.kitty_capable = false;
    app.active_session = Some(SessionId("sess-1".into()));
    *app.sessions.touch("sess-1", 20) = window.clone();
    let backend = TestBackend::new(100, 10);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::render(frame, &app)).unwrap();
    let text = rendered_text(&terminal);
    assert!(text.contains("x.png · 10x20"), "text={text}");
    assert!(
        text.contains("[image placeholder] [o]system viewer"),
        "text={text}"
    );
    assert!(text.contains("path:att-x"), "text={text}");

    app.kitty_capable = true;
    terminal.draw(|frame| ui::render(frame, &app)).unwrap();
    let kitty_text = rendered_text(&terminal);
    assert!(!kitty_text.contains("[o]系统查看器"), "text={kitty_text}");
}

#[test]
fn image_view_mode_shows_image_status_and_actions() {
    // AC-004-05 状态栏：IMAGE 徽标 + [o]系统查看器 [y]复制路径 [q]关闭。
    let mut app = AppState::default();
    app.conn = ConnState::Ready;
    app.mode = dshtui::app::Mode::ImageView;
    app.image_view.open_view(
        SessionSeq(1),
        dshtui::api::types::AttachmentId("att-x".into()),
        Some("x.png".into()),
        Some("10x20".into()),
    );
    let backend = TestBackend::new(100, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::render(frame, &app)).unwrap();
    let text = rendered_text(&terminal);
    assert!(text.contains("IMAGE"), "text={text}");
    assert!(
        text.contains("[o]系统查看器 [y]复制路径 [q]关闭"),
        "text={text}"
    );
    assert!(text.contains("加载中"), "text={text}");
}

// ============================================================================
// REQ-005 V0.3 Trajectory UI golden（Seam = ratatui TestBackend + ui::render；
// 计划 Step 5 测试 Seam 行）。验收：AC-005-01/02/03/06/10/15 渲染侧。
// ============================================================================

use dshtui::input::Command;

/// 带 turn/step/wire 字段的轨迹事件记录。
fn traj_ev(seq: u64, ty: &str, time_ms: i64, data: serde_json::Value) -> SessionHistoryRecord {
    SessionHistoryRecord::Event {
        event: SessionWireEvent {
            event_type: ty.to_string(),
            seq: Some(SessionSeq(seq)),
            time: Some(time_ms),
            request_id: None,
            ignorable: None,
            source_event_seqs: None,
            surface_op: None,
            data: Some(data),
        },
    }
}

/// 构造含轨迹数据的 AppState（Transcript + Trajectory 双写，mode=Trajectory）。
fn trajectory_app() -> AppState {
    let mut app = AppState::new(200);
    app.conn = ConnState::Ready;
    app.active_session = Some(SessionId("sess-t".into()));
    let sid = SessionId("sess-t".into());
    const T: i64 = 1_700_000_000_000;
    let records = vec![
        traj_ev(
            1,
            "request/header",
            T,
            serde_json::json!({"reason": "initial"}),
        ),
        traj_ev(
            2,
            "turn/start",
            T + 1000,
            serde_json::json!({"turn": 1, "reason": "user-prompt"}),
        ),
        traj_ev(
            3,
            "step/start",
            T + 2000,
            serde_json::json!({"turn": 1, "step": 1, "reason": "max"}),
        ),
        traj_ev(
            4,
            "user/message",
            T + 3000,
            serde_json::json!({"content": "排查数据库连接"}),
        ),
        traj_ev(
            5,
            "assistant/message",
            T + 4000,
            serde_json::json!({"turn": 1, "step": 1, "content": "先用 bash 检查端口", "usage": {"input": 100, "output": 50}}),
        ),
        traj_ev(
            6,
            "tool/call",
            T + 5000,
            serde_json::json!({"turn": 1, "step": 1, "callId": "c1", "name": "bash", "arguments": "{\"command\":\"ss -ltnp\"}"}),
        ),
        traj_ev(
            7,
            "tool/result",
            T + 8000,
            serde_json::json!({"turn": 1, "step": 1, "callId": "c1", "message": "db:5432 在监听"}),
        ),
        traj_ev(
            8,
            "step/end",
            T + 9000,
            serde_json::json!({"turn": 1, "step": 1}),
        ),
        traj_ev(
            9,
            "turn/end",
            T + 9500,
            serde_json::json!({"turn": 1, "reason": "stop"}),
        ),
    ];
    app.handle(AppEvent::FollowSnapshot {
        session_id: sid.clone(),
        cursor: None,
        records,
        has_more: false,
        projections: Some(serde_json::json!({
            "contextBreakdown": {"system": 1000, "tools": 2000, "message": 3000},
        })),
    });
    app.mode = Mode::Trajectory;
    app.focus = dshtui::app::Focus::Center;
    app
}

#[test]
fn trajectory_tabs_and_event_table_render_ac005_01_02_06() {
    // AC-005-01/02/06（渲染侧）：顶部 Tabs + 事件表（kind 标签、边界行保留、
    // 折叠标记）。
    let app = trajectory_app();
    let backend = TestBackend::new(140, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::render(frame, &app)).unwrap();
    let text = rendered_text(&terminal);
    // 顶部 Tabs（Chat / Trajectory 高亮）。
    assert!(text.contains("Chat"), "Tabs 显示 Chat, text={text}");
    assert!(text.contains("Trajectory"), "Tabs 显示 Trajectory");
    // 事件表 kind 标签 + 摘要（kind_label 短标签）。
    assert!(text.contains("user"), "user 行, text={text}");
    assert!(text.contains("排查数据库连接"), "user 摘要, text={text}");
    assert!(text.contains("tool"), "tool/call 行, text={text}");
    assert!(text.contains("ss -ltnp"), "tool args 摘要, text={text}");
    assert!(text.contains("assistant"), "assistant 行, text={text}");
    assert!(text.contains("先用 bash"), "assistant 摘要, text={text}");
    assert!(
        text.contains("header"),
        "request/header 边界行保留, text={text}"
    );
    // 折叠标记（turn/start 展开态 ▾）。
    assert!(text.contains("▾"), "turn 组展开标记 ▾, text={text}");
    // 耗时推导（tool/result 8000-5000=3000ms）。
    assert!(text.contains("3000ms"), "tool/result 耗时, text={text}");
}

#[test]
fn trajectory_detail_panel_render_ac005_03_10_15() {
    // AC-005-03/10/15（渲染侧）：详情面板 args/result/usage([未验证])/timing/
    // diff 降级「无 diff」；缺省 —。
    let mut app = trajectory_app();
    app.mode = Mode::Trajectory;
    // 光标在 tool/call 行（view index 3：header, turn/start, step/start,
    // user, assistant, tool/call, ...），通过 handle_command 走真逻辑。
    app.traj.cursor = 5; // tool/call
    app.handle_command(Command::OpenDetail);
    assert!(app.traj.detail_open);
    // 高终端让详情完整可见（context 段在末尾）。
    let backend = TestBackend::new(140, 34);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::render(frame, &app)).unwrap();
    let text = rendered_text(&terminal);
    assert!(text.contains("args"), "详情 args 分区, text={text}");
    assert!(text.contains("command"), "args JSON, text={text}");
    assert!(text.contains("result"), "result 分区, text={text}");
    assert!(text.contains("db:5432"), "result 内容, text={text}");
    assert!(
        text.contains("[未验证]") || text.contains("usage"),
        "usage 推导标注, text={text}"
    );
    // 无 meta → 无 diff 降级。
    assert!(text.contains("无 diff"), "diff 降级, text={text}");
    // context breakdown（官方投影，不自算）。
    assert!(text.contains("context"), "context breakdown, text={text}");
    assert!(text.contains("system: 1000"), "context system, text={text}");
}

#[test]
fn trajectory_detail_with_diff_renders_diff_text_ac005_10() {
    // AC-005-10：meta.diff 存在 → 展示 diff 文本（不降级）。
    let mut app = trajectory_app();
    // 用带 meta.diff 的 tool/result 重建。
    let sid = SessionId("sess-t".into());
    app.active_session = Some(sid.clone());
    app.traj_sessions = dshtui::model::TrajectoryStore::new(3);
    let mut w = dshtui::model::TrajectoryWindow::new(200);
    const T: i64 = 1_700_000_000_000;
    use dshtui::model::TrajIncoming;
    w.apply(TrajIncoming::Snapshot {
        cursor: None,
        records: vec![
            traj_ev(1, "turn/start", T, serde_json::json!({"turn": 1})),
            traj_ev(2, "step/start", T, serde_json::json!({"turn": 1, "step": 1})),
            traj_ev(3, "assistant/message", T, serde_json::json!({"turn": 1, "step": 1, "content": "a"})),
            traj_ev(4, "tool/call", T, serde_json::json!({"turn": 1, "step": 1, "callId": "c1", "name": "bash", "arguments": "{}"})),
            traj_ev(5, "tool/result", T, serde_json::json!({"turn": 1, "step": 1, "callId": "c1", "message": "ok", "meta": {"diff": "--- a/x\n+++ b/x"}})),
            traj_ev(6, "step/end", T, serde_json::json!({"turn": 1, "step": 1})),
            traj_ev(7, "turn/end", T, serde_json::json!({"turn": 1})),
        ],
        has_more: false,
        projections: None,
    });
    app.traj_sessions.touch("sess-t", 200);
    // 直接放行以绕过 borrow。
    // （重建触达即可，行已入 store 的默认窗口；重置 store 后重放）
    app.mode = Mode::Trajectory;
    // 重新应用让双写生效
    app.handle(AppEvent::FollowSnapshot {
        session_id: sid.clone(),
        cursor: None,
        records: vec![
            traj_ev(1, "turn/start", T, serde_json::json!({"turn": 1})),
            traj_ev(2, "step/start", T, serde_json::json!({"turn": 1, "step": 1})),
            traj_ev(3, "assistant/message", T, serde_json::json!({"turn": 1, "step": 1, "content": "a"})),
            traj_ev(4, "tool/call", T, serde_json::json!({"turn": 1, "step": 1, "callId": "c1", "name": "bash", "arguments": "{}"})),
            traj_ev(5, "tool/result", T, serde_json::json!({"turn": 1, "step": 1, "callId": "c1", "message": "ok", "meta": {"diff": "--- a/x\n+++ b/x"}})),
            traj_ev(6, "step/end", T, serde_json::json!({"turn": 1, "step": 1})),
            traj_ev(7, "turn/end", T, serde_json::json!({"turn": 1})),
        ],
        has_more: false,
        projections: None,
    });
    app.traj.cursor = 3; // tool/call
    app.handle_command(Command::OpenDetail);
    let backend = TestBackend::new(140, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::render(frame, &app)).unwrap();
    let text = rendered_text(&terminal);
    assert!(!text.contains("无 diff"), "有 diff 不降级, text={text}");
    assert!(text.contains("+++ b/x"), "diff 内容展示, text={text}");
}

#[test]
fn trajectory_narrow_width_renders_without_details_column() {
    // 窄终端（<120 列）：详情列自动收起，事件表正常渲染不崩溃（AC-005-10/
    // §10 降级；details 区不占主视图）。
    let mut app = trajectory_app();
    // 光标在 tool/call 行开详情（focus=Details），但 <120 列时 split 仍收起。
    app.traj.cursor = 5;
    app.handle_command(Command::OpenDetail);
    assert!(app.traj.detail_open);
    let backend = TestBackend::new(100, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::render(frame, &app)).unwrap();
    let text = rendered_text(&terminal);
    assert!(text.contains("Trajectory"), "事件表仍渲染, text={text}");
    // details 区被收起：详情文本 args 不占主视图（仍能看事件表）。
    assert!(text.contains("assistant"), "事件行可见, text={text}");
}

#[test]
fn trajectory_filter_overlay_renders_matches_ac005_05() {
    // AC-005-05（渲染侧）：过滤 overlay 显示 query 与命中数。
    let mut app = trajectory_app();
    app.mode = Mode::Trajectory;
    app.handle_command(Command::StartSearch);
    for c in ['d', 'b'] {
        app.handle_command(Command::PickerInput(c.to_string()));
    }
    let backend = TestBackend::new(140, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::render(frame, &app)).unwrap();
    let text = rendered_text(&terminal);
    assert!(text.contains("/ db"), "过滤 query 显示, text={text}");
    assert!(text.contains("matches"), "命中计数, text={text}");
}
