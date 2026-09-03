//! 窗口化转录本与一致性合并（Notes/06 §2 / REQ-001 §5；D-3/D-4=A）。
//!
//! 设计要点（Step 3 Prototype 验证结论，`examples/proto_step3.rs`）：
//! - 单一 `apply(Incoming) -> ApplyEffect` 漏斗：snapshot 重建、follow 尾追、
//!   page 前插、repair 整窗重建全部经此 seam（AppState 只消费 Effect）；
//! - **乱序事件不能盲尾插**：快路径 seq>尾部 O(1) 尾插；慢路径二分定位插入
//!   保持全局升序（AC-001-11「合并后顺序一致」）；
//! - requestId 幂等先于 seq 去重判断（D-4）：同 requestId 重放不产生重复块；
//! - 逐出最旧仅留 seq 锚点：`seen_seq` 保留逐出块的 seq，page 重叠直接丢弃；
//! - 视口稳定由 Effect 表达：`TailAppended.anchor_stable`（尾部追加未动窗口头）
//!   与 `HeadPrepend.anchor_shift`（前插条数），UI 据此平移滚动位置不抖动；
//! - 模型不臆断缺口：seq 稀疏是合法形态（chunks 无独立 seq）；缺口修复由
//!   follow/page 边界事实（hasMore、snapshot 重建）驱动，见 AppState。

use std::collections::{HashMap, HashSet, VecDeque};

use serde_json::Value;

use crate::api::types::{
    ChunkRow, SessionHistoryRecord, SessionLogOffset, SessionSeq, SessionWireEvent,
};

/// 窗口元素（packed chunk rows 原样存储，不展开为逐 delta）。
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    UserMessage {
        seq: SessionSeq,
        content: String,
        time: Option<i64>,
    },
    AssistantMessage {
        seq: SessionSeq,
        chunks: PackedChunks,
        time: Option<i64>,
    },
    ToolCall {
        seq: SessionSeq,
        call_id: Option<String>,
        name: Option<String>,
        args_raw: Option<Value>,
        time: Option<i64>,
    },
    ToolResult {
        seq: SessionSeq,
        call_id: Option<String>,
        content: String,
        is_error: bool,
        time: Option<i64>,
    },
    RequestHeader {
        seq: SessionSeq,
        summary: String,
    },
    Compaction {
        seq: SessionSeq,
        summary: String,
    },
    /// V0.1 仅占位（REQ-004 FR-004-01 依赖 identity 保留）。
    Image {
        seq: SessionSeq,
        attachment_id: Option<String>,
        name: Option<String>,
        dims: Option<String>,
    },
    /// 未知事件：event type + 原始 payload 原样保留（D-4 下游契约），不渲染不丢弃。
    Unknown {
        seq: SessionSeq,
        event_type: String,
        raw: Value,
    },
}

impl Block {
    pub fn seq(&self) -> SessionSeq {
        match self {
            Block::UserMessage { seq, .. }
            | Block::AssistantMessage { seq, .. }
            | Block::ToolCall { seq, .. }
            | Block::ToolResult { seq, .. }
            | Block::RequestHeader { seq, .. }
            | Block::Compaction { seq, .. }
            | Block::Image { seq, .. }
            | Block::Unknown { seq, .. } => *seq,
        }
    }

    pub fn is_assistant(&self) -> bool {
        matches!(self, Block::AssistantMessage { .. })
    }
}

/// packed chunk rows 原样存储（官方低内存优化的关键，Notes/03 §4.3）。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PackedChunks {
    pub rows: Vec<ChunkRow>,
}

/// 轮次元数据（turnOutline 解析结果；下游 REQ-003 FR-003-02/04 契约）。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TurnOutlineItem {
    pub turn: Option<u64>,
    pub seq: Option<SessionSeq>,
    pub prompt: Option<String>,
    pub response: Option<String>,
}

/// 进入窗口的输入（由 AppState 把 api 事件映射为此类型）。
#[derive(Debug, Clone)]
pub enum Incoming {
    Snapshot {
        cursor: Option<SessionLogOffset>,
        records: Vec<SessionHistoryRecord>,
        has_more: bool,
        projections: Option<Value>,
    },
    FollowEvent(SessionWireEvent),
    /// 独立 chunk row：归入最近一个 AssistantMessage。
    Chunks(ChunkRow),
    Page {
        records: Vec<SessionHistoryRecord>,
        has_more: Option<bool>,
    },
}

