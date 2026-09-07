//! TrajectoryWindow —— 轨迹独立事件投影（REQ-005 §5，D-23=A；Notes/04 §3.6）。
//!
//! 设计要点（Step 1 Prototype 验证，`examples/proto_traj_step1.rs`）：
//! - 与 `TranscriptWindow` 同构的单写漏斗 `apply(TrajIncoming) -> TrajEffect`
//!   （snapshot 重建 / follow 尾追加 / page 前插 / 缺口重建全走该 seam，
//!   AppState 只消费 Effect）；镜像 `src/model/session.rs` 的成功实现
//!   （seq 单调 + requestId 幂等双去重、二分前插保序、逐出留 seq 锚）；
//! - `RowId` 单调不回收：前插/逐出/折叠重算后身份稳定（详情锚点防串、
//!   搜索命中跳转零抖动，DESIGN-IT-TWICE hybrid 约束）；
//! - `FoldState` 是纯数据（HashSet），`view(&FoldState)` 每帧纯函数重算
//!   （AC-005-12 折叠 + 并发 append 零额外并发状态）；fold 状态独立于窗口，
//!   事件追加只按「未折叠组可见/隐藏」渲染，展开天然完整不丢行；
//! - **view 逐行推入 + truncate**：turn 组折叠在 turn/end 处截断到组首；
//!   禁止「闭组时 range flush」——已推入的行会二次进视图（原型 v1 教训，
//!   见 TASK-005 `## 踩坑记录`）；
//! - 边界行（step/start、step/end、turn/start、turn/end、request/header、
//!   compaction 族）从原始 `SessionHistoryRecord` 直接保留投影，**不扩展**
//!   已交付 `TranscriptWindow`（D-23）；api 层零新增端点；
//! - 未知事件原样保留 + 入 tracing（`Notes/03 §7` 兼容动作，REQ-001 §5
//!   `Unknown` 延续）；事件字段映射按官方 wire 实读（tool/call{callId,
//!   name,arguments}、tool/result{message,error{name,code},meta}、
//!   request/header{reason}、usage 挂 assistant/message.usage，FR-005-02/03
//!   事实修正 2026-09-04）。

use std::collections::{HashMap, HashSet, VecDeque};

use serde_json::Value;

use crate::api::types::{
    ChunkRow, SessionHistoryRecord, SessionLogOffset, SessionSeq, SessionWireEvent,
};

/// 稳定行身份（单调不回收；窗口重建/前插/逐出后不变）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RowId(pub u64);

/// 行类型标签（渲染/搜索/折叠分派的公共口径）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrajKind {
    TurnStart,
    TurnEnd,
    StepStart,
    StepEnd,
    UserMessage,
    AssistantMessage,
    ToolCall,
    ToolResult,
    RequestHeader,
    Compaction,
    Unknown,
}

/// `tool/result.error{name,code}`（官方 wire，FR-005-02 事实修正）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TrajError {
    pub name: String,
    pub code: String,
}

/// `assistant/message.usage`（官方 wire：input/output/cacheRead/cacheWrite/
/// think；`[未验证]` 推导源由 UI 层标注，AC-005-15）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TrajUsage {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
    pub think: Option<u64>,
}

/// 轨迹投影行（REQ-005 §5 字段表，11 变体）。
#[derive(Debug, Clone)]
pub enum TrajectoryRow {
    TurnStart {
        id: RowId,
        seq: SessionSeq,
        turn: Option<u64>,
        time: Option<i64>,
        reason: Option<String>,
    },
    /// turn/end 可带 error（官方 turn-end error，FR-005-02 派生展示）。
    TurnEnd {
        id: RowId,
        seq: SessionSeq,
        turn: Option<u64>,
        time: Option<i64>,
        reason: Option<String>,
        error: Option<TrajError>,
    },
    StepStart {
        id: RowId,
        seq: SessionSeq,
        turn: Option<u64>,
        step: Option<u64>,
        time: Option<i64>,
        /// step/start.reason（如 "max" 等摘要）。
        reason: Option<String>,
    },
    StepEnd {
        id: RowId,
        seq: SessionSeq,
        turn: Option<u64>,
        step: Option<u64>,
        time: Option<i64>,
    },
    UserMessage {
        id: RowId,
        seq: SessionSeq,
        time: Option<i64>,
        /// 消息摘要（content 文本，≤80 字符口径，Notes/05 §7 行）。
        summary: String,
    },
    AssistantMessage {
        id: RowId,
        seq: SessionSeq,
        turn: Option<u64>,
        step: Option<u64>,
        time: Option<i64>,
        /// 摘要（content 或 packed text chunks 汇总）。
        summary: String,
        /// reasoning chunks 存在（FR-005-04 折叠口径）。
        has_reasoning: bool,
        /// 同 step assistant/message.usage（无 per-tool，D-24）。
        usage: Option<TrajUsage>,
    },
    ToolCall {
        id: RowId,
        seq: SessionSeq,
        turn: Option<u64>,
        step: Option<u64>,
        time: Option<i64>,
        call_id: Option<String>,
        name: Option<String>,
        /// `arguments` 原始 JSON 字符串（未解析，展示需格式化，D-24）。
        args_raw: Option<Value>,
    },
    ToolResult {
        id: RowId,
        seq: SessionSeq,
        turn: Option<u64>,
        step: Option<u64>,
        time: Option<i64>,
        call_id: Option<String>,
        is_error: bool,
        summary: String,
        /// `tool/result.meta` 原始载荷（工具私有 diff 唯一来源，AC-005-10；
        /// 保留原文供 TrajectoryDetail.diff 提取，`meta_has_diff()` 派生）。
        meta: Option<Value>,
        error: Option<TrajError>,
    },
    RequestHeader {
        id: RowId,
        seq: SessionSeq,
        time: Option<i64>,
        /// reason: initial/resume/change/series。
        reason: Option<String>,
        summary: String,
    },
    /// compaction 族（compaction/start|end|prune|summary）降级展示。
    Compaction {
        id: RowId,
        seq: SessionSeq,
        time: Option<i64>,
        summary: String,
    },
    /// 未知事件：event_type + 原始 payload 原样保留（绝不丢弃）。
    Unknown {
        id: RowId,
        seq: SessionSeq,
        time: Option<i64>,
        event_type: String,
        raw: Value,
    },
}

