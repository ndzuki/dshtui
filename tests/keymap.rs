use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use dshtui::api::types::{SessionId, SessionLogOffset};
use dshtui::app::{AppEvent, AppState, Mode};
use dshtui::input::{map_key, Command, InputMode, KeyDecoder};

fn key(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn ctrl(c: char) -> Event {
    Event::Key(KeyEvent::new(
        crossterm::event::KeyCode::Char(c),
        KeyModifiers::CONTROL,
    ))
}

#[test]
fn normal_mode_gg_is_a_stateful_goto_top_command() {
    let mut decoder = KeyDecoder::new();

    assert_eq!(
        decoder.decode(InputMode::Normal, key(KeyCode::Char('g'))),
        None
    );
    assert_eq!(
        decoder.decode(InputMode::Normal, key(KeyCode::Char('g'))),
        Some(Command::GotoTop)
    );

    // A non-g continuation clears the pending prefix and is decoded normally.
    assert_eq!(
        decoder.decode(InputMode::Normal, key(KeyCode::Char('g'))),
        None
    );
    assert_eq!(
        decoder.decode(InputMode::Normal, key(KeyCode::Char('j'))),
        Some(Command::MoveDown)
    );
}

#[test]
fn ctrl_c_maps_to_quit_and_stateless_wrapper_matches_decoder() {
    let mut decoder = KeyDecoder::new();
    assert_eq!(
        decoder.decode(InputMode::Normal, ctrl('c')),
        Some(Command::Quit)
    );
    assert_eq!(map_key(InputMode::Normal, ctrl('c')), Some(Command::Quit));
}

#[test]
fn picker_keys_emit_input_navigation_and_confirmation_commands() {
    let mut decoder = KeyDecoder::new();
    assert_eq!(
        decoder.decode(InputMode::Picker, key(KeyCode::Char('b'))),
        Some(Command::PickerInput("b".into()))
    );
    assert_eq!(
        decoder.decode(InputMode::Picker, key(KeyCode::Down)),
        Some(Command::PickerDown)
    );
    assert_eq!(
        decoder.decode(InputMode::Picker, key(KeyCode::Char('k'))),
        Some(Command::PickerUp)
    );
    assert_eq!(
        decoder.decode(InputMode::Picker, key(KeyCode::Backspace)),
        Some(Command::PickerBackspace)
    );
    assert_eq!(
        decoder.decode(InputMode::Picker, key(KeyCode::Enter)),
        Some(Command::PickerConfirm)
    );
    assert_eq!(
        decoder.decode(InputMode::Picker, key(KeyCode::Esc)),
        Some(Command::ClosePicker)
    );
}

#[test]
fn picker_command_updates_app_mode_and_query() {
    let mut app = AppState::default();
    assert!(app.handle_command(Command::OpenPicker).is_empty());
    assert_eq!(app.mode, Mode::Picker);
    assert!(app.picker.open);

    app.handle_command(Command::PickerInput("build".into()));
    assert_eq!(app.picker.query, "build");
    app.handle_command(Command::PickerBackspace);
    assert_eq!(app.picker.query, "buil");

    app.handle_command(Command::ClosePicker);
    assert_eq!(app.mode, Mode::Normal);
    assert!(!app.picker.open);
}

#[test]
fn insert_mode_decodes_and_routes_composer_lifecycle_ac002_01_04() {
    // 无活动会话：i 不进入 INSERT（AC-002-12 状态层语义见 app 单测）。
    let mut app = AppState::default();
    let mut decoder = KeyDecoder::new();
    let cmd = decoder
        .decode(InputMode::Normal, key(KeyCode::Char('i')))
        .unwrap();
    assert_eq!(cmd, Command::InsertMode);
    app.handle_command(cmd);
    assert_eq!(app.mode, Mode::Normal, "无会话 i 不进入 INSERT");

    // 打开会话后 i → INSERT。
    app.handle_command(Command::OpenSession(SessionId("s1".into())));
    app.handle(AppEvent::FollowSnapshot {
        session_id: SessionId("s1".into()),
        cursor: Some(SessionLogOffset(0)),
        records: vec![],
        has_more: true,
        projections: Some(serde_json::json!({"running": false})),
    });
    app.handle_command(Command::InsertMode);
    assert_eq!(app.mode, Mode::Insert);
    assert!(app.composer.visible);

    // Ctrl+Enter 换行 → 草稿含 \n（modifier 判定修正）。
    let newline = decoder
        .decode(
            InputMode::Insert,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL)),
        )
        .unwrap();
    assert_eq!(newline, Command::PickerInput("\n".into()));
    app.handle_command(newline);
    assert_eq!(app.draft.as_ref().map(|d| d.text.as_str()), Some("\n"));

    // Esc 收起但草稿保留（AC-002-04）。
    app.handle_command(Command::ClosePicker);
    assert_eq!(app.mode, Mode::Normal);
    assert!(!app.composer.visible);
    assert_eq!(app.draft.as_ref().map(|d| d.text.as_str()), Some("\n"));

    // 再次 i：草稿还在。
    app.handle_command(Command::InsertMode);
    assert_eq!(app.draft.as_ref().map(|d| d.text.as_str()), Some("\n"));
    assert!(app.composer.visible);
}

