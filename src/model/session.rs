//! Windowed transcript and consistency merge (Notes/06 §2 / REQ-001 §5;
//! D-3/D-4=A).
//!
//! Design notes (Step 3 Prototype validation, `examples/proto_step3.rs`):
//! - a single `apply(Incoming) -> ApplyEffect` funnel: snapshot rebuild, follow
//!   tail append, page prepend and repair full-window rebuild all go through
//!   this seam (AppState only consumes the Effect);
//! - **out-of-order events must not be blindly tail-appended**: fast path
//!   seq>tail is an O(1) tail append; the slow path binary-search-inserts to
//!   keep the global ascending order (AC-001-11 "merged order consistent");
//! - requestId idempotency comes before seq dedup (D-4): replaying the same
//!   requestId never produces duplicate blocks;
//! - eviction of the oldest only keeps the seq anchor: `seen_seq` retains the
//!   seqs of evicted blocks, page overlap is discarded directly;
//! - viewport stability is expressed via Effect:
//!   `TailAppended.anchor_stable` (tail append did not move the window head)
//!   and `HeadPrepend.anchor_shift` (number of prepended entries); the UI
//!   shifts its scroll position accordingly without jitter;
//! - the model never invents gaps: sparse seq is a legal shape (chunks carry no
//!   independent seq); gap repair is driven by follow/page boundary facts
//!   (hasMore, snapshot rebuild), see AppState.

use std::collections::{HashMap, HashSet, VecDeque};

use serde_json::Value;

use crate::api::types::{
    ChunkData, ChunkRow, SessionHistoryRecord, SessionLogOffset, SessionRequestId, SessionSeq,
    SessionWireEvent,
};

/// Window element (packed chunk rows stored as-is, never expanded into
/// per-delta items).
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
        /// Assistant message identity (`message.id` from the wire `data`),
        /// retained for `messageFeedback` CAS targeting (REQ-007 AC-007-27).
        message_id: Option<String>,
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
    /// V0.1 placeholder only (REQ-004 FR-004-01 depends on identity
    /// preservation).
    Image {
        seq: SessionSeq,
        attachment_id: Option<String>,
        name: Option<String>,
        dims: Option<String>,
    },
    /// Unknown event: event type + raw payload preserved as-is (D-4 downstream
    /// contract), never rendered, never dropped.
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

/// packed chunk rows stored as-is (key to the official low-memory
/// optimization, Notes/03 §4.3).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PackedChunks {
    pub rows: Vec<ChunkRow>,
}

/// Turn metadata (turnOutline parse result; downstream REQ-003 FR-003-02/04
/// contract).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TurnOutlineItem {
    pub turn: Option<u64>,
    pub seq: Option<SessionSeq>,
    pub prompt: Option<String>,
    pub response: Option<String>,
}

/// Optimistic echo status (REQ-002 §5): reconciled away when a durable event
/// with the same requestId arrives; `Failed` marks this send only and is never
/// auto-resent (AC-002-08).
#[derive(Debug, Clone, PartialEq)]
pub enum PendingEchoStatus {
    Pending,
    Failed { code: String, message: String },
}

/// Optimistic echo reconciliation record (created by the send path; occupies
/// no seq and never touches the seen indexes).
#[derive(Debug, Clone, PartialEq)]
pub struct PendingEcho {
    pub request_id: SessionRequestId,
    pub text: String,
    pub status: PendingEchoStatus,
}

/// Input entering the window (AppState maps api events to this type).
#[derive(Debug, Clone)]
pub enum Incoming {
    Snapshot {
        cursor: Option<SessionLogOffset>,
        records: Vec<SessionHistoryRecord>,
        has_more: bool,
        projections: Option<Value>,
    },
    FollowEvent(SessionWireEvent),
    /// Independent chunk row: attached to the most recent AssistantMessage.
    Chunks(ChunkRow),
    Page {
        records: Vec<SessionHistoryRecord>,
        has_more: Option<bool>,
    },
}

/// Effect of apply (AppState uses it to trigger backfill/refollow and shift
/// the viewport).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyEffect {
    /// Full-window rebuild (snapshot).
    Rebuilt,
    /// Tail append: anchor_stable=true means the window head (scroll anchor)
    /// was not evicted.
    TailAppended {
        appended: usize,
        anchor_stable: bool,
    },
    /// Head prepend: anchor_shift=number of entries actually inserted (the UI
    /// shifts the scroll position by it).
    HeadPrepend {
        inserted: usize,
        anchor_shift: usize,
    },
    /// Everything deduplicated / idempotently skipped; no visible change.
    Noop,
}