impl TrajectoryRow {
    pub fn id(&self) -> RowId {
        match self {
            TrajectoryRow::TurnStart { id, .. }
            | TrajectoryRow::TurnEnd { id, .. }
            | TrajectoryRow::StepStart { id, .. }
            | TrajectoryRow::StepEnd { id, .. }
            | TrajectoryRow::UserMessage { id, .. }
            | TrajectoryRow::AssistantMessage { id, .. }
            | TrajectoryRow::ToolCall { id, .. }
            | TrajectoryRow::ToolResult { id, .. }
            | TrajectoryRow::RequestHeader { id, .. }
            | TrajectoryRow::Compaction { id, .. }
            | TrajectoryRow::Unknown { id, .. } => *id,
        }
    }

    pub fn seq(&self) -> SessionSeq {
        match self {
            TrajectoryRow::TurnStart { seq, .. }
            | TrajectoryRow::TurnEnd { seq, .. }
            | TrajectoryRow::StepStart { seq, .. }
            | TrajectoryRow::StepEnd { seq, .. }
            | TrajectoryRow::UserMessage { seq, .. }
            | TrajectoryRow::AssistantMessage { seq, .. }
            | TrajectoryRow::ToolCall { seq, .. }
            | TrajectoryRow::ToolResult { seq, .. }
            | TrajectoryRow::RequestHeader { seq, .. }
            | TrajectoryRow::Compaction { seq, .. }
            | TrajectoryRow::Unknown { seq, .. } => *seq,
        }
    }

    pub fn kind(&self) -> TrajKind {
        match self {
            TrajectoryRow::TurnStart { .. } => TrajKind::TurnStart,
            TrajectoryRow::TurnEnd { .. } => TrajKind::TurnEnd,
            TrajectoryRow::StepStart { .. } => TrajKind::StepStart,
            TrajectoryRow::StepEnd { .. } => TrajKind::StepEnd,
            TrajectoryRow::UserMessage { .. } => TrajKind::UserMessage,
            TrajectoryRow::AssistantMessage { .. } => TrajKind::AssistantMessage,
            TrajectoryRow::ToolCall { .. } => TrajKind::ToolCall,
            TrajectoryRow::ToolResult { .. } => TrajKind::ToolResult,
            TrajectoryRow::RequestHeader { .. } => TrajKind::RequestHeader,
            TrajectoryRow::Compaction { .. } => TrajKind::Compaction,
            TrajectoryRow::Unknown { .. } => TrajKind::Unknown,
        }
    }

    pub fn time(&self) -> Option<i64> {
        match self {
            TrajectoryRow::TurnStart { time, .. }
            | TrajectoryRow::TurnEnd { time, .. }
            | TrajectoryRow::StepStart { time, .. }
            | TrajectoryRow::StepEnd { time, .. }
            | TrajectoryRow::UserMessage { time, .. }
            | TrajectoryRow::AssistantMessage { time, .. }
            | TrajectoryRow::ToolCall { time, .. }
            | TrajectoryRow::ToolResult { time, .. }
            | TrajectoryRow::RequestHeader { time, .. }
            | TrajectoryRow::Compaction { time, .. }
            | TrajectoryRow::Unknown { time, .. } => *time,
        }
    }

    pub fn turn(&self) -> Option<u64> {
        match self {
            TrajectoryRow::TurnStart { turn, .. }
            | TrajectoryRow::TurnEnd { turn, .. }
            | TrajectoryRow::StepStart { turn, .. }
            | TrajectoryRow::StepEnd { turn, .. }
            | TrajectoryRow::AssistantMessage { turn, .. }
            | TrajectoryRow::ToolCall { turn, .. }
            | TrajectoryRow::ToolResult { turn, .. } => *turn,
            _ => None,
        }
    }

    pub fn step(&self) -> Option<u64> {
        match self {
            TrajectoryRow::StepStart { step, .. }
            | TrajectoryRow::StepEnd { step, .. }
            | TrajectoryRow::AssistantMessage { step, .. }
            | TrajectoryRow::ToolCall { step, .. }
            | TrajectoryRow::ToolResult { step, .. } => *step,
            _ => None,
        }
    }