// ---------- REQ-003：搜索 / 视觉 / 审批 / 大纲 / 历史键位 ----------

#[test]
fn normal_mode_req003_keys_decode_ac003() {
    let mut decoder = KeyDecoder::new();
    assert_eq!(
        decoder.decode(InputMode::Normal, key(KeyCode::Char('/'))),
        Some(Command::StartSearch)
    );
    assert_eq!(
        decoder.decode(InputMode::Normal, key(KeyCode::Char('v'))),
        Some(Command::VisualStart { line: false })
    );
    assert_eq!(
        decoder.decode(InputMode::Normal, key(KeyCode::Char('V'))),
        Some(Command::VisualStart { line: true })
    );
    assert_eq!(
        decoder.decode(InputMode::Normal, key(KeyCode::Char('y'))),
        Some(Command::YankContext)
    );
    assert_eq!(
        decoder.decode(InputMode::Normal, key(KeyCode::Char('O'))),
        Some(Command::OpenOutline)
    );
    assert_eq!(
        decoder.decode(InputMode::Normal, key(KeyCode::Char(']'))),
        Some(Command::NextTurn)
    );
    assert_eq!(
        decoder.decode(InputMode::Normal, key(KeyCode::Char('['))),
        Some(Command::PrevTurn)
    );
    // `o` 打开语义保持不变（D-19 不与 `O` 冲突）。
    assert_eq!(
        decoder.decode(InputMode::Normal, key(KeyCode::Char('o'))),
        Some(Command::OpenSelected)
    );
}

#[test]
fn search_mode_keys_decode_ac003_05_19() {
    let mut decoder = KeyDecoder::new();
    assert_eq!(
        decoder.decode(InputMode::Search, key(KeyCode::Char('c'))),
        Some(Command::PickerInput("c".into()))
    );
    assert_eq!(
        decoder.decode(InputMode::Search, key(KeyCode::Backspace)),
        Some(Command::PickerBackspace)
    );
    assert_eq!(
        decoder.decode(InputMode::Search, key(KeyCode::Enter)),
        Some(Command::PickerConfirm)
    );
    assert_eq!(
        decoder.decode(InputMode::Search, key(KeyCode::Esc)),
        Some(Command::ClosePicker)
    );
    assert_eq!(
        decoder.decode(InputMode::Search, key(KeyCode::Char('n'))),
        Some(Command::SearchNext)
    );
    assert_eq!(
        decoder.decode(InputMode::Search, key(KeyCode::Char('N'))),
        Some(Command::SearchPrev)
    );
    assert_eq!(
        decoder.decode(InputMode::Search, key(KeyCode::Char('y'))),
        Some(Command::YankContext)
    );
    assert_eq!(
        decoder.decode(InputMode::Search, key(KeyCode::Char('j'))),
        Some(Command::PickerDown)
    );
    assert_eq!(
        decoder.decode(InputMode::Search, key(KeyCode::Char('k'))),
        Some(Command::PickerUp)
    );
}

