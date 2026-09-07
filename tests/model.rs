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
        seq: Some(SessionSeq(seq)),
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
    let seqs: Vec<u64> = w.raw_rows().map(|r| r.seq().0).collect();
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
        cursor: Some(SessionLogOffset(5)),
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
    let seqs: Vec<u64> = w.raw_rows().map(|r| r.seq().0).collect();
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
        cursor: Some(SessionLogOffset(10)),
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
    let seqs: Vec<u64> = w.raw_rows().map(|r| r.seq().0).collect();
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
    replay.seq = Some(SessionSeq(3));
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
    let before: Vec<u64> = w.view(&fold).into_iter().map(|r| r.seq().0).collect();
    assert_eq!(
        before.iter().filter(|s| **s == 12 || **s == 13).count(),
        0,
        "折叠中 append 的成员行隐藏"
    );
    assert!(!w.toggle_group(&mut fold, asst2_id), "za 展开");
    let after: Vec<u64> = w.view(&fold).into_iter().map(|r| r.seq().0).collect();
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
    let view: Vec<u64> = w.view(&fold).into_iter().map(|r| r.seq().0).collect();
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
        Some(SessionSeq(5)),
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
