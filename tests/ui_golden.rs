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
    // 宽字符占两个 cell（后一 cell 为占位空格）：用 Span::width 跳过占位。
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