/// apply 的效果（AppState 据此发起 backfill/refollow、平移视口）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyEffect {
    /// 整窗重建（snapshot）。
    Rebuilt,
    /// 尾部追加：anchor_stable=true 表示窗口头（滚动锚点）未被逐出。
    TailAppended {
        appended: usize,
        anchor_stable: bool,
    },
    /// 头部前插：anchor_shift=实际新插入条数（UI 平移滚动位置）。
    HeadPrepend {
        inserted: usize,
        anchor_shift: usize,
    },
    /// 全部去重/幂等跳过，无可见变化。
    Noop,
}

/// 窗口化转录本（上限 window_messages，默认 200）。
#[derive(Debug, Clone)]
pub struct TranscriptWindow {
    blocks: VecDeque<Block>,
    cap: usize,
    /// 是否到顶（page hasMore=false）。
    head_has_more: bool,
    /// follow 快照 cursor（session/page 的 throughSeq；-1 表示空日志）。
    cursor: Option<SessionLogOffset>,
    /// 投影快照（官方口径，全部从 raw 读取，不自算）。
    projections: Value,
    /// 轮次元数据（下游契约）。
    turn_outline: Vec<TurnOutlineItem>,
    seen_seq: HashSet<u64>,
    seen_request: HashSet<String>,
}

impl Default for TranscriptWindow {
    fn default() -> Self {
        Self::new(200)
    }
}

impl TranscriptWindow {
    pub fn new(cap: usize) -> Self {
        Self {
            blocks: VecDeque::new(),
            cap: cap.max(1),
            head_has_more: true,
            cursor: None,
            projections: Value::Null,
            turn_outline: Vec::new(),
            seen_seq: HashSet::new(),
            seen_request: HashSet::new(),
        }
    }

    // ---------- 查询（UI 只读） ----------

    pub fn blocks(&self) -> impl Iterator<Item = &Block> {
        self.blocks.iter()
    }

    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// 窗口最旧已加载 seq（滚动锚点）。
    pub fn head_seq(&self) -> Option<SessionSeq> {
        self.blocks.front().map(|b| b.seq())
    }

    /// 窗口最新 seq（live tail）。
    pub fn tail_seq(&self) -> Option<SessionSeq> {
        self.blocks.back().map(|b| b.seq())
    }

    pub fn head_has_more(&self) -> bool {
        self.head_has_more
    }

    pub fn cursor(&self) -> Option<SessionLogOffset> {
        self.cursor
    }

    pub fn projections(&self) -> &Value {
        &self.projections
    }

    pub fn turn_outline(&self) -> &[TurnOutlineItem] {
        &self.turn_outline
    }

    /// 某 seq 是否已在窗口内。
    pub fn contains_seq(&self, seq: SessionSeq) -> bool {
        self.seen_seq.contains(&seq.0)
    }

    // ---------- 写入口（单一漏斗） ----------

    pub fn apply(&mut self, incoming: Incoming) -> ApplyEffect {
        match incoming {
            Incoming::Snapshot {
                cursor,
                records,
                has_more,
                projections,
            } => self.apply_snapshot(cursor, records, has_more, projections),
            Incoming::FollowEvent(ev) => self.apply_event(&ev),
            Incoming::Chunks(row) => self.apply_chunks(row),
            Incoming::Page { records, has_more } => self.apply_page(records, has_more),
        }
    }

    fn apply_snapshot(
        &mut self,
        cursor: Option<SessionLogOffset>,
        records: Vec<SessionHistoryRecord>,
        has_more: bool,
        projections: Option<Value>,
    ) -> ApplyEffect {
        // 整窗重建：清空全部索引（不可修复缺口 → rebuild 的语义，Notes/06 §2）。
        self.blocks.clear();
        self.seen_seq.clear();
        self.seen_request.clear();
        self.turn_outline.clear();
        self.cursor = cursor;
        self.head_has_more = has_more;
        self.projections = projections.unwrap_or(Value::Null);
        for rec in records {
            self.ingest_record(rec);
        }
        self.evict();
        self.parse_turn_outline();
        ApplyEffect::Rebuilt
    }