    pub fn summary(&self) -> &str {
        match self {
            TrajectoryRow::UserMessage { summary, .. }
            | TrajectoryRow::AssistantMessage { summary, .. }
            | TrajectoryRow::ToolResult { summary, .. }
            | TrajectoryRow::RequestHeader { summary, .. }
            | TrajectoryRow::Compaction { summary, .. } => summary,
            TrajectoryRow::TurnEnd {
                reason: Some(r), ..
            } => r.as_str(),
            TrajectoryRow::StepStart {
                reason: Some(r), ..
            } => r.as_str(),
            _ => "",
        }
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            TrajectoryRow::TurnStart { reason, .. }
            | TrajectoryRow::TurnEnd { reason, .. }
            | TrajectoryRow::StepStart { reason, .. }
            | TrajectoryRow::RequestHeader { reason, .. } => reason.as_deref(),
            _ => None,
        }
    }

    pub fn call_id(&self) -> Option<&str> {
        match self {
            TrajectoryRow::ToolCall { call_id, .. } | TrajectoryRow::ToolResult { call_id, .. } => {
                call_id.as_deref()
            }
            _ => None,
        }
    }

    pub fn name(&self) -> Option<&str> {
        match self {
            TrajectoryRow::ToolCall { name, .. } => name.as_deref(),
            _ => None,
        }
    }

    pub fn args_raw(&self) -> Option<&Value> {
        match self {
            TrajectoryRow::ToolCall { args_raw, .. } => args_raw.as_ref(),
            _ => None,
        }
    }

    pub fn is_error(&self) -> bool {
        match self {
            TrajectoryRow::ToolResult { is_error, .. } => *is_error,
            TrajectoryRow::TurnEnd { error, .. } => error.is_some(),
            _ => false,
        }
    }

    pub fn meta_has_diff(&self) -> bool {
        matches!(self, TrajectoryRow::ToolResult { meta: Some(_), .. })
    }

    /// `tool/result.meta` 原始载荷（详情 diff 提取源）。
    pub fn meta(&self) -> Option<&Value> {
        match self {
            TrajectoryRow::ToolResult { meta, .. } => meta.as_ref(),
            _ => None,
        }
    }

    pub fn error(&self) -> Option<&TrajError> {
        match self {
            TrajectoryRow::ToolResult { error, .. } | TrajectoryRow::TurnEnd { error, .. } => {
                error.as_ref()
            }
            _ => None,
        }
    }

    pub fn usage(&self) -> Option<&TrajUsage> {
        match self {
            TrajectoryRow::AssistantMessage { usage, .. } => usage.as_ref(),
            _ => None,
        }
    }

    pub fn has_reasoning(&self) -> bool {
        matches!(
            self,
            TrajectoryRow::AssistantMessage {
                has_reasoning: true,
                ..
            }
        )
    }

    pub fn event_type(&self) -> &str {
        match self {
            TrajectoryRow::Unknown { event_type, .. } => event_type,
            _ => "",
        }
    }

    pub fn raw(&self) -> Option<&Value> {
        match self {
            TrajectoryRow::Unknown { raw, .. } => Some(raw),
            _ => None,
        }
    }
}

/// 折叠组标识（D-25 键位 `z`/`za` 折叠 turn、assistant 组；reasoning chunks
/// 挂 assistant 组，不单设组——FR-005-04 由 assistant 折叠覆盖）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GroupId {
    Turn(u64),
    Assistant { turn: u64, step: u64 },
}

/// 折叠状态（纯数据：在集合 = 折叠；AppState 单写多读，`02 §3`）。
#[derive(Debug, Clone, Default)]
pub struct FoldState {
    collapsed: HashSet<GroupId>,
}

impl FoldState {
    pub fn is_collapsed(&self, group: GroupId) -> bool {
        self.collapsed.contains(&group)
    }

    /// 切换折叠并返回切换后的状态（true = 折叠）。
    pub fn toggle(&mut self, group: GroupId) -> bool {
        if self.collapsed.contains(&group) {
            self.collapsed.remove(&group);
            false
        } else {
            self.collapsed.insert(group);
            true
        }
    }
}

/// 输入漏斗（AppState 将 api 事件映射到该类型）。
#[derive(Debug, Clone)]
pub enum TrajIncoming {
    Snapshot {
        cursor: Option<SessionLogOffset>,
        records: Vec<SessionHistoryRecord>,
        has_more: bool,
        projections: Option<Value>,
    },
    FollowEvent(SessionWireEvent),
    /// 独立 packed chunk row（挂最近 assistant/tool 行）。
    Chunks(ChunkRow),
    Page {
        records: Vec<SessionHistoryRecord>,
        has_more: Option<bool>,
    },
}

/// apply 效果（AppState 用其触发回填/重跟随与视口平移）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrajEffect {
    Rebuilt,
    TailAppended {
        appended: usize,
    },
    HeadPrepend {
        inserted: usize,
        anchor_shift: usize,
    },
    Noop,
}

/// 窗口化轨迹投影（上限对齐 REQ-001 `window_messages` 语义，默认 200）。
#[derive(Debug, Clone)]
pub struct TrajectoryWindow {
    rows: VecDeque<TrajectoryRow>,
    cap: usize,
    head_has_more: bool,
    cursor: Option<SessionLogOffset>,
    projections: Value,
    seen_seq: HashSet<u64>,
    seen_request: HashSet<String>,
    next_id: u64,
    /// 逐出最旧后保留的 seq 锚：已逐出 seq 的 page 重叠直接丢弃。
    evicted_head_seq: Option<u64>,
}

impl Default for TrajectoryWindow {
    fn default() -> Self {
        Self::new(200)
    }
}

impl TrajectoryWindow {
    pub fn new(cap: usize) -> Self {
        Self {
            rows: VecDeque::new(),
            cap: cap.max(1),
            head_has_more: true,
            cursor: None,
            projections: Value::Null,
            seen_seq: HashSet::new(),
            seen_request: HashSet::new(),
            next_id: 1,
            evicted_head_seq: None,
        }
    }

