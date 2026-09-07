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

// ---------- REQ-004 V0.2 图片状态机（Step 6，AppState seam） ----------

/// AppState + 活动会话窗口含一张图片块（走 reducer 快照），焦点在其上。
fn image_app(kitty: bool) -> AppState {
    let mut app = AppState::new(20);
    app.conn = dshtui::app::ConnState::Ready;
    app.kitty_capable = kitty;
    app.active_session = Some(dshtui::api::types::SessionId("sess-1".into()));
    let records = vec![dshtui::api::types::SessionHistoryRecord::Event {
        event: dshtui::api::types::SessionWireEvent {
            event_type: "message/image".to_string(),
            seq: Some(dshtui::api::types::SessionSeq(1)),
            time: None,
            request_id: None,
            ignorable: None,
            source_event_seqs: None,
            surface_op: None,
            data: Some(serde_json::json!({
                "attachmentId": "att-1",
                "name": "a.png",
                "dims": "10x20"
            })),
        },
    }];
    app.focus = dshtui::app::Focus::Center;
    app.handle(dshtui::app::AppEvent::FollowSnapshot {
        session_id: dshtui::api::types::SessionId("sess-1".into()),
        cursor: None,
        records,
        has_more: false,
        projections: None,
    });
    assert!(
        app.focused_image_block().is_some(),
        "焦点锚点应落在图片块上"
    );
    app
}

#[test]
fn kitty_open_focused_image_enters_imageview_and_emits_fetch() {
    let mut app = image_app(true);
    let cmds = app.handle_command(Command::OpenFocused);
    assert_eq!(app.mode, Mode::ImageView);
    assert_eq!(app.image_view.phase, dshtui::model::ImageViewPhase::Loading);
    assert_eq!(
        app.image_view.block_seq,
        Some(dshtui::api::types::SessionSeq(1))
    );
    assert!(app
        .image_loading
        .contains(&dshtui::api::types::AttachmentId("att-1".into())));
    match cmds.as_slice() {
        [dshtui::app::Cmd::FetchAttachment {
            session_id,
            attachment_id,
            block_seq,
            for_viewer,
        }] => {
            assert_eq!(session_id.0, "sess-1");
            assert_eq!(attachment_id.0, "att-1");
            assert_eq!(block_seq.0, 1);
            assert!(!for_viewer);
        }
        other => panic!("期望单条 FetchAttachment，得到 {other:?}"),
    }
}

#[test]
fn repeated_open_is_idempotent_no_second_fetch() {
    // AC-004-08：在途/已开重复 o/Enter → 不重复 session/attachment、不叠加
    // 第二个 ImageView。
    let mut app = image_app(true);
    assert_eq!(app.handle_command(Command::OpenFocused).len(), 1);
    assert!(
        app.handle_command(Command::OpenFocused).is_empty(),
        "在途幂等"
    );
    assert!(
        app.handle_command(Command::OpenSelected).is_empty(),
        "o 同样幂等"
    );
    assert_eq!(app.mode, Mode::ImageView, "不叠加第二个 ImageView");
}

#[test]
fn non_center_open_does_not_open_chat_image() {
    let mut app = image_app(true);
    app.focus = dshtui::app::Focus::Sidebar;
    assert!(app.handle_command(Command::OpenFocused).is_empty());
    assert_eq!(app.mode, Mode::Normal);
}

#[test]
fn non_kitty_open_goes_directly_to_viewer_fetch_without_imageview() {
    // AC-004-07：非 Kitty 不进入终端内 ImageView，直达系统查看器。
    let mut app = image_app(false);
    let cmds = app.handle_command(Command::OpenFocused);
    assert_eq!(app.mode, Mode::Normal, "模式保持不变");
    assert!(!app.image_view.open);
    match cmds.as_slice() {
        [dshtui::app::Cmd::FetchAttachment { for_viewer, .. }] => assert!(for_viewer),
        other => panic!("期望 FetchAttachment{{for_viewer:true}}，得到 {other:?}"),
    }
    // 拉取完成 → 直达系统查看器。
    let temp = app
        .image_cache
        .write_temp_file(
            &dshtui::api::types::MediaType("image/png".into()),
            vec![1, 2, 3],
        )
        .unwrap();
    let cmds = app.handle(dshtui::app::AppEvent::AttachmentReady {
        session_id: dshtui::api::types::SessionId("sess-1".into()),
        attachment_id: dshtui::api::types::AttachmentId("att-1".into()),
        block_seq: dshtui::api::types::SessionSeq(1),
        meta: dshtui::model::AttachmentRef::from(
            &dshtui::api::attachment::parse_response(&serde_json::json!({
                "attachment": {
                    "attachmentId": "att-1",
                    "mediaType": "image/png",
                    "bytes": 3,
                    "width": 1,
                    "height": 1,
                    "name": "a.png"
                },
                "data": "AQ=="
            }))
            .unwrap(),
        ),
        frame: None,
        entry: dshtui::model::ImageCacheEntry {
            attachment_id: dshtui::api::types::AttachmentId("att-1".into()),
            media_type: dshtui::api::types::MediaType("image/png".into()),
            bytes: 3,
            width: 1,
            height: 1,
            temp_file: temp.clone(),
            last_used: 0,
        },
        cached: false,
        for_viewer: true,
    });
    match cmds.as_slice() {
        [dshtui::app::Cmd::OpenSystemViewer { path }] => assert_eq!(path, &temp),
        other => panic!("期望 OpenSystemViewer，得到 {other:?}"),
    }
    assert_eq!(app.mode, Mode::Normal);
    app.cleanup_transient_files();
}

#[test]
fn attachment_ready_renders_frame_and_marks_rendered_with_anchor() {
    let mut app = image_app(true);
    app.handle_command(Command::OpenFocused);
    let frame = dshtui::ui::image::kitty_frame(
        image::ImageBuffer::from_fn(16, 16, |x, y| image::Rgb([x as u8, y as u8, 0])).into(),
        (8, 16),
        ratatui::layout::Rect::new(0, 0, 40, 10),
        1,
    )
    .unwrap();
    let cmds = app.handle(dshtui::app::AppEvent::AttachmentReady {
        session_id: dshtui::api::types::SessionId("sess-1".into()),
        attachment_id: dshtui::api::types::AttachmentId("att-1".into()),
        block_seq: dshtui::api::types::SessionSeq(1),
        meta: dshtui::model::AttachmentRef::from(
            &dshtui::api::attachment::parse_response(&serde_json::json!({
                "attachment": {
                    "attachmentId": "att-1",
                    "mediaType": "image/png",
                    "bytes": 8,
                    "width": 16,
                    "height": 16,
                    "name": "fetched.png"
                },
                "data": "AQ=="
            }))
            .unwrap(),
        ),
        frame: Some(dshtui::app::KittyFrame(frame)),
        entry: dshtui::model::ImageCacheEntry {
            attachment_id: dshtui::api::types::AttachmentId("att-1".into()),
            media_type: dshtui::api::types::MediaType("image/png".into()),
            bytes: 8,
            width: 16,
            height: 16,
            temp_file: app
                .image_cache
                .write_temp_file(
                    &dshtui::api::types::MediaType("image/png".into()),
                    vec![0; 8],
                )
                .unwrap(),
            last_used: 0,
        },
        cached: false,
        for_viewer: false,
    });
    assert!(cmds.is_empty());
    assert_eq!(
        app.image_view.phase,
        dshtui::model::ImageViewPhase::Rendered
    );
    assert!(app.image_frame.is_some(), "Kitty 帧应已挂载");
    assert_eq!(
        app.image_meta
            .get(&dshtui::api::types::AttachmentId("att-1".into()))
            .and_then(|m| m.name.as_deref()),
        Some("fetched.png"),
        "元数据回填"
    );
    assert!(app
        .image_cache
        .get(&dshtui::api::types::AttachmentId("att-1".into()))
        .is_some());
    app.cleanup_transient_files();
}