    fn apply_event(&mut self, ev: &SessionWireEvent) -> ApplyEffect {
        let Some(seq) = ev.seq else {
            // 无 seq 事件：不可参与去重/排序，原样计入日志不落窗口。
            tracing::warn!(event_type = %ev.event_type, "事件缺少 seq，跳过（计入日志）");
            return ApplyEffect::Noop;
        };
        // requestId 幂等（D-4）先于 seq 去重：同 requestId 已 apply → 跳过（不论 seq）。
        if let Some(rid) = ev.request_id.as_deref() {
            if self.seen_request.contains(rid) {
                return ApplyEffect::Noop;
            }
        }
        if self.seen_seq.contains(&seq.0) {
            return ApplyEffect::Noop;
        }
        self.seen_seq.insert(seq.0);
        if let Some(rid) = ev.request_id.as_deref() {
            self.seen_request.insert(rid.to_string());
        }
        let head_before = self.head_seq();
        let block = block_from_event(ev, seq);
        // 快路径尾插 / 慢路径二分定位（乱序修复事件保持升序）。
        if self
            .blocks
            .back()
            .map(|b| b.seq())
            .is_none_or(|tail| seq > tail)
        {
            self.blocks.push_back(block);
        } else {
            let idx = self.blocks.partition_point(|b| b.seq() < seq);
            self.blocks.insert(idx, block);
        }
        self.evict();
        ApplyEffect::TailAppended {
            appended: 1,
            // anchor_stable：尾部追加未移动窗口头（滚动锚点稳定，UI 不抖）。
            anchor_stable: head_before.is_none() || self.head_seq() == head_before,
        }
    }

    fn apply_chunks(&mut self, row: ChunkRow) -> ApplyEffect {
        // chunks 无独立 seq：归入最近一个 AssistantMessage（官方顺序保证紧跟）。
        if let Some(last) = self.blocks.back_mut() {
            if let Block::AssistantMessage { chunks, .. } = last {
                chunks.rows.push(row);
                return ApplyEffect::TailAppended {
                    appended: 0, // 块数不变（行并入既有块），仅内容变化
                    anchor_stable: true,
                };
            }
        }
        tracing::warn!("chunkrow 到达但窗口尾非 assistant message，丢弃并计入日志");
        ApplyEffect::Noop
    }

    fn apply_page(
        &mut self,
        records: Vec<SessionHistoryRecord>,
        has_more: Option<bool>,
    ) -> ApplyEffect {
        if let Some(hm) = has_more {
            self.head_has_more = hm;
        }
        let mut inserted = 0usize;
        for rec in records {
            // page 记录与窗口/已逐出 seq 重叠 → 去重（AC-001-11 并发交错合并）。
            if let Some(seq) = record_seq(&rec) {
                if self.seen_seq.contains(&seq.0) {
                    continue;
                }
                if let Some(rid) = record_request_id(&rec) {
                    if self.seen_request.contains(rid) {
                        continue;
                    }
                }
            }
            self.ingest_record(rec);
            inserted += 1;
        }
        if inserted == 0 {
            return ApplyEffect::Noop;
        }
        self.evict();
        ApplyEffect::HeadPrepend {
            inserted,
            anchor_shift: inserted,
        }
    }

    /// 单条记录落窗（保持升序：二分插入）。
    fn ingest_record(&mut self, rec: SessionHistoryRecord) {
        match &rec {
            SessionHistoryRecord::Event { event } => {
                let Some(seq) = event.seq else {
                    tracing::warn!(event_type = %event.event_type, "快照事件缺少 seq，跳过");
                    return;
                };
                if self.seen_seq.contains(&seq.0) {
                    return;
                }
                if let Some(rid) = event.request_id.as_deref() {
                    if self.seen_request.contains(rid) {
                        return;
                    }
                    self.seen_request.insert(rid.to_string());
                }
                self.seen_seq.insert(seq.0);
                let block = block_from_event(event, seq);
                let idx = self
                    .blocks
                    .partition_point(|b| b.seq() < seq);
                self.blocks.insert(idx, block);
            }
            SessionHistoryRecord::Chunks { event: row } => {
                if let Some(last) = self.blocks.back_mut() {
                    if let Block::AssistantMessage { chunks, .. } = last {
                        chunks.rows.push(row.clone());
                        return;
                    }
                }
                tracing::warn!("chunkrow 无归属 assistant，丢弃并计入日志");
            }
        }
    }