#[test]
fn visual_mode_keys_decode_ac003_12() {
    let mut decoder = KeyDecoder::new();
    assert_eq!(
        decoder.decode(InputMode::Visual, key(KeyCode::Char('j'))),
        Some(Command::MoveDown)
    );
    assert_eq!(
        decoder.decode(InputMode::Visual, key(KeyCode::Char('y'))),
        Some(Command::YankContext)
    );
    assert_eq!(
        decoder.decode(InputMode::Visual, key(KeyCode::Esc)),
        Some(Command::ClosePicker)
    );
    assert_eq!(
        decoder.decode(InputMode::Visual, key(KeyCode::Char('v'))),
        Some(Command::ClosePicker)
    );
}

#[test]
fn approval_mode_keys_decode_ac003_07() {
    let mut decoder = KeyDecoder::new();
    assert_eq!(
        decoder.decode(InputMode::Approval, key(KeyCode::Char('y'))),
        Some(Command::ApprovalAllow)
    );
    assert_eq!(
        decoder.decode(InputMode::Approval, key(KeyCode::Char('n'))),
        Some(Command::ApprovalReject)
    );
    assert_eq!(
        decoder.decode(InputMode::Approval, key(KeyCode::Char('q'))),
        Some(Command::ApprovalCancel)
    );
    assert_eq!(
        decoder.decode(InputMode::Approval, key(KeyCode::Esc)),
        Some(Command::ApprovalCancel)
    );
    assert_eq!(
        decoder.decode(InputMode::Approval, key(KeyCode::Char('a'))),
        Some(Command::ApprovalAlways)
    );
    // Enter 在 APPROVAL 无语义（§3 键位边界，scope creep 移除）。
    assert_eq!(
        decoder.decode(InputMode::Approval, key(KeyCode::Enter)),
        None
    );
}

#[test]
fn insert_mode_history_keys_decode_ac003_10() {
    let mut decoder = KeyDecoder::new();
    assert_eq!(
        decoder.decode(InputMode::Insert, key(KeyCode::Up)),
        Some(Command::HistoryPrev)
    );
    assert_eq!(
        decoder.decode(InputMode::Insert, key(KeyCode::Down)),
        Some(Command::HistoryNext)
    );
}

#[test]
fn req003_key_route_updates_app_state() {
    let mut app = AppState::default();
    // `/` → SEARCH overlay。
    app.handle_command(Command::StartSearch);
    assert_eq!(app.mode, Mode::Search);
    assert!(app.search.open);
    app.handle_command(Command::ClosePicker);
    assert_eq!(app.mode, Mode::Normal);
    // `v`/`V` → VISUAL；y 退出并复制。
    app.handle_command(Command::OpenSession(SessionId("s1".into())));
    app.handle(AppEvent::FollowSnapshot {
        session_id: SessionId("s1".into()),
        cursor: Some(SessionLogOffset(1)),
        records: vec![dshtui::api::types::SessionHistoryRecord::Event {
            event: dshtui::api::types::SessionWireEvent {
                event_type: "user/message".into(),
                seq: Some(dshtui::api::types::SessionSeq(1)),
                time: None,
                request_id: None,
                ignorable: None,
                source_event_seqs: None,
                surface_op: None,
                data: Some(serde_json::json!({"content": "hi"})),
            },
        }],
        has_more: true,
        projections: Some(serde_json::json!({"running": false})),
    });
    app.handle_command(Command::VisualStart { line: true });
    assert_eq!(app.mode, Mode::Visual);
    app.handle_command(Command::ClosePicker);
    assert_eq!(app.mode, Mode::Normal);
    // `O` → 大纲列表。
    app.handle_command(Command::OpenOutline);
    assert!(app.outline.open);
    app.handle_command(Command::ClosePicker);
    assert!(!app.outline.open);
}