#[test]
fn stale_attachment_ready_is_discarded_no_cross_image() {
    // AC-004-09 防串图：锚点不匹配的迟到响应丢弃，不替换当前视图帧。
    let mut app = image_app(true);
    app.handle_command(Command::OpenFocused);
    let stale = app.handle(dshtui::app::AppEvent::AttachmentReady {
        session_id: dshtui::api::types::SessionId("sess-1".into()),
        attachment_id: dshtui::api::types::AttachmentId("att-1".into()),
        block_seq: dshtui::api::types::SessionSeq(99),
        meta: dshtui::model::AttachmentRef::from(
            &dshtui::api::attachment::parse_response(&serde_json::json!({
                "attachment": {
                    "attachmentId": "att-1",
                    "mediaType": "image/png"
                },
                "data": "AQ=="
            }))
            .unwrap(),
        ),
        frame: None,
        entry: dshtui::model::ImageCacheEntry {
            attachment_id: dshtui::api::types::AttachmentId("att-1".into()),
            media_type: dshtui::api::types::MediaType("image/png".into()),
            bytes: 1,
            width: 1,
            height: 1,
            temp_file: app
                .image_cache
                .write_temp_file(&dshtui::api::types::MediaType("image/png".into()), vec![1])
                .unwrap(),
            last_used: 0,
        },
        cached: false,
        for_viewer: false,
    });
    assert!(stale.is_empty());
    assert!(app.image_frame.is_none(), "迟到帧不得挂载");
    assert_eq!(app.image_view.phase, dshtui::model::ImageViewPhase::Loading);
    app.cleanup_transient_files();
}

#[test]
fn attachment_failed_permission_marks_error_without_reconnect() {
    // AC-004-06：权限/格式类错误 → 错误占位，不自动重试。
    let mut app = image_app(true);
    app.handle_command(Command::OpenFocused);
    let cmds = app.handle(dshtui::app::AppEvent::AttachmentFailed {
        session_id: dshtui::api::types::SessionId("sess-1".into()),
        attachment_id: dshtui::api::types::AttachmentId("att-1".into()),
        block_seq: dshtui::api::types::SessionSeq(1),
        code: "PERMISSION_DENIED".into(),
        message: "需审批".into(),
        retryable: false,
        for_viewer: false,
    });
    assert!(cmds.is_empty(), "权限类不得触发重连: {cmds:?}");
    assert_eq!(app.image_view.phase, dshtui::model::ImageViewPhase::Failed);
    let err = app.image_view.error.as_ref().unwrap();
    assert_eq!(err.code, "PERMISSION_DENIED");
    assert!(app
        .image_errors
        .get(&dshtui::api::types::AttachmentId("att-1".into()))
        .unwrap()
        .contains("PERMISSION_DENIED"));
    assert!(!app
        .image_loading
        .contains(&dshtui::api::types::AttachmentId("att-1".into())));
}

#[test]
fn attachment_failed_network_triggers_existing_reconnect() {
    // AC-004-06：断网走既有指数退避重连；恢复后可重开（abort 后在途清除）。
    let mut app = image_app(true);
    app.handle_command(Command::OpenFocused);
    let cmds = app.handle(dshtui::app::AppEvent::AttachmentFailed {
        session_id: dshtui::api::types::SessionId("sess-1".into()),
        attachment_id: dshtui::api::types::AttachmentId("att-1".into()),
        block_seq: dshtui::api::types::SessionSeq(1),
        code: "network".into(),
        message: "连接中断".into(),
        retryable: true,
        for_viewer: false,
    });
    assert!(matches!(
        cmds.as_slice(),
        [dshtui::app::Cmd::Reconnect { .. }]
    ));
    assert_eq!(app.conn, dshtui::app::ConnState::Reconnecting);
    // 恢复路径：失败视图仍在（q 关闭）→ 重开必须能再次发起拉取
    //（在途/错误不污染新请求）。
    app.handle(dshtui::app::AppEvent::Reconnected);
    assert_eq!(app.conn, dshtui::app::ConnState::Ready);
    app.handle_command(Command::ImageViewClose);
    let retry = app.handle_command(Command::OpenFocused);
    assert!(matches!(
        retry.as_slice(),
        [dshtui::app::Cmd::FetchAttachment { .. }]
    ));
}

#[test]
fn cached_hit_rerenders_without_refetch_and_copy_uses_cached_path() {
    // AC-004-08/09：缓存命中 → 不重复 session/attachment（RenderCachedImage
    // 而非 FetchAttachment）；`y` 复制缓存路径。
    let mut app = image_app(true);
    let temp = app
        .image_cache
        .write_temp_file(
            &dshtui::api::types::MediaType("image/png".into()),
            vec![1, 2, 3],
        )
        .unwrap();
    app.image_cache.complete(
        &dshtui::api::types::AttachmentId("att-1".into()),
        dshtui::model::ImageCacheEntry {
            attachment_id: dshtui::api::types::AttachmentId("att-1".into()),
            media_type: dshtui::api::types::MediaType("image/png".into()),
            bytes: 3,
            width: 1,
            height: 1,
            temp_file: temp.clone(),
            last_used: 0,
        },
    );
    let cmds = app.handle_command(Command::OpenFocused);
    match cmds.as_slice() {
        [dshtui::app::Cmd::RenderCachedImage {
            attachment_id,
            temp_file,
            ..
        }] => {
            assert_eq!(attachment_id.0, "att-1");
            assert_eq!(temp_file, &temp);
        }
        other => panic!("缓存命中应走 RenderCachedImage，得到 {other:?}"),
    }
    // 渲染成功后 y 复制缓存文件路径。
    app.handle(dshtui::app::AppEvent::AttachmentReady {
        session_id: dshtui::api::types::SessionId("sess-1".into()),
        attachment_id: dshtui::api::types::AttachmentId("att-1".into()),
        block_seq: dshtui::api::types::SessionSeq(1),
        meta: dshtui::model::AttachmentRef::from(
            &dshtui::api::attachment::parse_response(&serde_json::json!({
                "attachment": {
                    "attachmentId": "att-1",
                    "mediaType": "image/png",
                    "bytes": 3,
                    "width": 1,
                    "height": 1
                },
                "data": "AQ=="
            }))
            .unwrap(),
        ),
        frame: None,
        entry: dshtui::model::ImageCacheEntry {
            attachment_id: dshtui::api::types::AttachmentId("att-1".into()),
            media_type: dshtui::api::types::MediaType("image/png".into()),
            bytes: 3,
            width: 1,
            height: 1,
            temp_file: temp.clone(),
            last_used: 0,
        },
        cached: true,
        for_viewer: false,
    });
    let copy = app.handle_command(Command::ImageViewCopy);
    match copy.as_slice() {
        [dshtui::app::Cmd::CopyImageText { text }] => assert_eq!(text, &temp.to_string_lossy()),
        other => panic!("期望 CopyImageText，得到 {other:?}"),
    }
}