    // ---------- 写侧：单漏斗 ----------

    pub fn apply(&mut self, incoming: TrajIncoming) -> TrajEffect {
        match incoming {
            TrajIncoming::Snapshot {
                cursor,
                records,
                has_more,
                projections,
            } => self.apply_snapshot(cursor, records, has_more, projections),
            TrajIncoming::FollowEvent(ev) => self.apply_event(&ev),
            TrajIncoming::Chunks(row) => self.apply_chunks(&row),
            TrajIncoming::Page { records, has_more } => self.apply_page(records, has_more),
        }
    }

    fn apply_snapshot(
        &mut self,
        cursor: Option<SessionLogOffset>,
        records: Vec<SessionHistoryRecord>,
        has_more: bool,
        projections: Option<Value>,
    ) -> TrajEffect {
        // 全窗重建：清行与索引（缺口不可修复 → 重建语义，Notes/06 §2）；
        // RowId 分配源 next_id 不重置（身份单调不回收）。
        self.rows.clear();
        self.seen_seq.clear();
        self.seen_request.clear();
        self.cursor = cursor;
        self.head_has_more = has_more;
        self.projections = projections.unwrap_or(Value::Null);
        self.evicted_head_seq = None;
        for rec in records {
            self.ingest_record(rec);
        }
        self.evict();
        TrajEffect::Rebuilt
    }

    fn apply_event(&mut self, ev: &SessionWireEvent) -> TrajEffect {
        let Some(seq) = ev.seq else {
            tracing::warn!(event_type = %ev.event_type, "轨迹事件缺少 seq，跳过（计入日志）");
            return TrajEffect::Noop;
        };
        // requestId 幂等先于 seq 去重（D-4 口径，镜像 TranscriptWindow）。
        if let Some(rid) = ev.request_id.as_deref() {
            if self.seen_request.contains(rid) {
                return TrajEffect::Noop;
            }
        }
        if self.seen_seq.contains(&seq.0) {
            return TrajEffect::Noop;
        }
        self.seen_seq.insert(seq.0);
        if let Some(rid) = ev.request_id.as_deref() {
            self.seen_request.insert(rid.to_string());
        }
        let row = row_from_event(ev, seq, self.next_id);
        self.next_id += 1;
        if self
            .rows
            .back()
            .map(|r| r.seq())
            .map_or(true, |tail| seq > tail)
        {
            self.rows.push_back(row);
        } else {
            let idx = self.rows.partition_point(|r| r.seq() < seq);
            self.rows.insert(idx, row);
        }
        self.evict();
        TrajEffect::TailAppended { appended: 1 }
    }

    fn apply_chunks(&mut self, row: &ChunkRow) -> TrajEffect {
        // chunks 无独立 seq：文本/推理 chunks 挂最近 assistant 行（官方顺序
        // 保证紧随其后）；工具 chunks 挂最近 tool/call 行补 name/args。
        let mut touched = false;
        match row {
            ChunkRow::ReasoningChunks(d) => {
                if let Some(asst) = self.rows.iter_mut().rev().find_map(|r| match r {
                    TrajectoryRow::AssistantMessage { .. } => Some(r),
                    _ => None,
                }) {
                    if let TrajectoryRow::AssistantMessage {
                        has_reasoning,
                        summary,
                        ..
                    } = asst
                    {
                        *has_reasoning = true;
                        let text = d.texts.join("");
                        if !text.is_empty() && summary.is_empty() {
                            *summary = text;
                        }
                    }
                    touched = true;
                }
            }
            ChunkRow::TextChunks(d) => {
                if let Some(asst) = self.rows.iter_mut().rev().find_map(|r| match r {
                    TrajectoryRow::AssistantMessage { .. } => Some(r),
                    _ => None,
                }) {
                    if let TrajectoryRow::AssistantMessage { summary, .. } = asst {
                        if summary.is_empty() {
                            *summary = d.texts.join("");
                        } else {
                            summary.push_str(&d.texts.join(""));
                        }
                    }
                    touched = true;
                }
            }
            ChunkRow::ToolCallChunks(t) => {
                if let Some(call) = self.rows.iter_mut().rev().find_map(|r| match r {
                    TrajectoryRow::ToolCall { .. } => Some(r),
                    _ => None,
                }) {
                    if let TrajectoryRow::ToolCall { name, args_raw, .. } = call {
                        if name.is_none() {
                            *name = t.name.clone();
                        }
                        if args_raw.is_none() {
                            *args_raw = t.args.clone();
                        }
                    }
                    touched = true;
                }
            }
            ChunkRow::Unknown { .. } => {
                tracing::warn!("轨迹未知 chunkrow 类型：不入窗口（原样容忍，03 §7）");
            }
        }
        if touched {
            TrajEffect::TailAppended { appended: 0 }
        } else {
            tracing::warn!("chunkrow 到达但无 assistant/tool 宿主，丢弃并计入日志");
            TrajEffect::Noop
        }
    }

