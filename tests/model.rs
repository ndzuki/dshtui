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