#[test]
fn image_view_close_returns_to_normal_transcript() {
    // AC-004-05 `q`：关闭回 transcript（NORMAL）。
    let mut app = image_app(true);
    app.handle_command(Command::OpenFocused);
    assert_eq!(app.mode, Mode::ImageView);
    assert!(app.handle_command(Command::ImageViewClose).is_empty());
    assert_eq!(app.mode, Mode::Normal);
    assert!(!app.image_view.open);
    assert!(app.image_frame.is_none());
}

#[test]
fn stale_non_kitty_result_does_not_launch_viewer() {
    // AC-004-09 防串图：目标锚点不匹配的迟到响应不得打开系统查看器，
    // 且其临时文件被回收。
    let mut app = image_app(false);
    app.handle_command(Command::OpenFocused);
    assert!(app.pending_viewer.is_some(), "非 Kitty 已登记查看器目标");
    let stale_file = app
        .image_cache
        .write_temp_file(&dshtui::api::types::MediaType("image/png".into()), vec![9])
        .unwrap();
    let cmds = app.handle(dshtui::app::AppEvent::AttachmentReady {
        session_id: dshtui::api::types::SessionId("sess-1".into()),
        attachment_id: dshtui::api::types::AttachmentId("att-1".into()),
        block_seq: dshtui::api::types::SessionSeq(99),
        meta: dshtui::model::AttachmentRef::from(
            &dshtui::api::attachment::parse_response(&serde_json::json!({
                "attachment": {
                    "attachmentId": "att-1",
                    "mediaType": "image/png"
                },
                "data": "AQ=="
            }))
            .unwrap(),
        ),
        frame: None,
        entry: dshtui::model::ImageCacheEntry {
            attachment_id: dshtui::api::types::AttachmentId("att-1".into()),
            media_type: dshtui::api::types::MediaType("image/png".into()),
            bytes: 1,
            width: 1,
            height: 1,
            temp_file: stale_file.clone(),
            last_used: 0,
        },
        cached: false,
        for_viewer: true,
    });
    assert!(
        cmds.is_empty(),
        "stale 结果不得触发 OpenSystemViewer: {cmds:?}"
    );
    assert!(app.pending_viewer.is_some(), "在途目标保留");
    assert!(!stale_file.exists(), "stale 临时文件已回收");
    app.cleanup_transient_files();
}

#[test]
fn stale_attachment_failure_does_not_reconnect_active_session() {
    // AC-004-06：目标不匹配的迟到失败不得触发重连（不污染当前会话）。
    let mut app = image_app(true);
    app.handle_command(Command::OpenFocused);
    let cmds = app.handle(dshtui::app::AppEvent::AttachmentFailed {
        session_id: dshtui::api::types::SessionId("sess-1".into()),
        attachment_id: dshtui::api::types::AttachmentId("att-1".into()),
        block_seq: dshtui::api::types::SessionSeq(99),
        code: "network".into(),
        message: "stale".into(),
        retryable: true,
        for_viewer: false,
    });
    assert!(cmds.is_empty(), "stale 失败不得触发重连: {cmds:?}");
    assert_eq!(app.conn, dshtui::app::ConnState::Ready);
}

// =====================================================================
// REQ-005 V0.3 Trajectory 键位矩阵（Seam = KeyDecoder.decode + AppState
// handle_command；D-25/Notes/04 §3.6）。验收：AC-005-01/04/11。
// ============================================================================

#[test]
fn trajectory_mode_key_matrix_maps_to_commands() {
    let mut d = KeyDecoder::new();
    // gt（g 前缀双键）：Normal → Trajectory 切换命令。
    assert_eq!(d.decode(InputMode::Normal, key(KeyCode::Char('g'))), None);
    assert_eq!(
        d.decode(InputMode::Normal, key(KeyCode::Char('t'))),
        Some(Command::ToggleTrajectory)
    );
    // gT：回 Chat。
    let mut d2 = KeyDecoder::new();
    assert_eq!(d2.decode(InputMode::Normal, key(KeyCode::Char('g'))), None);
    assert_eq!(
        d2.decode(InputMode::Normal, key(KeyCode::Char('T'))),
        Some(Command::GotoChat)
    );
    // Trajectory 模式键位矩阵（D-25/Notes/04 §3.6）。
    let mut t = KeyDecoder::new();
    assert_eq!(
        t.decode(InputMode::Trajectory, key(KeyCode::Char('j'))),
        Some(Command::MoveDown)
    );
    assert_eq!(
        t.decode(InputMode::Trajectory, key(KeyCode::Char('k'))),
        Some(Command::MoveUp)
    );
    assert_eq!(
        t.decode(InputMode::Trajectory, key(KeyCode::Char('z'))),
        Some(Command::ToggleFold)
    );
    assert_eq!(
        t.decode(InputMode::Trajectory, key(KeyCode::Enter)),
        Some(Command::OpenDetail)
    );
    assert_eq!(
        t.decode(InputMode::Trajectory, key(KeyCode::Char('d'))),
        Some(Command::OpenDetail)
    );
    assert_eq!(
        t.decode(InputMode::Trajectory, key(KeyCode::Char('/'))),
        Some(Command::StartSearch)
    );
    assert_eq!(
        t.decode(InputMode::Trajectory, key(KeyCode::Char('y'))),
        Some(Command::YankContext)
    );
    assert_eq!(
        t.decode(InputMode::Trajectory, key(KeyCode::Char('q'))),
        Some(Command::Quit)
    );
    assert_eq!(
        t.decode(InputMode::Trajectory, ctrl('w')),
        Some(Command::CycleFocus)
    );
    assert_eq!(
        t.decode(InputMode::Trajectory, key(KeyCode::Char('2'))),
        Some(Command::ToggleTrajectory)
    );
    assert_eq!(
        t.decode(InputMode::Trajectory, key(KeyCode::Char('1'))),
        Some(Command::GotoChat)
    );
    // Trajectory 内 gt → 切回 Chat（Notes/04 §3.6）。
    let mut t2 = KeyDecoder::new();
    assert_eq!(
        t2.decode(InputMode::Trajectory, key(KeyCode::Char('g'))),
        None
    );
    assert_eq!(
        t2.decode(InputMode::Trajectory, key(KeyCode::Char('t'))),
        Some(Command::ToggleTrajectory)
    );
    // Chat 专属键不进入 Trajectory 键位（不串模式）。
    let mut t3 = KeyDecoder::new();
    assert_eq!(
        t3.decode(InputMode::Trajectory, key(KeyCode::Char('i'))),
        None
    );
    assert_eq!(
        t3.decode(InputMode::Trajectory, key(KeyCode::Char('f'))),
        None
    );
    assert_eq!(
        t3.decode(InputMode::Trajectory, key(KeyCode::Char('s'))),
        None
    );
}

#[test]
fn normal_gg_still_works_after_g_prefix_extension() {
    // gg 语义不被 gt/gT 扩展破坏（回归）。
    let mut d = KeyDecoder::new();
    assert_eq!(d.decode(InputMode::Normal, key(KeyCode::Char('g'))), None);
    assert_eq!(
        d.decode(InputMode::Normal, key(KeyCode::Char('g'))),
        Some(Command::GotoTop)
    );
    // g + 非 g/t/T 第二键 → 清前缀（x 按普通键解码 no-op）。
    let mut d2 = KeyDecoder::new();
    assert_eq!(d2.decode(InputMode::Normal, key(KeyCode::Char('g'))), None);
    assert_eq!(d2.decode(InputMode::Normal, key(KeyCode::Char('x'))), None);
    // 前缀已清：需重新 gg 才 GotoTop。
    assert_eq!(d2.decode(InputMode::Normal, key(KeyCode::Char('g'))), None);
    assert_eq!(
        d2.decode(InputMode::Normal, key(KeyCode::Char('g'))),
        Some(Command::GotoTop),
        "前缀被清 → 新 gg 生效"
    );
}

