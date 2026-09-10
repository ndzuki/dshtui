use dshtui::api::types::{
    SessionHistoryRecord, SessionLogOffset, SessionRequestId, SessionSeq, SessionWireEvent,
};
use dshtui::model::{ApplyEffect, Incoming, TranscriptWindow};

fn event(seq: u64, event_type: &str, request_id: Option<&str>) -> SessionWireEvent {
    SessionWireEvent {
        event_type: event_type.to_string(),
        seq: Some(SessionSeq::new(seq)),
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
    window.blocks().map(|block| block.seq().get()).collect()
}

#[test]
fn seq_order_request_id_dedup_and_page_merge_share_one_seam() {
    let mut window = TranscriptWindow::new(20);
    assert_eq!(
        window.apply(Incoming::Snapshot {
            cursor: Some(SessionLogOffset::new(30)),
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
    assert_eq!(window.head_seq(), Some(SessionSeq::new(5)));

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
    assert_eq!(window.turn_outline()[0].seq, Some(SessionSeq::new(2)));

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
    window.echo(SessionRequestId::new("req-x".into()), "echoed");
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
    window.echo(SessionRequestId::new("req-y".into()), "again");
    window.apply(Incoming::Snapshot {
        cursor: Some(SessionLogOffset::new(6)),
        records: vec![record(6, "user/message", Some("req-y"))],
        has_more: false,
        projections: None,
    });
    assert_eq!(window.pending().count(), 0, "快照对账 retire");
    assert_eq!(seqs(&window), vec![6]);
    assert_eq!(
        window.apply(Incoming::Snapshot {
            cursor: Some(SessionLogOffset::new(6)),
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
        seq: Some(SessionSeq::new(1)),
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
        seq: Some(SessionSeq::new(7)),
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
    assert_eq!(r.attachment_id.get(), "att-1");
    assert_eq!(r.media_type.get(), "image/png");
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
        SessionSeq::new(42),
        AttachmentId::new("att-1".into()),
        Some("a.png".into()),
        Some("10x20".into()),
    );
    assert!(state.open);
    assert_eq!(state.phase, ImageViewPhase::Loading);
    assert_eq!(state.block_seq, Some(SessionSeq::new(42)));
    assert_eq!(
        state.attachment_id.as_ref().map(|a| a.get()).as_deref(),
        Some("att-1")
    );
    assert_eq!(state.name.as_deref(), Some("a.png"));
    assert!(state.error.is_none());

    // Loading → Rendered。
    state.mark_rendered();
    assert_eq!(state.phase, ImageViewPhase::Rendered);

    // 重新打开（幂等锚点路径）先回 Loading 再失败：失败带 code/message。
    state.open_view(
        SessionSeq::new(43),
        AttachmentId::new("att-2".into()),
        None,
        None,
    );
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
        seq: SessionSeq::new(7),
        attachment_id: Some("att-7".into()),
        name: Some("x.png".into()),
        dims: Some("12x34".into()),
    };
    let r = image_block_of(&image).expect("image block identity");
    assert_eq!(r.seq, SessionSeq::new(7));
    assert_eq!(
        r.attachment_id.as_ref().map(|a| a.get()).as_deref(),
        Some("att-7")
    );
    assert_eq!(r.name.as_deref(), Some("x.png"));
    assert_eq!(r.dims.as_deref(), Some("12x34"));

    assert!(image_block_of(&Block::UserMessage {
        seq: SessionSeq::new(8),
        content: "hi".into(),
        time: None,
    })
    .is_none());
}

// ============================================================================
// REQ-005 V0.3 轨迹投影（TASK-005 Step 1）—— Seam = TrajectoryWindow 公共方法
// （apply/view/toggle_group/row/group_of/raw_rows），纯内存无 IO（计划 Step 1
// 测试 Seam 行）。验收：AC-005-02/06/07/08/12。
// ============================================================================

use dshtui::api::types::ChunkRow;
use dshtui::model::trajectory::{
    FoldState, GroupId, RowId, TrajEffect, TrajIncoming, TrajKind, TrajectoryStore,
    TrajectoryWindow,
};

/// 轨迹行事件构造（含官方 wire 字段实读映射：callId/arguments/message/error/
/// meta/usage/request-header reason，REQ-005 FR-005-02/03 事实修正）。
fn traj_event(seq: u64, event_type: &str, data: serde_json::Value) -> SessionWireEvent {
    SessionWireEvent {
        event_type: event_type.to_string(),
        seq: Some(SessionSeq::new(seq)),
        time: Some(1_700_000_000_000 + seq as i64),
        request_id: None,
        ignorable: None,
        source_event_seqs: None,
        surface_op: None,
        data: Some(data),
    }
}

fn traj_record(seq: u64, event_type: &str, data: serde_json::Value) -> SessionHistoryRecord {
    SessionHistoryRecord::Event {
        event: traj_event(seq, event_type, data),
    }
}

/// 27 turn / 1144 step 合成轨迹 fixture（每 turn 一轮 user→step→assistant→
/// tool/call→tool/result，边界事件齐全；复用 REQ-001 §8 27turn 口径，D-26）。
fn synth_trajectory(turns: u64, steps: usize) -> Vec<SessionHistoryRecord> {
    let mut seq = 1u64;
    let mut out = Vec::with_capacity(steps * 6 + turns as usize * 2);
    let mut step_done = 0usize;
    for turn in 1..=turns {
        out.push(traj_record(
            seq,
            "turn/start",
            serde_json::json!({"turn": turn, "reason": "user-prompt"}),
        ));
        seq += 1;
        let mut step = 1u64;
        while step_done < steps {
            out.push(traj_record(
                seq,
                "user/message",
                serde_json::json!({"turn": turn, "step": step, "content": format!("user {turn}-{step}")}),
            ));
            seq += 1;
            out.push(traj_record(
                seq,
                "step/start",
                serde_json::json!({"turn": turn, "step": step, "reason": "max"}),
            ));
            seq += 1;
            out.push(traj_record(
                seq,
                "assistant/message",
                serde_json::json!({
                    "turn": turn, "step": step,
                    "content": format!("assistant {turn}-{step}"),
                    "usage": {"input": 10, "output": 20, "cacheRead": 5, "cacheWrite": 3, "think": 7},
                }),
            ));
            seq += 1;
            out.push(traj_record(
                seq,
                "tool/call",
                serde_json::json!({"turn": turn, "step": step, "callId": format!("c{turn}-{step}"), "name": "bash", "arguments": "{\"command\":\"ls\"}"}),
            ));
            seq += 1;
            out.push(traj_record(
                seq,
                "tool/result",
                serde_json::json!({"turn": turn, "step": step, "callId": format!("c{turn}-{step}"), "message": "ok", "meta": {"diff": "x"}}),
            ));
            seq += 1;
            out.push(traj_record(
                seq,
                "step/end",
                serde_json::json!({"turn": turn, "step": step}),
            ));
            seq += 1;
            step += 1;
            step_done += 1;
        }
        out.push(traj_record(
            seq,
            "turn/end",
            serde_json::json!({"turn": turn, "reason": "stop"}),
        ));
        seq += 1;
    }
    out
}

#[test]
fn trajectory_snapshot_projects_boundary_rows_and_chunks_ac005_06() {
    // AC-005-06：TrajectoryWindow 独立投影保留边界行（step/start、step/end、
    // turn/start、turn/end、request/header）+ packed chunk rows，无逐 delta
    // 展开（D-23）。
    let mut w = TrajectoryWindow::new(200);
    let records = vec![
        traj_record(
            1,
            "request/header",
            serde_json::json!({"reason": "initial"}),
        ),
        traj_record(2, "turn/start", serde_json::json!({"turn": 1})),
        traj_record(
            3,
            "user/message",
            serde_json::json!({"turn": 1, "step": 1, "content": "部署排查"}),
        ),
        traj_record(
            4,
            "step/start",
            serde_json::json!({"turn": 1, "step": 1, "reason": "max"}),
        ),
        traj_record(
            5,
            "assistant/message",
            serde_json::json!({"turn": 1, "step": 1}),
        ),
        SessionHistoryRecord::Chunks {
            event: ChunkRow::TextChunks(dshtui::api::types::ChunkData {
                texts: vec!["packed".into(), " chunks".into()],
                turn: Some(1),
                step: Some(1),
                ..Default::default()
            }),
        },
        traj_record(
            6,
            "tool/call",
            serde_json::json!({"turn": 1, "step": 1, "callId": "c1", "name": "bash", "arguments": "{}"}),
        ),
        traj_record(
            7,
            "tool/result",
            serde_json::json!({"turn": 1, "step": 1, "callId": "c1", "message": "ok", "meta": {"diff": "a"}}),
        ),
        traj_record(8, "step/end", serde_json::json!({"turn": 1, "step": 1})),
        traj_record(
            9,
            "turn/end",
            serde_json::json!({"turn": 1, "reason": "stop"}),
        ),
        traj_record(
            10,
            "compaction/summary",
            serde_json::json!({"summary": "pruned 2 turns"}),
        ),
    ];
    let eff = w.apply(TrajIncoming::Snapshot {
        cursor: None,
        records,
        has_more: false,
        projections: None,
    });
    assert_eq!(eff, TrajEffect::Rebuilt);
    let kinds: Vec<TrajKind> = w.raw_rows().map(|r| r.kind()).collect();
    assert_eq!(
        kinds,
        vec![
            TrajKind::RequestHeader,
            TrajKind::TurnStart,
            TrajKind::UserMessage,
            TrajKind::StepStart,
            TrajKind::AssistantMessage,
            TrajKind::ToolCall,
            TrajKind::ToolResult,
            TrajKind::StepEnd,
            TrajKind::TurnEnd,
            TrajKind::Compaction,
        ],
        "边界行全部保留、无逐 delta 展开"
    );
    // packed chunks 汇总进 assistant 摘要（不是独立行）。
    let asst = w
        .raw_rows()
        .find(|r| r.kind() == TrajKind::AssistantMessage)
        .unwrap();
    assert_eq!(asst.summary(), "packed chunks");
    // 字段级映射：callId/arguments/meta/error/usage/reason。
    let call = w
        .raw_rows()
        .find(|r| r.kind() == TrajKind::ToolCall)
        .unwrap();
    assert_eq!(call.call_id(), Some("c1"));
    assert_eq!(call.name(), Some("bash"));
    assert_eq!(
        call.args_raw().and_then(|v| v.as_str()),
        Some("{}"),
        "arguments 原始 JSON 字符串原样保留"
    );
    let result = w
        .raw_rows()
        .find(|r| r.kind() == TrajKind::ToolResult)
        .unwrap();
    assert!(result.meta_has_diff(), "meta 存在 → diff 来源");
    assert!(!result.is_error());
    let header = w.raw_rows().next().unwrap();
    assert_eq!(header.reason(), Some("initial"));
}

#[test]
fn trajectory_27turn_1144step_ordered_window_and_tool_locatable_ac005_02() {
    // AC-005-02：27 turn/1144 step 会话事件链完整（窗口内严格升序、无空洞）
    // 且可定位任意 tool/call（RowId/seq 身份稳定）。
    let mut w = TrajectoryWindow::new(200);
    let records = synth_trajectory(27, 1144);
    let _ = w.apply(TrajIncoming::Snapshot {
        cursor: None,
        records,
        has_more: true,
        projections: None,
    });
    assert_eq!(w.len(), 200, "窗口上限 200（逐出最旧留 seq 锚）");
    let seqs: Vec<u64> = w.raw_rows().map(|r| r.seq().get()).collect();
    let sorted = {
        let mut s = seqs.clone();
        s.sort_unstable();
        s
    };
    assert_eq!(seqs, sorted, "严格升序无乱序");
    assert!(
        seqs.windows(2).all(|p| p[1] == p[0] + 1),
        "无空洞: {seqs:?}"
    );
    // 窗口内任一 tool/call 可定位（按 seq + 稳定 RowId）。
    for row in w.raw_rows().filter(|r| r.kind() == TrajKind::ToolCall) {
        let by_id = w.row(row.id()).expect("RowId 可定位");
        assert_eq!(by_id.seq(), row.seq());
    }
    assert!(w.tail_seq().is_some() && w.head_seq().is_some());
}

#[test]
fn trajectory_page_prepend_no_dup_no_gap_and_has_more_false_ac005_07() {
    // AC-005-07：向上翻页前插合并无重复无空洞；hasMore=false 到顶；边界行
    // 不丢（复用 REQ-001 AC-001-03/11 口径）。
    let mut w = TrajectoryWindow::new(200);
    w.apply(TrajIncoming::Snapshot {
        cursor: Some(SessionLogOffset::new(5)),
        records: vec![
            traj_record(4, "step/start", serde_json::json!({"turn": 1, "step": 1})),
            traj_record(5, "user/message", serde_json::json!({"content": "a"})),
        ],
        has_more: true,
        projections: None,
    });
    let eff = w.apply(TrajIncoming::Page {
        records: vec![
            traj_record(1, "turn/start", serde_json::json!({"turn": 1})),
            traj_record(
                2,
                "request/header",
                serde_json::json!({"reason": "initial"}),
            ),
            // 与窗口重叠的行：去重不重复插入。
            traj_record(4, "step/start", serde_json::json!({"turn": 1, "step": 1})),
        ],
        has_more: Some(false),
    });
    assert_eq!(
        eff,
        TrajEffect::HeadPrepend {
            inserted: 2,
            anchor_shift: 2
        }
    );
    assert!(!w.head_has_more(), "hasMore=false → 到顶");
    let seqs: Vec<u64> = w.raw_rows().map(|r| r.seq().get()).collect();
    assert_eq!(seqs, vec![1, 2, 4, 5], "无重复无空洞、边界行不丢");
    // 全重叠 → Noop。
    assert_eq!(
        w.apply(TrajIncoming::Page {
            records: vec![traj_record(1, "turn/start", serde_json::json!({"turn": 1}))],
            has_more: None,
        }),
        TrajEffect::Noop
    );
}

#[test]
fn trajectory_reconnect_snapshot_reconciles_seq_gap_ac005_08() {
    // AC-005-08（模型侧）：断网恢复后 snapshot 重建 + 按 seq 对账补齐缺口、
    // 事件链完整；旧索引清空后旧 seq 可重入。
    let mut w = TrajectoryWindow::new(200);
    w.apply(TrajIncoming::Snapshot {
        cursor: None,
        records: vec![traj_record(
            10,
            "user/message",
            serde_json::json!({"content": "a"}),
        )],
        has_more: true,
        projections: None,
    });
    w.apply(TrajIncoming::FollowEvent(traj_event(
        11,
        "assistant/message",
        serde_json::json!({}),
    )));
    // 重连快照：只带回 seq 10（缺口 11 由 follow 尾页补齐）。
    w.apply(TrajIncoming::Snapshot {
        cursor: Some(SessionLogOffset::new(10)),
        records: vec![traj_record(
            10,
            "user/message",
            serde_json::json!({"content": "a"}),
        )],
        has_more: true,
        projections: None,
    });
    // 旧 seq 可重入（索引已清）：补齐缺口后链完整。
    assert_eq!(
        w.apply(TrajIncoming::FollowEvent(traj_event(
            11,
            "assistant/message",
            serde_json::json!({}),
        ))),
        TrajEffect::TailAppended { appended: 1 }
    );
    let seqs: Vec<u64> = w.raw_rows().map(|r| r.seq().get()).collect();
    assert_eq!(seqs, vec![10, 11], "对账补齐后事件链完整");
}

#[test]
fn trajectory_seq_request_id_dedup_and_eviction_anchor_ac005_07_08() {
    let mut w = TrajectoryWindow::new(200);
    w.apply(TrajIncoming::Snapshot {
        cursor: None,
        records: vec![traj_record(
            1,
            "user/message",
            serde_json::json!({"content": "a"}),
        )],
        has_more: true,
        projections: None,
    });
    // requestId 幂等：同 rid 不同 seq 重放跳过（D-4/REQ-001 口径）。
    let mut e = traj_event(2, "assistant/message", serde_json::json!({}));
    e.request_id = Some("r1".into());
    assert_eq!(
        w.apply(TrajIncoming::FollowEvent(e.clone())),
        TrajEffect::TailAppended { appended: 1 }
    );
    let mut replay = e.clone();
    replay.seq = Some(SessionSeq::new(3));
    assert_eq!(w.apply(TrajIncoming::FollowEvent(replay)), TrajEffect::Noop);
    // seq 去重。
    assert_eq!(w.apply(TrajIncoming::FollowEvent(e)), TrajEffect::Noop);
    assert_eq!(w.len(), 2);
    // 恢复路径：去重拒绝后新事件正常落地（不被旧失败状态污染）。
    assert_eq!(
        w.apply(TrajIncoming::FollowEvent(traj_event(
            4,
            "turn/end",
            serde_json::json!({"turn": 1}),
        ))),
        TrajEffect::TailAppended { appended: 1 }
    );
    // 小窗口逐出留锚：已逐出 seq 的 page 重放 Noop。
    let mut small = TrajectoryWindow::new(2);
    for s in 1..=3 {
        small.apply(TrajIncoming::FollowEvent(traj_event(
            s,
            "user/message",
            serde_json::json!({"content": format!("{s}")}),
        )));
    }
    assert_eq!(small.len(), 2);
    assert_eq!(
        small.apply(TrajIncoming::Page {
            records: vec![traj_record(
                1,
                "user/message",
                serde_json::json!({"content": "1"})
            )],
            has_more: None,
        }),
        TrajEffect::Noop,
        "已逐出的 seq 锚点直接丢弃，不重复加载"
    );
}

#[test]
fn trajectory_fold_concurrent_append_keeps_all_rows_ac005_12() {
    // AC-005-12：turn/assistant 折叠正确且不丢行；折叠中并发 append 的事件
    // 展开后完整入组、不抖动。
    let mut w = TrajectoryWindow::new(200);
    let snapshot = vec![
        traj_record(1, "turn/start", serde_json::json!({"turn": 1})),
        traj_record(2, "step/start", serde_json::json!({"turn": 1, "step": 1})),
        traj_record(
            3,
            "assistant/message",
            serde_json::json!({"turn": 1, "step": 1, "content": "a1"}),
        ),
        traj_record(
            4,
            "tool/call",
            serde_json::json!({"turn": 1, "step": 1, "callId": "c1", "name": "bash", "arguments": "{}"}),
        ),
        traj_record(
            5,
            "tool/result",
            serde_json::json!({"turn": 1, "step": 1, "callId": "c1", "message": "ok"}),
        ),
        traj_record(6, "step/end", serde_json::json!({"turn": 1, "step": 1})),
        traj_record(7, "turn/end", serde_json::json!({"turn": 1})),
        traj_record(8, "turn/start", serde_json::json!({"turn": 2})),
        traj_record(9, "step/start", serde_json::json!({"turn": 2, "step": 2})),
        traj_record(
            10,
            "assistant/message",
            serde_json::json!({"turn": 2, "step": 2, "content": "a2"}),
        ),
        traj_record(11, "turn/end", serde_json::json!({"turn": 2})),
    ];
    w.apply(TrajIncoming::Snapshot {
        cursor: None,
        records: snapshot,
        has_more: true,
        projections: None,
    });
    let mut fold = FoldState::default();
    // z：折叠 assistant 组（turn 1 step 1）→ 组首可见、成员隐藏。
    let asst_id = w
        .raw_rows()
        .find(|r| r.kind() == TrajKind::AssistantMessage && r.turn() == Some(1))
        .unwrap()
        .id();
    assert!(w.toggle_group(&mut fold, asst_id));
    let view: Vec<TrajKind> = w.view(&fold).into_iter().map(|r| r.kind()).collect();
    assert_eq!(
        view,
        vec![
            TrajKind::TurnStart,
            TrajKind::StepStart,
            TrajKind::AssistantMessage, // 组首
            TrajKind::StepEnd,
            TrajKind::TurnEnd,
            TrajKind::TurnStart,
            TrajKind::StepStart,
            TrajKind::AssistantMessage,
            TrajKind::TurnEnd,
        ],
        "折叠 assistant 组：tool/call+tool/result 隐藏、其余不丢"
    );
    // 并发 append：同组新 tool/call + tool/result（seq 12/13，无 step/end 边界）。
    w.apply(TrajIncoming::FollowEvent(traj_event(
        12,
        "tool/call",
        serde_json::json!({"turn": 2, "step": 2, "callId": "c2", "name": "grep", "arguments": "{}"}),
    )));
    w.apply(TrajIncoming::FollowEvent(traj_event(
        13,
        "tool/result",
        serde_json::json!({"turn": 2, "step": 2, "callId": "c2", "message": "hit"}),
    )));
    // 折叠 turn 2 的 assistant 组再展开（za）→ 新事件完整入组不丢行。
    let asst2_id = w
        .raw_rows()
        .find(|r| r.kind() == TrajKind::AssistantMessage && r.turn() == Some(2))
        .unwrap()
        .id();
    assert!(w.toggle_group(&mut fold, asst2_id));
    let before: Vec<u64> = w.view(&fold).into_iter().map(|r| r.seq().get()).collect();
    assert_eq!(
        before.iter().filter(|s| **s == 12 || **s == 13).count(),
        0,
        "折叠中 append 的成员行隐藏"
    );
    assert!(!w.toggle_group(&mut fold, asst2_id), "za 展开");
    let after: Vec<u64> = w.view(&fold).into_iter().map(|r| r.seq().get()).collect();
    assert_eq!(
        after.iter().filter(|s| **s == 12 || **s == 13).count(),
        2,
        "展开后新事件完整入组: {after:?}"
    );
    // turn 折叠：仅组首可见。
    let turn_id = w
        .raw_rows()
        .find(|r| r.kind() == TrajKind::TurnStart && r.turn() == Some(1))
        .unwrap()
        .id();
    assert!(w.toggle_group(&mut fold, turn_id));
    let view: Vec<u64> = w.view(&fold).into_iter().map(|r| r.seq().get()).collect();
    let turn1: Vec<u64> = view
        .iter()
        .copied()
        .filter(|s| *s >= 1 && *s <= 7)
        .collect();
    assert_eq!(turn1, vec![1], "turn 1 折叠：仅组首 turn/start 可见");
    assert!(view.contains(&8), "turn 2 不受影响");
}

#[test]
fn trajectory_unknown_event_preserved_and_missing_seq_skipped() {
    let mut w = TrajectoryWindow::new(200);
    w.apply(TrajIncoming::Snapshot {
        cursor: None,
        records: vec![
            traj_record(9, "future/mystery", serde_json::json!({"k": "v"})),
            traj_record(10, "user/message", serde_json::json!({"content": "ok"})),
        ],
        has_more: true,
        projections: None,
    });
    let unknown = w.raw_rows().next().unwrap();
    assert_eq!(unknown.kind(), TrajKind::Unknown);
    assert_eq!(unknown.event_type(), "future/mystery");
    assert_eq!(
        unknown
            .raw()
            .and_then(|v| v.get("k"))
            .and_then(|v| v.as_str()),
        Some("v")
    );
    // 无 seq 事件跳过（入日志不崩溃，REQ §6）。
    let mut e = traj_event(99, "user/message", serde_json::json!({"content": "x"}));
    e.seq = None;
    assert_eq!(w.apply(TrajIncoming::FollowEvent(e)), TrajEffect::Noop);
    assert_eq!(w.len(), 2);
}

#[test]
fn trajectory_store_lru_keeps_three_recent_windows() {
    // REQ-005 §5：多会话轨迹缓存仅最近 3 窗口。
    let mut store = TrajectoryStore::new(3);
    store.touch("a", 200);
    store.touch("b", 200);
    store.touch("c", 200);
    store.touch("d", 200);
    assert!(store.get("a").is_none(), "LRU 逐出最久未用");
    assert!(store.get("b").is_some());
    store.touch("b", 200);
    store.touch("e", 200);
    assert!(store.get("c").is_none());
    assert!(store.get("b").is_some());
}

#[test]
fn trajectory_row_id_stable_across_prepend_and_toggle() {
    // RowId 单调不回收：前插/折叠/重算后身份稳定（详情锚点防串、命中跳转
    // 零抖动，DESIGN-IT-TWICE hybrid 约束）。
    let mut w = TrajectoryWindow::new(200);
    w.apply(TrajIncoming::Snapshot {
        cursor: None,
        records: vec![traj_record(
            5,
            "user/message",
            serde_json::json!({"content": "a"}),
        )],
        has_more: true,
        projections: None,
    });
    let id = w.raw_rows().next().unwrap().id();
    assert_eq!(id, RowId(1));
    w.apply(TrajIncoming::Page {
        records: vec![traj_record(1, "turn/start", serde_json::json!({"turn": 1}))],
        has_more: Some(false),
    });
    assert_eq!(
        w.row(id).map(|r| r.seq()),
        Some(SessionSeq::new(5)),
        "前插后 RowId 仍指向同一行"
    );
    let mut fold = FoldState::default();
    let turn_id = w
        .raw_rows()
        .find(|r| r.kind() == TrajKind::TurnStart)
        .unwrap()
        .id();
    assert_eq!(w.group_of(turn_id), Some(GroupId::Turn(1)));
    assert_eq!(w.group_of(id), None, "user 行无折叠组");
    let _ = w.toggle_group(&mut fold, turn_id);
    assert!(fold.is_collapsed(GroupId::Turn(1)));
}

// ============================================================================
// REQ-005 Step 2 详情数据（Seam = detail_for(row, window) 纯函数，无 IO，
// 计划 Step 2 测试 Seam 行）。验收：AC-005-03/10/11/15。
// ============================================================================

/// 带显式毫秒 time 的轨迹记录（详情 timing 推导测试用）。
fn traj_record_at(
    seq: u64,
    event_type: &str,
    data: serde_json::Value,
    time: i64,
) -> SessionHistoryRecord {
    let mut e = traj_event(seq, event_type, data);
    e.time = Some(time);
    SessionHistoryRecord::Event { event: e }
}

/// 标准小窗口：turn 1 step 1 完整工具链（step/start→assistant→tool/call→
/// tool/result），供详情推导。时间显式拉开（step 2s、assistant 3s、call
/// 4s、result 7s）以断言 time_seconds 推导。
fn detail_window_with(
    error: Option<serde_json::Value>,
    meta: Option<serde_json::Value>,
) -> TrajectoryWindow {
    const T: i64 = 1_700_000_000_000;
    let mut w = TrajectoryWindow::new(200);
    let mut records = vec![
        traj_record_at(1, "turn/start", serde_json::json!({"turn": 1}), T + 1000),
        traj_record_at(
            2,
            "step/start",
            serde_json::json!({"turn": 1, "step": 1, "reason": "max"}),
            T + 2000,
        ),
        traj_record_at(
            3,
            "assistant/message",
            serde_json::json!({"turn": 1, "step": 1, "content": "先看看", "usage": {"input": 100, "output": 50, "cacheRead": 10, "cacheWrite": 5, "think": 20}}),
            T + 3000,
        ),
        traj_record_at(
            4,
            "tool/call",
            serde_json::json!({"turn": 1, "step": 1, "callId": "c1", "name": "bash", "arguments": "{\"command\":\"grep -r x /tmp\"}"}),
            T + 4000,
        ),
    ];
    let mut result = serde_json::json!({
        "turn": 1, "step": 1, "callId": "c1",
        "message": "3 处命中"
    });
    if let Some(e) = error {
        result["error"] = e;
    }
    if let Some(m) = meta {
        result["meta"] = m;
    }
    records.push(traj_record_at(5, "tool/result", result, T + 7000));
    w.apply(TrajIncoming::Snapshot {
        cursor: None,
        records,
        has_more: true,
        projections: None,
    });
    w
}

#[test]
fn detail_for_tool_call_aggregates_args_result_usage_timing_ac005_03() {
    use dshtui::model::trajectory::detail_for;
    // AC-005-03：tool/call 详情含 args（原始 JSON 格式化）/result/usage
    // （同 step assistant 推导）/timing（事件 time 推导）。
    let w = detail_window_with(None, None);
    let call = w
        .raw_rows()
        .find(|r| r.kind() == TrajKind::ToolCall)
        .cloned()
        .unwrap();
    let detail = detail_for(&call, &w).expect("tool/call 行可开详情");
    assert_eq!(detail.source_kind, TrajKind::ToolCall);
    assert_eq!(detail.source_seq, SessionSeq::new(4));
    // arguments 原始 JSON 字符串 → 格式化 JSON（含换行缩进）。
    let args = detail.args_text.expect("args 存在");
    assert!(args.contains("\"command\""), "args 格式化: {args}");
    // result 聚合：同 callId 的 tool/result message。
    assert_eq!(detail.result_text.as_deref(), Some("3 处命中"));
    // usage = 同 step assistant/message.usage（无 per-tool，推导标注）。
    let usage = detail.usage.as_ref().expect("usage 同 step 推导");
    assert_eq!(usage.input, Some(100));
    assert_eq!(usage.output, Some(50));
    assert_eq!(usage.cache_read, Some(10));
    assert_eq!(usage.cache_write, Some(5));
    assert_eq!(usage.think, Some(20));
    // timing：started_at=call.time、completed=result.time、step_start=
    // step/start.time、first_token=assistant.time（T=1_700_000_000_000）。
    const T: i64 = 1_700_000_000_000;
    let timing = detail.timing.as_ref().expect("timing 事件 time 推导");
    assert_eq!(timing.started_at, Some(T + 4000));
    assert_eq!(timing.completed, Some(T + 7000));
    assert_eq!(timing.step_start, Some(T + 2000));
    assert_eq!(timing.first_token, Some(T + 3000));
    assert_eq!(
        timing.time_seconds,
        Some(3.0),
        "completed - started = 3000ms"
    );
}

#[test]
fn detail_for_tool_result_keeps_error_alongside_and_diff_ac005_10() {
    use dshtui::model::trajectory::detail_for;
    // AC-005-10：tool/result 无 meta（无 diff）→ diff=None（UI 降级「无 diff」
    // 不崩溃）；error 与 result 并列展示。
    let w = detail_window_with(
        Some(serde_json::json!({"name": "ExitCode", "code": "exit-1"})),
        None,
    );
    let result = w
        .raw_rows()
        .find(|r| r.kind() == TrajKind::ToolResult)
        .cloned()
        .unwrap();
    let detail = detail_for(&result, &w).expect("tool/result 行可开详情");
    assert!(detail.result_text.as_deref().is_some());
    let err = detail.error.as_ref().expect("error 并列展示");
    assert_eq!(err.name, "ExitCode");
    assert_eq!(err.code, "exit-1");
    assert!(detail.diff.is_none(), "无 meta → 无 diff（降级不崩溃）");
    // 有 meta 但无 diff 键 → 同样 None。
    let w2 = detail_window_with(None, Some(serde_json::json!({"other": 1})));
    let result2 = w2
        .raw_rows()
        .find(|r| r.kind() == TrajKind::ToolResult)
        .cloned()
        .unwrap();
    assert!(
        detail_for(&result2, &w2).unwrap().diff.is_none(),
        "meta 无 diff 键 → 无 diff"
    );
    // 有 meta.diff → Some（工具私有载荷，dsh-tool-fs 口径）。
    let w3 = detail_window_with(None, Some(serde_json::json!({"diff": "--- a/1\n+++ b/1"})));
    let result3 = w3
        .raw_rows()
        .find(|r| r.kind() == TrajKind::ToolResult)
        .cloned()
        .unwrap();
    let d3 = detail_for(&result3, &w3).unwrap();
    assert_eq!(
        d3.diff.as_deref(),
        Some("--- a/1\n+++ b/1"),
        "meta.diff 原样文本"
    );
}

#[test]
fn detail_usage_timing_missing_shows_none_ac005_15() {
    use dshtui::model::trajectory::detail_for;
    // AC-005-15：usage/timing 缺省 → None（UI 显示 —，不崩溃不编造）。
    let mut w = TrajectoryWindow::new(200);
    w.apply(TrajIncoming::Snapshot {
        cursor: None,
        records: vec![
            traj_record(1, "turn/start", serde_json::json!({"turn": 1})),
            // assistant 无 usage、无 time 字段。
            {
                let mut e = traj_event(
                    2,
                    "assistant/message",
                    serde_json::json!({"turn": 1, "step": 1, "content": "hi"}),
                );
                e.time = None;
                SessionHistoryRecord::Event { event: e }
            },
        ],
        has_more: true,
        projections: None,
    });
    let asst = w
        .raw_rows()
        .find(|r| r.kind() == TrajKind::AssistantMessage)
        .cloned()
        .unwrap();
    let detail = detail_for(&asst, &w).expect("assistant 行可开详情");
    assert!(detail.usage.is_none(), "无 usage → 缺省 —");
    assert!(detail.timing.is_none(), "无 time 推导 → 缺省 —");
}

#[test]
fn detail_for_non_detail_rows_returns_none() {
    use dshtui::model::trajectory::detail_for;
    let w = detail_window_with(None, None);
    for kind in [
        TrajKind::TurnStart,
        TrajKind::StepStart,
        TrajKind::UserMessage,
        TrajKind::Compaction,
        TrajKind::Unknown,
    ] {
        let row = w.raw_rows().find(|r| r.kind() == kind).cloned();
        if let Some(row) = row {
            assert!(detail_for(&row, &w).is_none(), "{kind:?} 行不提供详情");
        }
    }
}

#[test]
fn detail_yank_text_is_args_plus_result_plain_text_ac005_11() {
    use dshtui::model::trajectory::detail_for;
    // AC-005-11：复制目标 = args/result 纯文本（内存拼装、不落盘，落盘性由
    // 纯函数无 IO 保证；走 REQ-003 yank 后端链）。
    let w = detail_window_with(None, None);
    let call = w
        .raw_rows()
        .find(|r| r.kind() == TrajKind::ToolCall)
        .cloned()
        .unwrap();
    let detail = detail_for(&call, &w).unwrap();
    let yanked = detail.yank_text().expect("args/result 存在可复制");
    assert!(
        yanked.contains("grep -r x"),
        "args 纯文本进复制目标: {yanked}"
    );
    assert!(
        yanked.contains("3 处命中"),
        "result 纯文本进复制目标: {yanked}"
    );
    // 只有 args（无 result）也成立。
    let mut w2 = TrajectoryWindow::new(200);
    w2.apply(TrajIncoming::Snapshot {
        cursor: None,
        records: vec![traj_record(
            1,
            "tool/call",
            serde_json::json!({"turn": 1, "step": 1, "callId": "c9", "name": "bash", "arguments": "{\"command\":\"ls\"}"}),
        )],
        has_more: true,
        projections: None,
    });
    let call2 = w2.raw_rows().next().cloned().unwrap();
    let detail2 = detail_for(&call2, &w2).unwrap();
    assert!(detail2.yank_text().is_some(), "仅 args 也可复制");
}

// ============================================================================
// REQ-005 Step 4 轨迹内搜索索引（Seam = TrajectorySearchIndex query 纯函数 +
// RowId 跳转/折叠组展开纯函数；本地 nucleo 窗口内过滤，非 session/search
// 全历史，D-25）。验收：AC-005-05/13。
// ============================================================================

use dshtui::model::trajectory::{TrajectorySearchIndex, TrajectorySearchItem};

#[test]
fn trajectory_search_index_filters_event_rows_ac005_05() {
    // AC-005-05：输入关键词 → 窗口内过滤命中（即时本地，无网络）。
    let mut w = TrajectoryWindow::new(200);
    w.apply(TrajIncoming::Snapshot {
        cursor: None,
        records: vec![
            traj_record(1, "turn/start", serde_json::json!({"turn": 1})),
            traj_record(2, "step/start", serde_json::json!({"turn": 1, "step": 1})),
            traj_record(3, "user/message", serde_json::json!({"content": "排查数据库连接"})),
            traj_record(4, "assistant/message", serde_json::json!({"turn": 1, "step": 1, "content": "先用 bash 检查端口"})),
            traj_record(5, "tool/call", serde_json::json!({"turn": 1, "step": 1, "callId": "c1", "name": "bash", "arguments": "{\"command\":\"grep -r db /tmp\"}"})),
            traj_record(6, "tool/result", serde_json::json!({"turn": 1, "step": 1, "callId": "c1", "message": "db:5432 在监听"})),
            traj_record(7, "request/header", serde_json::json!({"reason": "initial"})),
            traj_record(8, "turn/end", serde_json::json!({"turn": 1})),
        ],
        has_more: false,
        projections: None,
    });
    let mut index = TrajectorySearchIndex::new();
    index.rebuild(w.raw_rows());
    // 空查询不触发（前序契约：空查询不触发）。
    assert!(index.query("  ").is_empty(), "空查询不触发");
    let hits = index.query("grep");
    assert!(!hits.is_empty(), "grep 命中 tool/call");
    let hit = &hits[0];
    let item = &index.items()[hit.item_index];
    assert_eq!(item.kind, TrajKind::ToolCall);
    assert!(item.text.contains("grep"), "text 含关键词: {}", item.text);
    assert!(!hit.positions.is_empty(), "命中高亮位置");
    // 中文命中 user 消息。
    let zh = index.query("排查");
    assert!(!zh.is_empty());
    let zh_item = &index.items()[zh[0].item_index];
    assert_eq!(zh_item.kind, TrajKind::UserMessage);
    // 边界行（无内容文本）不进索引：不会命中。
    assert!(index.query("turn/start").is_empty());
}

#[test]
fn trajectory_search_match_jumps_to_row_and_expands_fold_ac005_13() {
    // AC-005-13：Enter 跳转命中行（可展开其所在折叠组）。
    let mut w = TrajectoryWindow::new(200);
    w.apply(TrajIncoming::Snapshot {
        cursor: None,
        records: vec![
            traj_record(1, "turn/start", serde_json::json!({"turn": 1})),
            traj_record(2, "step/start", serde_json::json!({"turn": 1, "step": 1})),
            traj_record(3, "assistant/message", serde_json::json!({"turn": 1, "step": 1, "content": "a1"})),
            traj_record(4, "tool/call", serde_json::json!({"turn": 1, "step": 1, "callId": "c1", "name": "bash", "arguments": "{\"command\":\"grep x\"}"})),
            traj_record(5, "tool/result", serde_json::json!({"turn": 1, "step": 1, "callId": "c1", "message": "hit"})),
            traj_record(6, "step/end", serde_json::json!({"turn": 1, "step": 1})),
            traj_record(7, "turn/end", serde_json::json!({"turn": 1})),
            traj_record(8, "turn/start", serde_json::json!({"turn": 2})),
            traj_record(9, "step/start", serde_json::json!({"turn": 2, "step": 2})),
            traj_record(10, "assistant/message", serde_json::json!({"turn": 2, "step": 2, "content": "继续"})),
            traj_record(11, "tool/call", serde_json::json!({"turn": 2, "step": 2, "callId": "c2", "name": "grep", "arguments": "{\"command\":\"grep y\"}"})),
            traj_record(12, "turn/end", serde_json::json!({"turn": 2})),
        ],
        has_more: false,
        projections: None,
    });
    let mut index = TrajectorySearchIndex::new();
    index.rebuild(w.raw_rows());
    // 命中 turn 2 的 tool/call（grep y）。
    let hits = index.query("grep y");
    assert_eq!(hits.len(), 1);
    let item = &index.items()[hits[0].item_index];
    // 命中行在窗口内可定位（RowId 稳定跳转）。
    let row = w.row(item.row_id).expect("命中行可定位");
    assert_eq!(row.seq(), SessionSeq::new(11));
    // 折叠 turn 2 的 assistant 组后，命中行被隐藏；展开组后可见（跳转前置）。
    let mut fold = FoldState::default();
    let asst2 = w
        .raw_rows()
        .find(|r| r.kind() == TrajKind::AssistantMessage && r.turn() == Some(2))
        .expect("turn2 assistant")
        .id();
    assert!(w.toggle_group(&mut fold, asst2), "折叠 assistant 组");
    let hidden = w.view(&fold).iter().all(|r| r.id() != item.row_id);
    assert!(hidden, "折叠时命中行隐藏");
    // 展开命中行所在折叠组 → 行可见（可跳转/定位）。
    let group = w.group_of(item.row_id).expect("命中行有折叠组");
    fold.toggle(group);
    assert!(w.view(&fold).iter().any(|r| r.id() == item.row_id));
}

#[test]
fn trajectory_search_item_displays_kind_digest() {
    // TrajectorySearchItem 形状：kind 标签 + 摘要 digest（窗口内过滤列表行）。
    let item = TrajectorySearchItem {
        row_id: RowId(1),
        kind: TrajKind::ToolCall,
        seq: SessionSeq::new(5),
        text: "bash {\"command\":\"ls\"}".to_string(),
        display: "tool: bash".to_string(),
    };
    assert!(item.display.starts_with("tool:"));
}

// ============================================================================
// REQ-005 Step 7：27turn/1144step 轨迹性能（AC-005-14，D-26；复用 REQ-001
// §8 口径）。#[ignore] 由 release 手动跑（plan Step 7：性能日志沿用
// /tmp/dshtui-perf.log）：
//   cargo test --release --test model trajectory_27turn_1144step_perf -- --ignored
// ============================================================================

#[test]
#[ignore = "性能测量：release 手动运行（见上方命令）"]
fn trajectory_27turn_1144step_perf_ac005_14() {
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::time::{Duration, Instant};

    // 首屏构建：snapshot 全量 apply + 一次 view < 1s。
    let records = synth_trajectory(27, 1144);
    assert!(records.len() > 1144, "fixture 规模达标: {}", records.len());
    let t0 = Instant::now();
    let mut w = TrajectoryWindow::new(200);
    w.apply(TrajIncoming::Snapshot {
        cursor: None,
        records,
        has_more: true,
        projections: None,
    });
    let build = t0.elapsed();
    assert!(
        build < Duration::from_secs(1),
        "首屏构建必须 <1s: {build:?}"
    );
    // 单帧折叠 view 重算 p99 <33ms（折叠一半 turn 组）。
    let mut fold = FoldState::default();
    for t in (1..=27).step_by(2) {
        fold.toggle(GroupId::Turn(t)); // 折叠奇数 turn 组
    }
    let mut samples: Vec<Duration> = Vec::with_capacity(300);
    for _ in 0..300 {
        let s = Instant::now();
        let v = w.view(&fold);
        std::hint::black_box(v.len());
        samples.push(s.elapsed());
    }
    samples.sort();
    let p99 = samples[(samples.len() as f64 * 0.99) as usize];
    assert!(
        p99 < Duration::from_millis(33),
        "滚动帧 p99 必须 <33ms: {p99:?}"
    );
    // 定位任意 tool/call 并复制详情（AC-005-14 排障闭环）。
    use dshtui::model::detail_for;
    let mut located = 0usize;
    for row in w.raw_rows().filter(|r| r.kind() == TrajKind::ToolCall) {
        let d = detail_for(row, &w).expect("tool/call 详情");
        assert!(d.yank_text().is_some(), "复制目标存在");
        located += 1;
    }
    assert!(located > 0, "窗口内定位到 tool/call");
    // 性能证据写入 /tmp/dshtui-perf.log（既有机制）。
    let summary = format!(
        "TASK-005 AC-005-14 27turn/1144step: build={:?} rows={} view_p99={:?} located_tool_calls={}\n",
        build,
        w.len(),
        p99,
        located
    );
    if let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/dshtui-perf.log")
    {
        let _ = f.write_all(summary.as_bytes());
    }
    println!("PERF {summary}");
}

// ============================================================================
// REQ-007 V0.4: message.id retention (feedback CAS) + projection readers
// ============================================================================

#[test]
fn assistant_message_retains_wire_message_id_ac007_27() {
    use dshtui::model::Block;
    let mut window = TranscriptWindow::new(20);
    // assistant/message 事件 data 含官方 message id（messageFeedback 定位锚）。
    window.apply(Incoming::FollowEvent(SessionWireEvent {
        event_type: "assistant/message".into(),
        seq: Some(SessionSeq::new(1)),
        time: None,
        request_id: None,
        ignorable: None,
        source_event_seqs: None,
        surface_op: None,
        data: Some(serde_json::json!({
            "id": "msg-42",
            "turn": 1, "step": 1
        })),
    }));
    let last = window.blocks().last().cloned().unwrap();
    match last {
        Block::AssistantMessage { message_id, .. } => {
            assert_eq!(message_id.as_deref(), Some("msg-42"));
        }
        _ => panic!("尾部必须是 assistant"),
    }
}

#[test]
fn projection_goal_plan_todos_readers_ac007_11_26() {
    use dshtui::model::ProjectionSnapshot;
    let p = ProjectionSnapshot::new(serde_json::json!({
        "goal": {"goal": {"id": "g1", "revision": 2, "objective": "交付",
                          "phase": "paused"}, "roundsStarted": 1},
        "plan": {"active": true, "pending": false},
        "todos": [{"content": "写 ADR", "status": "done"}]
    }));
    let g = p.goal().unwrap();
    assert_eq!(g.revision, 2);
    assert_eq!(g.rounds_started, Some(1));
    assert_eq!(p.plan(), Some((true, false)));
    assert_eq!(p.todos().len(), 1);
    assert!(p.subagent_running_children().is_empty());
}