    fn apply_page(
        &mut self,
        records: Vec<SessionHistoryRecord>,
        has_more: Option<bool>,
    ) -> TrajEffect {
        if let Some(hm) = has_more {
            self.head_has_more = hm;
        }
        let head_before = self.head_seq();
        let mut inserted = 0usize;
        for rec in records {
            if let Some(seq) = record_seq(&rec) {
                if self.seen_seq.contains(&seq.0)
                    || self.evicted_head_seq.is_some_and(|anchor| seq.0 <= anchor)
                {
                    continue;
                }
                if let Some(rid) = record_request_id(&rec) {
                    if self.seen_request.contains(rid) {
                        continue;
                    }
                }
            }
            inserted += self.ingest_record(rec);
        }
        if inserted == 0 {
            return TrajEffect::Noop;
        }
        // 只统计真正移动旧头的记录（乱序中插不移动视口锚点，镜像先例）。
        let anchor_shift = head_before
            .map(|head| self.rows.iter().take_while(|r| r.seq() < head).count())
            .unwrap_or(0);
        self.evict();
        TrajEffect::HeadPrepend {
            inserted,
            anchor_shift,
        }
    }

    fn ingest_record(&mut self, rec: SessionHistoryRecord) -> usize {
        match &rec {
            SessionHistoryRecord::Event { event } => {
                let Some(seq) = event.seq else {
                    tracing::warn!(event_type = %event.event_type, "轨迹 event 缺 seq；跳过");
                    return 0;
                };
                if self.seen_seq.contains(&seq.0) {
                    return 0;
                }
                if let Some(rid) = event.request_id.as_deref() {
                    if self.seen_request.contains(rid) {
                        return 0;
                    }
                    self.seen_request.insert(rid.to_string());
                }
                self.seen_seq.insert(seq.0);
                let row = row_from_event(event, seq, self.next_id);
                self.next_id += 1;
                let idx = self.rows.partition_point(|r| r.seq() < seq);
                self.rows.insert(idx, row);
                1
            }
            SessionHistoryRecord::Chunks { event } => {
                let _ = self.apply_chunks(event);
                0
            }
        }
    }

    fn evict(&mut self) {
        while self.rows.len() > self.cap {
            if let Some(old) = self.rows.pop_front() {
                self.evicted_head_seq = Some(old.seq().0);
            }
        }
    }

    // ---------- 读侧：view 折叠重算 / 身份访问 ----------

    /// 折叠视图重算（每帧纯函数，O(n≤200)；零缓存零并发状态，AC-005-12/13）。
    ///
    /// 折叠判定按**行自身 (turn,step) 查表**而非顺序状态：组成员行（tool/
    /// call、tool/result）在组被折叠时隐藏、组首 assistant 恒可见——乱序
    /// 修复、turn 边界外并发 append 的事件同样正确入组（AC-005-12 折叠与
    /// 并发 append 竞态）。
    pub fn view(&self, fold: &FoldState) -> Vec<&TrajectoryRow> {
        let mut out = Vec::with_capacity(self.rows.len());
        let mut turn_start: Option<usize> = None;
        for (i, row) in self.rows.iter().enumerate() {
            match row.kind() {
                TrajKind::TurnStart => {
                    turn_start = Some(i);
                    out.push(row);
                }
                TrajKind::ToolCall | TrajKind::ToolResult => {
                    if assistant_group_collapsed(row, fold) {
                        continue; // 折叠组成员行隐藏（组首恒可见）
                    }
                    out.push(row);
                }
                TrajKind::TurnEnd => {
                    out.push(row);
                    if let Some(s) = turn_start.take() {
                        if self
                            .rows
                            .get(s)
                            .and_then(|r| r.turn())
                            .is_some_and(|t| fold.is_collapsed(GroupId::Turn(t)))
                        {
                            // turn 折叠：仅保留组首（turn/start）；组首恒已在 out。
                            if let Some(keep) = out.iter().position(|r| r.id() == self.rows[s].id())
                            {
                                out.truncate(keep + 1);
                            }
                        }
                    }
                }
                _ => out.push(row),
            }
        }
        // 流式进行中的最后一个 turn（无 turn/end 边界）同样折叠处理。
        if let Some(s) = turn_start {
            if self
                .rows
                .get(s)
                .and_then(|r| r.turn())
                .is_some_and(|t| fold.is_collapsed(GroupId::Turn(t)))
            {
                if let Some(keep) = out.iter().position(|r| r.id() == self.rows[s].id()) {
                    out.truncate(keep + 1);
                }
            }
        }
        out
    }

    /// 窗口原始行（窗口顺序，≤cap）。
    pub fn raw_rows(&self) -> impl Iterator<Item = &TrajectoryRow> {
        self.rows.iter()
    }

    /// RowId 稳定寻址（详情锚点/搜索跳转）。
    pub fn row(&self, id: RowId) -> Option<&TrajectoryRow> {
        self.rows.iter().find(|r| r.id() == id)
    }