// ============================================================================
// REQ-005 Trajectory reducer 状态机（Seam = AppState.handle_command +
// handle，无 IO；原型 PASS 条件对应落测）。验收：AC-005-01/04/11。
// ============================================================================

/// 构建含轨迹数据的 AppState（FollowSnapshot 双写 transcript + traj 窗口）。
fn traj_app() -> AppState {
    let mut app = AppState::new(200);
    app.conn = dshtui::app::ConnState::Ready;
    let sid = SessionId("sess-traj".into());
    app.active_session = Some(sid.clone());
    let rec = |seq: u64, ty: &str, data: serde_json::Value| {
        dshtui::api::types::SessionHistoryRecord::Event {
            event: dshtui::api::types::SessionWireEvent {
                event_type: ty.to_string(),
                seq: Some(dshtui::api::types::SessionSeq(seq)),
                time: Some(seq as i64),
                request_id: None,
                ignorable: None,
                source_event_seqs: None,
                surface_op: None,
                data: Some(data),
            },
        }
    };
    let records = vec![
        rec(1, "turn/start", serde_json::json!({"turn": 1})),
        rec(2, "step/start", serde_json::json!({"turn": 1, "step": 1})),
        rec(
            3,
            "assistant/message",
            serde_json::json!({"turn": 1, "step": 1, "content": "a1"}),
        ),
        rec(
            4,
            "tool/call",
            serde_json::json!({"turn": 1, "step": 1, "callId": "c1", "name": "bash", "arguments": "{\"command\":\"ls\"}"}),
        ),
        rec(
            5,
            "tool/result",
            serde_json::json!({"turn": 1, "step": 1, "callId": "c1", "message": "done"}),
        ),
        rec(6, "step/end", serde_json::json!({"turn": 1, "step": 1})),
        rec(7, "turn/end", serde_json::json!({"turn": 1})),
    ];
    app.handle(AppEvent::FollowSnapshot {
        session_id: sid,
        cursor: None,
        records,
        has_more: true,
        projections: None,
    });
    app
}

#[test]
fn gt_switch_tab_and_back_ac005_01() {
    // AC-005-01：gt → Trajectory；gT → Chat。
    let mut app = traj_app();
    assert_eq!(app.mode, Mode::Normal);
    app.handle_command(Command::ToggleTrajectory); // gt
    assert_eq!(app.mode, Mode::Trajectory, "gt 切到 Trajectory");
    app.handle_command(Command::GotoChat); // gT
    assert_eq!(app.mode, Mode::Normal, "gT 切回 Chat");
    // 再 gt 回 Trajectory（轨迹窗口有数据可浏览）。
    app.handle_command(Command::ToggleTrajectory);
    assert_eq!(app.mode, Mode::Trajectory);
}

#[test]
fn gt_switch_preserves_fold_and_cursor_ac005_01_04() {
    // gt/gT 互切不丢折叠/选中（Prototype PASS2 → reducer 层）。
    let mut app = traj_app();
    app.handle_command(Command::ToggleTrajectory);
    // 光标移到 tool/call 行（view index 3）。
    for _ in 0..3 {
        app.handle_command(Command::MoveDown);
    }
    assert_eq!(app.traj.cursor, 3);
    app.handle_command(Command::ToggleFold); // z 折叠 assistant 组
    let folded = app
        .traj
        .fold
        .is_collapsed(dshtui::model::GroupId::Assistant { turn: 1, step: 1 });
    assert!(folded, "z 折叠当前组");
    // 切走再切回：fold 保留（cursor 收敛到可见范围）。
    app.handle_command(Command::GotoChat);
    assert_eq!(app.mode, Mode::Normal);
    app.handle_command(Command::ToggleTrajectory);
    assert!(app
        .traj
        .fold
        .is_collapsed(dshtui::model::GroupId::Assistant { turn: 1, step: 1 }));
    // 展开（za 同一 toggle）。
    app.handle_command(Command::ToggleFold);
    assert!(!app
        .traj
        .fold
        .is_collapsed(dshtui::model::GroupId::Assistant { turn: 1, step: 1 }));
}

#[test]
fn enter_detail_yank_copy_and_q_modal_context_ac005_03_11() {
    // AC-005-03/11：Enter 开详情（tool/call 行）、y 复制 args/result、
    // 详情子层 q 关面板回轨迹、列表焦点 q 全局退出。
    let mut app = traj_app();
    app.handle_command(Command::ToggleTrajectory);
    app.traj.cursor = 3; // tool/call 行
    app.handle_command(Command::OpenDetail); // Enter
    assert!(app.traj.detail_open, "Enter 打开详情子层");
    assert_eq!(app.focus, dshtui::app::Focus::Details);
    let detail = app.traj.detail.as_ref().expect("详情已构建");
    assert_eq!(detail.source_kind, dshtui::model::TrajKind::ToolCall);
    assert!(detail.args_text.is_some(), "args 在详情");
    // y 复制 → CopyToClipboard 命令（内存文本，main 执行 arboard 后端）。
    let cmds = app.handle_command(Command::YankContext);
    assert_eq!(cmds.len(), 1);
    match &cmds[0] {
        dshtui::app::Cmd::CopyToClipboard { text } => {
            assert!(text.contains("command"), "复制目标含 args: {text}");
            assert!(text.contains("done"), "复制目标含 result: {text}");
        }
        other => panic!("期望 CopyToClipboard，得到 {other:?}"),
    }
    // 详情子层 q → 关面板回轨迹列表（不退出）。
    assert!(!app.exited);
    app.handle_command(Command::Quit);
    assert!(!app.traj.detail_open, "q 关闭详情");
    assert_eq!(app.mode, Mode::Trajectory, "q 回轨迹列表");
    assert!(!app.exited, "详情 q 不退出程序");
    // 列表焦点 q → 全局退出路径（quit 触发运行检查；无运行会话直接 exit 链）。
    app.handle_command(Command::Quit);
    assert!(
        app.quit_requested || app.exited,
        "列表焦点 q 走全局退出（quit_requested={} exited={}）",
        app.quit_requested,
        app.exited
    );
}

#[test]
fn ctrl_w_focus_cycle_includes_detail_sublayer() {
    // Prototype PASS3 → reducer：Ctrl+w 三向循环含详情子层。
    let mut app = traj_app();
    app.handle_command(Command::ToggleTrajectory);
    app.focus = dshtui::app::Focus::Sidebar;
    app.handle_command(Command::CycleFocus);
    assert_eq!(app.focus, dshtui::app::Focus::Center);
    // 详情关：Center → Sidebar（不进入 Details）。
    app.handle_command(Command::CycleFocus);
    assert_eq!(app.focus, dshtui::app::Focus::Sidebar);
    // 开详情后：Center → Details → Sidebar。
    app.traj.cursor = 3;
    app.handle_command(Command::OpenDetail);
    assert_eq!(app.focus, dshtui::app::Focus::Details);
    app.handle_command(Command::CycleFocus);
    assert_eq!(app.focus, dshtui::app::Focus::Sidebar);
}

