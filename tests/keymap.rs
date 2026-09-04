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