    /// 行所属折叠组（`z`/`za` 判定；user/边界/未知行无组）。
    pub fn group_of(&self, id: RowId) -> Option<GroupId> {
        let r = self.row(id)?;
        match r.kind() {
            TrajKind::TurnStart | TrajKind::TurnEnd => r.turn().map(GroupId::Turn),
            TrajKind::AssistantMessage | TrajKind::ToolCall | TrajKind::ToolResult => {
                match (r.turn(), r.step()) {
                    (Some(t), Some(s)) => Some(GroupId::Assistant { turn: t, step: s }),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// `z`/`za`：折叠行所在组并返回切换后状态（true = 折叠）。
    pub fn toggle_group(&self, fold: &mut FoldState, id: RowId) -> bool {
        match self.group_of(id) {
            Some(group) => fold.toggle(group),
            None => false,
        }
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn head_seq(&self) -> Option<SessionSeq> {
        self.rows.front().map(|r| r.seq())
    }

    pub fn tail_seq(&self) -> Option<SessionSeq> {
        self.rows.back().map(|r| r.seq())
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
}

/// 行的 assistant 组是否折叠（无 turn/step → 不可折叠，恒可见）。
fn assistant_group_collapsed(row: &TrajectoryRow, fold: &FoldState) -> bool {
    match (row.turn(), row.step()) {
        (Some(t), Some(s)) => fold.is_collapsed(GroupId::Assistant { turn: t, step: s }),
        _ => false,
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

/// 官方 wire 字段映射（FR-005-02/03 事实修正，2026-09-04 实读）：
/// - tool/call{turn,step,callId,name,arguments}（arguments 为模型原始 JSON 字符串）
/// - tool/result{message,error{name,code},meta}（meta 为工具私有 diff 载荷）
/// - usage 无 per-tool：挂同 step `assistant/message.usage`
/// - turn/end 可带 error；request/header{reason: initial/resume/change/series}
/// - compaction 族（compaction/summary 承载摘要，降级展示）
fn row_from_event(ev: &SessionWireEvent, seq: SessionSeq, id: u64) -> TrajectoryRow {
    let data = ev.data.clone().unwrap_or(Value::Null);
    let time = ev.time;
    let row_id = RowId(id);
    let get_u64 = |k: &str| data.get(k).and_then(Value::as_u64);
    let get_str = |k: &str| -> Option<String> {
        data.get(k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    let parse_usage = || -> Option<TrajUsage> {
        let u = data.get("usage")?;
        Some(TrajUsage {
            input: u.get("input").and_then(Value::as_u64),
            output: u.get("output").and_then(Value::as_u64),
            cache_read: u
                .get("cacheRead")
                .or_else(|| u.get("cache_read"))
                .and_then(Value::as_u64),
            cache_write: u
                .get("cacheWrite")
                .or_else(|| u.get("cache_write"))
                .and_then(Value::as_u64),
            think: u.get("think").and_then(Value::as_u64),
        })
    };
    let parse_error = || -> Option<TrajError> {
        let e = data.get("error")?;
        Some(TrajError {
            name: e
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("error")
                .to_string(),
            code: e
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        })
    };
    match ev.event_type.as_str() {
        "turn/start" => TrajectoryRow::TurnStart {
            id: row_id,
            seq,
            turn: get_u64("turn"),
            time,
            reason: get_str("reason"),
        },
        "turn/end" => TrajectoryRow::TurnEnd {
            id: row_id,
            seq,
            turn: get_u64("turn"),
            time,
            reason: get_str("reason"),
            error: parse_error(),
        },
        "step/start" => TrajectoryRow::StepStart {
            id: row_id,
            seq,
            turn: get_u64("turn"),
            step: get_u64("step"),
            time,
            reason: get_str("reason"),
        },
        "step/end" => TrajectoryRow::StepEnd {
            id: row_id,
            seq,
            turn: get_u64("turn"),
            step: get_u64("step"),
            time,
        },
        "user/message" => TrajectoryRow::UserMessage {
            id: row_id,
            seq,
            time,
            summary: user_summary(&data),
        },
        "assistant/message" => TrajectoryRow::AssistantMessage {
            id: row_id,
            seq,
            turn: get_u64("turn"),
            step: get_u64("step"),
            time,
            summary: get_str("content").unwrap_or_default(),
            has_reasoning: false,
            usage: parse_usage(),
        },
        "tool/call" => TrajectoryRow::ToolCall {
            id: row_id,
            seq,
            turn: get_u64("turn"),
            step: get_u64("step"),
            time,
            call_id: get_str("callId").or_else(|| get_str("id")),
            name: get_str("name"),
            args_raw: data
                .get("arguments")
                .cloned()
                .or_else(|| data.get("args").cloned()),
        },
        "tool/result" => TrajectoryRow::ToolResult {
            id: row_id,
            seq,
            turn: get_u64("turn"),
            step: get_u64("step"),
            time,
            call_id: get_str("callId").or_else(|| get_str("id")),
            is_error: parse_error().is_some(),
            summary: get_str("message").unwrap_or_default(),
            meta: data.get("meta").cloned().filter(|m| !m.is_null()),
            error: parse_error(),
        },
        "request/header" => TrajectoryRow::RequestHeader {
            id: row_id,
            seq,
            time,
            reason: get_str("reason"),
            summary: get_str("reason")
                .or_else(|| get_str("summary"))
                .unwrap_or_default(),
        },
        t if t.starts_with("compaction/") => TrajectoryRow::Compaction {
            id: row_id,
            seq,
            time,
            summary: get_str("summary")
                .or_else(|| get_str("text"))
                .unwrap_or_default(),
        },
        _ => TrajectoryRow::Unknown {
            id: row_id,
            seq,
            time,
            event_type: ev.event_type.clone(),
            raw: data,
        },
    }
}

/// user/message 摘要：content 文本（≤80 字符，数组 content 拼接 text 部分）。
fn user_summary(data: &Value) -> String {
    if let Some(s) = data.get("content").and_then(Value::as_str) {
        return s.chars().take(80).collect();
    }
    if let Some(parts) = data.get("content").and_then(Value::as_array) {
        let mut out = String::new();
        for part in parts {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                out.push_str(text);
            }
        }
        return out.chars().take(80).collect();
    }
    String::new()
}

/// 多会话轨迹窗口缓存（LRU 3，镜像 SessionStore 模式，REQ-005 §5）。
#[derive(Debug, Default)]
pub struct TrajectoryStore {
    windows: HashMap<String, TrajectoryWindow>,
    order: VecDeque<String>,
    cap: usize,
}

impl TrajectoryStore {
    pub fn new(cap: usize) -> Self {
        Self {
            windows: HashMap::new(),
            order: VecDeque::new(),
            cap,
        }
    }

    pub fn get(&self, id: &str) -> Option<&TrajectoryWindow> {
        self.windows.get(id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut TrajectoryWindow> {
        self.windows.get_mut(id)
    }

    /// 打开/触碰会话窗口（缺失即建，LRU 逐出最久未用）。
    pub fn touch(&mut self, id: &str, cap: usize) -> &mut TrajectoryWindow {
        if !self.windows.contains_key(id) {
            if self.order.len() >= self.cap.max(1) {
                if let Some(old) = self.order.pop_front() {
                    self.windows.remove(&old);
                }
            }
            self.windows
                .insert(id.to_string(), TrajectoryWindow::new(cap));
        } else if let Some(pos) = self.order.iter().position(|x| x == id) {
            self.order.remove(pos);
        }
        self.order.push_back(id.to_string());
        self.windows.get_mut(id).expect("刚插入")
    }
}

// ============================================================================
// TrajectoryDetail —— 详情面板字段（REQ-005 §5 字段表，D-24=A；官方
// `dsh-client-ui-trajectory` `TrajectoryCellProps`/`AssistantMetricDetail`
// 实读命名映射 [验证]）。构建为纯函数 `detail_for(row, window)`，无 IO：
// 详情/复制内容不落盘、不进日志明文（R8/`06 §9`，AC-005-11）。
// ============================================================================

/// timing 字段（事件 `time` 推导，`[未验证]` 推导源由 UI 层标注，AC-005-15；
/// 参考官方 trajectory `AssistantMetricDetail` 的
/// stepStart/firstToken/completedTime 推导）。
#[derive(Debug, Clone, PartialEq)]
pub struct TrajTiming {
    /// 详情锚点行自身 time（毫秒）。
    pub started_at: Option<i64>,
    /// completed - started_at（秒，推导；无 completed 时为 None）。
    pub time_seconds: Option<f64>,
    /// 同 step 的 step/start.time。
    pub step_start: Option<i64>,
    /// 同 step 的 assistant/message.time（首个 token 推导近似）。
    pub first_token: Option<i64>,
    /// 同 callId 的 tool/result.time（或同 step 的 step/end.time）。
    pub completed: Option<i64>,
}

/// 详情面板数据（source_seq/source_kind 锚点防串详情；选行变化即重建）。
#[derive(Debug, Clone, PartialEq)]
pub struct TrajectoryDetail {
    pub source_seq: SessionSeq,
    pub source_kind: TrajKind,
    /// 面板标题（如 `tool/call bash` / `tool/result` / `assistant`）。
    pub title: String,
    /// tool/call `arguments` 格式化 JSON（`y` 复制目标，D-24）。
    pub args_text: Option<String>,
    /// tool/result `message` 文本（assistant 行 = 消息摘要）。
    pub result_text: Option<String>,
    /// tool/result.error（与 result 并列展示，AC-005-03 含错误展示）。
    pub error: Option<TrajError>,
    /// `tool/result.meta` 内 diff（仅存在时；否则 UI 显示「无 diff」，
    /// AC-005-10）。
    pub diff: Option<String>,
    /// 同 step `assistant/message.usage` 推导（无 per-tool；`[未验证]`）。
    pub usage: Option<TrajUsage>,
    /// 事件 `time` 推导（`[未验证]`）。
    pub timing: Option<TrajTiming>,
    /// 原始块顺序展示（assistant = [摘要]；tool 行空，V0.3 不逐块）。
    pub source_blocks: Vec<String>,
}

impl TrajectoryDetail {
    /// 复制目标 = args/result 纯文本（AC-005-11；不落盘，由纯函数无 IO
    /// 保证；执行链走 REQ-003 yank 后端 arboard→OSC52→tmux）。
    pub fn yank_text(&self) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(args) = &self.args_text {
            parts.push(args.clone());
        }
        if let Some(result) = &self.result_text {
            if !parts.is_empty() {
                parts.push("\n\n".to_string());
            }
            parts.push(result.clone());
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.concat())
        }
    }
}

/// 行 → 详情。仅 ToolCall/ToolResult/AssistantMessage 提供详情（REQ §4/
/// AC-005-15）；其余行返回 None。
///
/// 同 step / 同 callId 的聚合数据从窗口行序列推导（`[未验证]` 口径）：
/// - ToolCall：args + 同 callId ToolResult（result/error/diff）+ 同 step
///   usage/timing；
/// - ToolResult：result/error/diff + 同 step usage/timing；
/// - AssistantMessage：摘要 + usage + timing（completed = 同 step
///   step/end.time，无则 None）。
pub fn detail_for(row: &TrajectoryRow, window: &TrajectoryWindow) -> Option<TrajectoryDetail> {
    match row {
        TrajectoryRow::ToolCall {
            seq,
            turn,
            step,
            time,
            name,
            args_raw,
            call_id,
            ..
        } => {
            let result = window
                .raw_rows()
                // 只匹配同 callId 的 tool/result（排除 call 行自身：其
                // callId 相同且窗口序更前）。
                .find(|r| r.kind() == TrajKind::ToolResult && same_call(r, call_id.as_deref()))
                .cloned();
            let usage = window
                .raw_rows()
                .find(|r| same_step(r, *turn, *step) && r.kind() == TrajKind::AssistantMessage)
                .and_then(|r| r.usage().cloned());
            Some(TrajectoryDetail {
                source_seq: *seq,
                source_kind: TrajKind::ToolCall,
                title: format!("tool/call {}", name.as_deref().unwrap_or("tool")),
                args_text: args_raw.as_ref().map(format_args_json),
                result_text: result.as_ref().and_then(|r| match r {
                    TrajectoryRow::ToolResult { summary, .. } => Some(summary.clone()),
                    _ => None,
                }),
                error: result.as_ref().and_then(|r| r.error().cloned()),
                diff: result.as_ref().and_then(extract_diff),
                usage,
                timing: timing_for(
                    *time,
                    window,
                    *turn,
                    *step,
                    result.as_ref().and_then(|r| r.time()),
                ),
                source_blocks: Vec::new(),
            })
        }
        TrajectoryRow::ToolResult {
            seq,
            turn,
            step,
            time,
            call_id,
            summary,
            error,
            meta,
            ..
        } => {
            let usage = window
                .raw_rows()
                .find(|r| same_step(r, *turn, *step) && r.kind() == TrajKind::AssistantMessage)
                .and_then(|r| r.usage().cloned());
            let completed = *time;
            Some(TrajectoryDetail {
                source_seq: *seq,
                source_kind: TrajKind::ToolResult,
                title: format!("tool/result {}", call_id.as_deref().unwrap_or(""))
                    .trim_end()
                    .to_string(),
                args_text: None,
                result_text: Some(summary.clone()),
                error: error.clone(),
                diff: meta.as_ref().and_then(extract_diff_from_value),
                usage,
                timing: timing_for(*time, window, *turn, *step, completed),
                source_blocks: Vec::new(),
            })
        }
        TrajectoryRow::AssistantMessage {
            seq,
            turn,
            step,
            time,
            summary,
            usage,
            ..
        } => {
            let completed = window
                .raw_rows()
                .find(|r| same_step(r, *turn, *step) && r.kind() == TrajKind::StepEnd)
                .and_then(|r| r.time());
            Some(TrajectoryDetail {
                source_seq: *seq,
                source_kind: TrajKind::AssistantMessage,
                title: "assistant".to_string(),
                args_text: None,
                result_text: (!summary.is_empty()).then(|| summary.clone()),
                error: None,
                diff: None,
                usage: usage.clone(),
                timing: timing_for(*time, window, *turn, *step, completed),
                source_blocks: if summary.is_empty() {
                    Vec::new()
                } else {
                    vec![summary.clone()]
                },
            })
        }
        _ => None,
    }
}

fn same_call(row: &TrajectoryRow, call_id: Option<&str>) -> bool {
    match call_id {
        Some(id) => row.call_id() == Some(id),
        None => false,
    }
}

fn same_step(row: &TrajectoryRow, turn: Option<u64>, step: Option<u64>) -> bool {
    row.turn() == turn && row.step() == step
}

/// tool/result.meta 内 diff 提取（`[未验证]` 口径：仅 meta.diff 键；字符串
/// 原样、对象/数组 JSON 文本化；无 diff 键 → None =「无 diff」降级）。
fn extract_diff(row: &TrajectoryRow) -> Option<String> {
    match row {
        TrajectoryRow::ToolResult { meta, .. } => meta.as_ref().and_then(extract_diff_from_value),
        _ => None,
    }
}

fn extract_diff_from_value(meta: &Value) -> Option<String> {
    let diff = meta.get("diff")?;
    match diff {
        Value::String(s) => Some(s.clone()),
        Value::Null => None,
        other => Some(format_json(other)),
    }
}

/// tool/call `arguments` 格式化：对象/数组 pretty JSON；字符串原样（模型
/// 原始 JSON 字符串——若可解析为 JSON 再 pretty，否则保留原文）。
pub fn format_args_json(v: &Value) -> String {
    match v {
        Value::String(s) => match serde_json::from_str::<Value>(s) {
            Ok(parsed) if !parsed.is_null() => format_json(&parsed),
            _ => s.clone(),
        },
        other => format_json(other),
    }
}

fn format_json(v: &Value) -> String {
    match serde_json::to_string_pretty(v) {
        Ok(pretty) => pretty,
        Err(_) => v.to_string(),
    }
}

/// timing 推导（`[未验证]`）：step_start = 同 step step/start.time；
/// first_token = 同 step assistant/message.time；completed 由调用方传入
/// （tool/call = 同 callId result.time；assistant = 同 step step/end.time）。
fn timing_for(
    started_at: Option<i64>,
    window: &TrajectoryWindow,
    turn: Option<u64>,
    step: Option<u64>,
    completed: Option<i64>,
) -> Option<TrajTiming> {
    if started_at.is_none() && completed.is_none() {
        return None;
    }
    let step_start = window
        .raw_rows()
        .find(|r| same_step(r, turn, step) && r.kind() == TrajKind::StepStart)
        .and_then(|r| r.time());
    let first_token = window
        .raw_rows()
        .find(|r| same_step(r, turn, step) && r.kind() == TrajKind::AssistantMessage)
        .and_then(|r| r.time());
    let time_seconds = match (started_at, completed) {
        (Some(s), Some(c)) if c >= s => Some((c - s) as f64 / 1000.0),
        _ => None,
    };
    Some(TrajTiming {
        started_at,
        time_seconds,
        step_start,
        first_token,
        completed,
    })
}
