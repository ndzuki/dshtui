use dshtui::api::types::{
    SessionHistoryRecord, SessionLogOffset, SessionRequestId, SessionSeq, SessionWireEvent,
};
use dshtui::model::{ApplyEffect, Incoming, TranscriptWindow};

fn event(seq: u64, event_type: &str, request_id: Option<&str>) -> SessionWireEvent {
    SessionWireEvent {
        event_type: event_type.to_string(),
        seq: Some(SessionSeq(seq)),
        time: None,
        request_id: request_id.map(str::to_string),
        ignorable: None,
        source_event_seqs: None,
        surface_op: None,
        data: None,
    }
}

fn record(seq: u64, event_type: &str, request_id: Option<&str>) -> SessionHistoryRecord {
    SessionHistoryRecord::Event {
        event: event(seq, event_type, request_id),
    }
}

fn seqs(window: &TranscriptWindow) -> Vec<u64> {
    window.blocks().map(|block| block.seq().0).collect()
}

#[test]
fn seq_order_request_id_dedup_and_page_merge_share_one_seam() {
    let mut window = TranscriptWindow::new(20);
    assert_eq!(
        window.apply(Incoming::Snapshot {
            cursor: Some(SessionLogOffset(30)),
            records: vec![
                record(20, "user/message", None),
                record(10, "user/message", None)
            ],
            has_more: true,
            projections: None,
        }),
        ApplyEffect::Rebuilt
    );
    assert_eq!(seqs(&window), vec![10, 20]);

    assert_eq!(
        window.apply(Incoming::FollowEvent(event(
            30,
            "assistant/message",
            Some("request-1"),
        ))),
        ApplyEffect::TailAppended {
            appended: 1,
            anchor_stable: true,
        }
    );

    // The request id is the idempotency key even when a replay carries a new seq.
    assert_eq!(
        window.apply(Incoming::FollowEvent(event(
            31,
            "assistant/message",
            Some("request-1"),
        ))),
        ApplyEffect::Noop
    );
    assert_eq!(seqs(&window), vec![10, 20, 30]);

    // A late event is inserted into sequence order rather than blindly appended.
    assert_eq!(
        window.apply(Incoming::FollowEvent(event(15, "user/message", None))),
        ApplyEffect::TailAppended {
            appended: 1,
            anchor_stable: true,
        }
    );
    assert_eq!(seqs(&window), vec![10, 15, 20, 30]);

    // Page overlap is discarded, while genuinely older records shift the head anchor.
    assert_eq!(
        window.apply(Incoming::Page {
            records: vec![
                record(5, "user/message", None),
                record(10, "user/message", None),
                record(30, "assistant/message", Some("request-1")),
            ],
            has_more: Some(false),
        }),
        ApplyEffect::HeadPrepend {
            inserted: 1,
            anchor_shift: 1,
        }
    );
    assert_eq!(seqs(&window), vec![5, 10, 15, 20, 30]);
    assert!(!window.head_has_more());
    assert_eq!(window.head_seq(), Some(SessionSeq(5)));

    assert_eq!(
        window.apply(Incoming::Page {
            records: vec![record(5, "user/message", None)],
            has_more: None,
        }),
        ApplyEffect::Noop
    );
}

#[test]
fn snapshot_rebuild_replaces_sequence_and_request_indexes() {
    let mut window = TranscriptWindow::new(10);
    window.apply(Incoming::FollowEvent(event(
        9,
        "assistant/message",
        Some("old-request"),
    )));

    assert_eq!(
        window.apply(Incoming::Snapshot {
            cursor: None,
            records: vec![record(2, "user/message", None)],
            has_more: false,
            projections: Some(serde_json::json!({
                "turnOutline": [{"turn": 1, "seq": 2, "prompt": "hi"}]
            })),
        }),
        ApplyEffect::Rebuilt
    );
    assert_eq!(seqs(&window), vec![2]);
    assert_eq!(window.turn_outline().len(), 1);
    assert_eq!(window.turn_outline()[0].seq, Some(SessionSeq(2)));

    // Rebuild clears both indexes, so the old seq/request id can be accepted again.
    assert_eq!(
        window.apply(Incoming::FollowEvent(event(
            9,
            "assistant/message",
            Some("old-request"),
        ))),
        ApplyEffect::TailAppended {
            appended: 1,
            anchor_stable: true,
        }
    );
    assert_eq!(seqs(&window), vec![2, 9]);
}

