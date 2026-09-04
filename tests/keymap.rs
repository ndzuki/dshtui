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