    fn evict(&mut self) {
        while self.blocks.len() > self.cap {
            // 逐出最旧，仅留 seq 锚点（seen_seq 保留：page 重叠重放直接丢弃）。
            self.blocks.pop_front();
        }
    }

    /// 从 projections.turnOutline 解析轮次元数据（解析失败保留空，不臆造）。
    fn parse_turn_outline(&mut self) {
        let Some(arr) = self
            .projections
            .get("turnOutline")
            .and_then(|v| v.as_array())
        else {
            return;
        };
        self.turn_outline = arr
            .iter()
            .filter_map(|item| {
                Some(TurnOutlineItem {
                    turn: item.get("turn").and_then(|v| v.as_u64()),
                    seq: item.get("seq").and_then(|v| v.as_u64()).map(SessionSeq),
                    prompt: item.get("prompt").and_then(|v| v.as_str()).map(String::from),
                    response: item
                        .get("response")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                })
            })
            .collect();
    }
}

/// 从 wire 事件构造 Block（未知类型原样保留 raw，D-4）。
fn block_from_event(ev: &SessionWireEvent, seq: SessionSeq) -> Block {
    let t = ev.event_type.as_str();
    let data = ev.data.clone().unwrap_or(Value::Null);
    let time = ev.time;
    let str_of = |key: &str| -> Option<String> {
        data.get(key)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    if t.starts_with("user/") {
        Block::UserMessage {
            seq,
            content: str_of("content")
                .or_else(|| data.get("content").map(|v| v.to_string()))
                .unwrap_or_default(),
            time,
        }
    } else if t.starts_with("assistant/") {
        Block::AssistantMessage {
            seq,
            chunks: PackedChunks::default(),
            time,
        }
    } else if t.starts_with("tool/call") {
        Block::ToolCall {
            seq,
            call_id: str_of("id").or_else(|| str_of("callId")),
            name: str_of("name"),
            args_raw: data.get("args").cloned(),
            time,
        }
    } else if t.starts_with("tool/result") {
        Block::ToolResult {
            seq,
            call_id: str_of("id").or_else(|| str_of("callId")),
            content: str_of("content")
                .or_else(|| data.get("content").map(|v| v.to_string()))
                .unwrap_or_default(),
            is_error: data.get("isError").and_then(|v| v.as_bool()).unwrap_or(false),
            time,
        }
    } else if t.contains("request") && t.contains("header") {
        Block::RequestHeader {
            seq,
            summary: str_of("summary").unwrap_or_default(),
        }
    } else if t.contains("compaction") {
        Block::Compaction {
            seq,
            summary: str_of("summary").unwrap_or_default(),
        }
    } else if t.contains("image") || t.contains("attachment") {
        Block::Image {
            seq,
            attachment_id: str_of("attachmentId").or_else(|| str_of("attachment_id")),
            name: str_of("name"),
            dims: data
                .get("dims")
                .map(|v| v.to_string())
                .or_else(|| {
                    Some(format!(
                        "{}x{}",
                        data.get("width").and_then(|v| v.as_u64()).unwrap_or(0),
                        data.get("height").and_then(|v| v.as_u64()).unwrap_or(0)
                    ))
                }),
        }
    } else {
        Block::Unknown {
            seq,
            event_type: ev.event_type.clone(),
            raw: data,
        }
    }
}

fn record_seq(rec: &SessionHistoryRecord) -> Option<SessionSeq> {
    match rec {
        SessionHistoryRecord::Event { event } => event.seq,
        SessionHistoryRecord::Chunks { .. } => None,
    }
}

fn record_request_id(rec: &SessionHistoryRecord) -> Option<&str> {
    match rec {
        SessionHistoryRecord::Event { event } => event.request_id.as_deref(),
        SessionHistoryRecord::Chunks { .. } => None,
    }
}

/// 多会话窗口缓存（LRU，仅保留最近 3 窗口，Notes/06 §7）。
#[derive(Debug, Default)]
pub struct SessionStore {
    windows: HashMap<String, TranscriptWindow>,
    order: VecDeque<String>,
    cap: usize,
}

impl SessionStore {
    pub fn new(cap: usize) -> Self {
        Self {
            windows: HashMap::new(),
            order: VecDeque::new(),
            cap,
        }
    }

    pub fn get(&self, id: &str) -> Option<&TranscriptWindow> {
        self.windows.get(id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut TranscriptWindow> {
        self.windows.get_mut(id)
    }

    /// 打开/触摸会话窗口：不存在则新建（LRU 逐出最久未用）。
    pub fn touch(&mut self, id: &str, cap: usize) -> &mut TranscriptWindow {
        if !self.windows.contains_key(id) {
            if self.order.len() >= self.cap.max(1) {
                if let Some(old) = self.order.pop_front() {
                    self.windows.remove(&old);
                }
            }
            self.windows.insert(id.to_string(), TranscriptWindow::new(cap));
        } else if let Some(pos) = self.order.iter().position(|x| x == id) {
            self.order.remove(pos);
        }
        self.order.push_back(id.to_string());
        self.windows.get_mut(id).expect("刚插入")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::types::SessionSeq;

    fn ev(seq: u64, r#type: &str, rid: Option<&str>, data: Option<Value>) -> SessionWireEvent {
        SessionWireEvent {
            event_type: r#type.into(),
            seq: Some(SessionSeq(seq)),
            time: Some(1000 + seq as i64),
            request_id: rid.map(String::from),
            ignorable: None,
            source_event_seqs: None,
            surface_op: None,
            data,
        }
    }

    fn event_rec(seq: u64, r#type: &str, rid: Option<&str>) -> SessionHistoryRecord {
        SessionHistoryRecord::Event { event: ev(seq, r#type, rid, None) }
    }

    #[test]
    fn snapshot_rebuilds_and_sorts() {
        let mut w = TranscriptWindow::new(200);
        let eff = w.apply(Incoming::Snapshot {
            cursor: Some(SessionLogOffset(5)),
            records: vec![
                event_rec(10, "user/message", None),
                event_rec(5, "user/message", None),
                event_rec(7, "user/message", None),
            ],
            has_more: true,
            projections: Some(serde_json::json!({"values": {"title": "t"}})),
        });
        assert_eq!(eff, ApplyEffect::Rebuilt);
        assert_eq!(w.len(), 3);
        let seqs: Vec<u64> = w.blocks().map(|b| b.seq().0).collect();
        assert_eq!(seqs, vec![5, 7, 10]);
        assert_eq!(w.cursor(), Some(SessionLogOffset(5)));
        assert!(w.head_has_more());
        assert_eq!(w.head_seq(), Some(SessionSeq(5)));
        assert_eq!(w.tail_seq(), Some(SessionSeq(10)));
    }

    #[test]
    fn follow_append_and_seq_dedup() {
        let mut w = TranscriptWindow::new(200);
        w.apply(Incoming::Snapshot {
            cursor: None,
            records: vec![event_rec(1, "user/message", None)],
            has_more: false,
            projections: None,
        });
        assert_eq!(
            w.apply(Incoming::FollowEvent(ev(2, "assistant/message", None, None))),
            ApplyEffect::TailAppended { appended: 1, anchor_stable: true }
        );
        // 重复 seq → Noop（AC-001-11）。
        assert_eq!(
            w.apply(Incoming::FollowEvent(ev(2, "assistant/message", None, None))),
            ApplyEffect::Noop
        );
        assert_eq!(w.len(), 2);
    }

    #[test]
    fn request_id_idempotent_across_seqs() {
        let mut w = TranscriptWindow::new(200);
        w.apply(Incoming::Snapshot {
            cursor: None,
            records: vec![event_rec(1, "user/message", None)],
            has_more: true,
            projections: None,
        });
        // 同 requestId 已 apply：不同 seq 重放也必须跳过（AC-001-10）。
        w.apply(Incoming::FollowEvent(ev(2, "assistant/message", Some("r1"), None)));
        assert_eq!(
            w.apply(Incoming::FollowEvent(ev(3, "assistant/message", Some("r1"), None))),
            ApplyEffect::Noop
        );
        // 同 seq 同 rid 再次重复。
        assert_eq!(
            w.apply(Incoming::FollowEvent(ev(2, "assistant/message", Some("r1"), None))),
            ApplyEffect::Noop
        );
        assert_eq!(w.len(), 2);
        // 恢复路径：重放被拒后，新 requestId 正常追加（状态不被污染）。
        assert_eq!(
            w.apply(Incoming::FollowEvent(ev(4, "assistant/message", Some("r2"), None))),
            ApplyEffect::TailAppended { appended: 1, anchor_stable: true }
        );
        assert_eq!(w.tail_seq(), Some(SessionSeq(4)));
    }

    #[test]
    fn page_prepend_overlap_dedup_and_order() {
        let mut w = TranscriptWindow::new(200);
        w.apply(Incoming::Snapshot {
            cursor: Some(SessionLogOffset(6)),
            records: vec![event_rec(6, "user/message", None), event_rec(7, "user/message", None)],
            has_more: true,
            projections: None,
        });
        let eff = w.apply(Incoming::Page {
            records: vec![
                event_rec(4, "user/message", None),
                event_rec(5, "user/message", None),
                // 与窗口重叠（AC-001-11 并发交错）。
                event_rec(6, "user/message", None),
            ],
            has_more: Some(false),
        });
        assert_eq!(eff, ApplyEffect::HeadPrepend { inserted: 2, anchor_shift: 2 });
        assert!(!w.head_has_more(), "hasMore=false → 到顶");
        let seqs: Vec<u64> = w.blocks().map(|b| b.seq().0).collect();
        assert_eq!(seqs, vec![4, 5, 6, 7]);
        // 全部重叠 → Noop。
        assert_eq!(
            w.apply(Incoming::Page {
                records: vec![event_rec(4, "user/message", None)],
                has_more: None,
            }),
            ApplyEffect::Noop
        );
    }

    #[test]
    fn out_of_order_event_inserted_sorted() {
        // 乱序事件（seq < 尾部）不能盲尾插——必须二分定位保持升序。
        let mut w = TranscriptWindow::new(200);
        w.apply(Incoming::Snapshot {
            cursor: None,
            records: vec![event_rec(10, "user/message", None), event_rec(20, "user/message", None)],
            has_more: true,
            projections: None,
        });
        w.apply(Incoming::FollowEvent(ev(30, "assistant/message", None, None)));
        // 修复到达 seq 15（介于 10/20 之间）。
        assert_eq!(
            w.apply(Incoming::FollowEvent(ev(15, "user/message", None, None))),
            ApplyEffect::TailAppended { appended: 1, anchor_stable: true }
        );
        let seqs: Vec<u64> = w.blocks().map(|b| b.seq().0).collect();
        assert_eq!(seqs, vec![10, 15, 20, 30]);
    }

    #[test]
    fn capacity_eviction_keeps_latest_and_anchor() {
        let mut w = TranscriptWindow::new(10);
        // 先放 5 条（未满）：追加不逐出 → anchor_stable。
        let records: Vec<SessionHistoryRecord> =
            (1..=5).map(|s| event_rec(s, "user/message", None)).collect();
        w.apply(Incoming::Snapshot { cursor: None, records, has_more: true, projections: None });
        for s in 6..=10 {
            assert_eq!(
                w.apply(Incoming::FollowEvent(ev(s, "assistant/message", None, None))),
                ApplyEffect::TailAppended { appended: 1, anchor_stable: true },
                "未满时追加 seq {s}：anchor 不动"
            );
        }
        // 窗口满（10 条）后每次追加逐出最旧 → anchor 前进（unstable）。
        for s in 11..=20 {
            assert_eq!(
                w.apply(Incoming::FollowEvent(ev(s, "assistant/message", None, None))),
                ApplyEffect::TailAppended { appended: 1, anchor_stable: false },
                "满窗追加 seq {s}：逐出最旧，anchor 前进"
            );
        }
        assert_eq!(w.len(), 10);
        let seqs: Vec<u64> = w.blocks().map(|b| b.seq().0).collect();
        assert_eq!(seqs, (11..=20).collect::<Vec<_>>());
        assert_eq!(w.head_seq(), Some(SessionSeq(11)));
        // 已逐出 seq 的 page 重放 → Noop（seq 锚点语义，不重复加载）。
        assert_eq!(
            w.apply(Incoming::Page { records: vec![event_rec(5, "user/message", None)], has_more: None }),
            ApplyEffect::Noop
        );
    }

    #[test]
    fn chunks_attach_to_last_assistant() {
        let mut w = TranscriptWindow::new(200);
        w.apply(Incoming::Snapshot {
            cursor: None,
            records: vec![
                event_rec(1, "user/message", None),
                event_rec(2, "assistant/message", None),
            ],
            has_more: true,
            projections: None,
        });
        let row = ChunkRow::TextChunks(crate::api::types::ChunkData {
            texts: vec!["hello".into()],
            ..Default::default()
        });
        assert_eq!(
            w.apply(Incoming::Chunks(row.clone())),
            ApplyEffect::TailAppended { appended: 0, anchor_stable: true }
        );
        let last = w.blocks().last().map(|b| b.clone()).unwrap();
        match last {
            Block::AssistantMessage { chunks, .. } => assert_eq!(chunks.rows, vec![row]),
            _ => panic!("尾部必须是 assistant"),
        }
    }

    #[test]
    fn unknown_event_preserves_raw() {
        let mut w = TranscriptWindow::new(200);
        w.apply(Incoming::Snapshot {
            cursor: None,
            records: vec![SessionHistoryRecord::Event {
                event: ev(9, "future/mystery", None, Some(serde_json::json!({"k": "v"}))),
            }],
            has_more: true,
            projections: None,
        });
        let first = w.blocks().next().map(|b| b.clone()).unwrap();
        match first {
            Block::Unknown { seq, event_type, raw } => {
                assert_eq!(seq, SessionSeq(9));
                assert_eq!(event_type, "future/mystery");
                assert_eq!(raw["k"], "v");
            }
            _ => panic!("未知事件必须保留为 Unknown 且不丢 payload"),
        }
    }

    #[test]
    fn rebuild_replaces_window_without_residue() {
        let mut w = TranscriptWindow::new(200);
        w.apply(Incoming::Snapshot {
            cursor: None,
            records: vec![event_rec(1, "user/message", None), event_rec(2, "user/message", None)],
            has_more: true,
            projections: None,
        });
        w.apply(Incoming::FollowEvent(ev(3, "assistant/message", None, None)));
        // 重连 rebuild（不可修复缺口 → 整窗重建）。
        w.apply(Incoming::Snapshot {
            cursor: None,
            records: vec![event_rec(7, "user/message", None)],
            has_more: true,
            projections: None,
        });
        let seqs: Vec<u64> = w.blocks().map(|b| b.seq().0).collect();
        assert_eq!(seqs, vec![7]);
        // 旧 requestId/seq 索引已被清空：旧 seq 可以重新进入。
        // 乱序事件插入窗口头之前 → anchor 位移（unstable 语义正确）。
        assert_eq!(
            w.apply(Incoming::FollowEvent(ev(2, "assistant/message", None, None))),
            ApplyEffect::TailAppended { appended: 1, anchor_stable: false }
        );
        assert_eq!(w.head_seq(), Some(SessionSeq(2)));
    }

    #[test]
    fn turn_outline_parsed_from_projections() {
        let mut w = TranscriptWindow::new(200);
        w.apply(Incoming::Snapshot {
            cursor: None,
            records: vec![],
            has_more: true,
            projections: Some(serde_json::json!({
                "turnOutline": [
                    {"turn": 1, "seq": 3, "prompt": "p1", "response": "r1"},
                    {"turn": 2, "seq": 6, "prompt": "p2", "response": "r2"}
                ]
            })),
        });
        assert_eq!(w.turn_outline().len(), 2);
        assert_eq!(w.turn_outline()[1].seq, Some(SessionSeq(6)));
        assert_eq!(w.turn_outline()[1].response.as_deref(), Some("r2"));
    }

    #[test]
    fn event_without_seq_logged_and_skipped() {
        let mut w = TranscriptWindow::new(200);
        w.apply(Incoming::Snapshot { cursor: None, records: vec![], has_more: true, projections: None });
        let mut e = ev(0, "user/message", None, None);
        e.seq = None;
        assert_eq!(w.apply(Incoming::FollowEvent(e)), ApplyEffect::Noop);
        assert!(w.is_empty());
    }

    #[test]
    fn session_store_lru_cap() {
        let mut store = SessionStore::new(3);
        store.touch("a", 200).apply(Incoming::Snapshot {
            cursor: None,
            records: vec![event_rec(1, "user/message", None)],
            has_more: true,
            projections: None,
        });
        store.touch("b", 200);
        store.touch("c", 200);
        store.touch("d", 200);
        assert!(store.get("a").is_none(), "LRU 逐出最久未用");
        assert!(store.get("b").is_some());
        // 触摸后 a 之外的顺序正确。
        store.touch("b", 200);
        store.touch("e", 200);
        assert!(store.get("c").is_none());
        assert!(store.get("b").is_some());
    }
}