/// Windowed transcript (bounded by window_messages, default 200).
#[derive(Debug, Clone)]
pub struct TranscriptWindow {
    blocks: VecDeque<Block>,
    cap: usize,
    /// Whether the top has been reached (page hasMore=false).
    head_has_more: bool,
    /// follow snapshot cursor (throughSeq for session/page; -1 means an empty
    /// log).
    cursor: Option<SessionLogOffset>,
    /// Projection snapshot (official convention; all reads go through raw,
    /// never self-computed).
    projections: Value,
    /// Turn metadata (downstream contract).
    turn_outline: Vec<TurnOutlineItem>,
    seen_seq: HashSet<u64>,
    seen_request: HashSet<String>,
    /// 本地乐观回显（与 durable 分存：pending 不占 seq、不写 seen 索引；
    /// durable 同 requestId 到达即对账移除——Step 3 Prototype 验证）。
    pending: VecDeque<PendingEcho>,
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
            pending: VecDeque::new(),
        }
    }

    // ---------- queries (UI read-only) ----------

    pub fn blocks(&self) -> impl Iterator<Item = &Block> {
        self.blocks.iter()
    }

    /// Index-addressable block access (REQ-003 focused-block cursor, visual
    /// selection and context yank).
    pub fn block(&self, index: usize) -> Option<&Block> {
        self.blocks.get(index)
    }

    /// Copy of the window blocks (search-index rebuild input; the window is
    /// ≤200 blocks, this is bounded).
    pub fn block_snapshot(&self) -> Vec<Block> {
        self.blocks.iter().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Oldest loaded seq in the window (scroll anchor).
    pub fn head_seq(&self) -> Option<SessionSeq> {
        self.blocks.front().map(|b| b.seq())
    }

    /// Newest seq in the window (live tail).
    pub fn tail_seq(&self) -> Option<SessionSeq> {
        self.blocks.back().map(|b| b.seq())
    }

    pub fn head_has_more(&self) -> bool {
        self.head_has_more
    }

    /// Block index of a seq within the loaded window, if present.
    /// Downstream contract: `loadThrough(seq)` scroll support (REQ-003) uses
    /// this to jump to a turn without loading through intermediate pages.
    pub fn offset_of(&self, seq: SessionSeq) -> Option<usize> {
        self.blocks.iter().position(|b| b.seq() == seq)
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

    /// Whether a seq is already inside the window.
    pub fn contains_seq(&self, seq: SessionSeq) -> bool {
        self.seen_seq.contains(&seq.get())
    }

    // ---------- optimistic echo seam (REQ-002 Step 3) ----------

    /// Optimistic echo records (send order).
    pub fn pending(&self) -> impl Iterator<Item = &PendingEcho> {
        self.pending.iter()
    }

    /// Create the local echo on send (no seq, no seen indexes; visible in the
    /// next frame).
    pub fn echo(&mut self, request_id: SessionRequestId, text: &str) {
        self.pending.push_back(PendingEcho {
            request_id,
            text: text.to_string(),
            status: PendingEchoStatus::Pending,
        });
    }

    /// Mark a send failed (Pending → Failed only; never auto-resend, AC-002-08).
    pub fn fail_echo(&mut self, request_id: &SessionRequestId, code: &str, message: &str) {
        if let Some(echo) = self
            .pending
            .iter_mut()
            .find(|e| e.request_id == *request_id && e.status == PendingEchoStatus::Pending)
        {
            echo.status = PendingEchoStatus::Failed {
                code: code.to_string(),
                message: message.to_string(),
            };
        }
    }

    /// Echoed text of a pending send (steer-unavailable recovery puts it back
    /// into the draft, AC-003-16).
    pub fn echo_text(&self, request_id: &SessionRequestId) -> Option<&str> {
        self.pending
            .iter()
            .find(|e| e.request_id == *request_id)
            .map(|e| e.text.as_str())
    }

    /// requestId reconciliation funnel: a durable hit retires the matching
    /// pending regardless of its status (a failed echo must not duplicate a
    /// later durable commit, AC-002-06).
    fn reconcile_pending(&mut self, ids: &HashSet<&str>) {
        if ids.is_empty() {
            return;
        }
        self.pending
            .retain(|e| !ids.contains(e.request_id.get().as_str()));
    }

    // ---------- write entry (single funnel) ----------

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
        // Full-window rebuild: clear all indexes (an unfixable gap → rebuild
        // semantics, Notes/06 §2).
        self.blocks.clear();
        self.seen_seq.clear();
        self.seen_request.clear();
        self.turn_outline.clear();
        self.cursor = cursor;
        self.head_has_more = has_more;
        self.projections = projections.unwrap_or(Value::Null);
        // Pending survives the rebuild: a snapshot carrying the same
        // requestId reconciles it away (AC-002-06).
        let ids = records_reconcile_ids(&records);
        self.reconcile_pending(&ids);
        for rec in records {
            self.ingest_record(rec);
        }
        self.evict();
        self.parse_turn_outline();
        ApplyEffect::Rebuilt
    }

    fn apply_event(&mut self, ev: &SessionWireEvent) -> ApplyEffect {
        // Reconcile first: pending never occupies the seen indexes, so the
        // durable event always passes the dedup gates and then retires the
        // matching pending (Step 3 Prototype validation).
        if let Some(id) = ev.reconcile_id() {
            let ids = HashSet::from([id]);
            self.reconcile_pending(&ids);
        }
        let Some(seq) = ev.seq else {
            // Event without seq: cannot take part in dedup/sorting; log it
            // as-is and never put it in the window.
            tracing::warn!(event_type = %ev.event_type, "事件缺少 seq，跳过（计入日志）");
            return ApplyEffect::Noop;
        };
        // requestId idempotency (D-4) comes before seq dedup: if the same
        // requestId was already applied → skip (regardless of seq).
        if let Some(rid) = ev.request_id.as_deref() {
            if self.seen_request.contains(rid) {
                return ApplyEffect::Noop;
            }
        }
        if self.seen_seq.contains(&seq.get()) {
            return ApplyEffect::Noop;
        }
        self.seen_seq.insert(seq.get());
        if let Some(rid) = ev.request_id.as_deref() {
            self.seen_request.insert(rid.to_string());
        }
        let head_before = self.head_seq();
        let blocks = blocks_from_event(ev, seq);
        let appended = blocks.len();
        // Fast-path tail append / slow-path binary-search insert (repair
        // events arriving out of order stay ascending). Multiple blocks from
        // one event (AC-004-01) keep their wire order.
        if self
            .blocks
            .back()
            .map(|b| b.seq())
            .map_or(true, |tail| seq > tail)
        {
            for block in blocks {
                self.blocks.push_back(block);
            }
        } else {
            let idx = self.blocks.partition_point(|b| b.seq() < seq);
            for (offset, block) in blocks.into_iter().enumerate() {
                self.blocks.insert(idx + offset, block);
            }
        }
        self.evict();
        ApplyEffect::TailAppended {
            appended,
            // anchor_stable: the tail append did not move the window head
            // (scroll anchor stable, no UI jitter).
            anchor_stable: head_before.is_none() || self.head_seq() == head_before,
        }
    }

    fn apply_chunks(&mut self, row: ChunkRow) -> ApplyEffect {
        // chunks carry no independent seq: attach to the most recent
        // AssistantMessage (the official ordering guarantees it follows
        // immediately).
        // Find the LAST assistant block (not just the window tail): an
        // out-of-order event inserted at the tail must not steal the chunks.
        if let Some(chunks) = self.blocks.iter_mut().rev().find_map(|b| match b {
            Block::AssistantMessage { chunks, .. } => Some(chunks),
            _ => None,
        }) {
            chunks.rows.push(row);
            return ApplyEffect::TailAppended {
                appended: 0, // block count unchanged (row merged into the
                // existing block), content only
                anchor_stable: true,
            };
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
        // Reconciliation runs outside the insertion count: a fully overlapping
        // page may still retire a pending echo.
        let ids = records_reconcile_ids(&records);
        self.reconcile_pending(&ids);
        let head_before = self.head_seq();
        let mut inserted = 0usize;
        for rec in records {
            // Page overlap is deduplicated before insertion.
            if let Some(seq) = record_seq(&rec) {
                if self.seen_seq.contains(&seq.get()) {
                    continue;
                }
                if let Some(rid) = record_request_id(&rec) {
                    if self.seen_request.contains(rid) {
                        continue;
                    }
                }
            }
            if self.ingest_record(rec) > 0 {
                inserted += 1;
            }
        }
        if inserted == 0 {
            return ApplyEffect::Noop;
        }
        // Count only records that actually moved the old head; an out-of-order
        // record inserted in the middle must not shift the viewport anchor.
        let anchor_shift = head_before
            .map(|head| self.blocks.iter().take_while(|b| b.seq() < head).count())
            .unwrap_or(0);
        self.evict();
        ApplyEffect::HeadPrepend {
            inserted,
            anchor_shift,
        }
    }

    /// Insert one record in sequence order. Returns the number of visible
    /// blocks inserted (one event may expand to host + image blocks,
    /// AC-004-01).
    fn ingest_record(&mut self, rec: SessionHistoryRecord) -> usize {
        match &rec {
            SessionHistoryRecord::Event { event } => {
                let Some(seq) = event.seq else {
                    tracing::warn!(event_type = %event.event_type, "event missing seq; skipped");
                    return 0;
                };
                if self.seen_seq.contains(&seq.get()) {
                    return 0;
                }
                if let Some(rid) = event.request_id.as_deref() {
                    if self.seen_request.contains(rid) {
                        return 0;
                    }
                    self.seen_request.insert(rid.to_string());
                }
                self.seen_seq.insert(seq.get());
                let blocks = blocks_from_event(event, seq);
                let inserted = blocks.len();
                let idx = self.blocks.partition_point(|b| b.seq() < seq);
                for (offset, block) in blocks.into_iter().enumerate() {
                    self.blocks.insert(idx + offset, block);
                }
                inserted
            }
            SessionHistoryRecord::Chunks { event: row } => {
                // Attach to the LAST assistant block in the window, not the
                // window tail: a repair event inserted at the tail after the
                // snapshot must not capture chunks that follow the assistant.
                if let Some(chunks) = self.blocks.iter_mut().rev().find_map(|b| match b {
                    Block::AssistantMessage { chunks, .. } => Some(chunks),
                    _ => None,
                }) {
                    chunks.rows.push(row.clone());
                    return 1;
                }
                tracing::warn!("chunkrow has no assistant owner; skipped");
                0
            }
        }
    }

    fn evict(&mut self) {
        while self.blocks.len() > self.cap {
            // Evict the oldest, keeping only the seq anchor (seen_seq retains the
            // evicted seqs: replayed page overlap is discarded directly).
            self.blocks.pop_front();
        }
    }

    /// Parse turn metadata from projections.turnOutline (on parse failure keep
    /// it empty, never invent).
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
            .map(|item| TurnOutlineItem {
                turn: item.get("turn").and_then(|v| v.as_u64()),
                seq: item
                    .get("seq")
                    .and_then(|v| v.as_u64())
                    .map(SessionSeq::new),
                prompt: item
                    .get("prompt")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                response: item
                    .get("response")
                    .and_then(|v| v.as_str())
                    .map(String::from),
            })
            .collect();
    }
}