#[test]
fn optimistic_echo_reconciles_against_durable_event_and_snapshot_ac002_06() {
    // 集成 seam：echo 后 durable 事件与重连快照都能对账，不重复显示。
    let mut window = TranscriptWindow::new(20);
    window.echo(SessionRequestId("req-x".into()), "echoed");
    assert_eq!(window.pending().count(), 1);
    assert_eq!(window.len(), 0, "pending 不占 blocks");

    assert_eq!(
        window.apply(Incoming::FollowEvent(event(
            5,
            "user/message",
            Some("req-x"),
        ))),
        ApplyEffect::TailAppended {
            appended: 1,
            anchor_stable: true,
        }
    );
    assert_eq!(window.pending().count(), 0, "durable 对账 retire");
    assert_eq!(seqs(&window), vec![5]);

    // 重连快照重放同 requestId：重建后仍只有一条。
    window.echo(SessionRequestId("req-y".into()), "again");
    window.apply(Incoming::Snapshot {
        cursor: Some(SessionLogOffset(6)),
        records: vec![record(6, "user/message", Some("req-y"))],
        has_more: false,
        projections: None,
    });
    assert_eq!(window.pending().count(), 0, "快照对账 retire");
    assert_eq!(seqs(&window), vec![6]);
    assert_eq!(
        window.apply(Incoming::Snapshot {
            cursor: Some(SessionLogOffset(6)),
            records: vec![record(6, "user/message", Some("req-y"))],
            has_more: false,
            projections: None,
        }),
        ApplyEffect::Rebuilt
    );
    assert_eq!(seqs(&window), vec![6], "快照重放不重复");
}

// ---------- REQ-004 V0.2 图片模型（Step 2） ----------

#[test]
fn nested_image_reference_is_found_through_children() {
    let mut window = TranscriptWindow::new(20);
    window.apply(Incoming::FollowEvent(SessionWireEvent {
        event_type: "assistant/message".into(),
        seq: Some(SessionSeq(1)),
        time: None,
        request_id: None,
        ignorable: None,
        source_event_seqs: None,
        surface_op: None,
        data: Some(serde_json::json!({
            "content": [{
                "type": "text",
                "children": [{
                    "kind": "attachment",
                    "attachmentId": "nested-att",
                    "name": "nested.png",
                    "width": 12,
                    "height": 8
                }]
            }]
        })),
    }));
    let blocks: Vec<_> = window.blocks().collect();
    assert_eq!(blocks.len(), 2, "host message + one image: {blocks:?}");
    assert!(
        matches!(blocks[0], dshtui::model::Block::AssistantMessage { .. }),
        "host block preserved: {blocks:?}"
    );
    match &blocks[1] {
        dshtui::model::Block::Image {
            attachment_id,
            name,
            ..
        } => {
            assert_eq!(attachment_id.as_deref(), Some("nested-att"));
            assert_eq!(name.as_deref(), Some("nested.png"));
        }
        other => panic!("expected nested image block, got {other:?}"),
    };
}

#[test]
fn same_event_multiple_images_expand_to_blocks_in_wire_order() {
    // AC-004-01: 同消息多图逐块独立占位；宿主消息文本不丢。
    let mut window = TranscriptWindow::new(20);
    let eff = window.apply(Incoming::FollowEvent(SessionWireEvent {
        event_type: "user/message".into(),
        seq: Some(SessionSeq(7)),
        time: None,
        request_id: None,
        ignorable: None,
        source_event_seqs: None,
        surface_op: None,
        data: Some(serde_json::json!({
            "content": [
                {"type": "text", "text": "look at these"},
                {"type": "image", "attachmentId": "img-1", "name": "one.png", "dims": "1x1"},
                {"type": "image", "attachmentId": "img-2", "name": "two.png", "width": 2, "height": 3}
            ]
        })),
    }));
    assert_eq!(
        eff,
        dshtui::model::ApplyEffect::TailAppended {
            appended: 3,
            anchor_stable: true,
        },
        "one event expands to three blocks"
    );
    let blocks: Vec<_> = window.blocks().collect();
    assert_eq!(blocks.len(), 3, "{blocks:?}");
    match &blocks[0] {
        dshtui::model::Block::UserMessage { content, .. } => {
            assert_eq!(content, "look at these", "宿主文本不丢");
        }
        other => panic!("expected host user message, got {other:?}"),
    }
    match (&blocks[1], &blocks[2]) {
        (
            dshtui::model::Block::Image {
                attachment_id: a,
                name: n,
                ..
            },
            dshtui::model::Block::Image {
                attachment_id: b,
                name: m,
                dims,
                ..
            },
        ) => {
            assert_eq!(a.as_deref(), Some("img-1"));
            assert_eq!(n.as_deref(), Some("one.png"));
            assert_eq!(b.as_deref(), Some("img-2"));
            assert_eq!(m.as_deref(), Some("two.png"));
            assert_eq!(dims.as_deref(), Some("2x3"), "width/height fallback dims");
        }
        other => panic!("expected two image blocks, got {other:?}"),
    }
    assert_eq!(window.len(), 3);
}