#[test]
fn trajectory_slash_does_not_enter_chat_search_mode() {
    // Prototype PASS4 → reducer：轨迹内 / 不进入 Chat 结构化搜索 Mode::Search。
    let mut app = traj_app();
    app.handle_command(Command::ToggleTrajectory);
    app.handle_command(Command::StartSearch);
    assert_eq!(app.mode, Mode::Trajectory, "轨迹内 / 不串 Chat 搜索模式");
    assert!(app.traj.filter.open, "过滤输入态打开");
    app.handle_command(Command::ClosePicker); // Esc
    assert!(!app.traj.filter.open);
    // Chat 模式 / → Mode::Search（对照不串）。
    app.handle_command(Command::ToggleTrajectory);
    app.handle_command(Command::StartSearch);
    assert_eq!(app.mode, Mode::Search);
}

#[test]
fn trajectory_filter_input_highlight_and_enter_jump_ac005_05_13() {
    // AC-005-05/13（reducer 层）：/ 打开过滤 → 输入关键词即时过滤 → j/k 移动
    // 命中选中 → Enter 跳转命中行（可展开其所在折叠组）。
    let mut app = traj_app();
    app.handle_command(Command::ToggleTrajectory);
    app.handle_command(Command::StartSearch);
    assert!(app.traj.filter.open);
    // 输入 "done"：过滤 query 累积（本地即时，无防抖风暴面）。
    for c in ['d', 'o', 'n', 'e'] {
        app.handle_command(Command::PickerInput(c.to_string()));
    }
    assert_eq!(app.traj.filter.query, "done");
    // 命中 tool/result 行（message=done）。
    let hits = app.traj_search_index.query("done");
    assert_eq!(hits.len(), 1, "过滤即时命中");
    let hit_row = app.traj_search_index.items()[hits[0].item_index].row_id;
    // Enter 跳转：cursor 定位命中行。
    app.handle_command(Command::PickerConfirm);
    assert!(!app.traj.filter.open, "Enter 后退出过滤");
    let window = app
        .active_session
        .as_ref()
        .and_then(|sid| app.traj_sessions.get(&sid.0))
        .unwrap();
    let view = window.view(&app.traj.fold);
    let pos = view
        .iter()
        .position(|r| r.id() == hit_row)
        .expect("命中行可见");
    assert_eq!(app.traj.cursor, pos, "cursor 跳转到命中行");
    // 跳到命中行后 Enter 可开详情（tool/result 详情）。
    app.handle_command(Command::OpenDetail);
    assert!(app.traj.detail_open);
    assert_eq!(
        app.traj.detail.as_ref().unwrap().source_kind,
        dshtui::model::TrajKind::ToolResult
    );
}

#[test]
fn trajectory_filter_jump_expands_collapsed_group_ac005_13() {
    // AC-005-13：命中行所在折叠组被折叠时，Enter 跳转自动展开（行可见）。
    let mut app = traj_app();
    app.handle_command(Command::ToggleTrajectory);
    // 折叠 turn 1 assistant 组（tool/result 行被隐藏）。
    let asst = {
        let window = app
            .active_session
            .as_ref()
            .and_then(|sid| app.traj_sessions.get(&sid.0))
            .unwrap();
        window
            .raw_rows()
            .find(|r| r.kind() == dshtui::model::TrajKind::AssistantMessage)
            .unwrap()
            .id()
    };
    app.traj
        .fold
        .toggle(dshtui::model::GroupId::Assistant { turn: 1, step: 1 });
    let _ = asst;
    app.handle_command(Command::StartSearch);
    for c in ['d', 'o', 'n', 'e'] {
        app.handle_command(Command::PickerInput(c.to_string()));
    }
    assert!(
        !app.traj_search_index.query("done").is_empty(),
        "命中仍可查到"
    );
    app.handle_command(Command::PickerConfirm);
    // 跳转自动展开折叠组：命中行可见。
    let window = app
        .active_session
        .as_ref()
        .and_then(|sid| app.traj_sessions.get(&sid.0))
        .unwrap();
    assert!(window
        .view(&app.traj.fold)
        .iter()
        .any(|r| { r.kind() == dshtui::model::TrajKind::ToolResult && r.summary() == "done" }));
}

#[test]
fn trajectory_filter_esc_and_backspace_ac005_05() {
    // Esc 退出过滤回完整列表；Backspace 删除字符。
    let mut app = traj_app();
    app.handle_command(Command::ToggleTrajectory);
    app.handle_command(Command::StartSearch);
    for c in ['d', 'o'] {
        app.handle_command(Command::PickerInput(c.to_string()));
    }
    assert_eq!(app.traj.filter.query, "do");
    app.handle_command(Command::PickerBackspace);
    assert_eq!(app.traj.filter.query, "d", "Backspace 删除");
    app.handle_command(Command::ClosePicker); // Esc
    assert!(!app.traj.filter.open);
    assert_eq!(app.mode, Mode::Trajectory, "退出过滤仍在 Trajectory tab");
    // 过滤中 q 也退出过滤（不退出程序）。
    app.handle_command(Command::StartSearch);
    for c in ['x', 'y'] {
        app.handle_command(Command::PickerInput(c.to_string()));
    }
    app.handle_command(Command::Quit);
    assert!(!app.traj.filter.open, "过滤中 q 关过滤");
    assert!(!app.exited, "过滤中 q 不退出程序");
}

#[test]
fn trajectory_page_permission_error_no_retry_ac005_09() {
    // AC-005-09：session/page 权限错误 → 提示 + 不自动重试、不崩溃
    // （Notes/03 §8：PERMISSION_DENIED 权限类不自动重试）。
    let mut app = traj_app();
    // 触发一页请求得到 generation（single-flight guard）。
    let cmds = app.handle_command(Command::GotoTop);
    let gen = match cmds.as_slice() {
        [dshtui::app::Cmd::RequestPage { generation, .. }] => *generation,
        other => panic!("期望 RequestPage，得到 {other:?}"),
    };
    // 权限错误（generation 匹配，非 stale）。
    let cmds = app.handle(AppEvent::PageError {
        session_id: SessionId("sess-traj".into()),
        generation: gen,
        error: dshtui::api::ClientError::Remote {
            code: "PERMISSION_DENIED".into(),
            message: "无权限读取历史".into(),
            class: dshtui::api::envelope::ErrorClass::PermissionDenied,
        },
    });
    assert!(cmds.is_empty(), "权限错误不自动重试: {cmds:?}");
    assert!(
        app.last_error.is_some(),
        "用户可见错误提示: {:?}",
        app.last_error
    );
    assert!(
        app.last_error.as_deref().unwrap().contains("权限"),
        "提示含权限语义"
    );
    // 恢复路径：权限错误后正常操作不被污染（重新滚动到底部可再次请求）。
    app.last_error = None;
    let cmds = app.handle_command(Command::GotoBottom);
    assert!(
        cmds.is_empty() || matches!(cmds.as_slice(), [dshtui::app::Cmd::CopyToClipboard { .. }])
    );
    assert_eq!(app.last_error, None, "失败状态不残留");
}

