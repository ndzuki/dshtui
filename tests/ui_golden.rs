use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use dshtui::api::types::{SessionHistoryRecord, SessionId, SessionSeq, SessionWireEvent};
use dshtui::app::{AppEvent, AppState, ConnState, DraftState, Mode, StopState};
use dshtui::model::Incoming;
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
    app.draft = Some(DraftState {
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