#[test]
fn media_type_whitelist_only_allows_png_jpeg_webp_gif() {
    use dshtui::model::image::is_supported_image;
    for ok in ["image/png", "image/jpeg", "image/webp", "image/gif"] {
        assert!(is_supported_image(ok), "{ok} must be whitelisted");
    }
    for bad in [
        "image/svg+xml",
        "video/mp4",
        "application/octet-stream",
        "",
        "image/bmp",
    ] {
        assert!(!is_supported_image(bad), "{bad} must be rejected");
    }
}

#[test]
fn attachment_ref_maps_api_payload_to_internal_snake_case() {
    use dshtui::api::attachment::parse_response;
    use dshtui::model::image::AttachmentRef;
    let data = parse_response(&serde_json::json!({
        "attachment": {
            "attachmentId": "att-1",
            "mediaType": "image/png",
            "bytes": 1024,
            "width": 640,
            "height": 480,
            "name": "design.png",
            "originalDimensions": {"width": 1280, "height": 960}
        },
        "data": "AQ=="
    }))
    .unwrap();
    let r: AttachmentRef = AttachmentRef::from(&data);
    assert_eq!(r.attachment_id.0, "att-1");
    assert_eq!(r.media_type.0, "image/png");
    assert_eq!(r.bytes, 1024);
    assert_eq!((r.width, r.height), (640, 480));
    assert_eq!(r.name.as_deref(), Some("design.png"));
    assert_eq!(
        r.original_dimensions.map(|d| (d.width, d.height)),
        Some((1280, 960))
    );

    // 可选字段缺失不崩（官方只剥离路径，name/originalDimensions 可缺）。
    let minimal = parse_response(&serde_json::json!({
        "attachment": {"attachmentId": "att-2", "mediaType": "image/gif"},
        "data": ""
    }))
    .unwrap();
    let minimal = AttachmentRef::from(&minimal);
    assert!(minimal.name.is_none());
    assert!(minimal.original_dimensions.is_none());
    assert!(minimal.width == 0 && minimal.height == 0);
}

#[test]
fn image_view_state_transitions_loading_rendered_failed_closed() {
    use dshtui::api::types::{AttachmentId, SessionSeq};
    use dshtui::model::image::{ImageViewPhase, ImageViewState};

    let mut state = ImageViewState::default();
    assert!(!state.open);

    // 打开 → Loading，锚点记录来源 Block.seq（防串图）。
    state.open_view(
        SessionSeq(42),
        AttachmentId("att-1".into()),
        Some("a.png".into()),
        Some("10x20".into()),
    );
    assert!(state.open);
    assert_eq!(state.phase, ImageViewPhase::Loading);
    assert_eq!(state.block_seq, Some(SessionSeq(42)));
    assert_eq!(
        state.attachment_id.as_ref().map(|a| a.0.as_str()),
        Some("att-1")
    );
    assert_eq!(state.name.as_deref(), Some("a.png"));
    assert!(state.error.is_none());

    // Loading → Rendered。
    state.mark_rendered();
    assert_eq!(state.phase, ImageViewPhase::Rendered);

    // 重新打开（幂等锚点路径）先回 Loading 再失败：失败带 code/message。
    state.open_view(SessionSeq(43), AttachmentId("att-2".into()), None, None);
    state.mark_failed("decode/unsupported".into(), "格式不支持".into());
    assert_eq!(state.phase, ImageViewPhase::Failed);
    let err = state.error.as_ref().unwrap();
    assert_eq!(
        (err.code.as_str(), err.message.as_str()),
        ("decode/unsupported", "格式不支持")
    );

    // 关闭 → open=false、清空展示上下文。
    state.close();
    assert!(!state.open);
    assert_eq!(state.phase, ImageViewPhase::Closed);
    assert!(state.error.is_none());
}

#[test]
fn image_block_identity_is_extracted_from_block_image_only() {
    use dshtui::api::types::SessionSeq;
    use dshtui::model::image::image_block_of;
    use dshtui::model::Block;
    let image = Block::Image {
        seq: SessionSeq(7),
        attachment_id: Some("att-7".into()),
        name: Some("x.png".into()),
        dims: Some("12x34".into()),
    };
    let r = image_block_of(&image).expect("image block identity");
    assert_eq!(r.seq, SessionSeq(7));
    assert_eq!(
        r.attachment_id.as_ref().map(|a| a.0.as_str()),
        Some("att-7")
    );
    assert_eq!(r.name.as_deref(), Some("x.png"));
    assert_eq!(r.dims.as_deref(), Some("12x34"));

    assert!(image_block_of(&Block::UserMessage {
        seq: SessionSeq(8),
        content: "hi".into(),
        time: None,
    })
    .is_none());
}