#[test]
fn trajectory_reconnect_snapshot_no_dup_and_gap_fill_ac005_08() {
    // AC-005-08（AppState 集成）：断网重连 snapshot 重建后轨迹窗口不重复；
    // 缺口（重建快照仅含部分）由 follow 补齐后事件链完整。
    let mut app = traj_app(); // 已含 turn1 7 行（seq 1..=7）
    let seq_before: Vec<u64> = {
        let w = app
            .active_session
            .as_ref()
            .and_then(|sid| app.traj_sessions.get(&sid.0))
            .unwrap();
        w.raw_rows().map(|r| r.seq().0).collect()
    };
    assert_eq!(seq_before.len(), 7);
    // 重连快照（模拟 unfixable gap 重建：同 seq 1..=7 重放）。
    app.handle(AppEvent::FollowSnapshot {
        session_id: SessionId("sess-traj".into()),
        cursor: None,
        records: vec![dshtui::api::types::SessionHistoryRecord::Event {
            event: dshtui::api::types::SessionWireEvent {
                event_type: "turn/start".into(),
                seq: Some(dshtui::api::types::SessionSeq(1)),
                time: Some(1),
                request_id: None,
                ignorable: None,
                source_event_seqs: None,
                surface_op: None,
                data: Some(serde_json::json!({"turn": 1})),
            },
        }],
        has_more: true,
        projections: None,
    });
    // 重建后窗口仅新快照内容；follow 补齐后续缺口（2..=7 逐个到达）不重复。
    let w = app
        .active_session
        .as_ref()
        .and_then(|sid| app.traj_sessions.get(&sid.0))
        .unwrap();
    assert_eq!(w.len(), 1, "重建只含快照行");
    let sid = SessionId("sess-traj".into());
    for s in 2..=7 {
        let data = match s {
            2 => serde_json::json!({"turn":1,"step":1}),
            3 => serde_json::json!({"turn":1,"step":1,"content":"a1"}),
            4 => {
                serde_json::json!({"turn":1,"step":1,"callId":"c1","name":"bash","arguments":"{}"})
            }
            5 => serde_json::json!({"turn":1,"step":1,"callId":"c1","message":"done"}),
            6 => serde_json::json!({"turn":1,"step":1}),
            _ => serde_json::json!({"turn":1}),
        };
        let ty = match s {
            2 => "step/start",
            3 => "assistant/message",
            4 => "tool/call",
            5 => "tool/result",
            6 => "step/end",
            _ => "turn/end",
        };
        let e = dshtui::api::types::SessionWireEvent {
            event_type: ty.into(),
            seq: Some(dshtui::api::types::SessionSeq(s)),
            time: Some(s as i64),
            request_id: None,
            ignorable: None,
            source_event_seqs: None,
            surface_op: None,
            data: Some(data),
        };
        app.handle(AppEvent::FollowEvent {
            session_id: sid.clone(),
            event: e,
        });
    }
    let w = app
        .active_session
        .as_ref()
        .and_then(|sid| app.traj_sessions.get(&sid.0))
        .unwrap();
    let seqs: Vec<u64> = w.raw_rows().map(|r| r.seq().0).collect();
    assert_eq!(seqs, (1..=7).collect::<Vec<_>>(), "缺口补齐、无重复无空洞");
}

#[test]
fn trajectory_open_detail_refreshes_on_cursor_move_ac005_03() {
    // Spec 审查 (c)1 修复：详情开着时（Ctrl+w 切回 Center）移动光标 →
    // 详情随新选中行重建（REQ-005 §5「选行变化即重建，source 锚点防串」）。
    // traj_app 记录：0 turn/start 1 step/start 2 assistant 3 tool/call
    // 4 tool/result 5 step/end 6 turn/end。
    let mut app = traj_app();
    app.handle_command(Command::ToggleTrajectory);
    app.traj.cursor = 3; // tool/call（c1 bash）
    app.handle_command(Command::OpenDetail);
    assert!(app.traj.detail_open);
    assert_eq!(
        app.traj.detail.as_ref().unwrap().source_kind,
        dshtui::model::TrajKind::ToolCall
    );
    // 切回 Center 焦点（Ctrl+w 三向）再下移光标到 tool/result 行。
    app.handle_command(Command::CycleFocus); // Details -> Sidebar
    app.handle_command(Command::CycleFocus); // Sidebar -> Center
    assert_eq!(app.focus, dshtui::app::Focus::Center);
    app.handle_command(Command::MoveDown); // 到 tool/result (view idx 4)
    assert_eq!(app.traj.cursor, 4);
    // 详情应刷新为 tool/result（非旧 tool/call 详情滞留）。
    let detail = app.traj.detail.as_ref().expect("详情仍开");
    assert_eq!(
        detail.source_kind,
        dshtui::model::TrajKind::ToolResult,
        "详情随新选中行重建"
    );
    assert_eq!(detail.source_seq, dshtui::api::types::SessionSeq(5));
    // 移到不可详查行（step/end idx 5）→ 详情关闭回列表。
    app.handle_command(Command::MoveDown);
    assert!(!app.traj.detail_open, "不可详查行关闭详情");
    assert_eq!(app.focus, dshtui::app::Focus::Center);
}

#[test]
fn trajectory_search_index_updates_on_stream_append_ac005_05() {
    // Spec 审查 (a)2 修复：轨迹搜索索引随事件流窗口变化重建——过滤中输入时
    // 新到事件立即进命中（AC-005-05「过滤即时」）。
    let mut app = traj_app();
    app.handle_command(Command::ToggleTrajectory);
    app.handle_command(Command::StartSearch);
    app.handle_command(Command::PickerInput("done".to_string()));
    assert_eq!(app.traj_search_index.query("done").len(), 1, "既有命中");
    // 流式 append：新 tool/result（message 含 "done"）。
    let sid = SessionId("sess-traj".into());
    app.handle(AppEvent::FollowEvent {
        session_id: sid.clone(),
        event: dshtui::api::types::SessionWireEvent {
            event_type: "tool/result".into(),
            seq: Some(dshtui::api::types::SessionSeq(8)),
            time: Some(8),
            request_id: None,
            ignorable: None,
            source_event_seqs: None,
            surface_op: None,
            data: Some(serde_json::json!({"callId": "c2", "message": "done again"})),
        },
    });
    // 新行立即进过滤命中（window_changed → traj_index_rebuild）。
    let hits = app.traj_search_index.query("done");
    assert_eq!(hits.len(), 2, "新到事件立即进命中: {hits:?}");
}

#[test]
fn trajectory_filter_decoder_special_keys_ac005_05_13() {
    // 真实 KeyDecoder 路径（code-review S1 修复证据）：特殊键必须不被
    // 兜底 `Char(c)` 遮蔽——q/Esc 退出过滤、j/k 命中选中移动、Backspace
    // 删除、Enter 跳转；普通字符进查询词。此前实现首个臂吞掉 q/j/k。
    let mut decoder = KeyDecoder::new();
    let dec = |d: &mut KeyDecoder, code| d.decode(InputMode::TrajectoryFilter, key(code));

    assert_eq!(
        dec(&mut decoder, KeyCode::Char('x')),
        Some(Command::PickerInput("x".into()))
    );
    assert_eq!(
        dec(&mut decoder, KeyCode::Char('j')),
        Some(Command::MoveDown),
        "j 命中选中下移（不被当查询字符）"
    );
    assert_eq!(
        dec(&mut decoder, KeyCode::Char('k')),
        Some(Command::MoveUp),
        "k 命中选中上移"
    );
    assert_eq!(
        dec(&mut decoder, KeyCode::Char('q')),
        Some(Command::ClosePicker),
        "q 退出过滤（不退出程序，AC-005-11 模态）"
    );
    assert_eq!(dec(&mut decoder, KeyCode::Esc), Some(Command::ClosePicker));
    assert_eq!(
        dec(&mut decoder, KeyCode::Backspace),
        Some(Command::PickerBackspace)
    );
    assert_eq!(
        dec(&mut decoder, KeyCode::Enter),
        Some(Command::PickerConfirm)
    );
}