/// Build Blocks from a wire event (unknown types keep raw as-is, D-4).
///
/// AC-004-01: a user/assistant/tool event carrying nested image references
/// yields the host block plus one `Block::Image` per reference in wire order —
/// each placeholder renders independently.
fn blocks_from_event(ev: &SessionWireEvent, seq: SessionSeq) -> Vec<Block> {
    let t = ev.event_type.as_str();
    let data = ev.data.clone().unwrap_or(Value::Null);
    let time = ev.time;
    let str_of = |key: &str| -> Option<String> {
        data.get(key)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    // Top-level attachment identity: the whole event is one image block.
    if let Some(object) = data
        .as_object()
        .filter(|o| o.contains_key("attachmentId") || o.contains_key("attachment_id"))
    {
        return vec![image_block_from(object, seq)];
    }
    let mut image_refs: Vec<&serde_json::Map<String, Value>> = Vec::new();
    collect_image_objects(&data, &mut image_refs);
    if !image_refs.is_empty() {
        let mut out = Vec::with_capacity(image_refs.len() + 1);
        out.push(host_block(t, &data, time, seq));
        out.extend(
            image_refs
                .into_iter()
                .map(|image| image_block_from(image, seq)),
        );
        return out;
    }
    vec![if t.starts_with("user/") {
        Block::UserMessage {
            seq,
            content: str_of("content")
                .or_else(|| {
                    data.get("content")
                        .filter(|v| !v.is_array() && !v.is_object())
                        .map(|v| v.to_string())
                })
                .unwrap_or_default(),
            time,
        }
    } else if t.starts_with("assistant/") {
        Block::AssistantMessage {
            seq,
            chunks: PackedChunks::default(),
            time,
            message_id: str_of("id"),
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
            is_error: data
                .get("isError")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
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
            // REQ-004 AC-004-01：dims 优先字符串形式（剥离 JSON 引号），
            // 否则以 width/height 回退为 `WxH`。
            dims: data
                .get("dims")
                .and_then(|v| v.as_str())
                .map(String::from)
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
    }]
}

/// Host block for an event that carries nested image references: surrounding
/// text/state is preserved instead of being swallowed by the image branch.
fn host_block(t: &str, data: &Value, time: Option<i64>, seq: SessionSeq) -> Block {
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
                .or_else(|| nested_text(data))
                .unwrap_or_default(),
            time,
        }
    } else if t.starts_with("assistant/") {
        // Text parts travel as packed chunk rows so the markdown path renders
        // them; image references became separate Image blocks.
        let mut chunks = PackedChunks::default();
        if let Some(text) = nested_text(data) {
            chunks.rows.push(ChunkRow::TextChunks(ChunkData {
                texts: vec![text],
                ..ChunkData::default()
            }));
        }
        Block::AssistantMessage {
            seq,
            chunks,
            time,
            message_id: str_of("id"),
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
                .or_else(|| nested_text(data))
                .unwrap_or_default(),
            is_error: data
                .get("isError")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
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
    } else {
        // Unknown host type: raw payload is preserved alongside the images
        // (D-4 never drops).
        Block::Unknown {
            seq,
            event_type: t.to_string(),
            raw: data.clone(),
        }
    }
}

/// Concatenate `{"type":"text","text":..}` entries found under `content` /
/// `parts` / `blocks` (multi-part message with images keeps its text).
fn nested_text(data: &Value) -> Option<String> {
    let mut texts: Vec<String> = Vec::new();
    collect_text_parts(data, &mut texts);
    if texts.is_empty() {
        return None;
    }
    Some(texts.join(" "))
}

fn collect_text_parts(value: &Value, out: &mut Vec<String>) {
    if let Some(object) = value.as_object() {
        let kind = object
            .get("type")
            .or_else(|| object.get("kind"))
            .and_then(Value::as_str);
        if kind == Some("text") {
            if let Some(t) = object.get("text").and_then(Value::as_str) {
                out.push(t.to_string());
            }
            return;
        }
        for key in ["content", "parts", "blocks", "children"] {
            if let Some(nested) = object.get(key) {
                collect_text_parts(nested, out);
            }
        }
        return;
    }
    if let Some(items) = value.as_array() {
        for item in items {
            collect_text_parts(item, out);
        }
    }
}

fn image_block_from(image: &serde_json::Map<String, Value>, seq: SessionSeq) -> Block {
    Block::Image {
        seq,
        attachment_id: image
            .get("attachmentId")
            .or_else(|| image.get("attachment_id"))
            .and_then(|v| v.as_str())
            .map(String::from),
        name: image.get("name").and_then(|v| v.as_str()).map(String::from),
        // REQ-004 AC-004-01：dims 优先字符串形式，否则以 width/height 回退。
        dims: image
            .get("dims")
            .and_then(|v| v.as_str())
            .map(String::from)
            .or_else(|| {
                Some(format!(
                    "{}x{}",
                    image.get("width").and_then(|v| v.as_u64()).unwrap_or(0),
                    image.get("height").and_then(|v| v.as_u64()).unwrap_or(0)
                ))
            }),
    }
}

/// Recursively collect every image/attachment reference under `content` /
/// `parts` / `blocks` / `children` in wire order (AC-004-01 multi-image).
fn collect_image_objects<'a>(value: &'a Value, out: &mut Vec<&'a serde_json::Map<String, Value>>) {
    if let Some(object) = value.as_object() {
        let kind = object
            .get("type")
            .or_else(|| object.get("kind"))
            .and_then(Value::as_str);
        if matches!(kind, Some("image" | "attachment"))
            && (object.contains_key("attachmentId") || object.contains_key("attachment_id"))
        {
            out.push(object);
            return;
        }
        for key in ["content", "parts", "blocks", "children"] {
            if let Some(nested) = object.get(key) {
                collect_image_objects(nested, out);
            }
        }
        return;
    }
    if let Some(items) = value.as_array() {
        for item in items {
            collect_image_objects(item, out);
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

/// Reconciliation requestIds across one batch of history records (wire shape
/// knowledge lives in `SessionWireEvent::reconcile_id`, api layer).
fn records_reconcile_ids(records: &[SessionHistoryRecord]) -> HashSet<&str> {
    let mut ids = HashSet::new();
    for rec in records {
        if let SessionHistoryRecord::Event { event } = rec {
            if let Some(id) = event.reconcile_id() {
                ids.insert(id);
            }
        }
    }
    ids
}

/// Multi-session window cache (LRU, keeps only the 3 most recent windows,
/// Notes/06 §7).
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

    /// Open/touch a session window: create it if absent (LRU-evicts the least
    /// recently used).
    pub fn touch(&mut self, id: &str, cap: usize) -> &mut TranscriptWindow {
        if !self.windows.contains_key(id) {
            if self.order.len() >= self.cap.max(1) {
                if let Some(old) = self.order.pop_front() {
                    self.windows.remove(&old);
                }
            }
            self.windows
                .insert(id.to_string(), TranscriptWindow::new(cap));
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
    use crate::api::types::{SessionRequestId, SessionSeq};

    fn ev(seq: u64, r#type: &str, rid: Option<&str>, data: Option<Value>) -> SessionWireEvent {
        SessionWireEvent {
            event_type: r#type.into(),
            seq: Some(SessionSeq::new(seq)),
            time: Some(1000 + seq as i64),
            request_id: rid.map(String::from),
            ignorable: None,
            source_event_seqs: None,
            surface_op: None,
            data,
        }
    }

    fn event_rec(seq: u64, r#type: &str, rid: Option<&str>) -> SessionHistoryRecord {
        SessionHistoryRecord::Event {
            event: ev(seq, r#type, rid, None),
        }
    }

    #[test]
    fn snapshot_rebuilds_and_sorts() {
        let mut w = TranscriptWindow::new(200);
        let eff = w.apply(Incoming::Snapshot {
            cursor: Some(SessionLogOffset::new(5)),
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
        let seqs: Vec<u64> = w.blocks().map(|b| b.seq().get()).collect();
        assert_eq!(seqs, vec![5, 7, 10]);
        assert_eq!(w.cursor(), Some(SessionLogOffset::new(5)));
        assert!(w.head_has_more());
        assert_eq!(w.head_seq(), Some(SessionSeq::new(5)));
        assert_eq!(w.tail_seq(), Some(SessionSeq::new(10)));
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
            w.apply(Incoming::FollowEvent(ev(
                2,
                "assistant/message",
                None,
                None
            ))),
            ApplyEffect::TailAppended {
                appended: 1,
                anchor_stable: true
            }
        );
        // Duplicate seq → Noop (AC-001-11).
        assert_eq!(
            w.apply(Incoming::FollowEvent(ev(
                2,
                "assistant/message",
                None,
                None
            ))),
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
        // Same requestId already applied: a replay with a different seq must
        // also be skipped (AC-001-10).
        w.apply(Incoming::FollowEvent(ev(
            2,
            "assistant/message",
            Some("r1"),
            None,
        )));
        assert_eq!(
            w.apply(Incoming::FollowEvent(ev(
                3,
                "assistant/message",
                Some("r1"),
                None
            ))),
            ApplyEffect::Noop
        );
        // Same seq and same rid repeated again.
        assert_eq!(
            w.apply(Incoming::FollowEvent(ev(
                2,
                "assistant/message",
                Some("r1"),
                None
            ))),
            ApplyEffect::Noop
        );
        assert_eq!(w.len(), 2);
        // Recovery path: after the replay is rejected, a new requestId
        // appends normally (state not polluted).
        assert_eq!(
            w.apply(Incoming::FollowEvent(ev(
                4,
                "assistant/message",
                Some("r2"),
                None
            ))),
            ApplyEffect::TailAppended {
                appended: 1,
                anchor_stable: true
            }
        );
        assert_eq!(w.tail_seq(), Some(SessionSeq::new(4)));
    }

    #[test]
    fn page_prepend_overlap_dedup_and_order() {
        let mut w = TranscriptWindow::new(200);
        w.apply(Incoming::Snapshot {
            cursor: Some(SessionLogOffset::new(6)),
            records: vec![
                event_rec(6, "user/message", None),
                event_rec(7, "user/message", None),
            ],
            has_more: true,
            projections: None,
        });
        let eff = w.apply(Incoming::Page {
            records: vec![
                event_rec(4, "user/message", None),
                event_rec(5, "user/message", None),
                // Overlaps the window (AC-001-11 concurrent interleaving).
                event_rec(6, "user/message", None),
            ],
            has_more: Some(false),
        });
        assert_eq!(
            eff,
            ApplyEffect::HeadPrepend {
                inserted: 2,
                anchor_shift: 2
            }
        );
        assert!(!w.head_has_more(), "hasMore=false → 到顶");
        let seqs: Vec<u64> = w.blocks().map(|b| b.seq().get()).collect();
        assert_eq!(seqs, vec![4, 5, 6, 7]);
        // Everything overlaps → Noop.
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
        // An out-of-order event (seq < tail) must not be blindly tail
        // appended — binary-search insert to stay ascending.
        let mut w = TranscriptWindow::new(200);
        w.apply(Incoming::Snapshot {
            cursor: None,
            records: vec![
                event_rec(10, "user/message", None),
                event_rec(20, "user/message", None),
            ],
            has_more: true,
            projections: None,
        });
        w.apply(Incoming::FollowEvent(ev(
            30,
            "assistant/message",
            None,
            None,
        )));
        // A repair arrives at seq 15 (between 10/20).
        assert_eq!(
            w.apply(Incoming::FollowEvent(ev(15, "user/message", None, None))),
            ApplyEffect::TailAppended {
                appended: 1,
                anchor_stable: true
            }
        );
        let seqs: Vec<u64> = w.blocks().map(|b| b.seq().get()).collect();
        assert_eq!(seqs, vec![10, 15, 20, 30]);
    }

    #[test]
    fn capacity_eviction_keeps_latest_and_anchor() {
        let mut w = TranscriptWindow::new(10);
        // First place 5 entries (not full yet): append evicts nothing →
        // anchor_stable.
        let records: Vec<SessionHistoryRecord> = (1..=5)
            .map(|s| event_rec(s, "user/message", None))
            .collect();
        w.apply(Incoming::Snapshot {
            cursor: None,
            records,
            has_more: true,
            projections: None,
        });
        for s in 6..=10 {
            assert_eq!(
                w.apply(Incoming::FollowEvent(ev(
                    s,
                    "assistant/message",
                    None,
                    None
                ))),
                ApplyEffect::TailAppended {
                    appended: 1,
                    anchor_stable: true
                },
                "未满时追加 seq {s}：anchor 不动"
            );
        }
        // Once the window is full (10 entries), every append evicts the oldest
        // → the anchor advances (unstable).
        for s in 11..=20 {
            assert_eq!(
                w.apply(Incoming::FollowEvent(ev(
                    s,
                    "assistant/message",
                    None,
                    None
                ))),
                ApplyEffect::TailAppended {
                    appended: 1,
                    anchor_stable: false
                },
                "满窗追加 seq {s}：逐出最旧，anchor 前进"
            );
        }
        assert_eq!(w.len(), 10);
        let seqs: Vec<u64> = w.blocks().map(|b| b.seq().get()).collect();
        assert_eq!(seqs, (11..=20).collect::<Vec<_>>());
        assert_eq!(w.head_seq(), Some(SessionSeq::new(11)));
        // Page replay of an already-evicted seq → Noop (seq anchor semantics,
        // never loaded twice).
        assert_eq!(
            w.apply(Incoming::Page {
                records: vec![event_rec(5, "user/message", None)],
                has_more: None
            }),
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
            ApplyEffect::TailAppended {
                appended: 0,
                anchor_stable: true
            }
        );
        let last = w.blocks().last().cloned().unwrap();
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
                event: ev(
                    9,
                    "future/mystery",
                    None,
                    Some(serde_json::json!({"k": "v"})),
                ),
            }],
            has_more: true,
            projections: None,
        });
        let first = w.blocks().next().cloned().unwrap();
        match first {
            Block::Unknown {
                seq,
                event_type,
                raw,
            } => {
                assert_eq!(seq, SessionSeq::new(9));
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
            records: vec![
                event_rec(1, "user/message", None),
                event_rec(2, "user/message", None),
            ],
            has_more: true,
            projections: None,
        });
        w.apply(Incoming::FollowEvent(ev(
            3,
            "assistant/message",
            None,
            None,
        )));
        // Reconnect rebuild (unfixable gap → full-window rebuild).
        w.apply(Incoming::Snapshot {
            cursor: None,
            records: vec![event_rec(7, "user/message", None)],
            has_more: true,
            projections: None,
        });
        let seqs: Vec<u64> = w.blocks().map(|b| b.seq().get()).collect();
        assert_eq!(seqs, vec![7]);
        // The old requestId/seq indexes are cleared: old seqs may re-enter.
        // An out-of-order event inserted before the window head → anchor shift
        // (unstable semantics correct).
        assert_eq!(
            w.apply(Incoming::FollowEvent(ev(
                2,
                "assistant/message",
                None,
                None
            ))),
            ApplyEffect::TailAppended {
                appended: 1,
                anchor_stable: false
            }
        );
        assert_eq!(w.head_seq(), Some(SessionSeq::new(2)));
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
        assert_eq!(w.turn_outline()[1].seq, Some(SessionSeq::new(6)));
        assert_eq!(w.turn_outline()[1].response.as_deref(), Some("r2"));
    }

    #[test]
    fn event_without_seq_logged_and_skipped() {
        let mut w = TranscriptWindow::new(200);
        w.apply(Incoming::Snapshot {
            cursor: None,
            records: vec![],
            has_more: true,
            projections: None,
        });
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
        // After the touch, the order is correct for everyone except a.
        store.touch("b", 200);
        store.touch("e", 200);
        assert!(store.get("c").is_none());
        assert!(store.get("b").is_some());
    }

    // ---------- REQ-002 乐观回显与 requestId 对账（Step 3） ----------

    fn pending_texts(w: &TranscriptWindow) -> Vec<String> {
        w.pending().map(|e| e.text.clone()).collect()
    }

    #[test]
    fn echo_visible_and_reconciled_by_durable_event_ac002_06() {
        let mut w = TranscriptWindow::new(200);
        w.apply(Incoming::Snapshot {
            cursor: None,
            records: vec![],
            has_more: true,
            projections: None,
        });
        w.echo(SessionRequestId::new("req-1".into()), "hello");
        assert_eq!(pending_texts(&w), vec!["hello"], "pending 立即可见");
        assert_eq!(w.len(), 0, "pending 不占 durable blocks/seq");

        // durable user/message 同 requestId 到达：只保留一个 durable 块。
        assert_eq!(
            w.apply(Incoming::FollowEvent(ev(
                10,
                "user/message",
                Some("req-1"),
                None
            ))),
            ApplyEffect::TailAppended {
                appended: 1,
                anchor_stable: true
            }
        );
        assert!(
            pending_texts(&w).is_empty(),
            "durable 到达 → pending 对账移除"
        );
        assert_eq!(w.len(), 1);

        // 重复事件/回放：Noop，不重复显示。
        assert_eq!(
            w.apply(Incoming::FollowEvent(ev(
                10,
                "user/message",
                Some("req-1"),
                None
            ))),
            ApplyEffect::Noop
        );
        assert_eq!(w.len(), 1);
        assert!(pending_texts(&w).is_empty(), "回放不再生 pending");
    }

    #[test]
    fn pending_never_pollutes_seen_indexes() {
        // FAIL 条件反证：pending 若写入 seen_request，同 requestId 的 durable
        // 事件会被幂等去重吞掉 → 消息永久丢失。这里断言 durable 能正常落地。
        let mut w = TranscriptWindow::new(200);
        w.apply(Incoming::Snapshot {
            cursor: None,
            records: vec![],
            has_more: true,
            projections: None,
        });
        w.echo(SessionRequestId::new("req-9".into()), "text");
        assert_eq!(
            w.apply(Incoming::FollowEvent(ev(
                3,
                "user/message",
                Some("req-9"),
                None
            ))),
            ApplyEffect::TailAppended {
                appended: 1,
                anchor_stable: true
            },
            "durable 必须穿过请求去重门（pending 不占 seen 索引）"
        );
        assert_eq!(w.len(), 1);
        assert!(pending_texts(&w).is_empty());
    }

    #[test]
    fn snapshot_rebuild_reconciles_pending_without_duplicate() {
        let mut w = TranscriptWindow::new(200);
        w.echo(SessionRequestId::new("req-7".into()), "snap");
        // 重连快照含同 requestId：pending 对账移除，仅一个 durable 块。
        w.apply(Incoming::Snapshot {
            cursor: Some(SessionLogOffset::new(7)),
            records: vec![event_rec(7, "user/message", Some("req-7"))],
            has_more: false,
            projections: None,
        });
        assert!(pending_texts(&w).is_empty());
        assert_eq!(w.len(), 1);
        // 快照重放（同内容再来一次）→ 重建后仍只有一个块。
        w.apply(Incoming::Snapshot {
            cursor: Some(SessionLogOffset::new(7)),
            records: vec![event_rec(7, "user/message", Some("req-7"))],
            has_more: false,
            projections: None,
        });
        assert_eq!(w.len(), 1);

        // 快照不含该 requestId：pending 保留，后到 durable 再 retire。
        let mut w2 = TranscriptWindow::new(200);
        w2.echo(SessionRequestId::new("req-8".into()), "keep");
        w2.apply(Incoming::Snapshot {
            cursor: Some(SessionLogOffset::new(1)),
            records: vec![event_rec(1, "user/message", None)],
            has_more: false,
            projections: None,
        });
        assert_eq!(pending_texts(&w2), vec!["keep"]);
        w2.apply(Incoming::FollowEvent(ev(
            9,
            "user/message",
            Some("req-8"),
            None,
        )));
        assert!(pending_texts(&w2).is_empty());
        assert_eq!(w2.len(), 2);
    }

    #[test]
    fn fail_echo_marks_error_and_durable_later_retires_it() {
        let mut w = TranscriptWindow::new(200);
        w.echo(SessionRequestId::new("req-f".into()), "boom");
        w.fail_echo(
            &SessionRequestId::new("req-f".into()),
            "gateway/bad-request",
            "非法请求",
        );
        let echo = w.pending().next().unwrap();
        assert!(matches!(
            &echo.status,
            PendingEchoStatus::Failed { code, message }
                if code == "gateway/bad-request" && message == "非法请求"
        ));
        // 失败不自动重发：window 自身不产生任何新块/回显。
        assert_eq!(w.len(), 0);
        assert_eq!(w.pending().count(), 1);
        // 服务端最终仍提交（错误响应与提交竞态）：durable 到达 retire 失败回显。
        w.apply(Incoming::FollowEvent(ev(
            20,
            "user/message",
            Some("req-f"),
            None,
        )));
        assert!(
            pending_texts(&w).is_empty(),
            "失败回显被 durable 对账 retire"
        );
        assert_eq!(w.len(), 1);
    }

    #[test]
    fn official_nested_source_rpc_id_reconciles_too() {
        // 官方 durable user 消息的 requestId 位于 source（user-rpc.rpcId，
        // 2026-09-04 实读），顶层可能缺 requestId。
        let mut w = TranscriptWindow::new(200);
        w.echo(SessionRequestId::new("req-nested".into()), "hi");
        let event = ev(
            5,
            "user/message",
            None,
            Some(serde_json::json!({
                "content": "hi",
                "source": {"user-rpc": {"kind": "user", "rpcId": "req-nested"}}
            })),
        );
        assert_eq!(
            w.apply(Incoming::FollowEvent(event)),
            ApplyEffect::TailAppended {
                appended: 1,
                anchor_stable: true
            }
        );
        assert!(pending_texts(&w).is_empty(), "官方嵌套 source.rpcId 也对账");
        assert_eq!(w.len(), 1);
    }
}