#[test]
fn trajectory_pending_g_cleared_on_non_prefix_keys_ac005_01() {
    // code-review S8 修复证据：Enter 开详情后按 t 不得触发 gt 切回 Chat。
    let mut decoder = KeyDecoder::new();
    assert_eq!(
        decoder.decode(InputMode::Trajectory, key(KeyCode::Char('g'))),
        None,
        "g 前缀置位"
    );
    // g 后按 Enter（非前缀键）→ 打开详情并清前缀。
    assert_eq!(
        decoder.decode(InputMode::Trajectory, key(KeyCode::Enter)),
        Some(Command::OpenDetail)
    );
    // 随后按 t 必须不是 ToggleTrajectory（pending_g 已清）。
    assert_eq!(
        decoder.decode(InputMode::Trajectory, key(KeyCode::Char('t'))),
        None,
        "详情打开后 t 不再触发 gt"
    );
    // Esc 路径同样清前缀。
    decoder.decode(InputMode::Trajectory, key(KeyCode::Char('g')));
    assert_eq!(
        decoder.decode(InputMode::Trajectory, key(KeyCode::Esc)),
        Some(Command::ClosePicker)
    );
    assert_eq!(
        decoder.decode(InputMode::Trajectory, key(KeyCode::Char('t'))),
        None,
        "Esc 后 t 不再触发 gt"
    );
}

#[test]
fn trajectory_gg_requests_traj_page_without_touching_chat_viewport_ac005_07() {
    // code-review S2 修复证据：轨迹 gg 触发轨迹自己的历史分页 seam，
    // 不再落全局 scroll 改隐藏 Chat 视口（AC-005-07 loadOlder 独立触发）。
    let mut app = traj_app();
    app.handle_command(Command::ToggleTrajectory);
    let vp_before = app.viewport.clone();
    let cmds = app.handle_command(Command::GotoTop);
    assert!(
        matches!(cmds.as_slice(), [dshtui::app::Cmd::RequestPage { .. }]),
        "轨迹 head_has_more + Ready → 发历史分页: {cmds:?}"
    );
    assert_eq!(app.viewport, vp_before, "gg 不改隐藏 Chat 视口");
    assert_eq!(app.traj.cursor, 0, "轨迹光标到顶");
}

#[test]
fn trajectory_goto_bottom_jumps_latest_and_no_page_ac005_07() {
    let mut app = traj_app();
    app.handle_command(Command::ToggleTrajectory);
    app.handle_command(Command::GotoTop); // 光标到顶
    let cmds = app.handle_command(Command::GotoBottom);
    assert!(
        !cmds
            .iter()
            .any(|c| matches!(c, dshtui::app::Cmd::RequestPage { .. })),
        "G 跳底不触发分页"
    );
    let len = app
        .traj_sessions
        .get("sess-traj")
        .unwrap()
        .view(&app.traj.fold)
        .len();
    assert_eq!(app.traj.cursor, len - 1, "光标跳列表底（最新）");
}

#[cfg(test)]
mod monitor_keymap_tests {
    use super::*;
    use dshtui::input::{Command, InputMode, KeyDecoder};

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn monitor_mode_maps_vim_style_keys() {
        let mut d = KeyDecoder::new();
        assert_eq!(
            d.decode(InputMode::Monitor, key(KeyCode::Char('j'))),
            Some(Command::MoveDown)
        );
        assert_eq!(
            d.decode(InputMode::Monitor, key(KeyCode::Char('k'))),
            Some(Command::MoveUp)
        );
        assert_eq!(
            d.decode(InputMode::Monitor, key(KeyCode::Char('G'))),
            Some(Command::GotoBottom)
        );
        assert_eq!(
            d.decode(InputMode::Monitor, key(KeyCode::Char('c'))),
            Some(Command::MonitorOpenChat)
        );
        assert_eq!(
            d.decode(InputMode::Monitor, key(KeyCode::Char('f'))),
            Some(Command::MonitorCheer)
        );
        assert_eq!(
            d.decode(InputMode::Monitor, key(KeyCode::Char('l'))),
            Some(Command::MonitorLocate)
        );
        assert_eq!(
            d.decode(InputMode::Monitor, key(KeyCode::Char('s'))),
            Some(Command::MonitorStats)
        );
        assert_eq!(
            d.decode(InputMode::Monitor, key(KeyCode::Char('/'))),
            Some(Command::StartSearch)
        );
        assert_eq!(
            d.decode(InputMode::Monitor, key(KeyCode::Char('?'))),
            Some(Command::OpenHelp)
        );
        assert_eq!(
            d.decode(InputMode::Monitor, key(KeyCode::Char('q'))),
            Some(Command::Quit)
        );
        assert_eq!(
            d.decode(InputMode::Monitor, key(KeyCode::Enter)),
            Some(Command::OpenFocused)
        );
        assert_eq!(
            d.decode(InputMode::Monitor, key(KeyCode::Esc)),
            Some(Command::ClosePicker)
        );
        // 未映射键不产生命令（不误触）。
        assert_eq!(d.decode(InputMode::Monitor, key(KeyCode::Char('z'))), None);
    }

    #[test]
    fn monitor_gg_is_two_key_goto_top() {
        let mut d = KeyDecoder::new();
        assert_eq!(d.decode(InputMode::Monitor, key(KeyCode::Char('g'))), None);
        assert_eq!(
            d.decode(InputMode::Monitor, key(KeyCode::Char('g'))),
            Some(Command::GotoTop)
        );
    }

    #[test]
    fn monitor_ctrl_c_quits_like_global_path() {
        let mut d = KeyDecoder::new();
        let ctrl_c = Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(d.decode(InputMode::Monitor, ctrl_c), Some(Command::Quit));
    }

    #[test]
    fn chat_app_ignores_monitor_commands() {
        // 主界面 AppState 对 monitor 键位 no-op（两套状态机共存）。
        let mut app = image_app(false);
        for cmd in [
            Command::MonitorOpenChat,
            Command::MonitorStats,
            Command::MonitorCheer,
            Command::MonitorLocate,
        ] {
            let cmds = app.handle_command(cmd);
            assert!(cmds.is_empty(), "Chat 上下文应忽略 monitor 命令");
        }
    }
}

// ---------- REQ-006 Sidebar 行光标与 gv（FR-006-02 / D-034） ----------

fn srec(seq: u64) -> dshtui::api::types::SessionHistoryRecord {
    dshtui::api::types::SessionHistoryRecord::Event {
        event: dshtui::api::types::SessionWireEvent {
            event_type: "user/message".into(),
            seq: Some(dshtui::api::types::SessionSeq(seq)),
            time: None,
            request_id: None,
            ignorable: None,
            source_event_seqs: None,
            surface_op: None,
            data: Some(serde_json::json!({"content": format!("row {seq}")})),
        },
    }
}

fn sidebar_app() -> AppState {
    use dshtui::api::types::SessionMeta;
    use dshtui::api::types::{SessionId, WorkspaceId};
    let mut app = AppState::new(20);
    app.mode = Mode::Normal;
    app.focus = dshtui::app::Focus::Sidebar;
    let meta = |id: &str, updated: i64, ws: Option<&str>| SessionMeta {
        id: SessionId(id.into()),
        title: Some(format!("S-{id}")),
        cwd: None,
        updated_at_ms: updated,
        running: false,
        blank: false,
        origin: None,
        parent_id: None,
        workspace: ws.map(|w| WorkspaceId(w.into())),
        last_turn_preview: None,
    };
    app.workspaces.upsert_session(meta("s1", 300, None));
    app.workspaces.upsert_session(meta("s2", 200, None));
    app.workspaces.upsert_session(meta("s3", 100, None));
    app
}

#[test]
fn sidebar_focus_jk_moves_cursor_without_chat_scroll_ac006_03() {
    let mut app = sidebar_app();
    app.focus = dshtui::app::Focus::Sidebar;
    // 给 Center 一个窗口：j/k 若误触发 Chat 滚动会改变 viewport。
    app.active_session = Some(SessionId("s1".into()));
    app.sessions
        .touch("s1", 20)
        .apply(dshtui::model::Incoming::Snapshot {
            cursor: None,
            records: vec![srec(1), srec(2), srec(3)],
            has_more: false,
            projections: Some(serde_json::json!({"running": false})),
        });
    let vp_before = app.viewport.clone();
    // Sidebar 焦点：j 移动行光标。
    app.handle_command(Command::MoveDown);
    assert_eq!(app.sidebar.cursor, 1, "j 移动光标到第 2 行");
    app.handle_command(Command::MoveDown);
    assert_eq!(app.sidebar.cursor, 2);
    app.handle_command(Command::MoveDown);
    assert_eq!(app.sidebar.cursor, 2, "末尾钳制");
    app.handle_command(Command::MoveUp);
    assert_eq!(app.sidebar.cursor, 1, "k 上移");
    assert_eq!(app.viewport, vp_before, "Sidebar 焦点 j/k 不触发 Chat 滚动");
}

#[test]
fn center_focus_jk_still_scrolls_chat_ac006_03_regression() {
    // 回归：Center/Details 焦点下 j/k 仍滚动 Chat（既有语义）。
    let mut app = sidebar_app();
    app.focus = dshtui::app::Focus::Center;
    app.active_session = Some(SessionId("s1".into()));
    app.sessions
        .touch("s1", 20)
        .apply(dshtui::model::Incoming::Snapshot {
            cursor: None,
            records: (1..=30).map(srec).collect(),
            has_more: false,
            projections: Some(serde_json::json!({"running": false})),
        });
    app.handle_command(Command::MoveDown);
    assert_eq!(app.sidebar.cursor, 0, "Center 焦点不改侧栏光标");
    assert!(
        app.viewport.offset > 0 || !app.viewport.follow_tail,
        "Center 焦点 j/k 仍滚动 Chat"
    );
}

#[test]
fn sidebar_enter_opens_cursor_session_row_ac006_02() {
    let mut app = sidebar_app();
    app.active_session = Some(SessionId("s1".into()));
    // 光标移到 s3 → Enter 打开 s3。
    app.sidebar.cursor = 2;
    app.handle_command(Command::OpenFocused);
    assert_eq!(
        app.active_session,
        Some(SessionId("s3".into())),
        "Enter 打开光标行会话"
    );
}

#[test]
fn sidebar_enter_on_workspace_header_toggles_collapse_ac006_03() {
    use dshtui::api::types::WorkspaceId;
    let mut app = AppState::new(20);
    app.mode = Mode::Normal;
    app.focus = dshtui::app::Focus::Sidebar;
    app.workspaces
        .upsert_workspace(WorkspaceId("ws1".into()), Some("项目A".into()));
    app.workspaces
        .upsert_session(dshtui::api::types::SessionMeta {
            id: SessionId("s1".into()),
            title: Some("S-s1".into()),
            cwd: None,
            updated_at_ms: 1,
            running: false,
            blank: false,
            origin: None,
            parent_id: None,
            workspace: Some(WorkspaceId("ws1".into())),
            last_turn_preview: None,
        });
    app.workspaces
        .attach_session_to_workspace(&WorkspaceId("ws1".into()), &SessionId("s1".into()));
    // 光标在第 0 行 = workspace header。
    app.sidebar.cursor = 0;
    app.handle_command(Command::OpenFocused);
    assert!(
        app.sidebar_view.is_collapsed(&WorkspaceId("ws1".into())),
        "Enter 折叠 workspace header"
    );
    assert_eq!(app.sidebar.cursor, 0);
    app.handle_command(Command::OpenFocused);
    assert!(
        !app.sidebar_view.is_collapsed(&WorkspaceId("ws1".into())),
        "再次 Enter 展开"
    );
}

#[test]
fn gv_cycles_local_view_without_write_cmd_ac006_11() {
    // AC-006-11：`gv` 仅更新本地视图态，reducer 不发任何写 Cmd（无远端写）。
    let mut app = sidebar_app();
    assert_eq!(app.sidebar_view.group_by, dshtui::model::GroupBy::Workspace);
    assert_eq!(app.sidebar_view.order_by, dshtui::model::OrderBy::Updated);
    let cmds = app.handle_command(Command::CycleSidebarView);
    assert!(cmds.is_empty(), "gv 不发写命令, cmds={cmds:?}");
    assert_eq!(app.sidebar_view.order_by, dshtui::model::OrderBy::Manual);
    let notice = app.notice.as_deref().unwrap();
    assert!(notice.contains("group=workspace"), "notice={notice}");
    assert!(notice.contains("order=manual"), "notice={notice}");
    app.handle_command(Command::CycleSidebarView);
    assert_eq!(app.sidebar_view.group_by, dshtui::model::GroupBy::Flat);
}

#[test]
fn h_l_collapse_expand_all_workspaces_ac006_03() {
    use dshtui::api::types::WorkspaceId;
    let mut app = AppState::new(20);
    app.workspaces
        .upsert_workspace(WorkspaceId("w1".into()), Some("A".into()));
    app.workspaces
        .upsert_workspace(WorkspaceId("w2".into()), Some("B".into()));
    app.handle_command(Command::CollapseProject); // h
    assert!(app.sidebar_view.is_collapsed(&WorkspaceId("w1".into())));
    assert!(app.sidebar_view.is_collapsed(&WorkspaceId("w2".into())));
    app.handle_command(Command::ExpandProject); // l
    assert!(!app.sidebar_view.is_collapsed(&WorkspaceId("w1".into())));
    assert!(!app.sidebar_view.is_collapsed(&WorkspaceId("w2".into())));
}

#[test]
fn gv_two_key_decodes_cycle_sidebar_view_ac006_11() {
    let mut d = KeyDecoder::new();
    assert_eq!(d.decode(InputMode::Normal, key(KeyCode::Char('g'))), None);
    assert_eq!(
        d.decode(InputMode::Normal, key(KeyCode::Char('v'))),
        Some(Command::CycleSidebarView),
        "gv = CycleSidebarView（g 前缀扩展）"
    );
    // g 前缀清空后普通单键语义不回归。
    assert_eq!(d.decode(InputMode::Normal, key(KeyCode::Char('g'))), None);
    assert_eq!(
        d.decode(InputMode::Normal, key(KeyCode::Char('t'))),
        Some(Command::ToggleTrajectory),
        "gt 仍切 Trajectory"
    );
}
